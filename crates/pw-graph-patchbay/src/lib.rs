//! Persistent connection sets and activation policies.
//!
//! The native qpwgraph format is XML and resolves rules by node/port names.
//! JSON remains supported as a convenient machine-readable format for tooling
//! and for compatibility with the first Rust prototype.

mod activation;
mod connections;
mod error;
mod model;
mod persistence;
mod reconciler;
mod selectors;
mod xml;

#[cfg(test)]
mod tests;

pub use error::PatchbayError;
pub use model::{
    ActivationReport, PatchConnection, Patchbay, PatchbayReconciler, PatchbayResolution,
    ReconcileReport, ReconcileRuleStatus, ReconcileStatus,
};
