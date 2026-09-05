//! Control-plane ownership for process-loopback captures.
//!
//! Process loopback is useful for more than rerouting.  A relay source and a
//! true RMS meter may both want the same application's PCM while the
//! application continues to play through its normal Windows endpoint.  This
//! module keeps those lifetime decisions in one place and, importantly, keys
//! them by a verified selector/PID generation rather than by a bare PID.
//!
//! The first consumer implemented here is the Windows session RMS meter.  The
//! relay still owns its realtime source because it has a different lifetime
//! and format boundary, but it uses the same selector verification before it
//! starts.  The registry types are deliberately reusable when those streams
//! are later fanned out from one activation.

use super::app_route_policy::verify_live_process_identity;
#[cfg(test)]
use super::app_route_policy::ProcessIdentity;
use super::process_loopback::{ProcessLoopbackMode, ProcessLoopbackSource};
use crate::api::{BackendError, BackendResult};
use crate::router::{AudioFormat, AudioSource, RingSource, StreamHealth};
use pw_graph_core::NodeId;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

const CAPTURE_RING_FRAMES: usize = 4_096;
const METER_BLOCK_FRAMES: usize = 480;
const RETRY_BACKOFF: Duration = Duration::from_secs(5);

/// A process capture identity.  `generation` comes from the activation, not
/// from Windows' PID, so a later activation of a reused PID is distinct.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProcessCaptureKey {
    pub selector: String,
    pub pid: u32,
    pub generation: u64,
    pub mode: ProcessLoopbackMode,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum CaptureIdentity {
    Selector {
        selector: String,
        pid: u32,
        mode: ProcessLoopbackMode,
    },
}

/// Consumers are tracked even before stream sharing is optimized.  This
/// prevents a later consumer from accidentally stopping a capture still in
/// use by a meter or diagnostic request.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ProcessCaptureConsumer {
    Meter(NodeId),
    Relay,
    OwnedRoute,
    Diagnostics,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessCaptureState {
    Active,
    Unavailable { reason: String },
    Lost { reason: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProcessMeterReading {
    pub rms: f32,
    pub peak: f32,
    pub age_ms: u32,
    pub available: bool,
    pub state: ProcessCaptureState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessCaptureStatus {
    pub key: ProcessCaptureKey,
    pub consumers: BTreeSet<ProcessCaptureConsumer>,
    pub state: ProcessCaptureState,
    pub last_error: Option<String>,
}

/// A target requested by the current meter policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessMeterTarget {
    pub node_id: NodeId,
    pub selector: String,
    pub pid: u32,
    pub mode: ProcessLoopbackMode,
}

/// A non-meter consumer that needs a capture to stay alive.  Route restore
/// uses this shape so the capture manager can own the same activation/backoff
/// policy as the meter path without inventing a graph edge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessCaptureRequest {
    pub selector: String,
    pub pid: u32,
    pub mode: ProcessLoopbackMode,
}

struct ActiveCapture {
    key: ProcessCaptureKey,
    source: RingSource,
    loopback: ProcessLoopbackSource,
    consumers: BTreeSet<ProcessCaptureConsumer>,
    state: ProcessCaptureState,
    last_error: Option<String>,
    last_meter: Option<(f32, f32, Instant)>,
    scratch: Vec<f32>,
}

struct FailedCapture {
    error: String,
    attempted_at: Instant,
    state: ProcessCaptureState,
}

/// The worker-thread manager for capture-only process consumers.
///
/// It never exposes a process-loopback source as a graph edge.  The only
/// graph-owned process source remains the isolated route path in
/// `windows::routing`; this manager is for read-only consumers.
#[derive(Default)]
pub struct ProcessCaptureManager {
    active: BTreeMap<ProcessCaptureKey, ActiveCapture>,
    failed: BTreeMap<CaptureIdentity, FailedCapture>,
    meter_targets: BTreeMap<NodeId, ProcessMeterTarget>,
    route_targets: BTreeSet<CaptureIdentity>,
    external_relay: Option<ProcessCaptureKey>,
}

impl ProcessCaptureManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reconcile the captures required by the current meter policy.  A
    /// selector that disappears releases only its capture worker; a selector
    /// returning with a new PID gets a new activation and generation.
    pub fn reconcile_meters(
        &mut self,
        targets: impl IntoIterator<Item = ProcessMeterTarget>,
        format: AudioFormat,
    ) {
        self.meter_targets = targets
            .into_iter()
            .filter(|target| {
                target.pid != 0 && validate_capture_selector(&target.selector, target.pid).is_ok()
            })
            .map(|target| (target.node_id, target))
            .collect();
        self.reconcile(format);
    }

    /// Reconcile captures required by saved application routes.  Meter and
    /// route leases are combined before any activation is opened, so a route
    /// cannot stop a capture that a visible meter still owns.
    pub fn reconcile_routes(
        &mut self,
        requests: impl IntoIterator<Item = ProcessCaptureRequest>,
        format: AudioFormat,
    ) {
        self.route_targets = requests
            .into_iter()
            .filter(|request| validate_capture_selector(&request.selector, request.pid).is_ok())
            .map(|request| CaptureIdentity::Selector {
                selector: request.selector,
                pid: request.pid,
                mode: request.mode,
            })
            .collect();
        self.reconcile(format);
    }

    /// Register the realtime relay activation that is owned by the relay
    /// worker rather than by this manager. The manager still records it as a
    /// shared consumer and exposes the same generation in diagnostics, while
    /// deliberately avoiding a second activation for the same process.
    pub fn set_external_relay(&mut self, key: Option<ProcessCaptureKey>) {
        self.external_relay = key;
        let relay_identity = self.external_relay.as_ref().map(capture_identity);
        for capture in self.active.values_mut() {
            let identity = capture_identity(&capture.key);
            if relay_identity.as_ref() == Some(&identity) {
                capture.consumers.insert(ProcessCaptureConsumer::Relay);
            } else {
                capture.consumers.remove(&ProcessCaptureConsumer::Relay);
            }
        }
    }

    fn reconcile(&mut self, format: AudioFormat) {
        let wanted = self.wanted_identities();
        // A failed activation is useful only while its selector/PID is still
        // requested. Once a session disappears, discard the diagnostic entry
        // as well; otherwise a long-running worker would accumulate one
        // stale status for every short-lived process (and falsely report
        // those old captures forever).
        self.failed.retain(|identity, _| wanted.contains(identity));
        let consumers: BTreeMap<_, _> = wanted
            .iter()
            .map(|identity| (identity.clone(), self.consumers_for(identity)))
            .collect();

        // A lost worker must not be treated as a healthy lease forever. Keep
        // its failure in the bounded backoff table so a dead PID is not
        // hammered while the session notification catches up.
        let lost: Vec<_> = self
            .active
            .values()
            .filter(|capture| matches!(capture.state, ProcessCaptureState::Lost { .. }))
            .map(|capture| (capture_identity(&capture.key), capture.last_error.clone()))
            .collect();
        for (identity, error) in lost {
            let message = error.unwrap_or_else(|| "process-loopback source was lost".into());
            self.failed.insert(
                identity,
                FailedCapture {
                    error: message.clone(),
                    attempted_at: Instant::now(),
                    state: ProcessCaptureState::Lost { reason: message },
                },
            );
        }

        self.active.retain(|key, capture| {
            let identity = capture_identity(key);
            if wanted.contains(&identity) && matches!(capture.state, ProcessCaptureState::Active) {
                capture.consumers = consumers.get(&identity).cloned().unwrap_or_default();
                true
            } else {
                capture.loopback.stop();
                false
            }
        });

        for identity in wanted {
            if let Some(capture) = self
                .active
                .values_mut()
                .find(|capture| capture_identity(&capture.key) == identity)
            {
                capture.consumers = consumers.get(&identity).cloned().unwrap_or_default();
                continue;
            }
            if self
                .failed
                .get(&identity)
                .is_some_and(|failure| failure.attempted_at.elapsed() < RETRY_BACKOFF)
            {
                continue;
            }

            let (selector, pid, mode) = identity_parts(&identity);
            if let Err(error) = verify_live_process_identity(&selector, pid) {
                let message = error.to_string();
                self.failed.insert(
                    identity,
                    FailedCapture {
                        error: message.clone(),
                        attempted_at: Instant::now(),
                        state: ProcessCaptureState::Unavailable { reason: message },
                    },
                );
                continue;
            }
            match ProcessLoopbackSource::open(pid, mode, format, CAPTURE_RING_FRAMES) {
                Ok((source, loopback)) => {
                    let key = ProcessCaptureKey {
                        selector,
                        pid,
                        generation: loopback.generation(),
                        mode,
                    };
                    self.active.insert(
                        key.clone(),
                        ActiveCapture {
                            key,
                            source,
                            loopback,
                            consumers: consumers.get(&identity).cloned().unwrap_or_default(),
                            state: ProcessCaptureState::Active,
                            last_error: None,
                            last_meter: None,
                            scratch: vec![0.0; format.samples(METER_BLOCK_FRAMES)],
                        },
                    );
                    self.failed.remove(&identity);
                }
                Err(error) => {
                    let message = error.to_string();
                    self.failed.insert(
                        identity,
                        FailedCapture {
                            error: message.clone(),
                            attempted_at: Instant::now(),
                            state: ProcessCaptureState::Unavailable { reason: message },
                        },
                    );
                }
            }
        }
    }

    fn wanted_identities(&self) -> BTreeSet<CaptureIdentity> {
        self.meter_targets
            .values()
            .map(|target| CaptureIdentity::Selector {
                selector: target.selector.clone(),
                pid: target.pid,
                mode: target.mode,
            })
            .chain(self.route_targets.iter().cloned())
            .collect()
    }

    fn consumers_for(&self, identity: &CaptureIdentity) -> BTreeSet<ProcessCaptureConsumer> {
        let mut consumers = BTreeSet::new();
        for target in self.meter_targets.values() {
            if identity_matches_target(identity, target) {
                consumers.insert(ProcessCaptureConsumer::Meter(target.node_id));
            }
        }
        if self.route_targets.contains(identity) {
            consumers.insert(ProcessCaptureConsumer::OwnedRoute);
        }
        if self
            .external_relay
            .as_ref()
            .is_some_and(|key| capture_identity(key) == *identity)
        {
            consumers.insert(ProcessCaptureConsumer::Relay);
        }
        consumers
    }

    /// Read one non-blocking block for a node and calculate true RMS and peak
    /// from the process PCM.  Native Core Audio peak remains the caller's
    /// fallback when this returns `None`.
    pub fn meter(&mut self, node_id: NodeId) -> Option<ProcessMeterReading> {
        let capture = self.active.values_mut().find(|capture| {
            capture
                .consumers
                .contains(&ProcessCaptureConsumer::Meter(node_id))
        });
        let capture = capture?;

        if !capture.loopback.is_running() {
            let reason = "process-loopback worker stopped".to_owned();
            capture.state = ProcessCaptureState::Lost {
                reason: reason.clone(),
            };
            capture.last_error = Some(reason.clone());
            return Some(ProcessMeterReading {
                rms: 0.0,
                peak: 0.0,
                age_ms: u32::MAX,
                available: false,
                state: capture.state.clone(),
            });
        }

        let read = capture.source.read(&mut capture.scratch);
        if read.health == StreamHealth::Lost {
            let reason = "process-loopback source was lost".to_owned();
            capture.state = ProcessCaptureState::Lost {
                reason: reason.clone(),
            };
            capture.last_error = Some(reason.clone());
            return Some(ProcessMeterReading {
                rms: 0.0,
                peak: 0.0,
                age_ms: u32::MAX,
                available: false,
                state: capture.state.clone(),
            });
        }

        if read.frames > 0 {
            let samples = read.frames * capture.source.format().channels as usize;
            let block = &capture.scratch[..samples.min(capture.scratch.len())];
            let sum = block.iter().map(|sample| sample * sample).sum::<f32>();
            let peak = block
                .iter()
                .map(|sample| sample.abs())
                .fold(0.0f32, f32::max)
                .clamp(0.0, 1.0);
            let rms = (sum / block.len().max(1) as f32).sqrt().clamp(0.0, 1.0);
            capture.last_meter = Some((rms, peak, Instant::now()));
        }

        let Some((rms, peak, timestamp)) = capture.last_meter else {
            return Some(ProcessMeterReading {
                rms: 0.0,
                peak: 0.0,
                age_ms: u32::MAX,
                available: false,
                state: capture.state.clone(),
            });
        };
        Some(ProcessMeterReading {
            rms,
            peak,
            age_ms: timestamp.elapsed().as_millis().min(u32::MAX as u128) as u32,
            available: true,
            state: capture.state.clone(),
        })
    }

    pub fn statuses(&self) -> Vec<ProcessCaptureStatus> {
        let mut statuses: Vec<_> = self
            .active
            .values()
            .map(|capture| ProcessCaptureStatus {
                key: capture.key.clone(),
                consumers: capture.consumers.clone(),
                state: capture.state.clone(),
                last_error: capture.last_error.clone(),
            })
            .collect();
        statuses.extend(self.failed.iter().map(|(identity, failure)| {
            let (selector, pid, mode) = match identity {
                CaptureIdentity::Selector {
                    selector,
                    pid,
                    mode,
                } => (selector.clone(), *pid, *mode),
            };
            ProcessCaptureStatus {
                key: ProcessCaptureKey {
                    selector,
                    pid,
                    generation: 0,
                    mode,
                },
                consumers: self.consumers_for(identity),
                state: failure.state.clone(),
                last_error: Some(failure.error.clone()),
            }
        }));
        if let Some(key) = &self.external_relay {
            let identity = capture_identity(key);
            if !self
                .active
                .keys()
                .any(|active| capture_identity(active) == identity)
            {
                statuses.push(ProcessCaptureStatus {
                    key: key.clone(),
                    consumers: BTreeSet::from([ProcessCaptureConsumer::Relay]),
                    state: ProcessCaptureState::Active,
                    last_error: None,
                });
            }
        }
        statuses.sort_by(|left, right| left.key.cmp(&right.key));
        statuses
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    pub fn clear_failures(&mut self) {
        self.failed.clear();
    }
}

fn capture_identity(key: &ProcessCaptureKey) -> CaptureIdentity {
    CaptureIdentity::Selector {
        selector: key.selector.clone(),
        pid: key.pid,
        mode: key.mode,
    }
}

fn identity_parts(identity: &CaptureIdentity) -> (String, u32, ProcessLoopbackMode) {
    match identity {
        CaptureIdentity::Selector {
            selector,
            pid,
            mode,
        } => (selector.clone(), *pid, *mode),
    }
}

fn identity_matches_target(identity: &CaptureIdentity, target: &ProcessMeterTarget) -> bool {
    matches!(
        identity,
        CaptureIdentity::Selector {
            selector,
            pid,
            mode,
        } if selector == &target.selector && *pid == target.pid && *mode == target.mode
    )
}

impl Drop for ProcessCaptureManager {
    fn drop(&mut self) {
        for capture in self.active.values_mut() {
            capture.loopback.stop();
        }
    }
}

/// Keep the error type in the module's public API useful to callers adding a
/// consumer without forcing them to construct an activation themselves.
pub fn validate_capture_selector(selector: &str, pid: u32) -> BackendResult<()> {
    if selector.trim().is_empty() {
        return Err(BackendError::unsupported(
            "process capture requires a stable application selector",
        ));
    }
    if pid == 0 {
        return Err(BackendError::unsupported(
            "process capture requires a live process",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_validation_rejects_pid_only_capture_requests() {
        assert!(validate_capture_selector("", 10).is_err());
        assert!(validate_capture_selector("sha256:abc", 0).is_err());
        assert!(validate_capture_selector("sha256:abc", 10).is_ok());
    }

    #[test]
    fn capture_key_keeps_generation_and_mode_in_identity() {
        let first = ProcessCaptureKey {
            selector: "sha256:abc".into(),
            pid: 10,
            generation: 1,
            mode: ProcessLoopbackMode::IncludeProcessTree,
        };
        let second = ProcessCaptureKey {
            generation: 2,
            ..first.clone()
        };
        assert_ne!(first, second);
    }

    #[test]
    fn activation_rechecks_the_live_process_identity() {
        let pid = std::process::id();
        let identity = ProcessIdentity::from_pid(pid).expect("the test process is queryable");
        let selector = identity
            .selector_key()
            .expect("the test process has a stable executable identity");

        assert!(verify_live_process_identity(&selector, pid).is_ok());
        assert!(verify_live_process_identity("sha256:not-the-test-process", pid).is_err());
    }

    #[test]
    fn external_relay_activation_is_visible_without_duplicate_capture() {
        let key = ProcessCaptureKey {
            selector: "aumid:example".into(),
            pid: 42,
            generation: 7,
            mode: ProcessLoopbackMode::IncludeProcessTree,
        };
        let mut manager = ProcessCaptureManager::new();

        manager.set_external_relay(Some(key.clone()));
        let statuses = manager.statuses();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].key, key);
        assert_eq!(
            statuses[0].consumers,
            BTreeSet::from([ProcessCaptureConsumer::Relay])
        );
        assert_eq!(statuses[0].state, ProcessCaptureState::Active);
        assert_eq!(manager.active_count(), 0);

        manager.set_external_relay(None);
        assert!(manager.statuses().is_empty());
    }

    #[test]
    fn stale_failed_capture_statuses_are_released_with_their_target() {
        let identity = CaptureIdentity::Selector {
            selector: "sha256:gone".into(),
            pid: 42,
            mode: ProcessLoopbackMode::IncludeProcessTree,
        };
        let mut manager = ProcessCaptureManager::new();
        manager.failed.insert(
            identity,
            FailedCapture {
                error: "target exited".into(),
                attempted_at: Instant::now(),
                state: ProcessCaptureState::Lost {
                    reason: "target exited".into(),
                },
            },
        );

        manager.reconcile_meters(std::iter::empty(), AudioFormat::new(48_000, 2));

        assert!(manager.statuses().is_empty());
    }
}
