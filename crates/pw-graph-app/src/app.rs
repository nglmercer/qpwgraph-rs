use pw_graph_backend::MeterPolicy;
use pw_graph_command::CommandStack;
use pw_graph_config::AppConfig;
use pw_graph_core::{Link, NodeId, PortKey};
use pw_graph_i18n::I18n;
use pw_graph_patchbay::Patchbay;
use pw_graph_ui::GraphViewState;
use std::path::PathBuf;
use std::time::Instant;

#[cfg(all(target_os = "linux", feature = "tray"))]
use crate::tray::tray_support;

mod bootstrap;
mod configuration;
pub(crate) mod effects;
mod graph_actions;
mod layout;
mod lifecycle;
mod metering;
mod patchbay;
#[cfg(feature = "relay")]
mod relay;
mod ui;

pub(crate) use lifecycle::run;
#[cfg(feature = "relay")]
pub(crate) use relay::RelayUiState;

pub(crate) struct QpwgraphApp {
    pub(crate) driver: Box<dyn crate::backend::AppDriver>,
    pub(crate) commands: CommandStack,
    /// Framework-neutral graph state projected into Slint by `UiBridge`.
    pub(crate) canvas: GraphViewState,
    pub(crate) patchbay: Patchbay,
    pub(crate) config: AppConfig,
    config_saved_snapshot: AppConfig,
    config_dirty_since: Option<Instant>,
    pub(crate) config_file: PathBuf,
    pub(crate) patchbay_file: PathBuf,
    pub(crate) status: String,
    pub(crate) debug: bool,
    pub(crate) start_minimized: bool,
    pub(crate) i18n: I18n,
    pub(crate) backend_name: String,
    pub(crate) show_shortcuts: bool,
    pub(crate) show_history: bool,
    pub(crate) show_preferences: bool,
    pub(crate) show_effects: bool,
    #[cfg(feature = "relay")]
    pub(crate) show_relay: bool,
    pub(crate) effect_gallery: Option<effects::EffectGalleryState>,
    pub(crate) effect_gallery_scroll_epoch: u32,
    pub(crate) last_meter_refresh: Instant,
    pub(crate) last_graph_refresh: Instant,
    pub(crate) meter_policy: MeterPolicy,
    #[cfg(feature = "relay")]
    pub(crate) relay: RelayUiState,
    #[cfg(all(target_os = "linux", feature = "tray"))]
    pub(crate) tray: Option<tray_support::State>,
}

impl QpwgraphApp {
    pub(crate) fn t(&self, key: &str) -> String {
        self.i18n.text(key)
    }

    pub(crate) fn tf(&self, key: &str, variables: &[(&str, String)]) -> String {
        self.i18n.format(key, variables)
    }

    pub(crate) fn status_error(&mut self, key: &str, error: &impl std::fmt::Display) {
        self.status = self.tf(key, &[("error", error.to_string())]);
        if self.debug {
            eprintln!("[qpwgraph] {}", self.status);
        }
    }

    pub(crate) fn persist_report(
        &mut self,
        result: Result<(), impl std::fmt::Display>,
        failure_key: &str,
    ) -> bool {
        match result {
            Ok(()) => true,
            Err(error) => {
                self.status_error(failure_key, &error);
                false
            }
        }
    }

    #[cfg(feature = "relay")]
    pub(crate) fn with_relay<R>(&mut self, f: impl FnOnce(&mut Self, &mut RelayUiState) -> R) -> R {
        let mut relay = std::mem::take(&mut self.relay);
        let result = f(self, &mut relay);
        self.relay = relay;
        result
    }

    pub(crate) fn links_touching_node(&self, node: NodeId) -> Vec<Link> {
        self.driver
            .graph()
            .links
            .values()
            .filter(|link| {
                self.driver
                    .graph()
                    .port(link.output_port)
                    .is_some_and(|port| port.node_id == node)
                    || self
                        .driver
                        .graph()
                        .port(link.input_port)
                        .is_some_and(|port| port.node_id == node)
            })
            .cloned()
            .collect()
    }

    pub(crate) fn stable_link_pairs(&self, links: &[Link]) -> Vec<(PortKey, PortKey)> {
        links
            .iter()
            .filter_map(|link| {
                self.driver
                    .graph()
                    .port_key(link.output_port)
                    .zip(self.driver.graph().port_key(link.input_port))
            })
            .collect()
    }
}
