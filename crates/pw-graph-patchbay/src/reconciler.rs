//! Persistent connection sets and activation policies.
//!
//! The native qpwgraph format is XML and resolves rules by node/port names.
//! JSON remains supported as a convenient machine-readable format for tooling
//! and for compatibility with the first Rust prototype.

use super::error::PatchbayError;
use super::model::{
    Patchbay, PatchbayReconciler, PatchbayResolution, ReconcileReport, ReconcileRuleStatus,
    ReconcileStatus, RetryState,
};
use pw_graph_backend::GraphDriver;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

impl PatchbayReconciler {
    pub fn new() -> Self {
        Self {
            pending: false,
            next_run: None,
            last_graph_generation: 0,
            retry_state: BTreeMap::new(),
            last_report: ReconcileReport::default(),
            debounce: Duration::from_millis(100),
        }
    }

    pub fn mark_dirty(&mut self, now: Instant) {
        self.pending = true;
        // A new graph generation is a fresh opportunity after a transient
        // backend failure. Retrying is still bounded for that generation,
        // while a destroy/recreate event does not leave a route permanently
        // stuck at its old failure count.
        self.retry_state.clear();
        self.next_run = Some(now + self.debounce);
    }

    pub fn schedule_now(&mut self, now: Instant) {
        self.pending = true;
        self.retry_state.clear();
        self.next_run = Some(now);
    }

    pub fn deactivate(&mut self) {
        self.pending = false;
        self.next_run = None;
        self.retry_state.clear();
    }

    pub fn last_report(&self) -> &ReconcileReport {
        &self.last_report
    }

    pub fn last_graph_generation(&self) -> u64 {
        self.last_graph_generation
    }

    fn retry_status(
        &mut self,
        rule_index: usize,
        now: Instant,
        error: impl Into<String>,
    ) -> ReconcileRuleStatus {
        let state = self.retry_state.entry(rule_index).or_insert(RetryState {
            attempts: 0,
            next_retry: now,
        });
        state.attempts = state.attempts.saturating_add(1).min(6);
        let delay = Duration::from_millis(100_u64 << state.attempts.min(5));
        state.next_retry = now + delay;
        if state.attempts < 6 {
            self.pending = true;
            self.next_run = Some(
                self.next_run
                    .map_or(state.next_retry, |current| current.min(state.next_retry)),
            );
        }
        ReconcileRuleStatus {
            rule_index,
            status: if state.attempts >= 6 {
                ReconcileStatus::Failed
            } else {
                ReconcileStatus::Retrying
            },
            detail: error.into(),
        }
    }

    pub fn reconcile_if_due(
        &mut self,
        patchbay: &Patchbay,
        driver: &mut dyn GraphDriver,
        exclusive: bool,
        auto_disconnect: bool,
        now: Instant,
        graph_generation: u64,
    ) -> Result<Option<ReconcileReport>, PatchbayError> {
        if !self.pending || self.next_run.is_some_and(|next| now < next) {
            return Ok(None);
        }
        self.pending = false;
        self.next_run = None;
        self.last_graph_generation = graph_generation;
        if let Err(error) = driver.refresh() {
            let retry = self.retry_status(usize::MAX, now, error.to_string());
            let mut report = ReconcileReport::default();
            report.warnings.push(format!(
                "graph refresh: {} ({})",
                retry.detail,
                retry.status.label()
            ));
            self.last_report = report.clone();
            return Ok(Some(report));
        }

        let mut report = ReconcileReport::default();
        let mut resolved = Vec::new();
        let mut all_rules_resolved = true;
        for (rule_index, _connection) in patchbay.connections.iter().enumerate() {
            match patchbay.resolve_rule(driver.graph(), rule_index) {
                PatchbayResolution::Resolved { output, input } => {
                    resolved.push((rule_index, output, input));
                }
                PatchbayResolution::WaitingForEndpoint { detail } => {
                    all_rules_resolved = false;
                    report.rules.push(ReconcileRuleStatus {
                        rule_index,
                        status: ReconcileStatus::WaitingForEndpoint,
                        detail,
                    });
                }
                PatchbayResolution::Ambiguous { detail } => {
                    all_rules_resolved = false;
                    report.rules.push(ReconcileRuleStatus {
                        rule_index,
                        status: ReconcileStatus::Ambiguous,
                        detail,
                    });
                }
            }
        }

        // Exclusive removal is only safe when the complete desired set is
        // known. A disappearing app must never make us tear down unrelated
        // session-manager links while its rule is merely waiting.
        if exclusive && all_rules_resolved {
            let desired: Vec<_> = resolved
                .iter()
                .filter_map(|(_, output, input)| {
                    Some((
                        driver.graph().port_key(*output)?,
                        driver.graph().port_key(*input)?,
                    ))
                })
                .collect();
            // A link can disappear between selector resolution and this
            // snapshot. Never interpret an incomplete desired set as an
            // instruction to remove every mutable session-manager link.
            if desired.len() == resolved.len() {
                let live: Vec<_> = driver
                    .graph()
                    .links
                    .values()
                    .filter(|link| driver.is_link_mutable(link.id))
                    .filter_map(|link| {
                        Some((
                            driver.graph().port_key(link.output_port)?,
                            driver.graph().port_key(link.input_port)?,
                        ))
                    })
                    .collect();
                let mut cleanup_failed = false;
                for (live_output, live_input) in live {
                    if desired
                        .iter()
                        .any(|(output, input)| output == &live_output && input == &live_input)
                    {
                        continue;
                    }
                    match driver
                        .disconnect_by_key_if_present_without_suppression(&live_output, &live_input)
                    {
                        Ok(Some(_)) => report.disconnected += 1,
                        Ok(None) => {}
                        Err(error) => {
                            let retry = self.retry_status(usize::MAX, now, error.to_string());
                            report.warnings.push(format!(
                                "exclusive cleanup: {} ({})",
                                retry.detail,
                                retry.status.label()
                            ));
                            cleanup_failed = true;
                            break;
                        }
                    }
                }
                if !cleanup_failed {
                    self.retry_state.remove(&usize::MAX);
                }
            }
        }

        'desired_rules: for (rule_index, output, input) in resolved {
            if self
                .retry_state
                .get(&rule_index)
                .is_some_and(|state| state.next_retry > now)
            {
                report.rules.push(ReconcileRuleStatus {
                    rule_index,
                    status: ReconcileStatus::Retrying,
                    detail: "waiting for the next bounded retry".into(),
                });
                continue;
            }
            let Some(output_key) = driver.graph().port_key(output) else {
                report.rules.push(self.retry_status(
                    rule_index,
                    now,
                    "source port disappeared during reconciliation",
                ));
                continue;
            };
            let Some(input_key) = driver.graph().port_key(input) else {
                report.rules.push(self.retry_status(
                    rule_index,
                    now,
                    "destination port disappeared during reconciliation",
                ));
                continue;
            };
            if driver
                .graph()
                .find_link_by_keys(&output_key, &input_key)
                .is_some()
            {
                report.already_present += 1;
                report.rules.push(ReconcileRuleStatus {
                    rule_index,
                    status: ReconcileStatus::Satisfied,
                    detail: "route already present".into(),
                });
                self.retry_state.remove(&rule_index);
                continue;
            }
            if auto_disconnect {
                let Some(input_id) = driver.graph().resolve_port_key(&input_key) else {
                    report.rules.push(self.retry_status(
                        rule_index,
                        now,
                        "destination port disappeared before auto-disconnect",
                    ));
                    continue;
                };
                let stale: Vec<_> = driver
                    .graph()
                    .links_for_port(input_id)
                    .filter(|link| link.input_port == input_id && driver.is_link_mutable(link.id))
                    .filter_map(|link| {
                        Some((
                            driver.graph().port_key(link.output_port)?,
                            driver.graph().port_key(link.input_port)?,
                        ))
                    })
                    .collect();
                for (stale_output, stale_input) in stale {
                    if let Err(error) = driver.disconnect_by_key_if_present_without_suppression(
                        &stale_output,
                        &stale_input,
                    ) {
                        report
                            .rules
                            .push(self.retry_status(rule_index, now, error.to_string()));
                        continue 'desired_rules;
                    }
                    report.disconnected += 1;
                }
            }
            match driver.connect_by_key_if_missing(&output_key, &input_key) {
                Ok(Some(_)) => {
                    report.connected += 1;
                    report.rules.push(ReconcileRuleStatus {
                        rule_index,
                        status: ReconcileStatus::Satisfied,
                        detail: "route connected".into(),
                    });
                    self.retry_state.remove(&rule_index);
                }
                Ok(None) => {
                    report.already_present += 1;
                    self.retry_state.remove(&rule_index);
                    report.rules.push(ReconcileRuleStatus {
                        rule_index,
                        status: ReconcileStatus::Satisfied,
                        detail: "route appeared during reconciliation".into(),
                    });
                }
                Err(error) => {
                    report
                        .rules
                        .push(self.retry_status(rule_index, now, error.to_string()));
                }
            }
        }
        self.last_report = report.clone();
        Ok(Some(report))
    }
}
