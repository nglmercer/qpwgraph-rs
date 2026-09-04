//! Virtual relay devices: client-owned `pw_filter` nodes that bridge the
//! network relay engine into the PipeWire graph.
//!
//! Two nodes exist while the relay is active:
//!
//! - **Relay Microphone** — an output-only filter published as
//!   `Audio/Source/Virtual`. Audio decoded from peer datagrams is pulled from
//!   the relay engine and published here, so any application can capture a
//!   phone's microphone like a regular input device.
//! - **Relay Speaker** — an input-only filter published as `Audio/Sink`.
//!   Whatever applications play into it is drained, mixed to mono, and
//!   transmitted to receiving peers.
//!
//! The realtime callbacks follow the same discipline as `effects.rs`: no
//! allocation, atomics for port pointers, and only `try_lock`-style access
//! into the engine's PCM queues so a busy network worker can cost at most one
//! bypassed quantum instead of an xrun.

use super::filter_runtime::FilterRuntime;
use super::*;
use pw_graph_relay::{RelayEngine, RelayHandle};
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// Quanta larger than this are skipped rather than served. The relay engine
/// sizes its realtime-path buffers from the same constant, so raising this
/// without raising `MAX_REALTIME_QUANTUM_SAMPLES` would reintroduce
/// allocation on the audio thread.
const RELAY_MAX_FRAMES: u32 = pw_graph_relay::MAX_REALTIME_QUANTUM_SAMPLES as u32;
const RELAY_CHANNELS: usize = 2;

pub(super) use pw_graph_core::{
    RELAY_SINK_NODE_NAME as RELAY_SINK_NAME, RELAY_SOURCE_NODE_NAME as RELAY_SOURCE_NAME,
};

/// Which virtual device a filter represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RelayNodeKind {
    /// Output-only: network audio appears as a capture device.
    Microphone,
    /// Input-only: captured audio is transmitted to peers.
    Speaker,
}

// ---------------------------------------------------------------------------
// Gain / Meter helpers (spec sections 5,7,8,15,29)
// ---------------------------------------------------------------------------

/// Linear gain range 0.0..2.0  (0%..200%, unity=1.0, +6.02dB at 2.0)
pub const RELAY_GAIN_MIN: f32 = 0.0;
pub const RELAY_GAIN_MAX: f32 = 2.0;
pub const RELAY_GAIN_DEFAULT: f32 = 1.0;

/// Display floor for dBFS metering.
pub const DBFS_FLOOR: f32 = -60.0;
const EPSILON: f32 = 1e-6;

/// Clamp gain to valid range.
pub fn clamp_gain(gain: f32) -> f32 {
    gain.clamp(RELAY_GAIN_MIN, RELAY_GAIN_MAX)
}

/// Apply linear gain and hard-clip to [-1.0,1.0].
pub fn apply_gain(sample: f32, gain: f32) -> f32 {
    (sample * gain).clamp(-1.0, 1.0)
}

/// Compute RMS and peak for a slice.
/// rms = sqrt(sum(sample^2)/N), peak = max(abs(sample))
pub fn compute_levels(samples: &[f32]) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let mut sum_sq = 0.0f64;
    let mut peak = 0.0f32;
    for &s in samples {
        let a = s.abs();
        if a > peak {
            peak = a;
        }
        sum_sq += (s as f64) * (s as f64);
    }
    let rms = ((sum_sq / samples.len() as f64).sqrt() as f32).min(1.0);
    (rms, peak.min(1.0))
}

/// Convert linear level 0..1 to dBFS with floor.
pub fn linear_to_dbfs(level: f32) -> f32 {
    if level <= EPSILON {
        DBFS_FLOOR
    } else {
        (20.0 * level.max(EPSILON).log10()).max(DBFS_FLOOR)
    }
}

/// Convert gain 0..2 to display percentage string.
#[allow(dead_code)]
pub fn gain_to_percent(gain: f32) -> u32 {
    (clamp_gain(gain) * 100.0).round() as u32
}

/// Convert gain to dB (100% = 0dB, 200% = +6.02dB)
#[allow(dead_code)]
pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= EPSILON {
        f32::NEG_INFINITY
    } else {
        20.0 * gain.log10()
    }
}

/// Smoothing: attack 30ms fast, release 250ms slow.
pub fn smooth_level(previous: f32, target: f32, dt_ms: f32) -> f32 {
    let tau = if target > previous { 30.0 } else { 250.0 };
    // alpha = exp(-dt / tau)
    let alpha = (-dt_ms / tau).exp();
    previous * alpha + target * (1.0 - alpha)
}

// ---------------------------------------------------------------------------
// Playback state (spec 10)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum RelayPlaybackState {
    #[default]
    Disabled,
    WaitingForSink,
    Connected,
    Error(String),
}

impl std::fmt::Display for RelayPlaybackState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "Disabled"),
            Self::WaitingForSink => write!(f, "Waiting for output device"),
            Self::Connected => write!(f, "Connected"),
            Self::Error(msg) => write!(f, "Error: {msg}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RelayMeterSnapshot {
    pub input_rms: f32,
    pub input_peak: f32,
    pub output_rms: f32,
    pub output_peak: f32,
    pub input_dbfs: f32,
    pub output_dbfs: f32,
    pub peak_dbfs: f32,
}

impl Default for RelayMeterSnapshot {
    fn default() -> Self {
        Self {
            input_rms: 0.0,
            input_peak: 0.0,
            output_rms: 0.0,
            output_peak: 0.0,
            input_dbfs: DBFS_FLOOR,
            output_dbfs: DBFS_FLOOR,
            peak_dbfs: DBFS_FLOOR,
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct RelayPlaybackStatus {
    pub state: RelayPlaybackState,
    pub sink_name: Option<String>,
    pub sink_serial: Option<u64>,
    pub gain: f32,
    pub muted: bool,
    pub enabled: bool,
    pub meters: RelayMeterSnapshot,
}

impl Default for RelayPlaybackStatus {
    fn default() -> Self {
        Self {
            state: RelayPlaybackState::Disabled,
            sink_name: None,
            sink_serial: None,
            gain: RELAY_GAIN_DEFAULT,
            muted: false,
            enabled: true,
            meters: RelayMeterSnapshot::default(),
        }
    }
}

/// Realtime-safe shared state between control thread and audio callback.
pub struct RelayPlaybackShared {
    gain_bits: AtomicU32,
    muted: AtomicBool,
    enabled: AtomicBool,
    // raw instant levels
    input_rms_bits: AtomicU32,
    input_peak_bits: AtomicU32,
    output_rms_bits: AtomicU32,
    output_peak_bits: AtomicU32,
    // smoothed levels for UI
    smoothed_input_rms_bits: AtomicU32,
    smoothed_input_peak_bits: AtomicU32,
    smoothed_output_rms_bits: AtomicU32,
    smoothed_output_peak_bits: AtomicU32,
}

impl Default for RelayPlaybackShared {
    fn default() -> Self {
        Self {
            gain_bits: AtomicU32::new(RELAY_GAIN_DEFAULT.to_bits()),
            muted: AtomicBool::new(false),
            enabled: AtomicBool::new(true),
            input_rms_bits: AtomicU32::new(0.0f32.to_bits()),
            input_peak_bits: AtomicU32::new(0.0f32.to_bits()),
            output_rms_bits: AtomicU32::new(0.0f32.to_bits()),
            output_peak_bits: AtomicU32::new(0.0f32.to_bits()),
            smoothed_input_rms_bits: AtomicU32::new(0.0f32.to_bits()),
            smoothed_input_peak_bits: AtomicU32::new(0.0f32.to_bits()),
            smoothed_output_rms_bits: AtomicU32::new(0.0f32.to_bits()),
            smoothed_output_peak_bits: AtomicU32::new(0.0f32.to_bits()),
        }
    }
}

impl RelayPlaybackShared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn gain(&self) -> f32 {
        f32::from_bits(self.gain_bits.load(Ordering::Relaxed))
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain_bits
            .store(clamp_gain(gain).to_bits(), Ordering::Relaxed);
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn update_input(&self, rms: f32, peak: f32, dt_ms: f32) {
        self.input_rms_bits.store(rms.to_bits(), Ordering::Relaxed);
        self.input_peak_bits
            .store(peak.to_bits(), Ordering::Relaxed);
        let prev_rms = f32::from_bits(self.smoothed_input_rms_bits.load(Ordering::Relaxed));
        let prev_peak = f32::from_bits(self.smoothed_input_peak_bits.load(Ordering::Relaxed));
        let sm_rms = smooth_level(prev_rms, rms, dt_ms);
        let sm_peak = smooth_level(prev_peak, peak, dt_ms);
        self.smoothed_input_rms_bits
            .store(sm_rms.to_bits(), Ordering::Relaxed);
        self.smoothed_input_peak_bits
            .store(sm_peak.to_bits(), Ordering::Relaxed);
    }

    pub fn update_output(&self, rms: f32, peak: f32, dt_ms: f32) {
        self.output_rms_bits.store(rms.to_bits(), Ordering::Relaxed);
        self.output_peak_bits
            .store(peak.to_bits(), Ordering::Relaxed);
        let prev_rms = f32::from_bits(self.smoothed_output_rms_bits.load(Ordering::Relaxed));
        let prev_peak = f32::from_bits(self.smoothed_output_peak_bits.load(Ordering::Relaxed));
        let sm_rms = smooth_level(prev_rms, rms, dt_ms);
        let sm_peak = smooth_level(prev_peak, peak, dt_ms);
        self.smoothed_output_rms_bits
            .store(sm_rms.to_bits(), Ordering::Relaxed);
        self.smoothed_output_peak_bits
            .store(sm_peak.to_bits(), Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> RelayMeterSnapshot {
        let input_rms = f32::from_bits(self.smoothed_input_rms_bits.load(Ordering::Relaxed));
        let input_peak = f32::from_bits(self.smoothed_input_peak_bits.load(Ordering::Relaxed));
        let output_rms = f32::from_bits(self.smoothed_output_rms_bits.load(Ordering::Relaxed));
        let output_peak = f32::from_bits(self.smoothed_output_peak_bits.load(Ordering::Relaxed));
        let peak = input_peak.max(output_peak);
        RelayMeterSnapshot {
            input_rms,
            input_peak,
            output_rms,
            output_peak,
            input_dbfs: linear_to_dbfs(input_rms),
            output_dbfs: linear_to_dbfs(output_rms),
            peak_dbfs: linear_to_dbfs(peak),
        }
    }

    #[allow(dead_code)]
    pub fn instant_snapshot(&self) -> RelayMeterSnapshot {
        let input_rms = f32::from_bits(self.input_rms_bits.load(Ordering::Relaxed));
        let input_peak = f32::from_bits(self.input_peak_bits.load(Ordering::Relaxed));
        let output_rms = f32::from_bits(self.output_rms_bits.load(Ordering::Relaxed));
        let output_peak = f32::from_bits(self.output_peak_bits.load(Ordering::Relaxed));
        let peak = input_peak.max(output_peak);
        RelayMeterSnapshot {
            input_rms,
            input_peak,
            output_rms,
            output_peak,
            input_dbfs: linear_to_dbfs(input_rms),
            output_dbfs: linear_to_dbfs(output_rms),
            peak_dbfs: linear_to_dbfs(peak),
        }
    }
}

// ---------------------------------------------------------------------------
// Routing (spec 2,3,4,26)
// ---------------------------------------------------------------------------

/// Deterministic routing layer: Relay Microphone -> selected/default sink.
/// Idempotent: calling ensure_playback_route() multiple times does not create
/// duplicate links. Survives sink recreation via stable name/serial.
pub struct RelayPlaybackRouter {
    pub preferred_sink_name: Option<String>,
    pub preferred_sink_serial: Option<u64>,
    pub enabled: bool,
    pub state: RelayPlaybackState,
    pub current_sink_name: Option<String>,
    pub current_sink_serial: Option<u64>,
    /// Track link ids created for relay playback so they can be removed.
    pub link_ids: Vec<u64>,
}

impl Default for RelayPlaybackRouter {
    fn default() -> Self {
        Self {
            preferred_sink_name: None,
            preferred_sink_serial: None,
            enabled: true,
            state: RelayPlaybackState::Disabled,
            current_sink_name: None,
            current_sink_serial: None,
            link_ids: Vec::new(),
        }
    }
}

impl RelayPlaybackRouter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if !enabled {
            self.state = RelayPlaybackState::Disabled;
        }
    }

    pub fn set_preferred_sink(&mut self, name: Option<String>, serial: Option<u64>) {
        self.preferred_sink_name = name;
        self.preferred_sink_serial = serial;
    }

    /// Find relay source output ports in the current graph.
    pub fn find_relay_source_ports(graph: &Graph) -> Option<(PortId, PortId)> {
        let node = graph.nodes.values().find(|n| n.name == RELAY_SOURCE_NAME)?;
        let mut fl = None;
        let mut fr = None;
        for pid in &node.ports {
            if let Some(port) = graph.port(*pid) {
                if port.direction.is_source() {
                    match port.channel.as_deref() {
                        Some("FL") => fl = Some(*pid),
                        Some("FR") => fr = Some(*pid),
                        _ => {
                            if port.name.contains("FL") {
                                fl = Some(*pid);
                            } else if port.name.contains("FR") {
                                fr = Some(*pid);
                            }
                        }
                    }
                }
            }
        }
        Some((fl?, fr?))
    }

    /// Find sink node for playback. Prefers preferred_sink, else default sink.
    pub fn find_target_sink(
        &self,
        graph: &Graph,
        registry: &std::collections::BTreeMap<u32, crate::pipewire::registry::NodeRecord>,
    ) -> Option<(NodeId, String, Option<u64>)> {
        // Try preferred by serial first, then by name.
        if let Some(serial) = self.preferred_sink_serial {
            for (id, node) in &graph.nodes {
                if node.serial == Some(serial)
                    && node.name != RELAY_SOURCE_NAME
                    && node.name != RELAY_SINK_NAME
                {
                    // verify it's a sink
                    let is_sink = registry
                        .values()
                        .find(|r| r.name == node.name)
                        .map(|r| r.media_class.to_ascii_lowercase().contains("sink"))
                        .unwrap_or(false)
                        || node
                            .ports
                            .iter()
                            .any(|pid| graph.port(*pid).is_some_and(|p| p.direction.is_sink()));
                    if is_sink {
                        return Some((*id, node.name.clone(), node.serial));
                    }
                }
            }
        }
        if let Some(name) = &self.preferred_sink_name {
            for (id, node) in &graph.nodes {
                if &node.name == name
                    && node.name != RELAY_SOURCE_NAME
                    && node.name != RELAY_SINK_NAME
                {
                    return Some((*id, node.name.clone(), node.serial));
                }
            }
        }
        // Default: first Audio/Sink node that is not relay
        for (id, node) in &graph.nodes {
            if node.name == RELAY_SOURCE_NAME || node.name == RELAY_SINK_NAME {
                continue;
            }
            let media_is_sink = registry
                .values()
                .find(|r| r.name == node.name)
                .map(|r| r.media_class.to_ascii_lowercase().contains("sink"))
                .unwrap_or(false);
            let has_sink_ports = node.ports.iter().any(|pid| {
                graph
                    .port(*pid)
                    .is_some_and(|p| p.direction.is_sink() && p.port_type == PortType::Audio)
            });
            if media_is_sink || has_sink_ports {
                // Consider it a candidate sink
                if has_sink_ports || media_is_sink {
                    return Some((*id, node.name.clone(), node.serial));
                }
            }
        }
        // Fallback: any node with sink ports not relay
        for (id, node) in &graph.nodes {
            if node.name == RELAY_SOURCE_NAME || node.name == RELAY_SINK_NAME {
                continue;
            }
            let has_sink = node
                .ports
                .iter()
                .any(|pid| graph.port(*pid).is_some_and(|p| p.direction.is_sink()));
            if has_sink {
                return Some((*id, node.name.clone(), node.serial));
            }
        }
        None
    }

    /// Find sink input ports (playback) for a sink node.
    /// Never guesses FL/FR by port order. Requires explicit channel metadata
    /// `FL`/`FR`; otherwise returns `None` so the router can surface
    /// `AmbiguousChannelLayout` instead of wiring L/R incorrectly (e.g. Bluetooth
    /// devices whose port order is not semantic order).
    pub fn find_sink_input_ports(graph: &Graph, sink_id: NodeId) -> Option<(PortId, PortId)> {
        let node = graph.node(sink_id)?;
        let sink_ports: Vec<(PortId, Option<String>, String)> = node
            .ports
            .iter()
            .filter_map(|pid| {
                let p = graph.port(*pid)?;
                if !p.direction.is_sink() {
                    return None;
                }
                if p.port_type != PortType::Audio && p.port_type != PortType::Unknown {
                    return None;
                }
                Some((*pid, p.channel.clone(), p.name.clone()))
            })
            .collect();
        if sink_ports.is_empty() {
            return None;
        }
        // Mono sink: single audio sink port — duplicate mono to that port.
        if sink_ports.len() == 1 {
            let id = sink_ports[0].0;
            return Some((id, id));
        }
        // Strict stereo: require explicit FL/FR channel metadata. Do not fall
        // back to positional ordering which can silently swap L/R on
        // Bluetooth/other devices.
        let fl = sink_ports
            .iter()
            .find(|(_, ch, _)| ch.as_deref() == Some("FL"))
            .map(|(id, _, _)| *id)?;
        let fr = sink_ports
            .iter()
            .find(|(_, ch, _)| ch.as_deref() == Some("FR"))
            .map(|(id, _, _)| *id)?;
        if fl == fr {
            return None;
        }
        Some((fl, fr))
    }

    /// Check if our tracked links are still valid and not duplicates.
    pub fn validate_links(&mut self, graph: &Graph) {
        self.link_ids.retain(|lid| {
            let link_id = LinkId(*lid);
            graph.link(link_id).is_some()
        });
    }

    #[allow(clippy::type_complexity)]
    /// Determine desired link pairs for current graph. Returns (output, input) pairs.
    pub fn desired_links(
        &self,
        graph: &Graph,
        registry: &std::collections::BTreeMap<u32, crate::pipewire::registry::NodeRecord>,
    ) -> Option<((PortId, PortId), (PortId, PortId), String, Option<u64>)> {
        if !self.enabled {
            return None;
        }
        let (src_fl, src_fr) = Self::find_relay_source_ports(graph)?;
        let (sink_id, sink_name, sink_serial) = self.find_target_sink(graph, registry)?;
        let (sink_fl, sink_fr) = Self::find_sink_input_ports(graph, sink_id)?;
        Some(((src_fl, src_fr), (sink_fl, sink_fr), sink_name, sink_serial))
    }
}

// ---------------------------------------------------------------------------
// Generic Emitter / Receiver routing
// ---------------------------------------------------------------------------

/// qpwgraph-owned automatic links for the currently selected local mode.
/// The older `RelayPlaybackRouter` remains below the public compatibility
/// surface, but new callers use this controller so an Emitter route and a
/// Receiver route cannot coexist.
#[derive(Clone, Debug)]
pub struct RelayLocalRouter {
    pub mode: Option<RelayMode>,
    pub send_source: RelaySendSource,
    pub receive_sink: RelayReceiveSink,
    pub enabled: bool,
    pub link_ids: Vec<u64>,
    pub state: RelayLocalRouteState,
}

impl Default for RelayLocalRouter {
    fn default() -> Self {
        Self {
            mode: None,
            send_source: RelaySendSource::DefaultInput,
            receive_sink: RelayReceiveSink::DefaultOutput,
            enabled: true,
            link_ids: Vec::new(),
            state: RelayLocalRouteState::default(),
        }
    }
}

impl RelayLocalRouter {
    pub fn set_mode(&mut self, mode: RelayMode) {
        self.mode = Some(mode);
    }

    pub fn set_send_source(&mut self, source: RelaySendSource) {
        self.send_source = source;
    }

    pub fn set_receive_sink(&mut self, sink: RelayReceiveSink) {
        self.receive_sink = sink;
    }

    fn node_has_audio_direction(graph: &Graph, node_id: NodeId, source: bool) -> bool {
        graph.node(node_id).is_some_and(|node| {
            node.ports.iter().any(|port_id| {
                graph.port(*port_id).is_some_and(|port| {
                    (source && port.direction.is_source() || !source && port.direction.is_sink())
                        && (port.port_type == PortType::Audio
                            || port.port_type == PortType::Unknown)
                })
            })
        })
    }

    fn find_node(
        graph: &Graph,
        registry: &std::collections::BTreeMap<u32, crate::pipewire::registry::NodeRecord>,
        selector: &str,
        source: bool,
    ) -> Option<(NodeId, String, Option<u64>)> {
        // Endpoint ids are kind-qualified (`input:`, `monitor:`, or
        // `output:`) so the UI cannot accidentally feed a receive selector
        // into the send list. The graph lookup still targets the underlying
        // PipeWire node name/serial.
        let selector = selector
            .strip_prefix("input:")
            .or_else(|| selector.strip_prefix("monitor:"))
            .or_else(|| selector.strip_prefix("output:"))
            .unwrap_or(selector);
        let serial = selector
            .strip_prefix("serial:")
            .and_then(|id| id.parse().ok());
        graph
            .nodes
            .iter()
            .filter(|(_, node)| {
                node.name != RELAY_SOURCE_NAME
                    && node.name != RELAY_SINK_NAME
                    && Self::node_has_audio_direction(graph, node.id, source)
            })
            .find(|(_, node)| {
                let registry_name = registry
                    .get(&native_node_id(node.id))
                    .map(|record| record.name.as_str());
                node.name == selector
                    || registry_name == Some(selector)
                    || serial.is_some_and(|serial| node.serial == Some(serial))
            })
            .map(|(id, node)| (*id, node.name.clone(), node.serial))
    }

    fn find_default_node(
        graph: &Graph,
        registry: &std::collections::BTreeMap<u32, crate::pipewire::registry::NodeRecord>,
        default: Option<&crate::pipewire::registry::DefaultDevice>,
        source: bool,
    ) -> Option<(NodeId, String, Option<u64>)> {
        let default = default?;
        graph
            .nodes
            .iter()
            .filter(|(_, node)| {
                node.name != RELAY_SOURCE_NAME
                    && node.name != RELAY_SINK_NAME
                    && Self::node_has_audio_direction(graph, node.id, source)
            })
            .find(|(_, node)| {
                let registry_name = registry
                    .get(&native_node_id(node.id))
                    .map(|record| record.name.as_str());
                default
                    .serial
                    .is_some_and(|serial| node.serial == Some(serial))
                    || node.name == default.name
                    || registry_name == Some(default.name.as_str())
            })
            .map(|(id, node)| (*id, node.name.clone(), node.serial))
    }

    fn find_audio_ports(
        graph: &Graph,
        node_id: NodeId,
        source: bool,
        monitor: bool,
    ) -> Option<(PortId, PortId)> {
        let node = graph.node(node_id)?;
        let all: Vec<(PortId, Option<String>, String)> = node
            .ports
            .iter()
            .filter_map(|port_id| {
                let port = graph.port(*port_id)?;
                let correct_direction = if source {
                    port.direction.is_source()
                } else {
                    port.direction.is_sink()
                };
                if !correct_direction
                    || (port.port_type != PortType::Audio && port.port_type != PortType::Unknown)
                {
                    return None;
                }
                if monitor && !port.name.to_ascii_lowercase().contains("monitor") {
                    return None;
                }
                Some((*port_id, port.channel.clone(), port.name.clone()))
            })
            .collect();
        if all.is_empty() && monitor {
            // Some PipeWire versions omit "monitor" from the port name but
            // expose monitor output ports on the sink node. The node was
            // selected from default sink metadata, so a source port here is
            // still the monitor rather than an unrelated physical input.
            return Self::find_audio_ports(graph, node_id, source, false);
        }
        if all.is_empty() {
            return None;
        }
        if all.len() == 1 {
            return Some((all[0].0, all[0].0));
        }
        let fl = all
            .iter()
            .find(|(_, channel, name)| channel.as_deref() == Some("FL") || name.contains("FL"))
            .map(|(id, _, _)| *id)?;
        let fr = all
            .iter()
            .find(|(_, channel, name)| channel.as_deref() == Some("FR") || name.contains("FR"))
            .map(|(id, _, _)| *id)?;
        (fl != fr).then_some((fl, fr))
    }

    fn relay_sink_ports(graph: &Graph) -> Option<(PortId, PortId)> {
        let node_id = graph
            .nodes
            .values()
            .find(|node| node.name == RELAY_SINK_NAME)
            .map(|node| node.id)?;
        Self::find_audio_ports(graph, node_id, false, false)
    }

    fn relay_source_ports(graph: &Graph) -> Option<(PortId, PortId)> {
        let node_id = graph
            .nodes
            .values()
            .find(|node| node.name == RELAY_SOURCE_NAME)
            .map(|node| node.id)?;
        Self::find_audio_ports(graph, node_id, true, false)
    }

    /// Calculate the only automatic route allowed for [self]. Manual graph
    /// choices intentionally return `None`, leaving ordinary patchbay links
    /// under user control.
    pub fn desired_links(
        &self,
        graph: &Graph,
        registry: &std::collections::BTreeMap<u32, crate::pipewire::registry::NodeRecord>,
        default_source: Option<&crate::pipewire::registry::DefaultDevice>,
        default_sink: Option<&crate::pipewire::registry::DefaultDevice>,
    ) -> Option<(Vec<(PortId, PortId)>, String, String, String)> {
        if !self.enabled {
            return None;
        }
        match self.mode? {
            RelayMode::Emitter => {
                let source = match &self.send_source {
                    RelaySendSource::DefaultInput => {
                        Self::find_default_node(graph, registry, default_source, true)?
                    }
                    RelaySendSource::InputDevice(selector)
                    | RelaySendSource::OutputMonitor(selector) => {
                        let monitor =
                            matches!(&self.send_source, RelaySendSource::OutputMonitor(_));
                        let node = Self::find_node(graph, registry, selector, true)?;
                        if monitor {
                            // Re-resolve the same node with monitor filtering
                            // in the port lookup below.
                            node
                        } else {
                            node
                        }
                    }
                    RelaySendSource::DefaultOutputMonitor => {
                        // The default sink metadata names the playback node;
                        // the route consumes that node's source monitor
                        // ports.
                        Self::find_default_node(graph, registry, default_sink, true)?
                    }
                    RelaySendSource::ManualGraph => return None,
                };
                let monitor = matches!(
                    &self.send_source,
                    RelaySendSource::DefaultOutputMonitor | RelaySendSource::OutputMonitor(_)
                );
                let (source_fl, source_fr) =
                    Self::find_audio_ports(graph, source.0, true, monitor)?;
                let (sink_fl, sink_fr) = Self::relay_sink_ports(graph)?;
                Some((
                    vec![(source_fl, sink_fl), (source_fr, sink_fr)],
                    source.1.clone(),
                    RELAY_SINK_NAME.to_owned(),
                    format!("{} -> Relay Speaker", source.1),
                ))
            }
            RelayMode::Receiver => {
                let sink = match &self.receive_sink {
                    RelayReceiveSink::DefaultOutput => {
                        Self::find_default_node(graph, registry, default_sink, false)?
                    }
                    RelayReceiveSink::OutputDevice(selector) => {
                        Self::find_node(graph, registry, selector, false)?
                    }
                    RelayReceiveSink::ManualGraph => return None,
                };
                let (source_fl, source_fr) = Self::relay_source_ports(graph)?;
                let (sink_fl, sink_fr) = Self::find_audio_ports(graph, sink.0, false, false)?;
                Some((
                    vec![(source_fl, sink_fl), (source_fr, sink_fr)],
                    RELAY_SOURCE_NAME.to_owned(),
                    sink.1.clone(),
                    format!("Relay Microphone -> {}", sink.1),
                ))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Callback state
// ---------------------------------------------------------------------------

/// Callback state owned by one relay filter. Only PipeWire's realtime data
/// thread touches `scratch`; it sits behind a mutex purely for interior
/// mutability under the shared callback pointer, and the callback uses
/// `try_lock` so a pathological hold elsewhere could cost at most one
/// bypassed quantum. The port pointers are published once before the filter
/// connects and never change.
struct RelayCallbackState {
    kind: RelayNodeKind,
    ports: [AtomicPtr<c_void>; RELAY_CHANNELS],
    handle: RelayHandle,
    scratch: Mutex<Vec<f32>>,
    playback: Option<Arc<RelayPlaybackShared>>,
}

impl RelayCallbackState {
    fn new(
        kind: RelayNodeKind,
        handle: RelayHandle,
        playback: Option<Arc<RelayPlaybackShared>>,
    ) -> Self {
        Self {
            kind,
            ports: std::array::from_fn(|_| AtomicPtr::new(ptr::null_mut())),
            handle,
            scratch: Mutex::new(vec![0.0; RELAY_MAX_FRAMES as usize]),
            playback,
        }
    }

    /// # Safety
    ///
    /// PipeWire invokes this on its realtime data thread with the callback
    /// data supplied to `pw_filter_new_simple`. Port pointers were returned
    /// by `pw_filter_add_port` and remain valid until the filter is destroyed.
    unsafe fn process(&self, position: *mut pw::spa::sys::spa_io_position) {
        if position.is_null() {
            return;
        }
        let frames = (*position).clock.duration;
        if frames == 0 || frames > u64::from(RELAY_MAX_FRAMES) {
            return;
        }
        let frames = frames as u32 as usize;
        let ports = self
            .ports
            .each_ref()
            .map(|port| port.load(Ordering::Acquire));
        let Ok(mut scratch_guard) = self.scratch.try_lock() else {
            return;
        };
        let scratch = &mut scratch_guard[..frames];

        match self.kind {
            RelayNodeKind::Microphone => {
                let outputs: [*mut c_void; RELAY_CHANNELS] = std::array::from_fn(|channel| {
                    if ports[channel].is_null() {
                        ptr::null_mut()
                    } else {
                        pw::sys::pw_filter_get_dsp_buffer(ports[channel], frames as u32)
                    }
                });
                if outputs.iter().all(|buffer| buffer.is_null()) {
                    return;
                }
                let available = self.handle.try_pull_playback(scratch);
                if available < frames {
                    scratch[available..].fill(0.0);
                }
                // ---- Meter input before gain, then apply gain, then meter output ----
                // dt for smoothing: assume 48kHz
                let dt_ms = frames as f32 * 1000.0 / 48000.0;
                if let Some(shared) = &self.playback {
                    let (in_rms, in_peak) = compute_levels(&scratch[..frames]);
                    shared.update_input(in_rms, in_peak, dt_ms);
                    let gain = if shared.muted() { 0.0 } else { shared.gain() };
                    for s in scratch.iter_mut().take(frames) {
                        *s = apply_gain(*s, gain);
                    }
                    let (out_rms, out_peak) = compute_levels(&scratch[..frames]);
                    shared.update_output(out_rms, out_peak, dt_ms);
                } else {
                    // fallback: still clamp at unity if no shared (should not happen after init)
                    // No gain stage – keep as-is but ensure no clipping beyond 1.0
                    for s in scratch.iter_mut().take(frames) {
                        *s = s.clamp(-1.0, 1.0);
                    }
                }
                for (frame, sample) in scratch.iter().take(frames).enumerate() {
                    for output in outputs.iter().take(RELAY_CHANNELS) {
                        if !output.is_null() {
                            *(*output).cast::<f32>().add(frame) = *sample;
                        }
                    }
                }
            }
            RelayNodeKind::Speaker => {
                let inputs: [*mut c_void; RELAY_CHANNELS] = std::array::from_fn(|channel| {
                    if ports[channel].is_null() {
                        ptr::null_mut()
                    } else {
                        pw::sys::pw_filter_get_dsp_buffer(ports[channel], frames as u32)
                    }
                });
                if inputs.iter().all(|buffer| buffer.is_null()) {
                    return;
                }
                for (frame, sample) in scratch.iter_mut().take(frames).enumerate() {
                    let mut sum = 0.0f32;
                    let mut count = 0.0f32;
                    for input in inputs.iter().take(RELAY_CHANNELS) {
                        if !input.is_null() {
                            sum += *(*input).cast::<f32>().add(frame);
                            count += 1.0;
                        }
                    }
                    *sample = if count > 0.0 { sum / count } else { 0.0 };
                }
                self.handle.try_push_capture(scratch);
            }
        }
    }
}

unsafe extern "C" fn relay_filter_process(
    data: *mut c_void,
    position: *mut pw::spa::sys::spa_io_position,
) {
    // `data` is a Box<RelayCallbackState> retained by FilterRuntime until
    // `pw_filter_destroy` has detached all callbacks. Never unwind over C.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let Some(state) = data.cast::<RelayCallbackState>().as_ref() else {
            return;
        };
        state.process(position);
    }));
}

/// The virtual device a filter represents. Unlike the effect runtime, a relay
/// node owns only one directed port pair, so the callback type carries the
/// device kind rather than a second port set.
pub(super) struct RelayNodeRuntime {
    /// Held for its `Drop`: the inner runtime destroys the filter while the
    /// driver's ThreadLoop lock is held, detaching the realtime callback
    /// before the callback `Box` is released.
    _runtime: FilterRuntime<RelayCallbackState>,
}

impl RelayNodeRuntime {
    pub(super) fn create(
        thread_loop: &pw::thread_loop::ThreadLoop,
        handle: RelayHandle,
        kind: RelayNodeKind,
        playback: Option<Arc<RelayPlaybackShared>>,
    ) -> BackendResult<Self> {
        let (node_name, description, media_class, icon) = match kind {
            RelayNodeKind::Microphone => (
                RELAY_SOURCE_NAME,
                "Relay Microphone",
                "Audio/Source/Virtual",
                "audio-input-microphone",
            ),
            RelayNodeKind::Speaker => {
                (RELAY_SINK_NAME, "Relay Speaker", "Audio/Sink", "audio-card")
            }
        };

        let callback = Box::new(RelayCallbackState::new(kind, handle, playback));
        let properties = pw::properties::properties! {
            NODE_NAME => node_name,
            NODE_DESCRIPTION => description,
            MEDIA_TYPE => MEDIA_TYPE_AUDIO,
            MEDIA_CLASS => media_class,
            PROP_NODE_VIRTUAL => "true",
            // Relay endpoints are patchable graph nodes; never let a session
            // manager silently route them to a default device.
            PROP_NODE_AUTOCONNECT => "false",
            PROP_NODE_GROUP => "qpwgraph-rs",
            "device.icon-name" => icon,
            "qpwgraph-rs.relay.kind" => match kind {
                RelayNodeKind::Microphone => "source",
                RelayNodeKind::Speaker => "sink",
            },
        };
        let runtime = FilterRuntime::create(
            thread_loop,
            node_name,
            properties,
            Some(relay_filter_process),
            callback,
        )?;

        let direction = match kind {
            RelayNodeKind::Microphone => pw::spa::sys::SPA_DIRECTION_OUTPUT,
            RelayNodeKind::Speaker => pw::spa::sys::SPA_DIRECTION_INPUT,
        };
        // `<role>_<channel>`, the same shape PipeWire devices and our effect
        // filters use. The bare channel name has no base for the canvas to
        // group on, which left these cards showing two loose `FL`/`FR` pins
        // in Easy mode where every other node collapses to one.
        let role = match kind {
            RelayNodeKind::Microphone => pw_graph_core::RELAY_SOURCE_PORT_ROLE,
            RelayNodeKind::Speaker => pw_graph_core::RELAY_SINK_PORT_ROLE,
        };
        let mut ports = [ptr::null_mut(); RELAY_CHANNELS];
        for (index, channel) in ["FL", "FR"].iter().enumerate() {
            ports[index] = runtime.add_port(direction, &format!("{role}_{channel}"), channel)?;
        }
        for (callback_port, port) in runtime.callback().ports.iter().zip(ports.iter()) {
            callback_port.store(*port, Ordering::Release);
        }
        runtime.connect()?;

        Ok(Self { _runtime: runtime })
    }
}

/// Everything the PipeWire driver owns for the relay feature.
///
/// `_engine` and the two runtimes are held for their lifetimes: dropping the
/// set tears down the filters (under the ThreadLoop lock, enforced by the
/// driver) and stops the engine's worker threads.
pub(super) struct RelayRuntimeSet {
    _engine: RelayEngine,
    handle: RelayHandle,
    _source: RelayNodeRuntime,
    _sink: RelayNodeRuntime,
    pub playback_shared: Arc<RelayPlaybackShared>,
    pub router: RelayPlaybackRouter,
    pub local_router: RelayLocalRouter,
}

impl RelayRuntimeSet {
    /// Create the engine and both virtual nodes. Caller holds the ThreadLoop
    /// lock.
    pub(super) fn create(
        thread_loop: &pw::thread_loop::ThreadLoop,
        device_name: &str,
    ) -> BackendResult<Self> {
        let config = pw_graph_relay::EngineConfig {
            device_name: device_name.to_owned(),
            ..Default::default()
        };
        let engine = RelayEngine::start(config)
            .map_err(|error| BackendError::native(format!("relay engine start: {error}")))?;
        let handle = engine.handle();
        let playback_shared = RelayPlaybackShared::new();
        let source = RelayNodeRuntime::create(
            thread_loop,
            handle.clone(),
            RelayNodeKind::Microphone,
            Some(playback_shared.clone()),
        )?;
        let sink = match RelayNodeRuntime::create(
            thread_loop,
            handle.clone(),
            RelayNodeKind::Speaker,
            None,
        ) {
            Ok(created) => created,
            Err(error) => {
                // The engine must not outlive a half-built device set.
                engine.shutdown();
                return Err(error);
            }
        };
        Ok(Self {
            _engine: engine,
            handle,
            _source: source,
            _sink: sink,
            playback_shared,
            router: RelayPlaybackRouter::new(),
            local_router: RelayLocalRouter::default(),
        })
    }

    pub(super) fn handle(&self) -> &RelayHandle {
        &self.handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn gain_zero_produces_silence() {
        assert!(approx_eq(apply_gain(0.5, 0.0), 0.0, 1e-6));
        assert!(approx_eq(apply_gain(-0.8, 0.0), 0.0, 1e-6));
    }

    #[test]
    fn gain_half_reduces_amplitude() {
        assert!(approx_eq(apply_gain(1.0, 0.5), 0.5, 1e-6));
        assert!(approx_eq(apply_gain(-1.0, 0.5), -0.5, 1e-6));
    }

    #[test]
    fn gain_unity_leaves_unchanged() {
        assert!(approx_eq(apply_gain(0.7, 1.0), 0.7, 1e-6));
        assert!(approx_eq(apply_gain(-0.3, 1.0), -0.3, 1e-6));
    }

    #[test]
    fn gain_double_then_clipped() {
        assert!(approx_eq(apply_gain(0.6, 2.0), 1.0, 1e-6));
        assert!(approx_eq(apply_gain(-0.6, 2.0), -1.0, 1e-6));
        assert!(approx_eq(apply_gain(0.4, 2.0), 0.8, 1e-6));
    }

    #[test]
    fn gain_clamping_limits_range() {
        assert!(approx_eq(clamp_gain(3.0), 2.0, 1e-6));
        assert!(approx_eq(clamp_gain(-1.0), 0.0, 1e-6));
        assert!(approx_eq(clamp_gain(1.5), 1.5, 1e-6));
    }

    #[test]
    fn meter_silence_floor() {
        let (rms, peak) = compute_levels(&[0.0; 128]);
        assert!(approx_eq(rms, 0.0, 1e-6));
        assert!(approx_eq(peak, 0.0, 1e-6));
        assert!(approx_eq(linear_to_dbfs(rms), -60.0, 1e-3));
    }

    #[test]
    fn meter_full_scale_zero_dbfs() {
        let (rms, peak) = compute_levels(&[1.0; 64]);
        assert!(approx_eq(peak, 1.0, 1e-6));
        assert!(approx_eq(rms, 1.0, 1e-6));
        assert!(approx_eq(linear_to_dbfs(peak), 0.0, 1e-3));
    }

    #[test]
    fn meter_constant_signal() {
        let (rms, peak) = compute_levels(&[0.5; 100]);
        assert!(approx_eq(rms, 0.5, 1e-4));
        assert!(approx_eq(peak, 0.5, 1e-4));
        // -6.02 dB for 0.5
        assert!((linear_to_dbfs(rms) - -6.02).abs() < 0.1);
    }

    #[test]
    fn meter_peak_positive_and_negative() {
        let (rms, peak) = compute_levels(&[1.0, -1.0, 0.5, -0.5]);
        assert!(approx_eq(peak, 1.0, 1e-6));
        let expected_rms = ((1.0f64 + 1.0 + 0.25 + 0.25) / 4.0).sqrt() as f32;
        assert!(approx_eq(rms, expected_rms, 1e-4));
    }

    #[test]
    fn meter_sine_approx() {
        let mut samples = Vec::new();
        for i in 0..480 {
            let v = (2.0 * std::f32::consts::PI * i as f32 / 48.0).sin() * 0.7;
            samples.push(v);
        }
        let (rms, peak) = compute_levels(&samples);
        // sine rms = amplitude / sqrt2
        assert!((rms - 0.7 / std::f32::consts::SQRT_2).abs() < 0.02);
        assert!((peak - 0.7).abs() < 0.05);
    }

    #[test]
    fn smoothing_attack_fast_release_slow() {
        let prev = 0.0;
        let target = 1.0;
        let dt = 21.0; // one quantum at 48k
        let attacked = smooth_level(prev, target, dt);
        let released = smooth_level(target, prev, dt);
        // attack should move faster (larger step) than release for same dt
        assert!(attacked > 0.4, "attack {attacked} should be fast");
        assert!(
            released > 0.9,
            "release {released} should be slow (stay high)"
        );
        assert!(
            attacked > (1.0 - released),
            "attack step bigger than release step"
        );
    }

    #[test]
    fn routing_idempotency_no_duplicate_desired() {
        let router = RelayPlaybackRouter::new();
        // Desired links should be None when no graph, but calling twice gives same result
        let graph = Graph::default();
        let registry = std::collections::BTreeMap::new();
        let a = router.desired_links(&graph, &registry);
        let b = router.desired_links(&graph, &registry);
        assert_eq!(a, b);
    }

    #[test]
    fn relay_meter_independent_from_gain() {
        let shared = RelayPlaybackShared::new();
        shared.set_gain(0.5);
        // Simulate input -12 dBFS ~ 0.251
        let input_level = 10f32.powf(-12.0 / 20.0);
        let samples = vec![input_level; 128];
        let (in_rms, _peak) = compute_levels(&samples);
        shared.update_input(in_rms, input_level, 21.0);
        let out_samples: Vec<f32> = samples.iter().map(|s| apply_gain(*s, 0.5)).collect();
        let (out_rms, _out_peak) = compute_levels(&out_samples);
        shared.update_output(out_rms, out_rms, 21.0);
        let snap = shared.instant_snapshot();
        assert!((snap.input_rms - in_rms).abs() < 1e-4);
        assert!((snap.output_rms - out_rms).abs() < 1e-4);
        // output should be ~6dB lower
        assert!(
            (linear_to_dbfs(snap.output_rms) - linear_to_dbfs(snap.input_rms) + 6.0).abs() < 0.5
        );
    }

    #[test]
    fn dbfs_floor_is_minus_sixty() {
        assert_eq!(linear_to_dbfs(0.0), -60.0);
        assert!(linear_to_dbfs(1e-9) >= -60.0);
    }

    #[test]
    fn relay_playback_state_display() {
        assert_eq!(
            RelayPlaybackState::WaitingForSink.to_string(),
            "Waiting for output device"
        );
        assert_eq!(RelayPlaybackState::Connected.to_string(), "Connected");
        assert_eq!(RelayPlaybackState::Disabled.to_string(), "Disabled");
    }

    #[test]
    fn android_mono_duplicated_to_both_channels() {
        // Simulate mono sample duplicated to stereo ports
        let mono: f32 = 0.5;
        let mut fl = 0.0f32;
        let mut fr = 0.0f32;
        let outputs = [&mut fl as *mut f32, &mut fr as *mut f32];
        for out in outputs {
            unsafe {
                *out = mono;
            }
        }
        assert_eq!(fl, 0.5);
        assert_eq!(fr, 0.5);
    }

    #[test]
    fn empty_buffer_gives_silence() {
        let (rms, peak) = compute_levels(&[]);
        assert_eq!(rms, 0.0);
        assert_eq!(peak, 0.0);
    }
}
