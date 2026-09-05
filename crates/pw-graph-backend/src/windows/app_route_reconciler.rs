//! Reconcile persisted Windows application routes against live Core Audio.
//!
//! A saved rule is not a command to blindly reconnect a PID.  It is a
//! durable identity plus a set of safety conditions that must all be true at
//! the same time.  Keeping that decision in a small state machine makes
//! refreshes, process restarts, endpoint churn, and missing driver support
//! observable instead of scattering partially-applied restore logic through
//! the graph worker.

use super::{AudioFlow, WindowsEndpointSelector};
use pw_graph_config::{WindowsApplicationRoute, WindowsApplicationSelector};
use pw_graph_effects::EffectInstanceConfig;
use std::collections::BTreeMap;

/// The states exposed to diagnostics and the UI for a persisted rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplicationRouteState {
    Disabled,
    WaitingForApplication,
    WaitingForIsolation,
    ActivatingCapture,
    ResolvingDestination,
    Active,
    UnsupportedOs,
    IdentityMismatch,
    VirtualDriverMissing,
    VirtualOutputNotSelected,
    DestinationMissing,
    ProcessCaptureFailed,
    EffectRestoreFailed,
    Degraded,
}

/// Readiness of the process-loopback capture needed before a local rerender
/// can be activated.  `Starting` is intentionally distinct from `Ready` so
/// the UI can explain why a saved route has not yet become active.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessCaptureReadiness {
    NotRequested,
    Starting,
    Ready,
    Failed(String),
}

/// A live render session candidate.  The selector is copied from the verified
/// process identity; `isolated` is true only when the session is proven to
/// render to QPWGraph Virtual Output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationRouteCandidate {
    pub selector: WindowsApplicationSelector,
    pub pid: u32,
    pub isolated: bool,
}

/// A plan emitted by the reconciler.  The caller may use `activation` only
/// after it has accepted the route transactionally.  A plan is safe to retry:
/// it contains the live PID for this observation, never a persisted PID.
#[derive(Clone, Debug, PartialEq)]
pub struct ApplicationRoutePlan {
    pub rule_index: usize,
    pub state: ApplicationRouteState,
    pub reason: Option<String>,
    pub application: Option<ApplicationRouteCandidate>,
    pub destination: Option<WindowsEndpointSelector>,
    pub activation: Option<ApplicationRouteActivation>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApplicationRouteActivation {
    pub rule_index: usize,
    pub selector: WindowsApplicationSelector,
    pub pid: u32,
    pub destination: WindowsEndpointSelector,
    pub effect_chain: Vec<String>,
    pub effect_instances: Vec<EffectInstanceConfig>,
    pub gain: f32,
}

/// Live facts used during one reconciliation pass.  The worker owns the
/// facts; this value is an owned snapshot so a plan can be inspected or
/// tested without borrowing a COM object.
#[derive(Clone, Debug, Default)]
pub struct ApplicationRouteEnvironment {
    pub os_supported: bool,
    pub virtual_driver_ready: bool,
    pub effects_available: bool,
    pub applications: Vec<ApplicationRouteCandidate>,
    pub endpoints: Vec<WindowsEndpointSelector>,
    /// Capture readiness is tied to both the stable selector and the live
    /// PID.  A relay or a stale failed activation for the same application
    /// must never make a newly resolved PID look ready.
    pub captures: BTreeMap<(String, u32), ProcessCaptureReadiness>,
}

/// Persisted-route state and the last decision for each configured rule.
#[derive(Clone, Debug, Default)]
pub struct ApplicationRouteReconciler {
    rules: Vec<WindowsApplicationRoute>,
    plans: BTreeMap<usize, ApplicationRoutePlan>,
}

impl ApplicationRouteReconciler {
    pub fn new(rules: Vec<WindowsApplicationRoute>) -> Self {
        Self {
            rules,
            plans: BTreeMap::new(),
        }
    }

    pub fn set_rules(&mut self, rules: Vec<WindowsApplicationRoute>) {
        self.rules = rules;
        self.plans.clear();
    }

    pub fn rules(&self) -> &[WindowsApplicationRoute] {
        &self.rules
    }

    pub fn plans(&self) -> impl Iterator<Item = &ApplicationRoutePlan> {
        self.plans.values()
    }

    pub fn plan(&self, rule_index: usize) -> Option<&ApplicationRoutePlan> {
        self.plans.get(&rule_index)
    }

    pub fn mark_degraded(&mut self, rule_index: usize, reason: String) {
        if let Some(plan) = self.plans.get_mut(&rule_index) {
            plan.state = ApplicationRouteState::Degraded;
            plan.reason = Some(reason);
            plan.activation = None;
        }
    }

    /// Mark a route as unable to restore its persisted processor chain. This
    /// is distinct from a transient application/endpoint degradation so the
    /// UI and diagnostics can tell a missing effect host from a missing app.
    pub fn mark_effect_restore_failed(&mut self, rule_index: usize, reason: String) {
        if let Some(plan) = self.plans.get_mut(&rule_index) {
            plan.state = ApplicationRouteState::EffectRestoreFailed;
            plan.reason = Some(reason);
            plan.activation = None;
        }
    }

    /// Reconcile every persisted rule against the current live snapshot.
    ///
    /// More specific matching rules win.  A less specific duplicate is
    /// reported as `Degraded` instead of being activated a second time.
    pub fn reconcile(&mut self, environment: &ApplicationRouteEnvironment) {
        self.plans.clear();
        for (rule_index, rule) in self.rules.iter().enumerate() {
            let plan = self.reconcile_rule(rule_index, rule, environment);
            self.plans.insert(rule_index, plan);
        }
    }

    /// Capture leases that should be handed to the process-capture control
    /// layer. This is deliberately separate from route activation: opening
    /// a capture is a prerequisite, not permission to create a local dry +
    /// processed route. Active plans remain in the lease set so a later
    /// refresh does not tear down the capture that backs the route.
    pub fn capture_requests(&self) -> Vec<(usize, WindowsApplicationSelector, u32)> {
        self.plans
            .values()
            .filter(|plan| {
                matches!(
                    plan.state,
                    ApplicationRouteState::ActivatingCapture | ApplicationRouteState::Active
                )
            })
            .filter_map(|plan| {
                plan.application.as_ref().map(|application| {
                    (
                        plan.rule_index,
                        application.selector.clone(),
                        application.pid,
                    )
                })
            })
            .collect()
    }

    /// Copy the strongest endpoint identity available for a restored legacy
    /// route into the new selector fields.  Old fields remain intact for
    /// backwards compatibility, but the next config save now carries the
    /// stable/current pair needed for future resolution.
    pub fn migrate_destination_selectors(&mut self, endpoints: &[WindowsEndpointSelector]) {
        for rule in &mut self.rules {
            let Some(endpoint) = resolve_destination(rule, endpoints).ok().flatten() else {
                continue;
            };
            if rule.destination_stable_id.is_none() {
                rule.destination_stable_id = endpoint.stable_id.clone();
            }
            if rule.destination_mmdevice_id.is_none() {
                rule.destination_mmdevice_id = endpoint.current_mmdevice_id.clone();
            }
            if rule.destination_name.is_none() {
                rule.destination_name = endpoint.friendly_name.clone();
            }
        }
    }

    fn reconcile_rule(
        &self,
        rule_index: usize,
        rule: &WindowsApplicationRoute,
        environment: &ApplicationRouteEnvironment,
    ) -> ApplicationRoutePlan {
        let base = |state, reason| ApplicationRoutePlan {
            rule_index,
            state,
            reason,
            application: None,
            destination: None,
            activation: None,
        };
        if !rule.enabled {
            return base(
                ApplicationRouteState::Disabled,
                Some("route is disabled".into()),
            );
        }
        if !environment.os_supported {
            return base(
                ApplicationRouteState::UnsupportedOs,
                Some("process capture or local application routing is unsupported on this Windows build".into()),
            );
        }
        let applications: Vec<_> = environment
            .applications
            .iter()
            .filter(|candidate| rule.application.matches(&candidate.selector))
            .cloned()
            .collect();
        let application = match applications.as_slice() {
            [] => {
                return base(
                    ApplicationRouteState::WaitingForApplication,
                    Some("no live render session matches the stable application selector".into()),
                )
            }
            [application] => application.clone(),
            _ => {
                return base(
                    ApplicationRouteState::Degraded,
                    Some(
                        "stable application selector matches multiple live processes; refusing to choose a PID"
                            .into(),
                    ),
                )
            }
        };
        if application.pid == 0 || !application.selector.is_stable() {
            return ApplicationRoutePlan {
                application: Some(application),
                ..base(
                    ApplicationRouteState::IdentityMismatch,
                    Some("live session has no verified stable identity".into()),
                )
            };
        }
        if self
            .rules
            .iter()
            .enumerate()
            .filter(|(_, other)| other.enabled && other.matches_application(&application.selector))
            .min_by(|(left_index, left), (right_index, right)| {
                right
                    .selector_specificity()
                    .cmp(&left.selector_specificity())
                    .then_with(|| left_index.cmp(right_index))
            })
            .is_some_and(|(winner, _)| winner != rule_index)
        {
            return ApplicationRoutePlan {
                application: Some(application),
                ..base(
                    ApplicationRouteState::Degraded,
                    Some("a more specific persisted route owns this application".into()),
                )
            };
        }
        // A persisted application route always rerenders locally. Even an
        // older config with `virtualization_required = false` must not turn
        // an ordinary capture-only session into a dry + processed duplicate.
        if !environment.virtual_driver_ready {
            return ApplicationRoutePlan {
                application: Some(application),
                ..base(
                    ApplicationRouteState::VirtualDriverMissing,
                    Some("the optional virtual endpoint is not ready".into()),
                )
            };
        }
        if !application.isolated {
            return ApplicationRoutePlan {
                application: Some(application),
                ..base(
                    ApplicationRouteState::VirtualOutputNotSelected,
                    Some("the application is not isolated on QPWGraph Virtual Output".into()),
                )
            };
        }

        let capture_key = application
            .selector
            .runtime_key()
            .map(|selector| (selector, application.pid));
        match capture_key
            .as_ref()
            .and_then(|key| environment.captures.get(key))
            .cloned()
            .unwrap_or(ProcessCaptureReadiness::NotRequested)
        {
            ProcessCaptureReadiness::NotRequested | ProcessCaptureReadiness::Starting => {
                return ApplicationRoutePlan {
                    application: Some(application),
                    ..base(
                        ApplicationRouteState::ActivatingCapture,
                        Some("waiting for process-loopback capture".into()),
                    )
                };
            }
            ProcessCaptureReadiness::Failed(reason) => {
                return ApplicationRoutePlan {
                    application: Some(application),
                    ..base(ApplicationRouteState::ProcessCaptureFailed, Some(reason))
                };
            }
            ProcessCaptureReadiness::Ready => {}
        }

        let destination = match resolve_destination(rule, &environment.endpoints) {
            Ok(Some(destination)) => destination,
            Ok(None) => {
                return ApplicationRoutePlan {
                    application: Some(application),
                    ..base(
                        ApplicationRouteState::DestinationMissing,
                        Some("saved destination is not currently available".into()),
                    )
                };
            }
            Err(reason) => {
                return ApplicationRoutePlan {
                    application: Some(application),
                    ..base(ApplicationRouteState::DestinationMissing, Some(reason))
                };
            }
        };

        let effect_instances = match rule.restorable_effect_instances() {
            Ok(effect_instances) => effect_instances,
            Err(reason) => {
                return ApplicationRoutePlan {
                    application: Some(application),
                    destination: Some(destination),
                    ..base(ApplicationRouteState::EffectRestoreFailed, Some(reason))
                }
            }
        };
        if !environment.effects_available && !effect_instances.is_empty() {
            return ApplicationRoutePlan {
                application: Some(application),
                destination: Some(destination),
                ..base(
                    ApplicationRouteState::EffectRestoreFailed,
                    Some("the Windows effect host is unavailable".into()),
                )
            };
        }

        ApplicationRoutePlan {
            rule_index,
            state: ApplicationRouteState::Active,
            reason: None,
            application: Some(application.clone()),
            destination: Some(destination.clone()),
            activation: Some(ApplicationRouteActivation {
                rule_index,
                selector: application.selector,
                pid: application.pid,
                destination,
                effect_chain: rule.effect_chain.clone(),
                effect_instances,
                gain: rule.gain,
            }),
        }
    }
}

fn resolve_destination(
    rule: &WindowsApplicationRoute,
    endpoints: &[WindowsEndpointSelector],
) -> Result<Option<WindowsEndpointSelector>, String> {
    let stable = rule.destination_stable_id.as_deref();
    let current = rule
        .destination_mmdevice_id
        .as_deref()
        .or(rule.destination_endpoint_id.as_deref());
    if let Some(stable) = stable {
        let matches: Vec<_> = endpoints
            .iter()
            .filter(|endpoint| {
                endpoint.data_flow == AudioFlow::Render
                    && endpoint
                        .stable_id
                        .as_deref()
                        .is_some_and(|candidate| candidate == stable)
            })
            .collect();
        match matches.as_slice() {
            [endpoint] => return Ok(Some((*endpoint).clone())),
            [] => {}
            _ => {
                return Err(
                    "saved stable destination ID matches more than one render endpoint".into(),
                )
            }
        }
    }
    if let Some(current) = current {
        let matches: Vec<_> = endpoints
            .iter()
            .filter(|endpoint| {
                endpoint.data_flow == AudioFlow::Render
                    && (endpoint.current_mmdevice_id.as_deref() == Some(current)
                        || endpoint.stable_id.as_deref() == Some(current))
            })
            .collect();
        match matches.as_slice() {
            [endpoint] => return Ok(Some((*endpoint).clone())),
            [] => {}
            _ => {
                return Err(
                    "saved current destination ID matches more than one render endpoint".into(),
                )
            }
        }
    }
    let Some(name) = rule.destination_name.as_deref() else {
        return Ok(None);
    };
    let matches: Vec<_> = endpoints
        .iter()
        .filter(|endpoint| {
            endpoint.data_flow == AudioFlow::Render
                && endpoint
                    .friendly_name
                    .as_deref()
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
        })
        .collect();
    match matches.as_slice() {
        [] => Ok(None),
        [endpoint] => Ok(Some((*endpoint).clone())),
        _ => Err("saved destination name matches more than one render endpoint".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(selector: &str, pid: u32, isolated: bool) -> ApplicationRouteCandidate {
        ApplicationRouteCandidate {
            selector: WindowsApplicationSelector {
                executable_path_hash: Some(selector.into()),
                executable_name: Some("player.exe".into()),
                ..WindowsApplicationSelector::default()
            },
            pid,
            isolated,
        }
    }

    fn endpoint(id: &str, name: &str) -> WindowsEndpointSelector {
        WindowsEndpointSelector {
            stable_id: Some(format!("stable-{id}")),
            current_mmdevice_id: Some(id.into()),
            friendly_name: Some(name.into()),
            data_flow: AudioFlow::Render,
        }
    }

    #[test]
    fn normal_app_waits_for_isolation_before_local_route() {
        let route = WindowsApplicationRoute {
            application: app("sha256:player", 1, false).selector,
            destination_endpoint_id: Some("speaker".into()),
            ..WindowsApplicationRoute::default()
        };
        let mut reconciler = ApplicationRouteReconciler::new(vec![route]);
        let environment = ApplicationRouteEnvironment {
            os_supported: true,
            virtual_driver_ready: true,
            applications: vec![app("sha256:player", 42, false)],
            endpoints: vec![endpoint("speaker", "Speakers")],
            ..ApplicationRouteEnvironment::default()
        };
        reconciler.reconcile(&environment);
        assert_eq!(
            reconciler.plan(0).map(|plan| plan.state),
            Some(ApplicationRouteState::VirtualOutputNotSelected)
        );
        assert!(reconciler.capture_requests().is_empty());
    }

    #[test]
    fn ambiguous_live_instances_do_not_choose_a_pid() {
        let route = WindowsApplicationRoute {
            application: app("sha256:player", 1, true).selector,
            destination_endpoint_id: Some("speaker".into()),
            ..WindowsApplicationRoute::default()
        };
        let mut reconciler = ApplicationRouteReconciler::new(vec![route]);
        let environment = ApplicationRouteEnvironment {
            os_supported: true,
            virtual_driver_ready: true,
            applications: vec![
                app("sha256:player", 41, true),
                app("sha256:player", 42, true),
            ],
            endpoints: vec![endpoint("speaker", "Speakers")],
            captures: BTreeMap::from([(
                ("sha256:player".into(), 42),
                ProcessCaptureReadiness::Ready,
            )]),
            ..ApplicationRouteEnvironment::default()
        };
        reconciler.reconcile(&environment);
        let plan = reconciler.plan(0).expect("a plan is always produced");
        assert_eq!(plan.state, ApplicationRouteState::Degraded);
        assert!(plan
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("multiple live processes")));
        assert!(reconciler.capture_requests().is_empty());
    }

    #[test]
    fn legacy_route_flag_cannot_bypass_isolation_safety_gate() {
        let route = WindowsApplicationRoute {
            application: app("sha256:player", 1, false).selector,
            virtualization_required: false,
            destination_endpoint_id: Some("speaker".into()),
            ..WindowsApplicationRoute::default()
        };
        let mut reconciler = ApplicationRouteReconciler::new(vec![route]);
        let environment = ApplicationRouteEnvironment {
            os_supported: true,
            virtual_driver_ready: true,
            applications: vec![app("sha256:player", 42, false)],
            endpoints: vec![endpoint("speaker", "Speakers")],
            ..ApplicationRouteEnvironment::default()
        };
        reconciler.reconcile(&environment);
        assert_eq!(
            reconciler.plan(0).map(|plan| plan.state),
            Some(ApplicationRouteState::VirtualOutputNotSelected)
        );
    }

    #[test]
    fn restart_uses_new_pid_only_after_capture_is_ready() {
        let route = WindowsApplicationRoute {
            application: app("sha256:player", 1, true).selector,
            destination_endpoint_id: Some("speaker".into()),
            ..WindowsApplicationRoute::default()
        };
        let mut reconciler = ApplicationRouteReconciler::new(vec![route]);
        let mut environment = ApplicationRouteEnvironment {
            os_supported: true,
            virtual_driver_ready: true,
            applications: vec![app("sha256:player", 84, true)],
            endpoints: vec![endpoint("speaker", "Speakers")],
            ..ApplicationRouteEnvironment::default()
        };
        reconciler.reconcile(&environment);
        assert_eq!(
            reconciler.plan(0).map(|plan| plan.state),
            Some(ApplicationRouteState::ActivatingCapture)
        );
        assert_eq!(reconciler.capture_requests()[0].2, 84);
        environment
            .captures
            .insert(("sha256:player".into(), 84), ProcessCaptureReadiness::Ready);
        reconciler.reconcile(&environment);
        let plan = reconciler.plan(0).unwrap();
        assert_eq!(plan.state, ApplicationRouteState::Active);
        assert_eq!(
            plan.activation.as_ref().map(|activation| activation.pid),
            Some(84)
        );
    }

    #[test]
    fn capture_readiness_for_an_old_pid_cannot_activate_a_restarted_app() {
        let route = WindowsApplicationRoute {
            application: app("sha256:player", 1, true).selector,
            destination_endpoint_id: Some("speaker".into()),
            ..WindowsApplicationRoute::default()
        };
        let mut reconciler = ApplicationRouteReconciler::new(vec![route]);
        let environment = ApplicationRouteEnvironment {
            os_supported: true,
            virtual_driver_ready: true,
            applications: vec![app("sha256:player", 85, true)],
            endpoints: vec![endpoint("speaker", "Speakers")],
            captures: BTreeMap::from([(
                ("sha256:player".into(), 84),
                ProcessCaptureReadiness::Ready,
            )]),
            ..ApplicationRouteEnvironment::default()
        };

        reconciler.reconcile(&environment);

        let plan = reconciler.plan(0).expect("a plan is always produced");
        assert_eq!(plan.state, ApplicationRouteState::ActivatingCapture);
        assert!(plan.activation.is_none());
    }

    #[test]
    fn duplicate_friendly_names_are_degraded() {
        let route = WindowsApplicationRoute {
            application: WindowsApplicationSelector {
                executable_path_hash: Some("sha256:player".into()),
                ..WindowsApplicationSelector::default()
            },
            destination_name: Some("Speakers".into()),
            ..WindowsApplicationRoute::default()
        };
        let mut reconciler = ApplicationRouteReconciler::new(vec![route]);
        let environment = ApplicationRouteEnvironment {
            os_supported: true,
            virtual_driver_ready: true,
            applications: vec![app("sha256:player", 84, true)],
            endpoints: vec![endpoint("one", "Speakers"), endpoint("two", "Speakers")],
            captures: [(("sha256:player".into(), 84), ProcessCaptureReadiness::Ready)]
                .into_iter()
                .collect(),
            ..ApplicationRouteEnvironment::default()
        };
        reconciler.reconcile(&environment);
        assert_eq!(
            reconciler.plan(0).map(|plan| plan.state),
            Some(ApplicationRouteState::DestinationMissing)
        );
    }

    #[test]
    fn stable_endpoint_ids_are_case_sensitive() {
        let route = WindowsApplicationRoute {
            application: app("sha256:player", 1, true).selector,
            destination_stable_id: Some("Stable-speaker".into()),
            ..WindowsApplicationRoute::default()
        };
        let mut reconciler = ApplicationRouteReconciler::new(vec![route]);
        let environment = ApplicationRouteEnvironment {
            os_supported: true,
            virtual_driver_ready: true,
            applications: vec![app("sha256:player", 84, true)],
            endpoints: vec![endpoint("speaker", "Speakers")],
            captures: [(("sha256:player".into(), 84), ProcessCaptureReadiness::Ready)]
                .into_iter()
                .collect(),
            ..ApplicationRouteEnvironment::default()
        };
        reconciler.reconcile(&environment);
        assert_eq!(
            reconciler.plan(0).map(|plan| plan.state),
            Some(ApplicationRouteState::DestinationMissing)
        );
    }

    #[test]
    fn duplicate_stable_endpoint_ids_are_degraded() {
        let route = WindowsApplicationRoute {
            application: app("sha256:player", 1, true).selector,
            destination_stable_id: Some("stable-speaker".into()),
            ..WindowsApplicationRoute::default()
        };
        let mut reconciler = ApplicationRouteReconciler::new(vec![route]);
        let environment = ApplicationRouteEnvironment {
            os_supported: true,
            virtual_driver_ready: true,
            applications: vec![app("sha256:player", 84, true)],
            endpoints: vec![endpoint("one", "Speakers"), endpoint("two", "Other")],
            captures: BTreeMap::from([(
                ("sha256:player".into(), 84),
                ProcessCaptureReadiness::Ready,
            )]),
            ..ApplicationRouteEnvironment::default()
        };
        let stable_endpoints = environment
            .endpoints
            .iter()
            .cloned()
            .map(|mut endpoint| {
                endpoint.stable_id = Some("stable-speaker".into());
                endpoint
            })
            .collect();
        let environment = ApplicationRouteEnvironment {
            endpoints: stable_endpoints,
            ..environment
        };

        reconciler.reconcile(&environment);

        assert_eq!(
            reconciler.plan(0).map(|plan| plan.state),
            Some(ApplicationRouteState::DestinationMissing)
        );
        assert!(reconciler
            .plan(0)
            .and_then(|plan| plan.reason.as_deref())
            .is_some_and(|reason| reason.contains("more than one")));
    }

    #[test]
    fn saved_effect_chain_is_not_silently_bypassed() {
        let route = WindowsApplicationRoute {
            application: app("sha256:player", 1, true).selector,
            destination_endpoint_id: Some("speaker".into()),
            effect_chain: vec!["builtin.gain".into()],
            ..WindowsApplicationRoute::default()
        };
        let mut reconciler = ApplicationRouteReconciler::new(vec![route]);
        let environment = ApplicationRouteEnvironment {
            os_supported: true,
            virtual_driver_ready: true,
            applications: vec![app("sha256:player", 42, true)],
            endpoints: vec![endpoint("speaker", "Speakers")],
            captures: [(("sha256:player".into(), 42), ProcessCaptureReadiness::Ready)]
                .into_iter()
                .collect(),
            ..ApplicationRouteEnvironment::default()
        };
        reconciler.reconcile(&environment);
        assert_eq!(
            reconciler.plan(0).map(|plan| plan.state),
            Some(ApplicationRouteState::EffectRestoreFailed)
        );
        assert!(reconciler.capture_requests().is_empty());
    }

    #[test]
    fn complete_effect_instances_are_part_of_the_active_transaction() {
        let route = WindowsApplicationRoute {
            application: app("sha256:player", 1, true).selector,
            destination_endpoint_id: Some("speaker".into()),
            effect_chain: vec!["builtin.noise-gate".into()],
            effect_instances: vec![EffectInstanceConfig {
                instance_id: "route-gate".into(),
                effect_id: "builtin.noise-gate".into(),
                module_path: None,
                enabled: false,
                parameters: [("threshold-db".into(), -42.0)].into_iter().collect(),
            }],
            gain: 0.75,
            ..WindowsApplicationRoute::default()
        };
        let mut reconciler = ApplicationRouteReconciler::new(vec![route]);
        let environment = ApplicationRouteEnvironment {
            os_supported: true,
            virtual_driver_ready: true,
            effects_available: true,
            applications: vec![app("sha256:player", 42, true)],
            endpoints: vec![endpoint("speaker", "Speakers")],
            captures: [(("sha256:player".into(), 42), ProcessCaptureReadiness::Ready)]
                .into_iter()
                .collect(),
        };
        reconciler.reconcile(&environment);
        let plan = reconciler.plan(0).expect("a plan is always produced");
        assert_eq!(plan.state, ApplicationRouteState::Active);
        let activation = plan.activation.as_ref().expect("route is active");
        assert_eq!(activation.effect_instances.len(), 1);
        assert_eq!(activation.effect_instances[0].instance_id, "route-gate");
        assert_eq!(
            activation.effect_instances[0].parameters["threshold-db"],
            -42.0
        );
        assert!(!activation.effect_instances[0].enabled);
        assert_eq!(activation.gain, 0.75);
    }

    #[test]
    fn complete_effect_instances_require_an_available_effect_host() {
        let route = WindowsApplicationRoute {
            application: app("sha256:player", 1, true).selector,
            destination_endpoint_id: Some("speaker".into()),
            effect_instances: vec![EffectInstanceConfig {
                instance_id: "route-gate".into(),
                effect_id: "builtin.noise-gate".into(),
                module_path: None,
                enabled: true,
                parameters: BTreeMap::new(),
            }],
            ..WindowsApplicationRoute::default()
        };
        let mut reconciler = ApplicationRouteReconciler::new(vec![route]);
        let environment = ApplicationRouteEnvironment {
            os_supported: true,
            virtual_driver_ready: true,
            applications: vec![app("sha256:player", 42, true)],
            endpoints: vec![endpoint("speaker", "Speakers")],
            captures: [(("sha256:player".into(), 42), ProcessCaptureReadiness::Ready)]
                .into_iter()
                .collect(),
            ..ApplicationRouteEnvironment::default()
        };
        reconciler.reconcile(&environment);
        assert_eq!(
            reconciler.plan(0).map(|plan| plan.state),
            Some(ApplicationRouteState::EffectRestoreFailed)
        );
        assert!(reconciler.capture_requests().is_empty());
    }
}
