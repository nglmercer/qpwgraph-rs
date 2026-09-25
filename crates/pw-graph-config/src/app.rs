//! TOML configuration compatible with the state surface described by qpwgraph.

use super::error::ConfigError;
use super::modes::{AudioDirection, RelayMode};
use super::recording::RecordingSaveMode;
use super::windows::{
    WindowsApplicationRoute, WindowsApplicationSelector, WindowsConfig, WindowsRelayReceiveTarget,
};
use pw_graph_core::{NodeAppearance, PortKey};
use pw_graph_effects::EffectInstanceConfig;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(default = "AppConfig::default")]
pub struct AppConfig {
    pub language: String,
    /// TOML table keys are strings, so node IDs are stored as decimal strings.
    pub node_positions: std::collections::BTreeMap<String, [f32; 2]>,
    /// Fallback layout keyed by backend-independent node type and name. This
    /// lets a saved layout survive PipeWire global node IDs changing between
    /// sessions. Ambiguous duplicate names are omitted by the app.
    pub node_positions_by_name: std::collections::BTreeMap<String, [f32; 2]>,
    /// Visual node overrides keyed by the same stable backend-independent key
    /// as the saved layout.
    pub node_view_by_name: std::collections::BTreeMap<String, NodeAppearance>,
    pub thumbnail_view: bool,
    pub minimap_visible: bool,
    pub window_width: f32,
    pub window_height: f32,
    pub zoom: f32,
    /// Multiplier for application chrome such as the toolbar and status bar.
    pub ui_text_scale: f32,
    /// Multiplier for navigation and Preferences panel text.
    pub panel_text_scale: f32,
    /// Multiplier for node titles, port labels, and node counters.
    pub node_text_scale: f32,
    pub media_filter: String,
    /// Case-insensitive text used to hide non-matching nodes and ports.
    pub graph_search: String,
    pub sort_type: String,
    pub sort_order: String,
    /// When helper streams may be attached to measure audio levels:
    /// `off`, `on-demand`, or `always`. See `pw_graph_backend::MeterPolicy`.
    pub audio_meters: String,
    pub repel_overlapping_nodes: bool,
    pub connect_through_nodes: bool,
    /// Node connect drag mode: `easy` (whole-node, matches all compatible
    /// ports) or `advanced` (precise, one port at a time).
    pub connect_mode: String,
    pub statusbar: bool,
    pub toolbar: bool,
    pub patchbay_toolbar: bool,
    pub patchbay_auto_pin: bool,
    pub patchbay_auto_disconnect: bool,
    pub patchbay_exclusive: bool,
    pub patchbay_activated: bool,
    pub patchbay_path: Option<PathBuf>,
    pub patchbay_dir: Option<PathBuf>,
    /// Most recently used patchbay files, newest first.
    pub recent_patchbay_paths: Vec<PathBuf>,
    /// Optional named patchbay profiles and their files.
    pub patchbay_profiles: std::collections::BTreeMap<String, PathBuf>,
    pub active_patchbay_profile: String,
    /// Effect definitions are kept in application config rather than the
    /// qpwgraph XML format, which has no portable representation for DSP
    /// modules.
    pub effects: Vec<PersistedEffect>,
    /// Directory used for automatic saves and as the initial directory for
    /// the native Save dialog. Active recordings are never persisted here.
    #[serde(default)]
    pub recording_dir: Option<PathBuf>,
    /// Pending crash-recovery files the user already dismissed. The
    /// recovery dialog auto-opens only for files not on this list, so a
    /// deferred decision stops nagging on every launch. Entries are
    /// pruned when their files disappear.
    #[serde(default)]
    pub recovery_dismissed: Vec<String>,
    #[serde(default = "default_recording_save_mode")]
    pub recording_save_mode: String,
    #[serde(default = "default_recording_filename_template")]
    pub recording_filename_template: String,
    #[serde(default = "default_recording_format")]
    pub recording_format: String,
    #[serde(default)]
    pub windows: WindowsConfig,
    #[serde(default)]
    pub windows_application_routes: Vec<WindowsApplicationRoute>,
    /// Stable identity for this installation. It is not a secret; it lets a
    /// peer recognize this device again after a Wi-Fi/USB address change.
    #[serde(default)]
    pub relay_device_id: String,
    /// Owner-only trusted relay credentials created after explicit PIN
    /// pairing. Secrets are hex encoded so the TOML remains portable.
    #[serde(default)]
    pub relay_trusted_peers: Vec<PersistedRelayPeer>,
    #[serde(default = "default_relay_auto_connect")]
    pub relay_auto_connect_trusted: bool,
    pub relay_device_name: String,
    /// Pairing PIN this machine offers when hosting.
    ///
    /// Deliberately not persisted. A PIN that lives in a config file is a
    /// long-lived shared secret in plaintext; a host PIN wants to be
    /// ephemeral, freshly generated per hosting session and shown on screen.
    /// A shipped default was worse still — every fresh install hosted behind
    /// the same globally known `123456`.
    #[serde(skip)]
    pub relay_host_pin: String,
    pub relay_host_port: u16,
    pub relay_client_target: String,
    /// PIN last used to pair with a host. Also not persisted: it is the
    /// host's secret, and keeping it on disk buys convenience at the cost of
    /// leaving a working credential in a world-readable file.
    #[serde(skip)]
    pub relay_client_pin: String,
    /// Deprecated direction-first relay setting. It is still deserialized
    /// from old files, but is never written again; new files use
    /// `relay_mode` below.
    #[serde(default, alias = "relay_role", skip_serializing)]
    pub relay_direction: AudioDirection,
    /// Monotonic generation used by the authenticated direction negotiation.
    /// It is persisted with the desired direction so a reconnect cannot
    /// resurrect an older offline choice.
    #[serde(default, skip_serializing)]
    pub relay_direction_generation: u64,
    /// Canonical generic role and generation. The deprecated direction fields
    /// above are kept readable for older callers and are synchronized by the
    /// application when a mode is changed.
    #[serde(default)]
    pub relay_mode: RelayMode,
    #[serde(default)]
    pub relay_mode_generation: u64,
    /// Canonical local Emitter source selector. The value is an opaque
    /// backend id such as `default-input`, `default-output-monitor`,
    /// `input:<id>`, `monitor:<id>`, or `application:<stable-selector>`.
    #[serde(default = "default_relay_send_source")]
    pub relay_send_source: String,
    /// Canonical local Receiver sink selector. The value is an opaque backend
    /// id such as `default-output` or `output:<id>`.
    #[serde(default = "default_relay_receive_sink")]
    pub relay_receive_sink: String,
    pub relay_codec: String,
    pub relay_frame_ms: u16,
    pub relay_transport: String,
    /// Windows relay endpoint selections. `None` follows the current default
    /// playback endpoint; the values are opaque Core Audio device IDs.
    #[serde(default, skip_serializing)]
    pub relay_capture_endpoint_id: Option<String>,
    #[serde(default, skip_serializing)]
    pub relay_playback_endpoint_id: Option<String>,
    /// Relay playback: whether received Android audio is routed to local speakers.
    #[serde(default = "default_true")]
    pub relay_playback_enabled: bool,
    /// Linear gain 0.0..2.0 (0%..200%)
    #[serde(default = "default_playback_gain")]
    pub relay_playback_gain: f32,
    #[serde(default)]
    pub relay_playback_muted: bool,
    /// Stable sink preference: node.name (serial resolves if available)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_playback_sink: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_playback_sink_serial: Option<u64>,
    /// Preserve fields written by a newer version so opening and saving a
    /// config with this version does not silently erase forward-compatible
    /// settings.
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}
impl fmt::Debug for AppConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppConfig")
            .field("language", &self.language)
            .field("relay_device_id", &self.relay_device_id)
            .field("relay_trusted_peers", &self.relay_trusted_peers)
            .field(
                "relay_auto_connect_trusted",
                &self.relay_auto_connect_trusted,
            )
            .field("relay_device_name", &self.relay_device_name)
            .field("relay_host_pin", &"<redacted>")
            .field("relay_host_port", &self.relay_host_port)
            .field("relay_client_target", &self.relay_client_target)
            .field("relay_client_pin", &"<redacted>")
            .field("relay_direction", &self.relay_direction)
            .field(
                "relay_direction_generation",
                &self.relay_direction_generation,
            )
            .field("relay_mode", &self.relay_mode)
            .field("relay_mode_generation", &self.relay_mode_generation)
            .field("relay_send_source", &self.relay_send_source)
            .field("relay_receive_sink", &self.relay_receive_sink)
            .field("relay_codec", &self.relay_codec)
            .field("relay_frame_ms", &self.relay_frame_ms)
            .field("relay_transport", &self.relay_transport)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Deserialize, PartialEq, Serialize)]
pub struct PersistedRelayPeer {
    pub peer_id: String,
    pub secret: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub address: String,
    /// Optional per-peer role preference. `None` preserves the global mode
    /// for older trusted-peer records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_mode: Option<RelayMode>,
}
impl fmt::Debug for PersistedRelayPeer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PersistedRelayPeer")
            .field("peer_id", &self.peer_id)
            .field("secret", &"<redacted>")
            .field("name", &self.name)
            .field("address", &self.address)
            .field("preferred_mode", &self.preferred_mode)
            .finish()
    }
}
fn default_relay_auto_connect() -> bool {
    true
}
fn default_relay_send_source() -> String {
    "default-input".into()
}
fn default_relay_receive_sink() -> String {
    "default-output".into()
}
fn default_true() -> bool {
    true
}
fn default_playback_gain() -> f32 {
    1.0
}
fn default_recording_save_mode() -> String {
    RecordingSaveMode::AskOnStop.as_str().into()
}
fn default_recording_filename_template() -> String {
    "Recording {date} {time}".into()
}
fn default_recording_format() -> String {
    "wav-f32".into()
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PersistedEffect {
    pub instance: EffectInstanceConfig,
    /// The original endpoints for an effect inserted into a link. A detached
    /// effect node deliberately has no endpoints until the user patches it.
    #[serde(default)]
    pub source: Option<PortKey>,
    #[serde(default)]
    pub destination: Option<PortKey>,
    /// Stored independently of graph node IDs because PipeWire assigns fresh
    /// IDs whenever an effect node is recreated on startup.
    #[serde(default = "default_effect_position")]
    pub position: [f32; 2],
}
fn default_effect_position() -> [f32; 2] {
    [260.0, 180.0]
}
impl Default for AppConfig {
    fn default() -> Self {
        Self {
            language: "en".into(),
            node_positions: std::collections::BTreeMap::new(),
            node_positions_by_name: std::collections::BTreeMap::new(),
            node_view_by_name: std::collections::BTreeMap::new(),
            thumbnail_view: false,
            minimap_visible: false,
            window_width: 1100.0,
            window_height: 760.0,
            zoom: 1.0,
            ui_text_scale: 1.10,
            panel_text_scale: 1.20,
            node_text_scale: 1.15,
            media_filter: "all".into(),
            graph_search: String::new(),
            sort_type: "name".into(),
            sort_order: "ascending".into(),
            audio_meters: "on-demand".into(),
            repel_overlapping_nodes: true,
            connect_through_nodes: false,
            connect_mode: "advanced".into(),
            statusbar: true,
            toolbar: true,
            patchbay_toolbar: true,
            patchbay_auto_pin: false,
            patchbay_auto_disconnect: false,
            patchbay_exclusive: false,
            patchbay_activated: false,
            patchbay_path: None,
            patchbay_dir: None,
            recent_patchbay_paths: Vec::new(),
            patchbay_profiles: std::collections::BTreeMap::new(),
            active_patchbay_profile: "default".into(),
            effects: Vec::new(),
            recording_dir: None,
            recovery_dismissed: Vec::new(),
            recording_save_mode: default_recording_save_mode(),
            recording_filename_template: default_recording_filename_template(),
            recording_format: default_recording_format(),
            windows: WindowsConfig::default(),
            windows_application_routes: Vec::new(),
            relay_device_id: String::new(),
            relay_trusted_peers: Vec::new(),
            relay_auto_connect_trusted: true,
            relay_device_name: "qpwgraph-rs".into(),
            relay_host_pin: String::new(),
            // The desktop application participates in direct USB discovery.
            // Keep its application default stable; the relay SDK still
            // accepts port 0 when an embedding application explicitly asks
            // the OS for an ephemeral port.
            relay_host_port: 48123,
            relay_client_target: String::new(),
            relay_client_pin: String::new(),
            relay_direction: AudioDirection::MobileToDesktop,
            relay_direction_generation: 0,
            relay_mode: RelayMode::Receiver,
            relay_mode_generation: 0,
            relay_send_source: default_relay_send_source(),
            relay_receive_sink: default_relay_receive_sink(),
            relay_codec: "opus".into(),
            // Ten milliseconds halves the codec-side latency floor of the
            // previous 20 ms default at the cost of doubling the packet rate
            // to 100/s, which local Wi-Fi and USB tether links carry
            // comfortably. The relay panel's advanced settings still expose
            // 5–60 ms for links that prefer fewer, larger packets.
            relay_frame_ms: 10,
            relay_transport: "auto".into(),
            relay_capture_endpoint_id: None,
            relay_playback_endpoint_id: None,
            relay_playback_enabled: true,
            relay_playback_gain: 1.0,
            relay_playback_muted: false,
            relay_playback_sink: None,
            relay_playback_sink_serial: None,
            extra: BTreeMap::new(),
        }
    }
}
impl AppConfig {
    /// Resolve the most specific enabled persisted Windows application route
    /// for a live stable selector.  A PID is deliberately absent from this
    /// operation, so a process restart or PID reuse cannot select an
    /// unrelated route.
    pub fn matching_windows_application_route(
        &self,
        candidate: &WindowsApplicationSelector,
    ) -> Option<&WindowsApplicationRoute> {
        self.windows_application_routes
            .iter()
            .enumerate()
            .filter(|(_, route)| route.matches_application(candidate))
            .fold(
                None,
                |best: Option<(usize, &WindowsApplicationRoute)>, current| {
                    let replace = best.as_ref().is_none_or(|(best_index, best_route)| {
                        current.1.selector_specificity() > best_route.selector_specificity()
                            || (current.1.selector_specificity()
                                == best_route.selector_specificity()
                                && current.0 < *best_index)
                    });
                    replace.then_some(current).or(best)
                },
            )
            .map(|(_, route)| route)
    }

    /// Set the canonical role and keep the legacy in-memory compatibility
    /// fields aligned for older embedders.
    pub fn set_relay_mode(&mut self, mode: RelayMode, generation: u64) {
        self.relay_mode = mode;
        self.relay_mode_generation = generation;
        self.relay_direction = mode.audio_direction();
        self.relay_direction_generation = generation;
    }

    /// Normalize a config loaded from a pre-generic direction key. This is
    /// intentionally explicit so callers that deserialize through TOML
    /// directly can opt into the same migration as [`Self::load_from`].
    pub fn migrate_relay_mode(&mut self) {
        // A non-zero legacy generation is an unambiguous signal that this
        // document predates the generic key. Copy both pieces of the old
        // preference before synchronizing the compatibility fields. For a
        // generation-zero document, preserve the historical default unless
        // its direction is the only available preference.
        if self.relay_mode_generation == 0 {
            if self.relay_direction_generation != 0 {
                self.relay_mode = RelayMode::from_audio_direction(self.relay_direction);
                self.relay_mode_generation = self.relay_direction_generation;
            } else if self.relay_mode == RelayMode::Receiver {
                self.relay_mode = RelayMode::from_audio_direction(self.relay_direction);
            }
        }
        self.relay_direction = self.relay_mode.audio_direction();
        self.relay_direction_generation = self.relay_mode_generation;
        self.migrate_relay_routes();
    }

    /// Convert endpoint preferences written by the old Windows playback pair
    /// into the platform-neutral source/sink selectors. New code only writes
    /// the generic fields, while old fields remain readable for one migration
    /// period.
    pub fn migrate_relay_routes(&mut self) {
        if self.relay_send_source == default_relay_send_source() {
            if let Some(endpoint) = self.relay_capture_endpoint_id.as_deref() {
                self.relay_send_source = format!("monitor:{endpoint}");
            }
        }
        // Keep the explicit Windows table and the platform-neutral selector
        // in lockstep. The neutral key is what the backend consumes, while
        // the nested key makes the Windows capability visible to users and
        // future frontends.
        if self.relay_receive_sink == default_relay_receive_sink()
            && self.windows.relay.receive_target == WindowsRelayReceiveTarget::VirtualMicrophone
        {
            self.relay_receive_sink = "virtual-microphone".into();
        }
        if self.relay_receive_sink == "virtual-microphone" {
            self.windows.relay.receive_target = WindowsRelayReceiveTarget::VirtualMicrophone;
        }
        if self.relay_receive_sink == default_relay_receive_sink() {
            if let Some(endpoint) = self.relay_playback_endpoint_id.as_deref() {
                self.relay_receive_sink = format!("output:{endpoint}");
            } else if let Some(sink) = self.relay_playback_sink.as_deref() {
                self.relay_receive_sink = sink.to_owned();
            }
        }
    }

    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(ConfigError::Read)?;
        let mut config: Self = toml::from_str(&text)?;
        config.migrate_relay_mode();
        Ok(config)
    }

    /// Save the configuration.
    ///
    /// The write is atomic (temporary sibling plus rename) so a crash or a
    /// full disk cannot destroy the only copy of the user's settings, and the
    /// file is created owner-only.
    pub fn save_to(&self, path: impl AsRef<Path>) -> Result<(), ConfigError> {
        // Callers that deserialize through `toml` directly may not have run
        // the migration hook. Normalize a clone here so every persisted file
        // uses the canonical generic keys, without mutating the live UI state
        // while it is being saved.
        let mut config = self.clone();
        config.migrate_relay_mode();
        let text = toml::to_string_pretty(&config)?;
        pw_graph_utils::atomic_write(path.as_ref(), text.as_bytes(), true)
            .map_err(ConfigError::Write)
    }
}
pub fn config_dir(app_name: &str) -> PathBuf {
    #[cfg(target_os = "windows")]
    if let Some(path) = std::env::var_os("APPDATA") {
        return PathBuf::from(path).join(app_name);
    }

    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path).join(app_name);
    }
    if let Some(path) = std::env::var_os("HOME") {
        return PathBuf::from(path).join(".config").join(app_name);
    }
    PathBuf::from(".").join(format!(".{app_name}"))
}
pub fn config_path(app_name: &str) -> PathBuf {
    config_dir(app_name).join("config.toml")
}
