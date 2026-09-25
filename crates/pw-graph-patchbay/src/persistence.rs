//! Persistent connection sets and activation policies.
//!
//! The native qpwgraph format is XML and resolves rules by node/port names.
//! JSON remains supported as a convenient machine-readable format for tooling
//! and for compatibility with the first Rust prototype.

use super::error::PatchbayError;
use super::model::{Patchbay, XmlSelectorSidecarEntry};
use super::selectors::{connection_fingerprint, load_xml_selector_sidecar, selector_sidecar_path};
use std::path::Path;

impl Patchbay {
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
}
