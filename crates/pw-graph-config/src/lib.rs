//! TOML configuration compatible with the state surface described by qpwgraph.

mod app;
mod error;
mod modes;
mod recording;
mod windows;

#[cfg(test)]
mod tests;

pub use app::{config_dir, config_path, AppConfig, PersistedEffect, PersistedRelayPeer};
pub use error::ConfigError;
pub use modes::{AudioDirection, RelayMode};
pub use pw_graph_core::NodeAppearance;
pub use recording::RecordingSaveMode;
pub use windows::{
    WindowsApplicationRoute, WindowsApplicationSelector, WindowsConfig, WindowsRelayConfig,
    WindowsRelayReceiveTarget, WindowsVirtualAudioConfig,
};
