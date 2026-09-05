//! TOML configuration compatible with the state surface described by qpwgraph.

pub use pw_graph_core::NodeAppearance;
use pw_graph_core::PortKey;
use pw_graph_effects::EffectInstanceConfig;
use serde::de::Error as DeserializeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Windows-only behavior is persisted on every platform so a configuration
/// remains portable, but non-Windows backends simply ignore this table.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct WindowsConfig {
    pub enable_process_loopback: bool,
    /// The AudioPolicyConfig ABI is undocumented and therefore opt-in.
    pub experimental_app_routing: bool,
    pub prefer_virtual_app_routes: bool,
    pub virtual_audio: WindowsVirtualAudioConfig,
    pub relay: WindowsRelayConfig,
}

impl Default for WindowsConfig {
    fn default() -> Self {
        Self {
            enable_process_loopback: true,
            experimental_app_routing: false,
            prefer_virtual_app_routes: true,
            virtual_audio: WindowsVirtualAudioConfig::default(),
            relay: WindowsRelayConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct WindowsVirtualAudioConfig {
    pub enabled: bool,
}

impl Default for WindowsVirtualAudioConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct WindowsRelayConfig {
    pub receive_target: WindowsRelayReceiveTarget,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WindowsRelayReceiveTarget {
    #[default]
    Direct,
    VirtualMicrophone,
}

impl WindowsRelayReceiveTarget {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::VirtualMicrophone => "virtual-microphone",
        }
    }
}

impl Serialize for WindowsRelayReceiveTarget {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for WindowsRelayReceiveTarget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "direct" => Ok(Self::Direct),
            "virtual-microphone" => Ok(Self::VirtualMicrophone),
            value => Err(D::Error::custom(format!(
                "invalid Windows relay receive target '{value}'"
            ))),
        }
    }
}

/// Stable application identity used by persisted Windows routes. A PID is
/// intentionally absent: Windows may reuse it for an unrelated process.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct WindowsApplicationSelector {
    pub executable_path_hash: Option<String>,
    pub executable_name: Option<String>,
    pub package_family_name: Option<String>,
    pub app_user_model_id: Option<String>,
    pub display_name: Option<String>,
}

impl WindowsApplicationSelector {
    /// A persisted selector is safe to resolve only when it contains at least
    /// one identity that survives a process restart. A display name alone is
    /// intentionally not enough because unrelated applications may reuse it.
    pub fn is_stable(&self) -> bool {
        self.executable_path_hash.is_some()
            || self.app_user_model_id.is_some()
            || (self.package_family_name.is_some() && self.executable_name.is_some())
    }

    pub fn stable_key(&self) -> Option<&str> {
        self.app_user_model_id
            .as_deref()
            .or(self.package_family_name.as_deref())
            .or(self.executable_path_hash.as_deref())
    }

    /// Runtime key used to share process-loopback leases. A package family
    /// can contain more than one executable, so package-family-only identity
    /// is not sufficient for an in-process capture registry. Persisted route
    /// matching still uses the complete selector fields above.
    pub fn runtime_key(&self) -> Option<String> {
        if let Some(aumid) = self.app_user_model_id.as_deref() {
            return Some(aumid.to_owned());
        }
        if let Some(package) = self.package_family_name.as_deref() {
            let executable = self
                .executable_name
                .as_deref()
                .or(self.executable_path_hash.as_deref())?;
            return Some(format!("package-family:{package}|executable:{executable}"));
        }
        self.executable_path_hash.clone()
    }

    /// Match this selector against a live candidate using the strongest
    /// identity available. AUMID and package identity deliberately outrank
    /// the executable path: package updates are allowed to move the binary
    /// while preserving the application's durable identity. `display_name`
    /// is retained as a UI hint and never gates activation.
    pub fn matches(&self, candidate: &Self) -> bool {
        if !self.is_stable() {
            return false;
        }
        if let Some(expected) = self.app_user_model_id.as_deref() {
            return candidate
                .app_user_model_id
                .as_deref()
                .is_some_and(|actual| expected.eq_ignore_ascii_case(actual));
        }
        if let (Some(expected_package), Some(expected_name)) = (
            self.package_family_name.as_deref(),
            self.executable_name.as_deref(),
        ) {
            return candidate
                .package_family_name
                .as_deref()
                .is_some_and(|actual| expected_package.eq_ignore_ascii_case(actual))
                && candidate
                    .executable_name
                    .as_deref()
                    .is_some_and(|actual| expected_name.eq_ignore_ascii_case(actual));
        }
        self.executable_path_hash
            .as_deref()
            .zip(candidate.executable_path_hash.as_deref())
            .is_some_and(|(expected, actual)| expected.eq_ignore_ascii_case(actual))
    }

    fn specificity(&self) -> usize {
        [
            &self.executable_path_hash,
            &self.executable_name,
            &self.package_family_name,
            &self.app_user_model_id,
            &self.display_name,
        ]
        .into_iter()
        .filter(|field| field.is_some())
        .count()
    }
}

/// Persisted qpwgraph-owned application route.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct WindowsApplicationRoute {
    pub application: WindowsApplicationSelector,
    /// Stable endpoint identity introduced after the original route schema.
    /// The older `destination_endpoint_id` and `destination_name` fields stay
    /// readable so existing configurations can be upgraded lazily.
    pub destination_stable_id: Option<String>,
    pub destination_mmdevice_id: Option<String>,
    pub destination_endpoint_id: Option<String>,
    pub destination_name: Option<String>,
    pub virtualization_required: bool,
    /// Legacy effect identifiers retained for old configuration files. They
    /// are intentionally not enough to restore a route because they omit
    /// parameters, bypass state, enabled state, and instance identity.
    pub effect_chain: Vec<String>,
    /// Complete, ordered effect instances for a Windows application route.
    /// This is separate from `effect_chain` so old TOML remains readable and
    /// old callers do not accidentally restore an effect with defaults.
    #[serde(default)]
    pub effect_instances: Vec<EffectInstanceConfig>,
    pub gain: f32,
    pub enabled: bool,
}

impl WindowsApplicationRoute {
    /// Whether this persisted route is eligible for a live candidate. A
    /// disabled route or a selector with only a display name is never applied.
    pub fn matches_application(&self, candidate: &WindowsApplicationSelector) -> bool {
        self.enabled && self.application.matches(candidate)
    }

    /// More specific selectors win when a user has retained both a broad
    /// executable rule and a package/display-name override.  Ties preserve
    /// file order, making the result deterministic without inventing a
    /// hidden priority field in the persisted format.
    pub fn selector_specificity(&self) -> usize {
        self.application.specificity()
    }

    /// Return the effect configuration that can be restored transactionally.
    ///
    /// A legacy route may contain only effect IDs in `effect_chain`; treating
    /// those as a request to create default processors would silently change
    /// the user's audio. Such routes are therefore rejected until they have
    /// been upgraded with complete `effect_instances` data.
    pub fn restorable_effect_instances(&self) -> Result<Vec<EffectInstanceConfig>, String> {
        if self.effect_instances.is_empty() {
            return if self.effect_chain.is_empty() {
                Ok(Vec::new())
            } else {
                Err("saved route contains legacy effect IDs without instance configuration".into())
            };
        }

        if !self.effect_chain.is_empty()
            && (self.effect_chain.len() != self.effect_instances.len()
                || self
                    .effect_chain
                    .iter()
                    .zip(&self.effect_instances)
                    .any(|(id, instance)| id != &instance.effect_id))
        {
            return Err(
                "saved route effect ID list does not match its instance configuration".into(),
            );
        }
        Ok(self.effect_instances.clone())
    }
}

impl Default for WindowsApplicationRoute {
    fn default() -> Self {
        Self {
            application: WindowsApplicationSelector::default(),
            destination_stable_id: None,
            destination_mmdevice_id: None,
            destination_endpoint_id: None,
            destination_name: None,
            virtualization_required: true,
            effect_chain: Vec::new(),
            effect_instances: Vec::new(),
            gain: 1.0,
            enabled: true,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read config: {0}")]
    Read(#[source] std::io::Error),
    #[error("could not write config: {0}")]
    Write(#[source] std::io::Error),
    #[error("invalid config TOML: {0}")]
    Format(#[from] toml::de::Error),
    #[error("could not serialize config TOML: {0}")]
    Serialize(#[from] toml::ser::Error),
}

/// The user-facing direction of a relay session.
///
/// Relay roles are deliberately not part of the persisted desktop state. The
/// bridge derives a receive-only host or an emit-only client from this value,
/// which keeps the direction visible without exposing the old three-way
/// `emit`/`receive`/`both` switch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AudioDirection {
    /// Audio flows from the phone into the desktop relay microphone.
    #[default]
    MobileToDesktop,
    /// Audio flows from the desktop relay speaker into the phone.
    DesktopToMobile,
}

/// Platform-neutral local relay role. The desktop uses this field for its
/// own role; Android has the same two-value model at its JNI boundary.
/// `AudioDirection` remains as a serde/API compatibility shim for configs
/// written by the direction-first release.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RelayMode {
    Emitter,
    /// A desktop installation historically defaulted to receiving phone
    /// audio, so preserve that default while the generic key is introduced.
    #[default]
    Receiver,
}

impl RelayMode {
    pub const EMITTER: &'static str = "emitter";
    pub const RECEIVER: &'static str = "receiver";

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Emitter => Self::EMITTER,
            Self::Receiver => Self::RECEIVER,
        }
    }

    /// Parse canonical values and the desktop's one-release legacy role
    /// values. Legacy `both` is intentionally collapsed to Receiver rather
    /// than being allowed to create a bidirectional session.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "emitter" | "emit" | "desktop_to_mobile" | "pc_to_mobile" => Some(Self::Emitter),
            "receiver" | "receive" | "both" | "mobile_to_desktop" | "mobile_to_pc" => {
                Some(Self::Receiver)
            }
            _ => None,
        }
    }

    pub const fn from_audio_direction(direction: AudioDirection) -> Self {
        match direction {
            AudioDirection::MobileToDesktop => Self::Receiver,
            AudioDirection::DesktopToMobile => Self::Emitter,
        }
    }

    pub const fn audio_direction(self) -> AudioDirection {
        match self {
            Self::Emitter => AudioDirection::DesktopToMobile,
            Self::Receiver => AudioDirection::MobileToDesktop,
        }
    }
}

impl Serialize for RelayMode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RelayMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).ok_or_else(|| {
            D::Error::custom(format!(
                "invalid relay mode '{value}'; expected emitter or receiver"
            ))
        })
    }
}

impl AudioDirection {
    pub const MOBILE_TO_DESKTOP: &'static str = "mobile_to_desktop";
    pub const DESKTOP_TO_MOBILE: &'static str = "desktop_to_mobile";

    /// Canonical value written to the configuration file.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MobileToDesktop => Self::MOBILE_TO_DESKTOP,
            Self::DesktopToMobile => Self::DESKTOP_TO_MOBILE,
        }
    }

    /// Parse both the current direction values and the desktop's legacy role
    /// values. Legacy `emit` means the desktop was the client, so it maps to
    /// PC → Mobile; `receive` and `both` map deterministically to Mobile → PC.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "mobile_to_desktop" | "mobile_to_pc" | "receive" | "both" => {
                Some(Self::MobileToDesktop)
            }
            "desktop_to_mobile" | "pc_to_mobile" | "emit" => Some(Self::DesktopToMobile),
            _ => None,
        }
    }
}

impl Serialize for AudioDirection {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AudioDirection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).ok_or_else(|| {
            D::Error::custom(format!(
                "invalid relay direction '{value}'; expected mobile_to_desktop or desktop_to_mobile"
            ))
        })
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        // No shipped default: a fresh install must not host behind a PIN
        // every other install also has.
        assert!(AppConfig::default().relay_host_pin.is_empty());
        assert!(AppConfig::default().relay_client_pin.is_empty());
        assert_eq!(AppConfig::default().relay_host_port, 48123);
        assert_eq!(
            AppConfig::default().relay_direction,
            AudioDirection::MobileToDesktop
        );
        assert_eq!(AppConfig::default().relay_direction_generation, 0);
        let directory =
            std::env::temp_dir().join(format!("pw-graph-config-{}", std::process::id()));
        let path = directory.join("config.toml");
        let expected = AppConfig {
            relay_device_id: "studio-installation".into(),
            relay_trusted_peers: vec![PersistedRelayPeer {
                peer_id: "phone-installation".into(),
                secret: "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff".into(),
                name: "phone".into(),
                address: "192.168.42.2:48123".into(),
                preferred_mode: None,
            }],
            relay_auto_connect_trusted: false,
            relay_device_name: "studio-pc".into(),
            relay_host_pin: String::new(),
            relay_host_port: 0,
            relay_client_target: "192.168.1.20:48123".into(),
            relay_send_source: "monitor:capture-endpoint".into(),
            relay_receive_sink: "output:playback-endpoint".into(),
            ..AppConfig::default()
        };
        expected.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_from(&path).unwrap(), expected);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn relay_direction_migrates_legacy_desktop_roles_and_writes_only_the_generic_key() {
        let cases = [
            ("emit", AudioDirection::DesktopToMobile),
            ("receive", AudioDirection::MobileToDesktop),
            ("both", AudioDirection::MobileToDesktop),
        ];
        for (legacy_role, expected) in cases {
            let mut config: AppConfig =
                toml::from_str(&format!("language = 'en'\nrelay_role = '{legacy_role}'\n"))
                    .unwrap();
            config.migrate_relay_mode();
            assert_eq!(config.relay_direction, expected);
            let serialized = toml::to_string(&config).unwrap();
            assert!(serialized.contains("relay_mode ="));
            assert!(!serialized.contains("relay_direction ="));
            assert!(!serialized.contains("relay_role ="));
        }
    }

    #[test]
    fn relay_direction_accepts_canonical_values_and_canonicalizes_pc_alias() {
        let mobile_to_desktop: AppConfig =
            toml::from_str("language = 'en'\nrelay_direction = 'mobile_to_desktop'\n").unwrap();
        assert_eq!(
            mobile_to_desktop.relay_direction,
            AudioDirection::MobileToDesktop
        );

        let mut desktop_to_mobile: AppConfig =
            toml::from_str("language = 'en'\nrelay_direction = 'pc_to_mobile'\n").unwrap();
        assert_eq!(
            desktop_to_mobile.relay_direction,
            AudioDirection::DesktopToMobile
        );
        desktop_to_mobile.migrate_relay_mode();
        assert_eq!(desktop_to_mobile.relay_mode, RelayMode::Emitter);
        let serialized = toml::to_string(&desktop_to_mobile).unwrap();
        assert!(serialized.contains("relay_mode = \"emitter\""));
        assert!(!serialized.contains("relay_direction ="));
    }

    #[test]
    fn relay_direction_generation_is_persisted_with_the_canonical_direction() {
        let mut config: AppConfig = toml::from_str(
            "language = 'en'\nrelay_direction = 'desktop_to_mobile'\nrelay_direction_generation = 17\n",
        )
        .unwrap();
        config.migrate_relay_mode();
        assert_eq!(config.relay_direction, AudioDirection::DesktopToMobile);
        assert_eq!(config.relay_direction_generation, 17);

        let serialized = toml::to_string(&config).unwrap();
        assert!(serialized.contains("relay_mode = \"emitter\""));
        assert!(serialized.contains("relay_mode_generation = 17"));
        assert!(!serialized.contains("relay_direction ="));
        assert!(!serialized.contains("relay_direction_generation ="));
        assert!(!serialized.contains("relay_role ="));
    }

    #[test]
    fn pairing_pins_never_reach_disk() {
        // Keep this directory distinct from `defaults_round_trip`: tests run
        // concurrently, and that test removes its directory after saving.
        let directory =
            std::env::temp_dir().join(format!("pw-graph-config-pins-{}", std::process::id()));
        let path = directory.join("pins.toml");
        let config = AppConfig {
            relay_host_pin: "864209".into(),
            relay_client_pin: "135790".into(),
            ..AppConfig::default()
        };
        config.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("864209"), "the host PIN was written to disk");
        assert!(
            !text.contains("135790"),
            "the client PIN was written to disk"
        );
        let reloaded = AppConfig::load_from(&path).unwrap();
        assert!(reloaded.relay_host_pin.is_empty());
        assert!(reloaded.relay_client_pin.is_empty());
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn config_debug_redacts_pairing_pins_and_trusted_secrets() {
        let config = AppConfig {
            relay_host_pin: "864209".into(),
            relay_client_pin: "135790".into(),
            relay_trusted_peers: vec![PersistedRelayPeer {
                peer_id: "phone".into(),
                secret: "ab".repeat(32),
                name: "phone".into(),
                address: "192.168.42.2:48123".into(),
                preferred_mode: None,
            }],
            ..AppConfig::default()
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("864209"));
        assert!(!debug.contains("135790"));
        assert!(!debug.contains(&"ab".repeat(32)));
        assert!(debug.contains("redacted"));
    }

    #[cfg(unix)]
    #[test]
    fn the_config_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir()
            .join(format!("pw-graph-config-mode-{}", std::process::id()))
            .join("config.toml");
        AppConfig::default().save_to(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn old_configs_default_relay_endpoint_choices_to_system_default() {
        let config: AppConfig = toml::from_str("language = 'en'\n").unwrap();
        assert_eq!(config.relay_capture_endpoint_id, None);
        assert_eq!(config.relay_playback_endpoint_id, None);
        assert_eq!(config.relay_send_source, "default-input");
        assert_eq!(config.relay_receive_sink, "default-output");
    }

    #[test]
    fn old_windows_endpoint_choices_migrate_to_generic_route_selectors() {
        let mut config: AppConfig = toml::from_str(
            "language = 'en'\nrelay_capture_endpoint_id = 'capture-id'\nrelay_playback_endpoint_id = 'output-id'\n",
        )
        .unwrap();
        config.migrate_relay_mode();
        assert_eq!(config.relay_send_source, "monitor:capture-id");
        assert_eq!(config.relay_receive_sink, "output:output-id");
        let serialized = toml::to_string(&config).unwrap();
        assert!(!serialized.contains("relay_capture_endpoint_id"));
        assert!(!serialized.contains("relay_playback_endpoint_id"));
    }

    #[test]
    fn node_positions_round_trip() {
        let directory =
            std::env::temp_dir().join(format!("pw-graph-config-positions-{}", std::process::id()));
        let path = directory.join("config.toml");
        let mut expected = AppConfig::default();
        expected.node_positions.insert("42".into(), [120.5, -18.0]);
        expected
            .node_positions
            .insert("9001".into(), [640.0, 240.25]);
        expected
            .node_positions_by_name
            .insert("PipeWire:Capture".into(), [120.5, -18.0]);
        expected.node_view_by_name.insert(
            "PipeWire:Capture".into(),
            NodeAppearance {
                collapsed: true,
                custom_name: Some("Microphone".into()),
                color: Some([82, 207, 133, 255]),
            },
        );
        expected.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_from(&path).unwrap(), expected);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn effect_configuration_round_trips() {
        let directory =
            std::env::temp_dir().join(format!("pw-graph-config-effects-{}", std::process::id()));
        let path = directory.join("config.toml");
        let mut expected = AppConfig::default();
        expected.effects.push(PersistedEffect {
            instance: EffectInstanceConfig {
                instance_id: "effect-1".into(),
                effect_id: "builtin.noise-gate".into(),
                module_path: None,
                enabled: true,
                parameters: [("threshold-db".into(), -42.0)].into_iter().collect(),
            },
            source: Some(PortKey {
                node_name: "Capture".into(),
                node_serial: None,
                node_type: pw_graph_core::NodeType::PipeWire,
                port_name: "out_FL".into(),
                channel: Some("FL".into()),
                direction: pw_graph_core::Direction::Source,
                port_type: pw_graph_core::PortType::Audio,
            }),
            destination: Some(PortKey {
                node_name: "Playback".into(),
                node_serial: None,
                node_type: pw_graph_core::NodeType::PipeWire,
                port_name: "in_FL".into(),
                channel: Some("FL".into()),
                direction: pw_graph_core::Direction::Sink,
                port_type: pw_graph_core::PortType::Audio,
            }),
            position: [260.0, 180.0],
        });
        expected.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_from(&path).unwrap(), expected);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn legacy_effect_without_routing_or_position_loads_as_a_standalone_node() {
        let config: AppConfig = toml::from_str(
            r#"
effects = [{ instance = { instance_id = "legacy-effect", effect_id = "builtin.noise-gate" } }]
"#,
        )
        .unwrap();

        let effect = config.effects.first().unwrap();
        assert_eq!(effect.instance.instance_id, "legacy-effect");
        assert_eq!(effect.source, None);
        assert_eq!(effect.destination, None);
        assert_eq!(effect.position, [260.0, 180.0]);
    }

    #[test]
    fn unknown_fields_survive_a_config_round_trip() {
        let directory =
            std::env::temp_dir().join(format!("pw-graph-config-extra-{}", std::process::id()));
        let path = directory.join("config.toml");
        let original = r#"
language = "es"
future_setting = "keep me"
[future_table]
enabled = true
"#;
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(&path, original).unwrap();

        let config = AppConfig::load_from(&path).unwrap();
        assert_eq!(
            config.extra.get("future_setting"),
            Some(&toml::Value::String("keep me".into()))
        );
        config.save_to(&path).unwrap();
        let restored = AppConfig::load_from(&path).unwrap();
        assert_eq!(restored.extra, config.extra);
        assert_eq!(
            restored.extra.get("future_table"),
            config.extra.get("future_table")
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn windows_capabilities_and_application_routes_round_trip() {
        let mut config = AppConfig::default();
        config.windows.experimental_app_routing = true;
        config.windows.relay.receive_target = WindowsRelayReceiveTarget::VirtualMicrophone;
        let mut effect_parameters = BTreeMap::new();
        effect_parameters.insert("threshold-db".into(), -42.0);
        config
            .windows_application_routes
            .push(WindowsApplicationRoute {
                application: WindowsApplicationSelector {
                    executable_path_hash: Some("sha256:deadbeef".into()),
                    executable_name: Some("tone.exe".into()),
                    ..WindowsApplicationSelector::default()
                },
                destination_endpoint_id: Some("endpoint-instance-id".into()),
                destination_name: Some("Speakers".into()),
                effect_chain: vec!["builtin.noise-gate".into()],
                effect_instances: vec![EffectInstanceConfig {
                    instance_id: "route-gate".into(),
                    effect_id: "builtin.noise-gate".into(),
                    module_path: None,
                    enabled: false,
                    parameters: effect_parameters,
                }],
                ..WindowsApplicationRoute::default()
            });
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(text.contains("experimental_app_routing = true"));
        assert!(text.contains("receive_target = \"virtual-microphone\""));
        let restored: AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(restored.windows, config.windows);
        assert_eq!(
            restored.windows_application_routes,
            config.windows_application_routes
        );
        let route = restored.windows_application_routes.first().unwrap();
        let restored_effect = route.restorable_effect_instances().unwrap().pop().unwrap();
        assert_eq!(restored_effect.instance_id, "route-gate");
        assert_eq!(restored_effect.parameters["threshold-db"], -42.0);
        assert!(!restored_effect.enabled);
    }

    #[test]
    fn legacy_route_effect_ids_fail_closed_until_upgraded() {
        let route = WindowsApplicationRoute {
            effect_chain: vec!["builtin.noise-gate".into()],
            ..WindowsApplicationRoute::default()
        };
        let error = route.restorable_effect_instances().unwrap_err();
        assert!(error.contains("legacy effect IDs"));
    }

    #[test]
    fn route_effect_ids_must_match_complete_instances() {
        let route = WindowsApplicationRoute {
            effect_chain: vec!["builtin.noise-gate".into()],
            effect_instances: vec![EffectInstanceConfig {
                instance_id: "route-gate".into(),
                effect_id: "builtin.adaptive-noise-suppressor".into(),
                module_path: None,
                enabled: true,
                parameters: BTreeMap::new(),
            }],
            ..WindowsApplicationRoute::default()
        };
        let error = route.restorable_effect_instances().unwrap_err();
        assert!(error.contains("does not match"));
    }

    #[test]
    fn windows_virtual_microphone_preference_migrates_to_backend_selector() {
        let mut config: AppConfig =
            toml::from_str("[windows.relay]\nreceive_target = 'virtual-microphone'\n").unwrap();
        config.migrate_relay_mode();
        assert_eq!(config.relay_receive_sink, "virtual-microphone");
        let text = toml::to_string(&config).unwrap();
        assert!(text.contains("receive_target = \"virtual-microphone\""));
    }

    #[test]
    fn application_selector_requires_stable_identity() {
        let selector = WindowsApplicationSelector {
            executable_path_hash: Some("sha256:abc".into()),
            executable_name: Some("player.exe".into()),
            ..WindowsApplicationSelector::default()
        };
        let candidate = WindowsApplicationSelector {
            executable_path_hash: Some("SHA256:ABC".into()),
            executable_name: Some("PLAYER.EXE".into()),
            display_name: Some("Player".into()),
            ..WindowsApplicationSelector::default()
        };
        assert!(selector.is_stable());
        assert_eq!(selector.stable_key(), Some("sha256:abc"));
        assert!(selector.matches(&candidate));
        assert!(!WindowsApplicationSelector {
            display_name: Some("Player".into()),
            ..WindowsApplicationSelector::default()
        }
        .is_stable());
    }

    #[test]
    fn package_family_runtime_key_keeps_executables_distinct() {
        let first = WindowsApplicationSelector {
            package_family_name: Some("Example_123".into()),
            executable_name: Some("first.exe".into()),
            ..WindowsApplicationSelector::default()
        };
        let second = WindowsApplicationSelector {
            package_family_name: Some("Example_123".into()),
            executable_name: Some("second.exe".into()),
            ..WindowsApplicationSelector::default()
        };
        assert_ne!(first.runtime_key(), second.runtime_key());
        assert_eq!(
            first.runtime_key().as_deref(),
            Some("package-family:Example_123|executable:first.exe")
        );
        assert!(!WindowsApplicationSelector {
            package_family_name: Some("Example_123".into()),
            ..WindowsApplicationSelector::default()
        }
        .is_stable());
    }

    #[test]
    fn package_family_runtime_key_survives_executable_path_changes() {
        let before = WindowsApplicationSelector {
            executable_path_hash: Some("sha256:old".into()),
            executable_name: Some("player.exe".into()),
            package_family_name: Some("Example_123".into()),
            ..WindowsApplicationSelector::default()
        };
        let after = WindowsApplicationSelector {
            executable_path_hash: Some("sha256:new".into()),
            executable_name: Some("PLAYER.EXE".into()),
            package_family_name: Some("example_123".into()),
            ..WindowsApplicationSelector::default()
        };
        assert_eq!(
            before.runtime_key().as_deref(),
            Some("package-family:Example_123|executable:player.exe")
        );
        assert_eq!(
            before.runtime_key().map(|key| key.to_ascii_lowercase()),
            after.runtime_key().map(|key| key.to_ascii_lowercase())
        );
    }

    #[test]
    fn durable_packaged_identity_survives_path_and_display_name_changes() {
        let selector = WindowsApplicationSelector {
            executable_path_hash: Some("sha256:old".into()),
            executable_name: Some("player.exe".into()),
            package_family_name: Some("Player_123".into()),
            app_user_model_id: Some("Player_123!App".into()),
            display_name: Some("Old Player".into()),
        };
        let candidate = WindowsApplicationSelector {
            executable_path_hash: Some("sha256:new".into()),
            executable_name: Some("player.exe".into()),
            package_family_name: Some("player_123".into()),
            app_user_model_id: Some("player_123!app".into()),
            display_name: Some("New Player".into()),
        };
        assert!(selector.matches(&candidate));
    }

    #[test]
    fn most_specific_enabled_application_route_wins_without_a_pid() {
        let candidate = WindowsApplicationSelector {
            executable_path_hash: Some("sha256:abc".into()),
            executable_name: Some("player.exe".into()),
            package_family_name: Some("Player_123!App".into()),
            ..WindowsApplicationSelector::default()
        };
        let config = AppConfig {
            windows_application_routes: vec![
                WindowsApplicationRoute {
                    application: WindowsApplicationSelector {
                        executable_path_hash: Some("sha256:abc".into()),
                        ..WindowsApplicationSelector::default()
                    },
                    destination_name: Some("broad".into()),
                    ..WindowsApplicationRoute::default()
                },
                WindowsApplicationRoute {
                    application: WindowsApplicationSelector {
                        executable_path_hash: Some("sha256:abc".into()),
                        package_family_name: Some("player_123!app".into()),
                        executable_name: Some("player.exe".into()),
                        ..WindowsApplicationSelector::default()
                    },
                    destination_name: Some("specific".into()),
                    ..WindowsApplicationRoute::default()
                },
            ],
            ..AppConfig::default()
        };
        assert_eq!(
            config
                .matching_windows_application_route(&candidate)
                .and_then(|route| route.destination_name.as_deref()),
            Some("specific")
        );
    }
}
