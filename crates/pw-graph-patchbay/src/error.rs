//! Persistent connection sets and activation policies.
//!
//! The native qpwgraph format is XML and resolves rules by node/port names.
//! JSON remains supported as a convenient machine-readable format for tooling
//! and for compatibility with the first Rust prototype.

use pw_graph_backend::BackendError;
use thiserror::Error;

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
