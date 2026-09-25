//! Persistent connection sets and activation policies.
//!
//! The native qpwgraph format is XML and resolves rules by node/port names.
//! JSON remains supported as a convenient machine-readable format for tooling
//! and for compatibility with the first Rust prototype.

use super::selectors::selectors_equivalent;
use pw_graph_core::{EndpointSelector, NodeType, PortId, PortKey, PortType};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

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
pub(crate) struct XmlSelectorSidecarEntry {
    #[serde(default)]
    pub(crate) index: Option<usize>,
    pub(crate) fingerprint: String,
    pub(crate) output_selector: Option<EndpointSelector>,
    pub(crate) input_selector: Option<EndpointSelector>,
    #[serde(default)]
    pub(crate) output_node_type: Option<NodeType>,
    #[serde(default)]
    pub(crate) input_node_type: Option<NodeType>,
    #[serde(default)]
    pub(crate) port_type: Option<PortType>,
}

impl PatchConnection {
    pub(crate) fn effective_output_node_type(&self) -> NodeType {
        self.output_node_type.unwrap_or(self.node_type)
    }

    pub(crate) fn effective_input_node_type(&self) -> NodeType {
        self.input_node_type.unwrap_or(self.node_type)
    }

    pub(crate) fn matches_stable_pair(&self, output: &PortKey, input: &PortKey) -> bool {
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
pub(crate) struct RetryState {
    pub(crate) attempts: u8,
    pub(crate) next_retry: Instant,
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
    pub(crate) pending: bool,
    pub(crate) next_run: Option<Instant>,
    pub(crate) last_graph_generation: u64,
    pub(crate) retry_state: BTreeMap<usize, RetryState>,
    pub(crate) last_report: ReconcileReport,
    pub(crate) debounce: Duration,
}

impl Default for PatchbayReconciler {
    fn default() -> Self {
        Self::new()
    }
}
