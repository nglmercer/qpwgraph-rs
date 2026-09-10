//! Persistent connection sets and activation policies.
//!
//! The native qpwgraph format is XML and resolves rules by node/port names.
//! JSON remains supported as a convenient machine-readable format for tooling
//! and for compatibility with the first Rust prototype.

use pw_graph_backend::{BackendError, GraphDriver};
use pw_graph_core::{
    Direction, EndpointResolution, EndpointSelector, Graph, NodeIdentity, NodeType, PortId,
    PortKey, PortType,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as FmtWrite;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use thiserror::Error;

mod xml;

#[derive(Debug, Error)]
pub enum PatchbayError {
    #[error("could not read patchbay file: {0}")]
    Read(#[source] std::io::Error),
    #[error("could not write patchbay file: {0}")]
    Write(#[source] std::io::Error),
    #[error("invalid patchbay JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid patchbay XML: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("could not serialize patchbay XML: {0}")]
    XmlWrite(#[source] std::io::Error),
    #[error("patchbay XML contains invalid attributes")]
    XmlAttributes,
    #[error(transparent)]
    Backend(#[from] BackendError),
    /// Activation failed and one or both mutation directions could not be
    /// restored. The graph is in neither the previous nor the saved state, and
    /// the user has to be told rather than shown a bare error.
    #[error(
        "activation failed ({cause}); {created_links_left} created link(s) remain and {removed_links_not_restored} removed link(s) could not be restored"
    )]
    ActivationNotRolledBack {
        cause: String,
        created_links_left: usize,
        removed_links_not_restored: usize,
    },
}

/// Undo the complete mutation made by one activation. Created links are
/// removed first, in reverse order; links removed by exclusive or
/// auto-disconnect policy are then restored, also in reverse order. Both loops
/// deliberately continue after an error so the final report says which side
/// of the original graph remains stranded.
fn rollback_activation(
    driver: &mut dyn GraphDriver,
    created_by_activation: &[(PortKey, PortKey)],
    removed_by_activation: &[(PortKey, PortKey)],
    cause: BackendError,
) -> PatchbayError {
    let mut created_links_left = 0;
    for (output, input) in created_by_activation.iter().rev() {
        match driver.disconnect_by_key_if_present_without_suppression(output, input) {
            Ok(Some(_)) => driver.allow_connection(output, input),
            Ok(None) => {}
            Err(_) => created_links_left += 1,
        }
    }

    let mut removed_links_not_restored = 0;
    for (output, input) in removed_by_activation.iter().rev() {
        if driver.connect_by_key_if_missing(output, input).is_err() {
            removed_links_not_restored += 1;
        }
    }
    if created_links_left == 0 && removed_links_not_restored == 0 {
        PatchbayError::Backend(cause)
    } else {
        PatchbayError::ActivationNotRolledBack {
            cause: cause.to_string(),
            created_links_left,
            removed_links_not_restored,
        }
    }
}

/// Record a successful removal in the activation transaction. A route that
/// this same activation created and then removed is not part of the original
/// graph, so it must be forgotten from the created set rather than restored as
/// though it had existed before activation.
fn record_activation_removal(
    removed_by_activation: &mut Vec<(PortKey, PortKey)>,
    created_by_activation: &mut Vec<(PortKey, PortKey)>,
    output: PortKey,
    input: PortKey,
) {
    if let Some(index) = created_by_activation
        .iter()
        .position(|(created_output, created_input)| {
            created_output == &output && created_input == &input
        })
    {
        created_by_activation.remove(index);
    } else {
        removed_by_activation.push((output, input));
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct Patchbay {
    pub version: u32,
    pub name: String,
    pub connections: Vec<PatchConnection>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PatchConnection {
    #[serde(default)]
    pub output_port: PortId,
    #[serde(default)]
    pub input_port: PortId,
    #[serde(default)]
    pub pinned: bool,
    /// Legacy output-side type used by the qpwgraph-compatible format.
    ///
    /// Older files have a single node type for both endpoints. Keep this
    /// field so those files remain readable, while the optional endpoint
    /// fields below retain the real type of each side for new files.
    #[serde(default)]
    pub node_type: NodeType,
    /// Explicit type of the output node. `None` denotes a legacy rule whose
    /// single `node_type` applied to both endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_node_type: Option<NodeType>,
    /// Explicit type of the input node. `None` denotes a legacy rule whose
    /// single `node_type` applied to both endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_node_type: Option<NodeType>,
    #[serde(default)]
    pub port_type: PortType,
    #[serde(default)]
    pub output_node: String,
    #[serde(default)]
    pub output_name: String,
    #[serde(default)]
    pub input_node: String,
    #[serde(default)]
    pub input_name: String,
    /// Rich selectors are optional so files written by older qpwgraph-rs
    /// versions and native qpwgraph XML remain readable without a rewrite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_selector: Option<EndpointSelector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_selector: Option<EndpointSelector>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct XmlSelectorSidecarEntry {
    #[serde(default)]
    index: Option<usize>,
    fingerprint: String,
    output_selector: Option<EndpointSelector>,
    input_selector: Option<EndpointSelector>,
    #[serde(default)]
    output_node_type: Option<NodeType>,
    #[serde(default)]
    input_node_type: Option<NodeType>,
    #[serde(default)]
    port_type: Option<PortType>,
}

impl PatchConnection {
    fn effective_output_node_type(&self) -> NodeType {
        self.output_node_type.unwrap_or(self.node_type)
    }

    fn effective_input_node_type(&self) -> NodeType {
        self.input_node_type.unwrap_or(self.node_type)
    }

    fn matches_stable_pair(&self, output: &PortKey, input: &PortKey) -> bool {
        let legacy_match = self.output_node == output.node_name
            && self.output_name == output.port_name
            && self.input_node == input.node_name
            && self.input_name == input.port_name;
        let selector_match = self
            .output_selector
            .as_ref()
            .zip(self.input_selector.as_ref())
            .is_some_and(|(saved_output, saved_input)| {
                selectors_equivalent(saved_output, &output.selector())
                    && selectors_equivalent(saved_input, &input.selector())
            });
        // Once a rule has rich selectors, its display names are only a
        // compatibility/cache view. Using `legacy_match || selector_match`
        // here would delete two same-named applications together when one of
        // them is explicitly disconnected.
        if self.output_selector.is_some() && self.input_selector.is_some() {
            selector_match
        } else {
            legacy_match
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActivationReport {
    pub connected: usize,
    pub already_present: usize,
    pub disconnected: usize,
    pub failed: Vec<String>,
    pub waiting: Vec<String>,
    pub ambiguous: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatchbayResolution {
    Resolved { output: PortId, input: PortId },
    WaitingForEndpoint { detail: String },
    Ambiguous { detail: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconcileStatus {
    Satisfied,
    WaitingForEndpoint,
    Ambiguous,
    Retrying,
    Failed,
}

impl ReconcileStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Satisfied => "Satisfied",
            Self::WaitingForEndpoint => "Waiting for endpoint",
            Self::Ambiguous => "Ambiguous",
            Self::Retrying => "Retrying",
            Self::Failed => "Failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconcileRuleStatus {
    pub rule_index: usize,
    pub status: ReconcileStatus,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReconcileReport {
    pub connected: usize,
    pub already_present: usize,
    pub disconnected: usize,
    pub rules: Vec<ReconcileRuleStatus>,
    /// Control-plane warnings which are not attributable to one saved rule,
    /// such as a transient failure while enforcing exclusive cleanup.
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
struct RetryState {
    attempts: u8,
    next_retry: Instant,
}

/// Debounced desired-state reconciliation for an activated patchbay.
///
/// The reconciler is deliberately independent from the UI timer. A backend
/// marks it dirty when its registry changes, and the application invokes
/// `reconcile_if_due` after the graph snapshot is refreshed. That coalesces a
/// node/port/link burst into one idempotent pass and leaves missing rules
/// pending instead of deleting them.
#[derive(Debug)]
pub struct PatchbayReconciler {
    pending: bool,
    next_run: Option<Instant>,
    last_graph_generation: u64,
    retry_state: BTreeMap<usize, RetryState>,
    last_report: ReconcileReport,
    debounce: Duration,
}

impl Default for PatchbayReconciler {
    fn default() -> Self {
        Self::new()
    }
}

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

impl Patchbay {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            version: 2,
            name: name.into(),
            connections: Vec::new(),
        }
    }

    pub fn add_connection(&mut self, output_port: PortId, input_port: PortId, pinned: bool) {
        if let Some(connection) = self.connections.iter_mut().find(|connection| {
            connection.output_port == output_port && connection.input_port == input_port
        }) {
            connection.pinned |= pinned;
            return;
        }
        self.connections.push(PatchConnection {
            output_port,
            input_port,
            pinned,
            ..PatchConnection::default()
        });
    }

    pub fn add_graph_connection(
        &mut self,
        graph: &Graph,
        output_port: PortId,
        input_port: PortId,
        pinned: bool,
    ) {
        let Some(output) = graph.port(output_port) else {
            self.add_connection(output_port, input_port, pinned);
            return;
        };
        let Some(input) = graph.port(input_port) else {
            self.add_connection(output_port, input_port, pinned);
            return;
        };
        let Some(output_node) = graph.node(output.node_id) else {
            self.add_connection(output_port, input_port, pinned);
            return;
        };
        let Some(input_node) = graph.node(input.node_id) else {
            self.add_connection(output_port, input_port, pinned);
            return;
        };
        let output_selector = graph.port_key(output_port).map(|key| key.selector());
        let input_selector = graph.port_key(input_port).map(|key| key.selector());
        if let Some(connection) = self.connections.iter_mut().find(|connection| {
            let selector_match = connection
                .output_selector
                .as_ref()
                .zip(connection.input_selector.as_ref())
                .zip(output_selector.as_ref().zip(input_selector.as_ref()))
                .is_some_and(
                    |((saved_output, saved_input), (current_output, current_input))| {
                        selectors_equivalent(saved_output, current_output)
                            && selectors_equivalent(saved_input, current_input)
                    },
                );
            let legacy_match = connection.output_selector.is_none()
                && connection.input_selector.is_none()
                && connection.output_node == output_node.name
                && connection.output_name == output.name
                && connection.input_node == input_node.name
                && connection.input_name == input.name;
            (connection.output_port == output_port && connection.input_port == input_port)
                || selector_match
                || legacy_match
        }) {
            connection.pinned |= pinned;
            connection.output_port = output_port;
            connection.input_port = input_port;
            connection.node_type = output_node.node_type;
            connection.output_node_type = Some(output_node.node_type);
            connection.input_node_type = Some(input_node.node_type);
            connection.port_type = output.port_type;
            connection.output_node = output_node.name.clone();
            connection.output_name = output.name.clone();
            connection.input_node = input_node.name.clone();
            connection.input_name = input.name.clone();
            connection.output_selector = output_selector;
            connection.input_selector = input_selector;
            return;
        }
        self.connections.push(PatchConnection {
            output_port,
            input_port,
            pinned,
            node_type: output_node.node_type,
            output_node_type: Some(output_node.node_type),
            input_node_type: Some(input_node.node_type),
            port_type: output.port_type,
            output_node: output_node.name.clone(),
            output_name: output.name.clone(),
            input_node: input_node.name.clone(),
            input_name: input.name.clone(),
            output_selector,
            input_selector,
        });
    }

    pub fn remove_connection(&mut self, output_port: PortId, input_port: PortId) -> bool {
        let original_len = self.connections.len();
        self.connections.retain(|connection| {
            connection.output_port != output_port || connection.input_port != input_port
        });
        original_len != self.connections.len()
    }

    /// Remove a saved rule by stable endpoint identity. Numeric PipeWire IDs
    /// can change while an application is paused, so deleting a newly
    /// recreated link must also remove the older saved rule.
    pub fn remove_stable_connection(&mut self, output: &PortKey, input: &PortKey) -> bool {
        let original_len = self.connections.len();
        self.connections
            .retain(|connection| !connection.matches_stable_pair(output, input));
        original_len != self.connections.len()
    }

    /// Remove every saved rule touching a node whose stable name is no longer
    /// present. This is used when an effect instance is deliberately removed;
    /// ordinary graph refreshes must retain unresolved rules for later
    /// activation.
    pub fn remove_connections_for_node(&mut self, node_name: &str) -> bool {
        let original_len = self.connections.len();
        self.connections.retain(|connection| {
            connection.output_node != node_name && connection.input_node != node_name
        });
        original_len != self.connections.len()
    }

    pub fn snapshot_graph(&mut self, graph: &Graph, pinned: bool) {
        let links: Vec<_> = graph.links.values().cloned().collect();
        self.connections.clear();
        for link in links {
            self.add_graph_connection(graph, link.output_port, link.input_port, pinned);
        }
    }

    /// Snapshot only links that the driver can mutate. Observed relationships
    /// may still be displayed in the live graph, but persisting them as
    /// reconnectable rules would make a later activation appear corrupted.
    pub fn snapshot_driver(&mut self, driver: &dyn GraphDriver, pinned: bool) {
        let graph = driver.graph();
        let links: Vec<_> = graph
            .links
            .values()
            .filter(|link| driver.is_link_mutable(link.id))
            .cloned()
            .collect();
        self.connections.clear();
        for link in links {
            self.add_graph_connection(graph, link.output_port, link.input_port, pinned);
        }
    }

    /// Return only rules that touch an effect endpoint. Effect routing is part
    /// of the persisted effect-node state and can be restored independently of
    /// the user's optional full patchbay-on-startup setting.
    pub fn effect_connections(&self) -> Self {
        Self {
            version: self.version,
            name: self.name.clone(),
            connections: self
                .connections
                .iter()
                .filter(|connection| {
                    connection.effective_output_node_type() == NodeType::Effect
                        || connection.effective_input_node_type() == NodeType::Effect
                })
                .cloned()
                .collect(),
        }
    }

    /// Resolve one persisted rule without mutating the graph. This is also
    /// used by the continuous reconciler so missing and ambiguous endpoints
    /// remain observable rather than being silently dropped.
    pub fn resolve_rule(&self, graph: &Graph, rule_index: usize) -> PatchbayResolution {
        let Some(connection) = self.connections.get(rule_index) else {
            return PatchbayResolution::WaitingForEndpoint {
                detail: format!("rule {rule_index} is no longer present"),
            };
        };
        let has_names = !connection.output_node.is_empty()
            && !connection.output_name.is_empty()
            && !connection.input_node.is_empty()
            && !connection.input_name.is_empty();
        if !has_names && connection.output_selector.is_none() && connection.input_selector.is_none()
        {
            let Some(output) = graph.port(connection.output_port) else {
                return PatchbayResolution::WaitingForEndpoint {
                    detail: "legacy output port is missing".into(),
                };
            };
            let Some(input) = graph.port(connection.input_port) else {
                return PatchbayResolution::WaitingForEndpoint {
                    detail: "legacy input port is missing".into(),
                };
            };
            return if output.direction == Direction::Source && input.direction == Direction::Sink {
                PatchbayResolution::Resolved {
                    output: output.id,
                    input: input.id,
                }
            } else {
                PatchbayResolution::WaitingForEndpoint {
                    detail: "legacy numeric endpoints have incompatible directions".into(),
                }
            };
        }
        let output = resolve_selector(graph, connection.output_selector.as_ref(), || {
            legacy_selector(
                connection.output_node.clone(),
                connection.output_name.clone(),
                Direction::Source,
                connection.port_type,
                connection.effective_output_node_type(),
            )
        });
        let input = resolve_selector(graph, connection.input_selector.as_ref(), || {
            legacy_selector(
                connection.input_node.clone(),
                connection.input_name.clone(),
                Direction::Sink,
                connection.port_type,
                connection.effective_input_node_type(),
            )
        });
        match (output, input) {
            (
                EndpointResolution::Exact(output) | EndpointResolution::UniqueFallback(output),
                EndpointResolution::Exact(input) | EndpointResolution::UniqueFallback(input),
            ) => PatchbayResolution::Resolved { output, input },
            (EndpointResolution::Ambiguous(ids), _) => PatchbayResolution::Ambiguous {
                detail: format!("output endpoint is ambiguous: {} candidates", ids.len()),
            },
            (_, EndpointResolution::Ambiguous(ids)) => PatchbayResolution::Ambiguous {
                detail: format!("input endpoint is ambiguous: {} candidates", ids.len()),
            },
            (EndpointResolution::Missing, _) => PatchbayResolution::WaitingForEndpoint {
                detail: "output endpoint is missing".into(),
            },
            (_, EndpointResolution::Missing) => PatchbayResolution::WaitingForEndpoint {
                detail: "input endpoint is missing".into(),
            },
        }
    }

    /// Build a detailed, on-demand report for the patchbay diagnostics view.
    ///
    /// This is intentionally not part of the regular model synchronization
    /// path: it formats every saved selector and current resolution, which is
    /// useful when investigating a recreated stream but unnecessary for the
    /// compact rule cards.
    pub fn debug_report(&self, graph: &Graph, report: &ReconcileReport) -> String {
        let mut text = String::new();
        let _ = writeln!(text, "Patchbay diagnostics");
        let _ = writeln!(text, "name={}", self.name);
        let _ = writeln!(text, "schema_version={}", self.version);
        let _ = writeln!(text, "saved_rules={}", self.connections.len());
        let _ = writeln!(text, "reconcile_connected={}", report.connected);
        let _ = writeln!(text, "reconcile_already_present={}", report.already_present);
        let _ = writeln!(text, "reconcile_disconnected={}", report.disconnected);
        if !report.warnings.is_empty() {
            let _ = writeln!(text, "warnings={:?}", report.warnings);
        }
        let _ = writeln!(text, "\nRules:");
        for (index, connection) in self.connections.iter().enumerate() {
            let (output, input) = connection_selectors(connection);
            let _ = writeln!(
                text,
                "rule[{index}] pinned={} display={}:{} -> {}:{}",
                connection.pinned,
                connection.output_node,
                connection.output_name,
                connection.input_node,
                connection.input_name,
            );
            let _ = writeln!(text, "  output_selector={output:?}");
            let _ = writeln!(
                text,
                "  output_resolution={}",
                graph.endpoint_resolution_explanation(&output)
            );
            let _ = writeln!(text, "  input_selector={input:?}");
            let _ = writeln!(
                text,
                "  input_resolution={}",
                graph.endpoint_resolution_explanation(&input)
            );
            if let Some(status) = report
                .rules
                .iter()
                .find(|status| status.rule_index == index)
            {
                let _ = writeln!(
                    text,
                    "  reconcile_status={} detail={}",
                    status.status.label(),
                    status.detail
                );
            } else {
                let _ = writeln!(text, "  reconcile_status=not sampled in the last pass");
            }
        }
        text
    }

    pub fn save_to(&self, path: impl AsRef<Path>) -> Result<(), PatchbayError> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(PatchbayError::Write)?;
        }
        let is_xml = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| matches!(extension.to_ascii_lowercase().as_str(), "qpwgraph" | "xml"))
            .unwrap_or(false);
        let text = if is_xml {
            self.to_xml()?
        } else {
            serde_json::to_string_pretty(self)?
        };
        pw_graph_utils::atomic_write(path, text.as_bytes(), false).map_err(PatchbayError::Write)?;
        if is_xml {
            let sidecar = selector_sidecar_path(path);
            let entries = self
                .connections
                .iter()
                .enumerate()
                .map(|(index, connection)| XmlSelectorSidecarEntry {
                    index: Some(index),
                    fingerprint: connection_fingerprint(connection),
                    output_selector: connection.output_selector.clone(),
                    input_selector: connection.input_selector.clone(),
                    output_node_type: connection.output_node_type,
                    input_node_type: connection.input_node_type,
                    port_type: Some(connection.port_type),
                })
                .collect::<Vec<_>>();
            let sidecar_text = serde_json::to_string_pretty(&entries)?;
            pw_graph_utils::atomic_write(&sidecar, sidecar_text.as_bytes(), false)
                .map_err(PatchbayError::Write)?;
        }
        Ok(())
    }

    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, PatchbayError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(PatchbayError::Read)?;
        if text.trim_start().starts_with('<') {
            let mut patchbay = Self::from_xml(&text)?;
            load_xml_selector_sidecar(path, &mut patchbay);
            Ok(patchbay)
        } else {
            let mut patchbay: Self = serde_json::from_str(&text)?;
            patchbay.version = patchbay.version.max(2);
            Ok(patchbay)
        }
    }

    /// Connect all saved edges. Name-based rules are resolved against the
    /// current registry snapshot, allowing IDs to change between sessions.
    /// Backend mutations are atomic: a fatal connect/disconnect error rolls
    /// back every mutation made by this invocation. Unresolved rules are
    /// skipped as before and are not mutations.
    pub fn activate(
        &self,
        driver: &mut dyn GraphDriver,
        exclusive: bool,
        auto_disconnect: bool,
    ) -> Result<ActivationReport, PatchbayError> {
        driver.refresh()?;
        let mut report = ActivationReport::default();
        let capabilities = driver.capabilities();
        if !self.connections.is_empty() && !capabilities.connect {
            report
                .failed
                .push("connection activation is not supported by this backend".into());
            return Ok(report);
        }
        if (exclusive || auto_disconnect) && !capabilities.disconnect {
            report
                .failed
                .push("connection removal is not supported by this backend".into());
            return Ok(report);
        }
        let mut resolved = Vec::new();
        for (rule_index, _) in self.connections.iter().enumerate() {
            match self.resolve_rule(driver.graph(), rule_index) {
                PatchbayResolution::Resolved { output, input } => {
                    let Some(output) = driver.graph().port_key(output) else {
                        continue;
                    };
                    let Some(input) = driver.graph().port_key(input) else {
                        continue;
                    };
                    resolved.push((output, input));
                }
                PatchbayResolution::WaitingForEndpoint { detail } => {
                    report.waiting.push(format!("rule {rule_index}: {detail}"));
                }
                PatchbayResolution::Ambiguous { detail } => {
                    report
                        .ambiguous
                        .push(format!("rule {rule_index}: {detail}"));
                }
            }
        }

        let mut removed_by_activation: Vec<(PortKey, PortKey)> = Vec::new();
        let mut created_by_activation: Vec<(PortKey, PortKey)> = Vec::new();

        // Exclusive mode is only safe once every saved selector has resolved.
        // A dynamic application can legitimately be between its node and port
        // registry events; removing the currently-live links from that partial
        // snapshot would make a transient disappearance destructive.
        if exclusive && report.waiting.is_empty() && report.ambiguous.is_empty() {
            let live: Vec<_> = driver
                .graph()
                .links
                .values()
                // Observed relationships are not the patchbay's to remove.
                // A composite backend can expose immutable Core Audio session
                // links next to mutable MIDI ones and still report that it
                // supports disconnection overall.
                .filter(|link| driver.is_link_mutable(link.id))
                .filter_map(|link| {
                    Some((
                        driver.graph().port_key(link.output_port)?,
                        driver.graph().port_key(link.input_port)?,
                    ))
                })
                .collect();
            for (live_output, live_input) in live {
                let saved = resolved
                    .iter()
                    .any(|(output, input)| output == &live_output && input == &live_input);
                if saved {
                    continue;
                }
                match driver
                    .disconnect_by_key_if_present_without_suppression(&live_output, &live_input)
                {
                    Ok(Some(_)) => {
                        report.disconnected += 1;
                        record_activation_removal(
                            &mut removed_by_activation,
                            &mut created_by_activation,
                            live_output,
                            live_input,
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        return Err(rollback_activation(
                            driver,
                            &created_by_activation,
                            &removed_by_activation,
                            error,
                        ));
                    }
                }
            }
        }

        for (output, input) in resolved {
            if driver.graph().find_link_by_keys(&output, &input).is_some() {
                report.already_present += 1;
                continue;
            }

            if auto_disconnect {
                let Some(input_port) = driver.graph().resolve_port_key(&input) else {
                    continue;
                };
                let stale: Vec<(PortKey, PortKey)> = driver
                    .graph()
                    .links_for_port(input_port)
                    .filter(|link| link.input_port == input_port)
                    .filter(|link| driver.is_link_mutable(link.id))
                    .filter_map(|link| {
                        Some((
                            driver.graph().port_key(link.output_port)?,
                            driver.graph().port_key(link.input_port)?,
                        ))
                    })
                    .collect();
                for (stale_output, stale_input) in stale {
                    match driver.disconnect_by_key_if_present_without_suppression(
                        &stale_output,
                        &stale_input,
                    ) {
                        Ok(Some(_)) => {
                            report.disconnected += 1;
                            record_activation_removal(
                                &mut removed_by_activation,
                                &mut created_by_activation,
                                stale_output,
                                stale_input,
                            );
                        }
                        Ok(None) => {}
                        Err(error) => {
                            return Err(rollback_activation(
                                driver,
                                &created_by_activation,
                                &removed_by_activation,
                                error,
                            ));
                        }
                    }
                }
            }

            match driver.connect_by_key_if_missing(&output, &input) {
                Ok(Some(_)) => {
                    report.connected += 1;
                    created_by_activation.push((output, input));
                }
                Ok(None) => report.already_present += 1,
                Err(error) => {
                    return Err(rollback_activation(
                        driver,
                        &created_by_activation,
                        &removed_by_activation,
                        error,
                    ));
                }
            }
        }
        Ok(report)
    }
}

fn legacy_selector(
    node_name: String,
    port_name: String,
    direction: Direction,
    port_type: PortType,
    node_type: NodeType,
) -> EndpointSelector {
    EndpointSelector {
        node_type,
        identity: NodeIdentity::with_node_name(node_name),
        port_name,
        channel: None,
        direction,
        port_type,
        match_mode: pw_graph_core::EndpointMatchMode::Instance,
    }
}

fn connection_selectors(connection: &PatchConnection) -> (EndpointSelector, EndpointSelector) {
    let output = connection.output_selector.clone().unwrap_or_else(|| {
        legacy_selector(
            connection.output_node.clone(),
            connection.output_name.clone(),
            Direction::Source,
            connection.port_type,
            connection.effective_output_node_type(),
        )
    });
    let input = connection.input_selector.clone().unwrap_or_else(|| {
        legacy_selector(
            connection.input_node.clone(),
            connection.input_name.clone(),
            Direction::Sink,
            connection.port_type,
            connection.effective_input_node_type(),
        )
    });
    (output, input)
}

fn resolve_selector<F>(
    graph: &Graph,
    selector: Option<&EndpointSelector>,
    legacy: F,
) -> EndpointResolution
where
    F: FnOnce() -> EndpointSelector,
{
    let selector = selector.cloned().unwrap_or_else(legacy);
    let result = graph.resolve_endpoint(&selector);
    if !matches!(result, EndpointResolution::Missing) {
        return result;
    }
    // A pre-v2 rule used one broad node type for both endpoints. Preserve the
    // old compatibility fallback, but only after the precise typed selector
    // had no candidate.
    if selector.node_type != NodeType::Unknown && selector.identity.effect_instance_id.is_none() {
        let mut fallback = selector;
        fallback.node_type = NodeType::Unknown;
        return graph.resolve_endpoint(&fallback);
    }
    result
}

fn selectors_equivalent(saved: &EndpointSelector, current: &EndpointSelector) -> bool {
    let app_channel_matches = !matches!(
        saved.match_mode,
        pw_graph_core::EndpointMatchMode::NamePattern
    ) && !matches!(
        current.match_mode,
        pw_graph_core::EndpointMatchMode::NamePattern
    ) && saved.channel.is_some()
        && saved.channel == current.channel
        && (saved.identity.application_id.is_some()
            || saved.identity.process_binary.is_some()
            || saved.identity.application_name.is_some());
    let port_name_matches = saved.port_name == current.port_name || app_channel_matches;
    if saved.node_type != current.node_type
        || !port_name_matches
        || saved.channel != current.channel
        || saved.direction != current.direction
        || saved.port_type != current.port_type
    {
        return false;
    }
    let a = &saved.identity;
    let b = &current.identity;
    if a.effect_instance_id.is_some() || b.effect_instance_id.is_some() {
        return a.effect_instance_id == b.effect_instance_id;
    }
    if a.application_id.is_some() || b.application_id.is_some() {
        return a.application_id.is_some() && a.application_id == b.application_id;
    }
    ((a.process_binary.is_some() || b.process_binary.is_some())
        && a.process_binary == b.process_binary
        && (a.application_name.is_none()
            || b.application_name.is_none()
            || a.application_name == b.application_name))
        || (a.application_id.is_none()
            && b.application_id.is_none()
            && a.process_binary.is_none()
            && b.process_binary.is_none()
            && a.node_name == b.node_name)
}

fn selector_sidecar_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("patchbay");
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{file_name}.qpwgraph-rs-selectors.json"))
}

fn connection_fingerprint(connection: &PatchConnection) -> String {
    serde_json::to_string(&(
        &connection.output_node,
        &connection.output_name,
        &connection.input_node,
        &connection.input_name,
        connection.node_type,
        connection.output_node_type,
        connection.input_node_type,
        connection.port_type,
    ))
    .expect("patchbay selector fingerprint is serializable")
}

fn load_xml_selector_sidecar(path: &Path, patchbay: &mut Patchbay) {
    let sidecar = selector_sidecar_path(path);
    let Ok(text) = std::fs::read_to_string(sidecar) else {
        return;
    };
    let Ok(entries) = serde_json::from_str::<Vec<XmlSelectorSidecarEntry>>(&text) else {
        return;
    };
    let mut used = vec![false; entries.len()];
    for (connection_index, connection) in patchbay.connections.iter_mut().enumerate() {
        let fingerprint = connection_fingerprint(connection);
        let match_index = entries
            .iter()
            .enumerate()
            .find(|(index, entry)| !used[*index] && entry.index == Some(connection_index))
            .map(|(index, _)| index)
            .or_else(|| {
                entries
                    .iter()
                    .enumerate()
                    .find(|(index, entry)| !used[*index] && entry.fingerprint == fingerprint)
                    .map(|(index, _)| index)
            });
        let Some(index) = match_index else {
            continue;
        };
        let entry = &entries[index];
        connection.output_selector = entry.output_selector.clone();
        connection.input_selector = entry.input_selector.clone();
        connection.output_node_type = entry.output_node_type;
        connection.input_node_type = entry.input_node_type;
        if let Some(port_type) = entry.port_type {
            connection.port_type = port_type;
        }
        used[index] = true;
    }
    patchbay.version = patchbay.version.max(2);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pw_graph_backend::{
        BackendResult, EffectDriver, EffectNodeRequest, GraphDriver, InMemoryDriver,
    };
    use std::collections::BTreeMap;

    struct ObservedDemo {
        inner: InMemoryDriver,
    }

    impl EffectDriver for ObservedDemo {}

    impl GraphDriver for ObservedDemo {
        fn refresh(&mut self) -> BackendResult<Vec<pw_graph_core::Node>> {
            self.inner.refresh()
        }

        fn connect(&mut self, src: PortId, dst: PortId) -> BackendResult<pw_graph_core::Link> {
            self.inner.connect(src, dst)
        }

        fn disconnect(
            &mut self,
            link: pw_graph_core::LinkId,
        ) -> BackendResult<pw_graph_core::Link> {
            self.inner.disconnect(link)
        }

        fn is_link_mutable(&self, _link: pw_graph_core::LinkId) -> bool {
            false
        }

        fn graph(&self) -> &Graph {
            self.inner.graph()
        }
    }

    fn graph_with_named_audio_edge(
        output_node_type: NodeType,
        input_node_type: NodeType,
        node_offset: u64,
        port_offset: u64,
    ) -> Graph {
        let mut graph = Graph::default();
        graph
            .add_node(pw_graph_core::Node::new(
                pw_graph_core::NodeId(node_offset + 1),
                "Capture",
                output_node_type,
            ))
            .unwrap();
        graph
            .add_node(pw_graph_core::Node::new(
                pw_graph_core::NodeId(node_offset + 2),
                "Noise Gate (gate-1)",
                input_node_type,
            ))
            .unwrap();
        graph
            .add_port(pw_graph_core::Port::new(
                PortId(port_offset + 1),
                pw_graph_core::NodeId(node_offset + 1),
                "output",
                Direction::Source,
                PortType::Audio,
            ))
            .unwrap();
        graph
            .add_port(pw_graph_core::Port::new(
                PortId(port_offset + 2),
                pw_graph_core::NodeId(node_offset + 2),
                "input",
                Direction::Sink,
                PortType::Audio,
            ))
            .unwrap();
        graph
    }

    fn key(driver: &InMemoryDriver, port: u64) -> PortKey {
        driver
            .graph()
            .port_key(PortId(port))
            .expect("demo port exists")
    }

    #[test]
    fn activates_and_is_idempotent() {
        let mut patchbay = Patchbay::new("demo");
        patchbay.add_connection(PortId(1), PortId(3), true);
        let mut driver = InMemoryDriver::demo();
        assert_eq!(
            patchbay
                .activate(&mut driver, false, false)
                .unwrap()
                .connected,
            1
        );
        assert_eq!(
            patchbay
                .activate(&mut driver, false, false)
                .unwrap()
                .already_present,
            1
        );
    }

    #[test]
    fn a_failed_activation_restores_what_it_disconnected() {
        // The user-visible regression: activating a patchbay took working
        // links down, the desired route was refused, and the old links stayed
        // gone — so a failed activation silently killed audio.
        let mut driver = InMemoryDriver::demo();
        let existing = driver.connect(PortId(1), PortId(3)).unwrap();
        driver.fail_connect_of(PortId(2), PortId(3));

        let mut patchbay = Patchbay::default();
        patchbay.add_graph_connection(driver.graph(), PortId(2), PortId(3), false);

        let error = patchbay
            .activate(&mut driver, false, true)
            .expect_err("a fatal mutation failure must abort activation");
        assert!(matches!(error, PatchbayError::Backend(_)), "{error:?}");
        assert!(
            driver.graph().link(existing.id).is_some()
                || driver
                    .graph()
                    .find_link_by_keys(
                        &driver.graph().port_key(PortId(1)).unwrap(),
                        &driver.graph().port_key(PortId(3)).unwrap()
                    )
                    .is_some(),
            "the pre-existing route must survive a failed activation"
        );
    }

    #[test]
    fn exclusive_activation_rolls_back_created_and_removed_links_as_one_transaction() {
        let mut driver = InMemoryDriver::demo();
        driver.connect(PortId(1), PortId(3)).unwrap(); // A
        driver.fail_connect_of(PortId(2), PortId(3)); // Y fails after X
        let mut patchbay = Patchbay::default();
        patchbay.add_connection(PortId(1), PortId(4), false); // X
        patchbay.add_connection(PortId(2), PortId(3), false); // Y

        let error = patchbay
            .activate(&mut driver, true, false)
            .expect_err("the later desired route fails");
        assert!(matches!(error, PatchbayError::Backend(_)), "{error:?}");
        assert!(driver
            .graph()
            .find_link_by_keys(&key(&driver, 1), &key(&driver, 3))
            .is_some());
        assert!(driver
            .graph()
            .find_link_by_keys(&key(&driver, 1), &key(&driver, 4))
            .is_none());
        assert!(driver
            .graph()
            .find_link_by_keys(&key(&driver, 2), &key(&driver, 3))
            .is_none());
    }

    #[test]
    fn rollback_does_not_restore_a_route_created_and_removed_by_this_activation() {
        // X is created for the first desired route, then auto-disconnected
        // while attempting the second route. X never belonged to the graph
        // before activation and must not come back alongside the original C.
        let mut driver = InMemoryDriver::demo();
        driver.connect(PortId(1), PortId(4)).unwrap(); // C: original graph
        driver.fail_connect_of(PortId(2), PortId(3)); // Y fails after X

        let mut patchbay = Patchbay::default();
        patchbay.add_connection(PortId(1), PortId(3), false); // X
        patchbay.add_connection(PortId(2), PortId(3), false); // Y

        assert!(patchbay.activate(&mut driver, false, true).is_err());
        assert!(driver
            .graph()
            .find_link_by_keys(&key(&driver, 1), &key(&driver, 4))
            .is_some());
        assert!(driver
            .graph()
            .find_link_by_keys(&key(&driver, 1), &key(&driver, 3))
            .is_none());
        assert!(driver
            .graph()
            .find_link_by_keys(&key(&driver, 2), &key(&driver, 3))
            .is_none());
    }

    #[test]
    fn auto_disconnect_failure_leaves_the_stale_route_intact() {
        let mut driver = InMemoryDriver::demo();
        driver.connect(PortId(1), PortId(3)).unwrap();
        driver.fail_disconnect_of_pair(PortId(1), PortId(3));
        let mut patchbay = Patchbay::default();
        patchbay.add_connection(PortId(2), PortId(3), false);

        let error = patchbay
            .activate(&mut driver, false, true)
            .expect_err("stale-route removal fails");
        assert!(matches!(error, PatchbayError::Backend(_)), "{error:?}");
        assert!(driver
            .graph()
            .find_link_by_keys(&key(&driver, 1), &key(&driver, 3))
            .is_some());
        assert!(driver
            .graph()
            .find_link_by_keys(&key(&driver, 2), &key(&driver, 3))
            .is_none());
    }

    #[test]
    fn exclusive_removal_failure_leaves_the_original_graph_intact() {
        let mut driver = InMemoryDriver::demo();
        driver.connect(PortId(1), PortId(3)).unwrap();
        driver.fail_disconnect_of_pair(PortId(1), PortId(3));
        let mut patchbay = Patchbay::default();
        patchbay.add_connection(PortId(2), PortId(4), false);

        let error = patchbay
            .activate(&mut driver, true, false)
            .expect_err("exclusive removal fails");
        assert!(matches!(error, PatchbayError::Backend(_)), "{error:?}");
        assert!(driver
            .graph()
            .find_link_by_keys(&key(&driver, 1), &key(&driver, 3))
            .is_some());
        assert!(driver
            .graph()
            .find_link_by_keys(&key(&driver, 2), &key(&driver, 4))
            .is_none());
    }

    #[test]
    fn activation_reports_created_and_removed_stranding_separately() {
        let mut driver = InMemoryDriver::demo();
        driver.connect(PortId(1), PortId(3)).unwrap(); // removed A
        driver.fail_connect_of(PortId(2), PortId(3)); // primary failure Y
        driver.fail_disconnect_of_pair(PortId(1), PortId(4)); // X cannot roll back
        driver.fail_connect_of(PortId(1), PortId(3)); // A cannot be restored
        let mut patchbay = Patchbay::default();
        patchbay.add_connection(PortId(1), PortId(4), false);
        patchbay.add_connection(PortId(2), PortId(3), false);

        let error = patchbay
            .activate(&mut driver, true, false)
            .expect_err("both rollback directions fail");
        match error {
            PatchbayError::ActivationNotRolledBack {
                created_links_left,
                removed_links_not_restored,
                ..
            } => {
                assert_eq!(created_links_left, 1);
                assert_eq!(removed_links_not_restored, 1);
            }
            other => panic!("expected separate rollback counts, got {other:?}"),
        }
        assert!(driver
            .graph()
            .find_link_by_keys(&key(&driver, 1), &key(&driver, 4))
            .is_some());
        assert!(driver
            .graph()
            .find_link_by_keys(&key(&driver, 1), &key(&driver, 3))
            .is_none());
    }

    #[test]
    fn immutable_and_already_present_links_are_never_rolled_back_as_new() {
        let mut driver = InMemoryDriver::demo();
        let immutable = driver.connect(PortId(1), PortId(3)).unwrap();
        driver.mark_link_observed(immutable.id);
        driver.connect(PortId(1), PortId(4)).unwrap();
        driver.fail_connect_of(PortId(2), PortId(3));
        let mut patchbay = Patchbay::default();
        patchbay.add_connection(PortId(1), PortId(4), false);
        patchbay.add_connection(PortId(2), PortId(3), false);

        assert!(patchbay.activate(&mut driver, true, false).is_err());
        assert!(driver
            .graph()
            .find_link_by_keys(&key(&driver, 1), &key(&driver, 3))
            .is_some());
        assert!(driver
            .graph()
            .find_link_by_keys(&key(&driver, 1), &key(&driver, 4))
            .is_some());
    }

    #[test]
    fn exclusive_activation_leaves_observed_links_alone() {
        // Observed relationships belong to the backend, not the patchbay.
        let mut driver = InMemoryDriver::demo();
        let observed = driver.connect(PortId(1), PortId(3)).unwrap();
        driver.mark_link_observed(observed.id);

        let patchbay = Patchbay::default();
        let report = patchbay.activate(&mut driver, true, false).unwrap();
        assert_eq!(report.disconnected, 0);
        assert!(driver.graph().link(observed.id).is_some());
    }

    #[test]
    fn snapshot_driver_omits_observed_links() {
        let mut driver = ObservedDemo {
            inner: InMemoryDriver::demo(),
        };
        driver.connect(PortId(1), PortId(3)).unwrap();

        let mut patchbay = Patchbay::new("observed");
        patchbay.snapshot_driver(&driver, true);

        assert!(patchbay.connections.is_empty());
    }

    #[test]
    fn qpwgraph_xml_round_trip_uses_names() {
        let mut patchbay = Patchbay::new("demo");
        let driver = InMemoryDriver::demo();
        patchbay.add_graph_connection(driver.graph(), PortId(1), PortId(3), true);
        let path =
            std::env::temp_dir().join(format!("qpwgraph-rs-{}.qpwgraph", std::process::id()));
        patchbay.save_to(&path).unwrap();
        let loaded = Patchbay::load_from(&path).unwrap();
        assert_eq!(loaded.connections.len(), 1);
        assert_eq!(loaded.connections[0].output_node, "Audio Capture");
        assert_eq!(loaded.connections[0].output_name, "capture_FL");
        assert!(loaded.connections[0].output_selector.is_some());
        assert!(loaded.connections[0].input_selector.is_some());
        let sidecar = selector_sidecar_path(&path);
        std::fs::remove_file(path).unwrap();
        let _ = std::fs::remove_file(sidecar);
    }

    #[test]
    fn saving_a_patchbay_creates_missing_parent_directories() {
        let root =
            std::env::temp_dir().join(format!("pw-graph-patchbay-parent-{}", std::process::id()));
        let path = root.join("nested").join("connections.json");
        Patchbay::new("nested").save_to(&path).unwrap();
        assert!(path.is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn effect_node_connections_restore_after_the_node_is_recreated() {
        let mut first = InMemoryDriver::demo();
        let effect = first
            .create_effect_node(EffectNodeRequest {
                instance_id: "persistent-gate".into(),
                effect_id: "builtin.noise-gate".into(),
                module_path: None,
                enabled: true,
                parameters: BTreeMap::new(),
                channel_policy: Default::default(),
                position: [260.0, 180.0],
            })
            .unwrap();
        first.connect(PortId(1), effect.input_port).unwrap();
        first.connect(effect.output_port, PortId(3)).unwrap();

        let mut saved = Patchbay::new("effects");
        saved.snapshot_graph(first.graph(), true);
        assert_eq!(saved.effect_connections().connections.len(), 2);

        let mut recreated = InMemoryDriver::demo();
        let recreated_effect = recreated
            .create_effect_node(EffectNodeRequest {
                instance_id: "persistent-gate".into(),
                effect_id: "builtin.noise-gate".into(),
                module_path: None,
                enabled: true,
                parameters: BTreeMap::new(),
                channel_policy: Default::default(),
                position: [260.0, 180.0],
            })
            .unwrap();
        let report = saved.activate(&mut recreated, false, false).unwrap();

        assert_eq!(report.connected, 2);
        assert!(recreated
            .graph()
            .links
            .values()
            .any(|link| link.output_port == PortId(1)
                && link.input_port == recreated_effect.input_port));
        assert!(recreated
            .graph()
            .links
            .values()
            .any(|link| link.output_port == recreated_effect.output_port
                && link.input_port == PortId(3)));
    }

    #[test]
    fn activation_prefers_names_when_pipewire_ids_change() {
        let mut saved = Patchbay::new("demo");
        let original = InMemoryDriver::demo();
        saved.add_graph_connection(original.graph(), PortId(1), PortId(3), true);

        let mut graph = Graph::default();
        graph
            .add_node(pw_graph_core::Node::new(
                pw_graph_core::NodeId(101),
                "Audio Capture",
                NodeType::PipeWire,
            ))
            .unwrap();
        graph
            .add_node(pw_graph_core::Node::new(
                pw_graph_core::NodeId(102),
                "Audio Playback",
                NodeType::PipeWire,
            ))
            .unwrap();
        graph
            .add_port(pw_graph_core::Port::new(
                PortId(201),
                pw_graph_core::NodeId(101),
                "capture_FL",
                Direction::Source,
                PortType::Audio,
            ))
            .unwrap();
        graph
            .add_port(pw_graph_core::Port::new(
                PortId(203),
                pw_graph_core::NodeId(102),
                "playback_FL",
                Direction::Sink,
                PortType::Audio,
            ))
            .unwrap();
        let mut current = InMemoryDriver::new(graph);

        let report = saved.activate(&mut current, false, false).unwrap();
        assert_eq!(report.connected, 1);
        assert!(current
            .graph()
            .links
            .values()
            .any(|link| link.output_port == PortId(201) && link.input_port == PortId(203)));
    }

    #[test]
    fn restores_effect_connections_with_independent_endpoint_types() {
        for (output_node_type, input_node_type) in [
            (NodeType::PipeWire, NodeType::Effect),
            (NodeType::Effect, NodeType::PipeWire),
        ] {
            let original = graph_with_named_audio_edge(output_node_type, input_node_type, 10, 20);
            let mut patchbay = Patchbay::new("effects");
            patchbay.add_graph_connection(&original, PortId(21), PortId(22), true);

            let connection = &patchbay.connections[0];
            assert_eq!(connection.node_type, output_node_type);
            assert_eq!(connection.output_node_type, Some(output_node_type));
            assert_eq!(connection.input_node_type, Some(input_node_type));

            let mut current = InMemoryDriver::new(graph_with_named_audio_edge(
                output_node_type,
                input_node_type,
                100,
                200,
            ));
            let report = patchbay.activate(&mut current, false, false).unwrap();
            assert_eq!(report.connected, 1);
            assert!(current
                .graph()
                .links
                .values()
                .any(|link| { link.output_port == PortId(201) && link.input_port == PortId(202) }));
        }
    }

    #[test]
    fn legacy_single_type_rule_can_restore_an_effect_endpoint() {
        let mut patchbay = Patchbay::new("legacy-effects");
        patchbay.connections.push(PatchConnection {
            node_type: NodeType::PipeWire,
            port_type: PortType::Audio,
            output_node: "Capture".into(),
            output_name: "output".into(),
            input_node: "Noise Gate (gate-1)".into(),
            input_name: "input".into(),
            ..PatchConnection::default()
        });

        let mut current = InMemoryDriver::new(graph_with_named_audio_edge(
            NodeType::PipeWire,
            NodeType::Effect,
            100,
            200,
        ));
        let report = patchbay.activate(&mut current, false, false).unwrap();
        assert_eq!(report.connected, 1);
        assert_eq!(report.failed, Vec::<String>::new());
    }

    #[test]
    fn qpwgraph_xml_round_trip_preserves_effect_endpoint_types() {
        let original = graph_with_named_audio_edge(NodeType::Effect, NodeType::PipeWire, 10, 20);
        let mut patchbay = Patchbay::new("effects");
        patchbay.add_graph_connection(&original, PortId(21), PortId(22), true);

        let xml = patchbay.to_xml().unwrap();
        assert!(!xml.contains("output-node-type"));
        assert!(!xml.contains("input-node-type"));

        let path = std::env::temp_dir().join(format!(
            "qpwgraph-rs-effect-types-{}.qpwgraph",
            std::process::id()
        ));
        patchbay.save_to(&path).unwrap();
        let loaded = Patchbay::load_from(&path).unwrap();
        let connection = &loaded.connections[0];
        assert_eq!(connection.effective_output_node_type(), NodeType::Effect);
        assert_eq!(connection.effective_input_node_type(), NodeType::PipeWire);

        let same_type = graph_with_named_audio_edge(NodeType::Effect, NodeType::Effect, 30, 40);
        let mut same_type_patchbay = Patchbay::new("effects");
        same_type_patchbay.add_graph_connection(&same_type, PortId(41), PortId(42), true);
        let same_type_xml = same_type_patchbay.to_xml().unwrap();
        assert!(!same_type_xml.contains("input-node-type"));
        let same_type_path = std::env::temp_dir().join(format!(
            "qpwgraph-rs-effect-types-same-{}.qpwgraph",
            std::process::id()
        ));
        same_type_patchbay.save_to(&same_type_path).unwrap();
        let same_type_loaded = Patchbay::load_from(&same_type_path).unwrap();
        assert_eq!(
            same_type_loaded.connections[0].output_node_type,
            Some(NodeType::Effect)
        );
        assert_eq!(
            same_type_loaded.connections[0].input_node_type,
            Some(NodeType::Effect)
        );
        let _ = std::fs::remove_file(path.clone());
        let _ = std::fs::remove_file(selector_sidecar_path(&path));
        let _ = std::fs::remove_file(same_type_path.clone());
        let _ = std::fs::remove_file(selector_sidecar_path(&same_type_path));
    }

    #[test]
    fn json_round_trip_preserves_effect_endpoint_types() {
        let original = graph_with_named_audio_edge(NodeType::PipeWire, NodeType::Effect, 10, 20);
        let mut patchbay = Patchbay::new("effects");
        patchbay.add_graph_connection(&original, PortId(21), PortId(22), true);

        let loaded: Patchbay =
            serde_json::from_str(&serde_json::to_string(&patchbay).unwrap()).unwrap();
        let connection = &loaded.connections[0];
        assert_eq!(connection.output_node_type, Some(NodeType::PipeWire));
        assert_eq!(connection.input_node_type, Some(NodeType::Effect));

        let mut current = InMemoryDriver::new(graph_with_named_audio_edge(
            NodeType::PipeWire,
            NodeType::Effect,
            100,
            200,
        ));
        assert_eq!(
            loaded
                .activate(&mut current, false, false)
                .unwrap()
                .connected,
            1
        );
    }

    #[test]
    fn legacy_json_without_endpoint_types_still_loads() {
        let patchbay: Patchbay = serde_json::from_str(
            r#"{
                "version": 1,
                "name": "legacy",
                "connections": [{
                    "output_port": 1,
                    "input_port": 3,
                    "pinned": true,
                    "node_type": "PipeWire",
                    "port_type": "Audio",
                    "output_node": "Audio Capture",
                    "output_name": "capture_FL",
                    "input_node": "Audio Playback",
                    "input_name": "playback_FL"
                }]
            }"#,
        )
        .unwrap();
        let connection = &patchbay.connections[0];
        assert_eq!(connection.output_node_type, None);
        assert_eq!(connection.input_node_type, None);
        assert_eq!(connection.effective_output_node_type(), NodeType::PipeWire);
        assert_eq!(connection.effective_input_node_type(), NodeType::PipeWire);
    }

    /// A graph holding both relay virtual nodes with their current
    /// role-prefixed ports, plus a normal card that still names its ports
    /// with the bare channel.
    fn relay_graph() -> Graph {
        let mut graph = Graph::default();
        for (id, name) in [
            (1u64, pw_graph_core::RELAY_SOURCE_NODE_NAME),
            (2, pw_graph_core::RELAY_SINK_NODE_NAME),
            (3, "alsa_output.pci-0000_00"),
        ] {
            graph
                .add_node(pw_graph_core::Node::new(
                    pw_graph_core::NodeId(id),
                    name,
                    NodeType::PipeWire,
                ))
                .unwrap();
        }
        for (id, node, name, direction) in [
            (10u64, 1u64, "capture_FL", Direction::Source),
            (11, 1, "capture_FR", Direction::Source),
            (12, 2, "playback_FL", Direction::Sink),
            (13, 2, "playback_FR", Direction::Sink),
            (14, 3, "FL", Direction::Sink),
            (15, 3, "FR", Direction::Sink),
        ] {
            graph
                .add_port(pw_graph_core::Port::new(
                    PortId(id),
                    pw_graph_core::NodeId(node),
                    name,
                    direction,
                    PortType::Audio,
                ))
                .unwrap();
        }
        graph
    }

    fn relay_patchbay(output_name: &str, input_node: &str, input_name: &str) -> Patchbay {
        serde_json::from_str(&format!(
            r#"{{
                "version": 1,
                "name": "legacy relay",
                "connections": [{{
                    "output_port": 0,
                    "input_port": 0,
                    "pinned": true,
                    "port_type": "Audio",
                    "output_node_type": "PipeWire",
                    "input_node_type": "PipeWire",
                    "output_node": "{}",
                    "output_name": "{output_name}",
                    "input_node": "{input_node}",
                    "input_name": "{input_name}"
                }}]
            }}"#,
            pw_graph_core::RELAY_SOURCE_NODE_NAME
        ))
        .unwrap()
    }

    #[test]
    fn a_patchbay_saved_with_the_current_relay_port_names_activates() {
        let patchbay = relay_patchbay(
            "capture_FL",
            pw_graph_core::RELAY_SINK_NODE_NAME,
            "playback_FL",
        );
        let mut driver = InMemoryDriver::new(relay_graph());
        assert_eq!(
            patchbay
                .activate(&mut driver, false, false)
                .unwrap()
                .connected,
            1
        );
    }

    #[test]
    fn a_patchbay_saved_before_the_relay_port_rename_still_activates() {
        // Regression: relay ports were renamed `FL` -> `capture_FL` /
        // `playback_FL` to make the canvas group them as a stereo pair.
        // Without a compatibility rewrite every relay connection in an
        // already-saved patchbay silently stopped reconnecting.
        let patchbay = relay_patchbay("FL", pw_graph_core::RELAY_SINK_NODE_NAME, "FL");
        let mut driver = InMemoryDriver::new(relay_graph());
        let report = patchbay.activate(&mut driver, false, false).unwrap();
        assert_eq!(report.connected, 1);

        let link = driver
            .graph()
            .links
            .values()
            .find(|link| link.output_port == PortId(10) && link.input_port == PortId(12));
        assert!(
            link.is_some(),
            "legacy relay FL keys must land on the role-prefixed ports"
        );
    }

    #[test]
    fn a_legacy_fl_rule_for_an_ordinary_card_is_not_rewritten() {
        // The rewrite is scoped to the relay nodes: a normal card whose ports
        // really are named `FL` must still resolve to its own pins.
        let patchbay = relay_patchbay("FL", "alsa_output.pci-0000_00", "FL");
        let mut driver = InMemoryDriver::new(relay_graph());
        assert_eq!(
            patchbay
                .activate(&mut driver, false, false)
                .unwrap()
                .connected,
            1
        );
        assert!(driver
            .graph()
            .links
            .values()
            .any(|link| link.output_port == PortId(10) && link.input_port == PortId(14)));
    }

    #[test]
    fn removing_an_effect_removes_all_rules_touching_that_node() {
        let graph = graph_with_named_audio_edge(NodeType::PipeWire, NodeType::Effect, 10, 20);
        let mut patchbay = Patchbay::new("effects");
        patchbay.add_graph_connection(&graph, PortId(21), PortId(22), true);
        patchbay.connections.push(PatchConnection {
            output_node: "Other".into(),
            output_name: "output".into(),
            input_node: "Sink".into(),
            input_name: "input".into(),
            ..PatchConnection::default()
        });

        assert!(patchbay.remove_connections_for_node("Noise Gate (gate-1)"));
        assert_eq!(patchbay.connections.len(), 1);
        assert_eq!(patchbay.connections[0].output_node, "Other");
        assert!(!patchbay.remove_connections_for_node("Noise Gate (gate-1)"));
    }

    fn dynamic_application_graph(
        application_node_id: u64,
        application_port_id: u64,
        serial: u64,
        application_name: &str,
        sink_node_id: u64,
        sink_port_id: u64,
    ) -> Graph {
        let mut graph = Graph::default();
        graph
            .add_node(
                pw_graph_core::Node::new(
                    pw_graph_core::NodeId(application_node_id),
                    application_name,
                    NodeType::PipeWire,
                )
                .with_identity(NodeIdentity {
                    application_id: Some("org.example.browser".into()),
                    application_name: Some("Browser".into()),
                    process_binary: Some("browser".into()),
                    node_name: application_name.into(),
                    media_role: Some("music".into()),
                    object_serial: Some(serial),
                    ..NodeIdentity::default()
                }),
            )
            .unwrap();
        graph
            .add_node(pw_graph_core::Node::new(
                pw_graph_core::NodeId(sink_node_id),
                "Built-in sink",
                NodeType::PipeWire,
            ))
            .unwrap();
        graph
            .add_port(
                pw_graph_core::Port::new(
                    PortId(application_port_id),
                    pw_graph_core::NodeId(application_node_id),
                    "output",
                    Direction::Source,
                    PortType::Audio,
                )
                .with_channel("FL"),
            )
            .unwrap();
        graph
            .add_port(
                pw_graph_core::Port::new(
                    PortId(sink_port_id),
                    pw_graph_core::NodeId(sink_node_id),
                    "input",
                    Direction::Sink,
                    PortType::Audio,
                )
                .with_channel("FL"),
            )
            .unwrap();
        graph
    }

    fn reconcile_now(
        reconciler: &mut PatchbayReconciler,
        patchbay: &Patchbay,
        driver: &mut InMemoryDriver,
        now: Instant,
        exclusive: bool,
        auto_disconnect: bool,
    ) -> ReconcileReport {
        reconciler.schedule_now(now);
        reconciler
            .reconcile_if_due(patchbay, driver, exclusive, auto_disconnect, now, 1)
            .unwrap()
            .expect("scheduled reconciliation should run")
    }

    #[test]
    fn reconciler_restores_a_recreated_application_stream() {
        let old = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
        let mut patchbay = Patchbay::new("dynamic");
        patchbay.add_graph_connection(&old, PortId(20), PortId(40), true);
        let mut driver = InMemoryDriver::new(old);
        driver.connect(PortId(20), PortId(40)).unwrap();

        let recreated = dynamic_application_graph(300, 455, 800, "Browser stream", 600, 655);
        driver.replace_graph(recreated);
        let now = Instant::now();
        let mut reconciler = PatchbayReconciler::new();
        let report = reconcile_now(&mut reconciler, &patchbay, &mut driver, now, false, false);

        assert_eq!(report.connected, 1);
        assert_eq!(report.rules[0].status, ReconcileStatus::Satisfied);
        assert!(driver
            .graph()
            .links
            .values()
            .any(|link| link.output_port == PortId(455) && link.input_port == PortId(655)));
    }

    #[test]
    fn reconciler_coalesces_registry_bursts_and_never_duplicates_a_route() {
        let graph = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
        let mut patchbay = Patchbay::new("dynamic");
        patchbay.add_graph_connection(&graph, PortId(20), PortId(40), true);
        let mut driver = InMemoryDriver::new(graph);
        let now = Instant::now();
        let mut reconciler = PatchbayReconciler::new();
        for offset in [0, 20, 40, 60] {
            reconciler.mark_dirty(now + Duration::from_millis(offset));
        }
        assert!(reconciler
            .reconcile_if_due(
                &patchbay,
                &mut driver,
                false,
                false,
                now + Duration::from_millis(99),
                4,
            )
            .unwrap()
            .is_none());
        let report = reconciler
            .reconcile_if_due(
                &patchbay,
                &mut driver,
                false,
                false,
                now + Duration::from_millis(161),
                4,
            )
            .unwrap()
            .expect("one debounced pass should run");
        assert_eq!(report.connected, 1);
        assert!(reconciler
            .reconcile_if_due(
                &patchbay,
                &mut driver,
                false,
                false,
                now + Duration::from_millis(161),
                4,
            )
            .unwrap()
            .is_none());
        assert_eq!(driver.graph().links.len(), 1);
    }

    #[test]
    fn missing_endpoint_stays_pending_without_mutating_the_graph() {
        let old = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
        let mut patchbay = Patchbay::new("dynamic");
        patchbay.add_graph_connection(&old, PortId(20), PortId(40), true);
        let mut current = old.clone();
        current.ports.remove(&PortId(20));
        current
            .nodes
            .get_mut(&pw_graph_core::NodeId(10))
            .unwrap()
            .ports
            .clear();
        let mut driver = InMemoryDriver::new(current);
        let mut reconciler = PatchbayReconciler::new();
        let report = reconcile_now(
            &mut reconciler,
            &patchbay,
            &mut driver,
            Instant::now(),
            false,
            false,
        );

        assert_eq!(report.rules[0].status, ReconcileStatus::WaitingForEndpoint);
        assert!(driver.graph().links.is_empty());
        assert_eq!(patchbay.connections.len(), 1);
    }

    #[test]
    fn node_then_ports_is_retried_after_the_next_graph_generation() {
        let old = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
        let mut patchbay = Patchbay::new("dynamic");
        patchbay.add_graph_connection(&old, PortId(20), PortId(40), true);
        let mut partial = old.clone();
        partial.ports.remove(&PortId(20));
        partial
            .nodes
            .get_mut(&pw_graph_core::NodeId(10))
            .unwrap()
            .ports
            .clear();
        let mut driver = InMemoryDriver::new(partial);
        let mut reconciler = PatchbayReconciler::new();
        let first = reconcile_now(
            &mut reconciler,
            &patchbay,
            &mut driver,
            Instant::now(),
            false,
            false,
        );
        assert_eq!(first.rules[0].status, ReconcileStatus::WaitingForEndpoint);

        driver.replace_graph(dynamic_application_graph(
            300,
            455,
            800,
            "Browser stream",
            600,
            655,
        ));
        let second = reconcile_now(
            &mut reconciler,
            &patchbay,
            &mut driver,
            Instant::now(),
            false,
            false,
        );
        assert_eq!(second.connected, 1);
    }

    #[test]
    fn equal_application_candidates_are_reported_ambiguous() {
        let old = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
        let mut patchbay = Patchbay::new("dynamic");
        patchbay.add_graph_connection(&old, PortId(20), PortId(40), true);
        let mut current = dynamic_application_graph(300, 455, 800, "Browser stream", 600, 655);
        let second = dynamic_application_graph(301, 456, 801, "Browser stream", 600, 655);
        current.nodes.insert(
            pw_graph_core::NodeId(301),
            second.nodes[&pw_graph_core::NodeId(301)].clone(),
        );
        current
            .ports
            .insert(PortId(456), second.ports[&PortId(456)].clone());
        let mut driver = InMemoryDriver::new(current);
        let mut reconciler = PatchbayReconciler::new();
        let report = reconcile_now(
            &mut reconciler,
            &patchbay,
            &mut driver,
            Instant::now(),
            false,
            false,
        );

        assert_eq!(report.rules[0].status, ReconcileStatus::Ambiguous);
        assert!(driver.graph().links.is_empty());
    }

    #[test]
    fn transient_backend_failure_uses_bounded_exponential_retry() {
        let graph = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
        let mut patchbay = Patchbay::new("dynamic");
        patchbay.add_graph_connection(&graph, PortId(20), PortId(40), true);
        let mut driver = InMemoryDriver::new(graph);
        driver.fail_connect_of(PortId(20), PortId(40));
        let mut reconciler = PatchbayReconciler::new();
        let mut now = Instant::now();
        let mut final_status = None;
        reconciler.schedule_now(now);
        for attempt in 0..6 {
            let report = reconciler
                .reconcile_if_due(&patchbay, &mut driver, false, false, now, 1)
                .unwrap()
                .expect("the next retry should be due");
            final_status = report.rules.first().map(|rule| rule.status);
            if attempt < 5 {
                now += Duration::from_millis(100_u64 << (attempt + 1));
            }
        }
        assert_eq!(final_status, Some(ReconcileStatus::Failed));
        assert!(reconciler
            .reconcile_if_due(
                &patchbay,
                &mut driver,
                false,
                false,
                now + Duration::from_secs(60),
                1,
            )
            .unwrap()
            .is_none());
    }

    #[test]
    fn deleting_a_saved_rule_prevents_reconciliation_from_recreating_it() {
        let graph = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
        let output = graph.port_key(PortId(20)).unwrap();
        let input = graph.port_key(PortId(40)).unwrap();
        let mut patchbay = Patchbay::new("dynamic");
        patchbay.add_graph_connection(&graph, PortId(20), PortId(40), true);
        let mut driver = InMemoryDriver::new(graph);
        let mut reconciler = PatchbayReconciler::new();
        reconcile_now(
            &mut reconciler,
            &patchbay,
            &mut driver,
            Instant::now(),
            false,
            false,
        );
        let link = driver.graph().links.values().next().unwrap().id;
        driver.disconnect(link).unwrap();
        assert!(patchbay.remove_stable_connection(&output, &input));

        let report = reconcile_now(
            &mut reconciler,
            &patchbay,
            &mut driver,
            Instant::now(),
            false,
            false,
        );
        assert!(report.rules.is_empty());
        assert!(driver.graph().links.is_empty());
    }

    #[test]
    fn same_named_application_rules_remain_distinct_and_delete_only_one() {
        let first = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
        let mut second = dynamic_application_graph(11, 21, 101, "Browser stream", 30, 40);
        second
            .nodes
            .get_mut(&pw_graph_core::NodeId(11))
            .unwrap()
            .identity
            .application_id = Some("org.example.other-browser".into());
        let mut graph = first.clone();
        graph.nodes.insert(
            pw_graph_core::NodeId(11),
            second.nodes[&pw_graph_core::NodeId(11)].clone(),
        );
        graph
            .ports
            .insert(PortId(21), second.ports[&PortId(21)].clone());

        let mut patchbay = Patchbay::new("same-name");
        patchbay.add_graph_connection(&graph, PortId(20), PortId(40), true);
        patchbay.add_graph_connection(&graph, PortId(21), PortId(40), true);
        assert_eq!(patchbay.connections.len(), 2);

        let first_output = graph.port_key(PortId(20)).unwrap();
        let input = graph.port_key(PortId(40)).unwrap();
        assert!(patchbay.remove_stable_connection(&first_output, &input));
        assert_eq!(patchbay.connections.len(), 1);
        assert_eq!(
            patchbay.connections[0]
                .output_selector
                .as_ref()
                .and_then(|selector| selector.identity.application_id.as_deref()),
            Some("org.example.other-browser")
        );
        assert_eq!(patchbay.connections[0].output_port, PortId(21));
    }

    #[test]
    fn exclusive_reconciliation_settles_without_oscillation() {
        let graph = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
        let mut extra = graph.clone();
        extra
            .add_node(pw_graph_core::Node::new(
                pw_graph_core::NodeId(50),
                "Other source",
                NodeType::PipeWire,
            ))
            .unwrap();
        extra
            .add_node(pw_graph_core::Node::new(
                pw_graph_core::NodeId(51),
                "Other sink",
                NodeType::PipeWire,
            ))
            .unwrap();
        extra
            .add_port(pw_graph_core::Port::new(
                PortId(60),
                pw_graph_core::NodeId(50),
                "output",
                Direction::Source,
                PortType::Audio,
            ))
            .unwrap();
        extra
            .add_port(pw_graph_core::Port::new(
                PortId(61),
                pw_graph_core::NodeId(51),
                "input",
                Direction::Sink,
                PortType::Audio,
            ))
            .unwrap();
        let mut patchbay = Patchbay::new("exclusive");
        patchbay.add_graph_connection(&graph, PortId(20), PortId(40), true);
        let mut driver = InMemoryDriver::new(extra);
        driver.connect(PortId(20), PortId(40)).unwrap();
        driver.connect(PortId(60), PortId(61)).unwrap();
        let mut reconciler = PatchbayReconciler::new();
        let first = reconcile_now(
            &mut reconciler,
            &patchbay,
            &mut driver,
            Instant::now(),
            true,
            false,
        );
        assert_eq!(first.disconnected, 1);
        assert_eq!(driver.graph().links.len(), 1);
        let second = reconcile_now(
            &mut reconciler,
            &patchbay,
            &mut driver,
            Instant::now(),
            true,
            false,
        );
        assert_eq!(second.disconnected, 0);
        assert_eq!(driver.graph().links.len(), 1);
    }

    #[test]
    fn one_shot_exclusive_activation_does_not_remove_links_for_waiting_rules() {
        let old = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
        let mut patchbay = Patchbay::new("exclusive-waiting");
        patchbay.add_graph_connection(&old, PortId(20), PortId(40), true);

        let mut current = old.clone();
        current.nodes.remove(&pw_graph_core::NodeId(10));
        current.ports.remove(&PortId(20));
        current
            .nodes
            .get_mut(&pw_graph_core::NodeId(30))
            .unwrap()
            .ports
            .clear();
        current
            .add_node(pw_graph_core::Node::new(
                pw_graph_core::NodeId(50),
                "Other source",
                NodeType::PipeWire,
            ))
            .unwrap();
        current
            .add_node(pw_graph_core::Node::new(
                pw_graph_core::NodeId(51),
                "Other sink",
                NodeType::PipeWire,
            ))
            .unwrap();
        current
            .add_port(pw_graph_core::Port::new(
                PortId(60),
                pw_graph_core::NodeId(50),
                "output",
                Direction::Source,
                PortType::Audio,
            ))
            .unwrap();
        current
            .add_port(pw_graph_core::Port::new(
                PortId(61),
                pw_graph_core::NodeId(51),
                "input",
                Direction::Sink,
                PortType::Audio,
            ))
            .unwrap();
        let mut driver = InMemoryDriver::new(current);
        driver.connect(PortId(60), PortId(61)).unwrap();

        let report = patchbay.activate(&mut driver, true, false).unwrap();
        assert_eq!(report.disconnected, 0);
        assert_eq!(report.waiting.len(), 1);
        assert!(driver
            .graph()
            .find_link_by_keys(&key(&driver, 60), &key(&driver, 61))
            .is_some());
    }

    #[test]
    fn repeated_application_recreation_does_not_accumulate_routes_or_stale_ids() {
        let first = dynamic_application_graph(10, 20, 100, "Browser stream", 30, 40);
        let mut patchbay = Patchbay::new("dynamic");
        patchbay.add_graph_connection(&first, PortId(20), PortId(40), true);
        let mut driver = InMemoryDriver::new(first);
        let mut reconciler = PatchbayReconciler::new();
        let mut now = Instant::now();
        for generation in 0..100_u64 {
            let base = 1000 + generation * 4;
            driver.replace_graph(dynamic_application_graph(
                base,
                base + 1,
                10_000 + generation,
                "Browser stream",
                base + 2,
                base + 3,
            ));
            reconcile_now(&mut reconciler, &patchbay, &mut driver, now, false, false);
            assert_eq!(driver.graph().links.len(), 1);
            assert_eq!(patchbay.connections.len(), 1);
            now += Duration::from_millis(1);
        }
    }
}
