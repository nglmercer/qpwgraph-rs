//! Realtime audio effects and the public effect SDK.
//!
//! The processing trait intentionally receives an already allocated,
//! interleaved `f32` buffer. Implementations must not allocate, block, or do
//! filesystem/network work from [`EffectProcessor::process`]. This makes the
//! same API usable from a PipeWire realtime callback and from an offline test.

use adaptive_noise::AdaptiveNoiseSuppressorFactory;
use serde::de::Error as DeserializeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use thiserror::Error;

mod adaptive_noise;
mod hush_noise;
mod hush_worker;
pub mod lifecycle;
pub mod wasm;
pub use adaptive_noise::AdaptiveNoiseSuppressor;
pub use hush_noise::HushNoiseSuppressor;
pub use hush_worker::{HushDiagnostics, HushHealth, HushOverloadReason};
pub use lifecycle::{
    EffectCancellation, EffectComponentManager, EffectLifecycle, EffectLoadStage,
    EffectPreparationEvent, EffectPrepareRequest, EffectTicket, PreparedEffect,
};

pub const NOISE_GATE_ID: &str = "builtin.noise-gate";
pub const NOISE_SUPPRESSOR_ID: &str = "builtin.adaptive-noise-suppressor";
pub use hush_noise::{DEFAULT_EFFECT_ID, HUSH_NOISE_SUPPRESSOR_ID};

/// The user's channel-layout intent.  This is deliberately separate from
/// the channel count negotiated with a live graph: `Auto` must remain `Auto`
/// when an effect is saved, even if the first topology observed at runtime is
/// mono.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ChannelPolicy {
    #[default]
    Auto,
    Fixed(u16),
}

impl Serialize for ChannelPolicy {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Auto => serializer.serialize_str("auto"),
            // Keep explicit layouts compact and readable.  This also means
            // new config files remain easy to consume by older tooling that
            // understood numeric `channels` values.
            Self::Fixed(channels) => serializer.serialize_u16(*channels),
        }
    }
}

impl<'de> Deserialize<'de> for ChannelPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Representation {
            Number(u16),
            Text(String),
        }

        match Representation::deserialize(deserializer)? {
            Representation::Number(channels) => Ok(Self::Fixed(channels)),
            Representation::Text(value) => {
                let normalized = value.trim().to_ascii_lowercase();
                match normalized.as_str() {
                    "auto" => Ok(Self::Auto),
                    "mono" => Ok(Self::Fixed(1)),
                    "stereo" => Ok(Self::Fixed(2)),
                    value if value.strip_prefix("fixed:").is_some() => value
                        .strip_prefix("fixed:")
                        .and_then(|channels| channels.parse::<u16>().ok())
                        .map(Self::Fixed)
                        .ok_or_else(|| {
                            D::Error::custom(format!(
                                "invalid fixed channel policy '{value}'"
                            ))
                        }),
                    _ => Err(D::Error::custom(format!(
                        "invalid channel policy '{value}', expected auto, mono, stereo, or a channel count"
                    ))),
                }
            }
        }
    }
}

impl ChannelPolicy {
    pub const fn requested_channels(self) -> Option<u16> {
        match self {
            Self::Auto => None,
            Self::Fixed(channels) => Some(channels),
        }
    }
}

/// Generic IO constraints advertised by an effect provider.  The backend
/// uses these constraints for negotiation instead of branching on a concrete
/// effect ID.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EffectIoCapabilities {
    pub min_channels: u16,
    pub max_channels: u16,
    pub independent_channels: bool,
    /// Safe channel count when `ChannelPolicy::Auto` has no topology yet.
    pub preferred_channels: u16,
}

impl EffectIoCapabilities {
    pub const fn stereo() -> Self {
        Self {
            min_channels: 1,
            max_channels: 2,
            independent_channels: false,
            preferred_channels: 2,
        }
    }

    pub const fn independent(min_channels: u16, max_channels: u16) -> Self {
        Self {
            min_channels,
            max_channels,
            independent_channels: true,
            preferred_channels: min_channels,
        }
    }

    pub fn validate(self) -> Result<(), EffectError> {
        if self.min_channels == 0
            || self.min_channels > self.max_channels
            || self.preferred_channels < self.min_channels
            || self.preferred_channels > self.max_channels
        {
            return Err(EffectError::InvalidIoCapabilities {
                min_channels: self.min_channels,
                max_channels: self.max_channels,
            });
        }
        Ok(())
    }
}

/// Resolve a persisted channel policy against currently known topology.  The
/// fallback only applies when `Auto` has no topology yet; it is a runtime
/// choice and must never be written back as a fixed policy.
pub fn negotiate_channels(
    policy: ChannelPolicy,
    capabilities: EffectIoCapabilities,
    topology_channels: Option<u16>,
) -> Result<u16, EffectError> {
    capabilities.validate()?;
    let channels = match policy {
        ChannelPolicy::Fixed(channels) => channels,
        ChannelPolicy::Auto => topology_channels.unwrap_or(capabilities.preferred_channels),
    };
    if channels < capabilities.min_channels || channels > capabilities.max_channels {
        return Err(EffectError::UnsupportedChannelCount {
            channels,
            min_channels: capabilities.min_channels,
            max_channels: capabilities.max_channels,
        });
    }
    Ok(channels)
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct AudioSpec {
    pub sample_rate: u32,
    pub channels: u16,
    pub max_frames: u32,
}

impl AudioSpec {
    pub fn validate(&self) -> Result<(), EffectError> {
        if self.sample_rate == 0 || self.channels == 0 || self.max_frames == 0 {
            return Err(EffectError::InvalidAudioSpec {
                sample_rate: self.sample_rate,
                channels: self.channels,
                max_frames: self.max_frames,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EffectParameter {
    pub id: String,
    pub name: String,
    pub minimum: f32,
    pub maximum: f32,
    pub default: f32,
    pub unit: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EffectDescriptor {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub parameters: Vec<EffectParameter>,
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum EffectError {
    #[error("invalid audio specification: sample rate {sample_rate}, channels {channels}, max frames {max_frames}")]
    InvalidAudioSpec {
        sample_rate: u32,
        channels: u16,
        max_frames: u32,
    },
    #[error("effect has not been prepared")]
    NotPrepared,
    #[error("audio buffer has {actual} samples, expected {expected}")]
    InvalidBufferLength { actual: usize, expected: usize },
    #[error("audio buffer exceeds the prepared frame limit: {frames} > {max_frames}")]
    FrameLimitExceeded { frames: u32, max_frames: u32 },
    #[error("unsupported effect parameter: {0}")]
    UnsupportedParameter(String),
    #[error("unknown effect: {0}")]
    UnknownEffect(String),
    #[error("invalid parameter value for {id}: {value}")]
    InvalidParameter { id: String, value: f32 },
    #[error("effect internal buffer capacity was exceeded")]
    InternalBufferOverflow,
    #[error("sample rate {sample_rate} is outside the supported range {minimum}..={maximum} Hz")]
    UnsupportedSampleRate {
        sample_rate: u32,
        minimum: u32,
        maximum: u32,
    },
    #[error("effect module is missing export: {0}")]
    MissingWasmExport(String),
    #[error("invalid effect module manifest: {0}")]
    InvalidWasmManifest(String),
    #[error("Hush model unavailable: {0}")]
    ModelUnavailable(String),
    #[error("Hush worker unavailable: {0}")]
    WorkerUnavailable(String),
    #[error("invalid effect IO capabilities: channel range {min_channels}..={max_channels}")]
    InvalidIoCapabilities {
        min_channels: u16,
        max_channels: u16,
    },
    #[error(
        "effect does not support {channels} channels (supported range {min_channels}..={max_channels})"
    )]
    UnsupportedChannelCount {
        channels: u16,
        min_channels: u16,
        max_channels: u16,
    },
    #[error("effect preparation queue is full")]
    PreparationQueueFull,
    #[error("effect preparation was cancelled")]
    PreparationCancelled,
}

/// A processor is created and prepared off the realtime thread.
pub trait EffectProcessor: Send {
    fn descriptor(&self) -> &EffectDescriptor;
    fn prepare(&mut self, spec: AudioSpec) -> Result<(), EffectError>;
    /// Process `frames` of interleaved audio in place.
    ///
    /// Implementations must not allocate, block, panic, or perform I/O here.
    fn process(&mut self, buffer: &mut [f32], frames: u32) -> Result<(), EffectError>;
    fn set_parameter(&mut self, id: &str, value: f32) -> Result<(), EffectError>;
    /// Update the realtime connection mask without allocating. Hosts use this
    /// to force disconnected stateful channels to silence and to invalidate
    /// stale worker generations.
    fn set_channel_mask(&mut self, _mask: u16) {}
    /// Set host bypass without a timeline discontinuity. Return true when
    /// process() must still run to maintain the processor's aligned dry path.
    fn set_host_bypass(&mut self, _bypassed: bool) -> bool {
        false
    }
    /// Report a persistent control/worker failure while still allowing the
    /// processor to publish its deterministic audio fallback. Hosts can
    /// surface the condition without replacing that aligned fallback with an
    /// instantaneous dry copy.
    /// Obtain a shared diagnostic handle during control-thread setup.
    fn hush_diagnostics(&self) -> Option<std::sync::Arc<HushDiagnostics>> {
        None
    }
    fn has_failed(&self) -> bool {
        false
    }
    fn reset(&mut self);
}

/// A source of effect instances and the metadata needed to negotiate them.
///
/// The provider owns the non-realtime construction boundary.  The default
/// implementation is sufficient for built-in processors; resource-backed
/// providers (Hush, WASM, and future native providers) can override
/// `prepare_instance` to load or validate their component before an instance
/// is activated by a host.
pub trait EffectProvider: Send + Sync {
    fn descriptor(&self) -> &EffectDescriptor;
    fn create(&self) -> Box<dyn EffectProcessor>;
    fn io_capabilities(&self) -> EffectIoCapabilities {
        EffectIoCapabilities::stereo()
    }

    /// Prepare one instance entirely outside the realtime path.  This hook is
    /// called by [`EffectComponentManager`](crate::EffectComponentManager)
    /// on its bounded loader pool.
    fn prepare_instance(
        &self,
        request: EffectPrepareRequest,
    ) -> Result<PreparedEffect, EffectError> {
        let started = std::time::Instant::now();
        let mut processor = self.create();
        processor.prepare(request.spec)?;
        apply_parameters(&mut *processor, &request.parameters)?;
        Ok(PreparedEffect {
            descriptor: self.descriptor().clone(),
            spec: request.spec,
            processor,
            preparation_duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

/// Compatibility name for callers written before providers became the
/// lifecycle abstraction.  It is an alias, not a second trait, so existing
/// implementations and registrations remain source-compatible.
pub use EffectProvider as EffectFactory;

#[derive(Clone, Default)]
pub struct EffectHost {
    providers: BTreeMap<String, Arc<dyn EffectProvider>>,
}

impl EffectHost {
    pub fn new() -> Self {
        let mut host = Self::default();
        host.register(Box::new(NoiseGateFactory));
        host.register(Box::new(AdaptiveNoiseSuppressorFactory));
        host.register(hush_noise::HushNoiseSuppressor::factory());
        host
    }

    pub fn register(&mut self, provider: Box<dyn EffectProvider>) {
        let id = provider.descriptor().id.clone();
        self.providers.insert(id, Arc::from(provider));
    }

    pub fn descriptors(&self) -> Vec<EffectDescriptor> {
        self.providers
            .values()
            .map(|provider| provider.descriptor().clone())
            .collect()
    }

    pub fn io_capabilities(&self, id: &str) -> Result<EffectIoCapabilities, EffectError> {
        self.providers
            .get(id)
            .map(|provider| provider.io_capabilities())
            .ok_or_else(|| EffectError::UnknownEffect(id.into()))
    }

    /// Negotiate an effect's runtime channel count without changing the
    /// persisted policy.  Hosts should call this for standalone creation,
    /// link insertion, and restore paths alike.
    pub fn negotiate_channels(
        &self,
        id: &str,
        policy: ChannelPolicy,
        topology_channels: Option<u16>,
    ) -> Result<u16, EffectError> {
        negotiate_channels(policy, self.io_capabilities(id)?, topology_channels)
    }

    pub fn create(&self, id: &str) -> Result<Box<dyn EffectProcessor>, EffectError> {
        self.providers
            .get(id)
            .map(|provider| provider.create())
            .ok_or_else(|| EffectError::UnknownEffect(id.into()))
    }

    /// Synchronously prepare one instance through its provider.  This is a
    /// compatibility API for hosts that still expose a synchronous create
    /// operation; heavyweight callers should use [`EffectComponentManager`]
    /// instead so this work runs on its bounded loader pool.
    pub fn prepare_instance(
        &self,
        request: EffectPrepareRequest,
    ) -> Result<PreparedEffect, EffectError> {
        self.providers
            .get(&request.effect_id)
            .ok_or_else(|| EffectError::UnknownEffect(request.effect_id.clone()))?
            .prepare_instance(request)
    }

    pub(crate) fn provider(&self, id: &str) -> Result<Arc<dyn EffectProvider>, EffectError> {
        self.providers
            .get(id)
            .cloned()
            .ok_or_else(|| EffectError::UnknownEffect(id.into()))
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EffectInstanceConfig {
    pub instance_id: String,
    pub effect_id: String,
    #[serde(default)]
    pub module_path: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub parameters: BTreeMap<String, f32>,
    /// Persisted channel intent.  The `channels` alias is read for backward
    /// compatibility with configurations written before channel policy was a
    /// first-class type; `channels = 1/2` migrates to `Fixed(1/2)` and an
    /// omitted value migrates to `Auto`.
    #[serde(default, alias = "channels")]
    pub channel_policy: ChannelPolicy,
}

fn default_true() -> bool {
    true
}

pub const NOISE_GATE_THRESHOLD: &str = "threshold-db";
pub const NOISE_GATE_ATTACK: &str = "attack-ms";
pub const NOISE_GATE_HOLD: &str = "hold-ms";
pub const NOISE_GATE_RELEASE: &str = "release-ms";
pub const NOISE_GATE_BYPASS: &str = "bypass";
pub const NOISE_SUPPRESSOR_REDUCTION: &str = "reduction-db";
pub const NOISE_SUPPRESSOR_ADAPTATION: &str = "adaptation";
pub const NOISE_SUPPRESSOR_VOICE_PRESERVE: &str = "voice-preserve";
pub const NOISE_SUPPRESSOR_BYPASS: &str = "bypass";

fn noise_gate_descriptor() -> EffectDescriptor {
    EffectDescriptor {
        id: NOISE_GATE_ID.into(),
        name: "Noise Gate".into(),
        vendor: "qpwgraph-rs".into(),
        version: "1.0.0".into(),
        parameters: vec![
            EffectParameter {
                id: NOISE_GATE_THRESHOLD.into(),
                name: "Threshold".into(),
                minimum: -80.0,
                maximum: 0.0,
                default: -45.0,
                unit: "dB".into(),
            },
            EffectParameter {
                id: NOISE_GATE_ATTACK.into(),
                name: "Attack".into(),
                minimum: 0.0,
                maximum: 500.0,
                default: 5.0,
                unit: "ms".into(),
            },
            EffectParameter {
                id: NOISE_GATE_HOLD.into(),
                name: "Hold".into(),
                minimum: 0.0,
                maximum: 2000.0,
                default: 40.0,
                unit: "ms".into(),
            },
            EffectParameter {
                id: NOISE_GATE_RELEASE.into(),
                name: "Release".into(),
                minimum: 0.0,
                maximum: 2000.0,
                default: 120.0,
                unit: "ms".into(),
            },
            EffectParameter {
                id: NOISE_GATE_BYPASS.into(),
                name: "Bypass".into(),
                minimum: 0.0,
                maximum: 1.0,
                default: 0.0,
                unit: "boolean".into(),
            },
        ],
    }
}

struct NoiseGateFactory;

impl EffectProvider for NoiseGateFactory {
    fn descriptor(&self) -> &EffectDescriptor {
        static DESCRIPTOR: std::sync::OnceLock<EffectDescriptor> = std::sync::OnceLock::new();
        DESCRIPTOR.get_or_init(noise_gate_descriptor)
    }

    fn create(&self) -> Box<dyn EffectProcessor> {
        Box::new(NoiseGate::default())
    }
}

#[derive(Clone, Debug)]
pub struct NoiseGate {
    descriptor: EffectDescriptor,
    spec: Option<AudioSpec>,
    threshold_db: f32,
    attack_ms: f32,
    hold_ms: f32,
    release_ms: f32,
    bypass: bool,
    gain: f32,
    hold_frames: u32,
}

impl Default for NoiseGate {
    fn default() -> Self {
        Self {
            descriptor: noise_gate_descriptor(),
            spec: None,
            threshold_db: -45.0,
            attack_ms: 5.0,
            hold_ms: 40.0,
            release_ms: 120.0,
            bypass: false,
            gain: 0.0,
            hold_frames: 0,
        }
    }
}

impl NoiseGate {
    fn parameter(&self, id: &str) -> Option<(f32, f32)> {
        self.descriptor
            .parameters
            .iter()
            .find(|parameter| parameter.id == id)
            .map(|parameter| (parameter.minimum, parameter.maximum))
    }

    fn coefficient(milliseconds: f32, sample_rate: u32) -> f32 {
        if milliseconds <= 0.0 {
            0.0
        } else {
            (-1.0 / (milliseconds * 0.001 * sample_rate as f32)).exp()
        }
    }
}

impl EffectProcessor for NoiseGate {
    fn descriptor(&self) -> &EffectDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, spec: AudioSpec) -> Result<(), EffectError> {
        spec.validate()?;
        self.spec = Some(spec);
        self.gain = 0.0;
        self.hold_frames = 0;
        Ok(())
    }

    fn process(&mut self, buffer: &mut [f32], frames: u32) -> Result<(), EffectError> {
        let Some(spec) = self.spec.as_ref() else {
            return Err(EffectError::NotPrepared);
        };
        if frames > spec.max_frames {
            return Err(EffectError::FrameLimitExceeded {
                frames,
                max_frames: spec.max_frames,
            });
        }
        let expected = frames as usize * spec.channels as usize;
        if buffer.len() != expected {
            return Err(EffectError::InvalidBufferLength {
                actual: buffer.len(),
                expected,
            });
        }
        // Bypass is transparent for valid samples, but a malformed sample is
        // still replaced with digital silence. This keeps the standalone SDK
        // contract consistent with the PipeWire host's realtime sanitizer.
        if self.bypass {
            for sample in buffer {
                if !sample.is_finite() {
                    *sample = 0.0;
                }
            }
            return Ok(());
        }

        let threshold = 10.0_f32.powf(self.threshold_db / 20.0);
        let attack = Self::coefficient(self.attack_ms, spec.sample_rate);
        let release = Self::coefficient(self.release_ms, spec.sample_rate);
        let hold_limit = (self.hold_ms * 0.001 * spec.sample_rate as f32) as u32;
        let channels = spec.channels as usize;

        for frame in buffer.chunks_exact_mut(channels) {
            let mut level = 0.0_f32;
            for sample in frame.iter_mut() {
                if !sample.is_finite() {
                    *sample = 0.0;
                }
                level = level.max(sample.abs());
            }

            if level >= threshold {
                self.hold_frames = hold_limit;
                self.gain = 1.0 - (1.0 - self.gain) * attack;
            } else if self.hold_frames > 0 {
                self.hold_frames -= 1;
            } else {
                self.gain *= release;
            }

            for sample in frame {
                *sample *= self.gain;
            }
        }
        Ok(())
    }

    fn set_parameter(&mut self, id: &str, value: f32) -> Result<(), EffectError> {
        let Some((minimum, maximum)) = self.parameter(id) else {
            return Err(EffectError::UnsupportedParameter(id.into()));
        };
        if !value.is_finite() {
            return Err(EffectError::InvalidParameter {
                id: id.into(),
                value,
            });
        }
        let value = value.clamp(minimum, maximum);
        match id {
            NOISE_GATE_THRESHOLD => self.threshold_db = value,
            NOISE_GATE_ATTACK => self.attack_ms = value,
            NOISE_GATE_HOLD => self.hold_ms = value,
            NOISE_GATE_RELEASE => self.release_ms = value,
            NOISE_GATE_BYPASS => {
                let bypass = value >= 0.5;
                if self.bypass != bypass {
                    self.bypass = bypass;
                    self.reset();
                }
            }
            _ => unreachable!("descriptor and parameter match are kept together"),
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.gain = 0.0;
        self.hold_frames = 0;
    }
}

/// A small helper for hosts applying persisted parameters before preparation.
pub fn apply_parameters(
    processor: &mut dyn EffectProcessor,
    parameters: &BTreeMap<String, f32>,
) -> Result<(), EffectError> {
    for (id, value) in parameters {
        processor.set_parameter(id, *value)?;
    }
    Ok(())
}

/// Return the names exported by a module that every host must require.
pub fn required_wasm_exports() -> BTreeSet<&'static str> {
    wasm::required_exports()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared_gate() -> NoiseGate {
        let mut gate = NoiseGate::default();
        gate.prepare(AudioSpec {
            sample_rate: 48_000,
            channels: 2,
            max_frames: 128,
        })
        .unwrap();
        gate
    }

    #[test]
    fn silence_is_attenuated() {
        let mut gate = prepared_gate();
        let mut audio = vec![0.001; 64];
        gate.process(&mut audio, 32).unwrap();
        assert!(audio.iter().all(|sample| sample.abs() < 0.001));
    }

    #[test]
    fn exact_silence_stays_exactly_silent() {
        let mut gate = prepared_gate();
        let mut audio = vec![0.0; 64];
        gate.process(&mut audio, 32).unwrap();
        assert_eq!(audio, vec![0.0; 64]);
    }

    #[test]
    fn a_disconnected_stereo_channel_cannot_open_the_gate() {
        let mut gate = prepared_gate();
        gate.set_parameter(NOISE_GATE_ATTACK, 0.0).unwrap();
        gate.set_parameter(NOISE_GATE_HOLD, 0.0).unwrap();
        let mut audio = Vec::with_capacity(64);
        for _ in 0..32 {
            // FL is below the default threshold; FR represents a missing
            // channel supplied to the processor as exact zero.
            audio.extend_from_slice(&[0.001, 0.0]);
        }

        gate.process(&mut audio, 32).unwrap();

        assert_eq!(audio, vec![0.0; 64]);
    }

    #[test]
    fn loud_signal_opens_gate() {
        let mut gate = prepared_gate();
        gate.set_parameter(NOISE_GATE_ATTACK, 0.0).unwrap();
        let mut audio = vec![0.8; 64];
        gate.process(&mut audio, 32).unwrap();
        assert!(audio.iter().all(|sample| (*sample - 0.8).abs() < 1e-6));
    }

    #[test]
    fn hold_keeps_gate_open_after_signal_drops() {
        let mut gate = prepared_gate();
        gate.set_parameter(NOISE_GATE_ATTACK, 0.0).unwrap();
        gate.set_parameter(NOISE_GATE_HOLD, 100.0).unwrap();
        let mut loud = vec![0.8; 2];
        gate.process(&mut loud, 1).unwrap();
        let mut quiet = vec![0.001; 2];
        gate.process(&mut quiet, 1).unwrap();
        assert!(quiet[0] > 0.0009);
    }

    #[test]
    fn invalid_values_are_safely_sanitized() {
        let mut gate = prepared_gate();
        let mut audio = vec![f32::NAN, f32::INFINITY, -f32::INFINITY, 0.0];
        gate.process(&mut audio, 2).unwrap();
        assert!(audio.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn parameters_are_clamped() {
        let mut gate = prepared_gate();
        gate.set_parameter(NOISE_GATE_THRESHOLD, 100.0).unwrap();
        gate.set_parameter(NOISE_GATE_BYPASS, 4.0).unwrap();
        let mut audio = vec![0.5; 2];
        gate.process(&mut audio, 1).unwrap();
        assert_eq!(audio, vec![0.5, 0.5]);
    }

    #[test]
    fn resetting_the_gate_clears_its_open_state() {
        let mut gate = prepared_gate();
        gate.set_parameter(NOISE_GATE_ATTACK, 0.0).unwrap();
        gate.set_parameter(NOISE_GATE_HOLD, 100.0).unwrap();
        let mut loud = vec![0.8; 2];
        gate.process(&mut loud, 1).unwrap();
        gate.reset();

        let mut quiet = vec![0.001; 2];
        gate.process(&mut quiet, 1).unwrap();

        assert_eq!(quiet, vec![0.0; 2]);
    }

    #[test]
    fn bypass_sanitizes_non_finite_samples_but_preserves_valid_audio() {
        let mut gate = prepared_gate();
        gate.set_parameter(NOISE_GATE_BYPASS, 1.0).unwrap();
        let mut audio = vec![0.25, f32::NAN, f32::INFINITY, -0.5];

        gate.process(&mut audio, 2).unwrap();

        assert_eq!(audio, vec![0.25, 0.0, 0.0, -0.5]);
    }

    #[test]
    fn host_exposes_the_adaptive_noise_suppressor() {
        let descriptors = EffectHost::new().descriptors();
        assert!(descriptors
            .iter()
            .any(|descriptor| descriptor.id == NOISE_SUPPRESSOR_ID));
    }

    #[test]
    fn host_exposes_hush_as_a_distinct_effect_and_default_identity() {
        let descriptors = EffectHost::new().descriptors();
        assert!(descriptors
            .iter()
            .any(|descriptor| descriptor.id == HUSH_NOISE_SUPPRESSOR_ID));
        assert_eq!(DEFAULT_EFFECT_ID, HUSH_NOISE_SUPPRESSOR_ID);
        assert_ne!(DEFAULT_EFFECT_ID, NOISE_SUPPRESSOR_ID);
    }

    #[test]
    fn hush_descriptor_has_only_real_controls() {
        let descriptor = EffectHost::new()
            .descriptors()
            .into_iter()
            .find(|descriptor| descriptor.id == HUSH_NOISE_SUPPRESSOR_ID)
            .expect("Hush descriptor");
        assert_eq!(descriptor.parameters.len(), 2);
        assert!(descriptor
            .parameters
            .iter()
            .any(|parameter| { parameter.id == "reduction-db" && parameter.default == 25.0 }));
        assert!(descriptor
            .parameters
            .iter()
            .any(|parameter| parameter.id == "bypass" && parameter.unit == "boolean"));
    }
}
