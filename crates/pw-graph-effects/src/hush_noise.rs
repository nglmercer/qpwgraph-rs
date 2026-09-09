//! Hush neural noise suppressor.
//!
//! Model loading and denoiser construction happen during `prepare()`, which
//! hosts call from their control/setup thread.  The realtime `process()` path
//! only sanitizes samples, advances a preallocated dry delay, and performs
//! bounded SPSC queue operations.

use crate::hush_worker::{HushDiagnostics, HushRuntime, HUSH_MAX_BACKLOG_MS, HUSH_SCHEDULING_MS};
use crate::{
    AudioSpec, EffectDescriptor, EffectError, EffectFactory, EffectParameter, EffectProcessor,
};
use nnnoiseless::{HushModel, HUSH_SYNTHESIS_DELAY_SAMPLES};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Instant;

pub const HUSH_NOISE_SUPPRESSOR_ID: &str = "builtin.hush-noise-suppressor";
pub const DEFAULT_EFFECT_ID: &str = HUSH_NOISE_SUPPRESSOR_ID;
pub const HUSH_NOISE_SUPPRESSOR_REDUCTION: &str = "reduction-db";
pub const HUSH_NOISE_SUPPRESSOR_BYPASS: &str = "bypass";

const HUSH_MODEL_SHA256: &str = "45632ccaa82b71bb743d6caa7c78e983fe2f2790a3af7f6ec48e6ed7ba085df6";
const EMBEDDED_HUSH_MODEL: &[u8] =
    include_bytes!("../resources/hush/advanced_dfnet16k_model_best_onnx.tar.gz");

const EMBEDDED_HUSH_MODEL_NAME: &str = "advanced_dfnet16k_model_best_onnx.tar.gz";

/// Immutable metadata for the process-wide Hush model load.  The byte
/// contents are intentionally not retained here; the parsed model owns what
/// inference needs and diagnostics only need provenance and timings.
#[derive(Clone, Debug, Default)]
pub(crate) struct HushModelLoadInfo {
    pub(crate) source: String,
    pub(crate) name: String,
    pub(crate) path: Option<String>,
    pub(crate) embedded: bool,
    pub(crate) compressed_bytes: u64,
    pub(crate) decompressed_bytes: Option<u64>,
    pub(crate) checksum: String,
    pub(crate) load_started: bool,
    pub(crate) load_completed: bool,
    pub(crate) load_duration_ms: u64,
    pub(crate) parse_started: bool,
    pub(crate) parse_completed: bool,
    pub(crate) error: Option<String>,
}

#[inline]
fn channel_is_connected(mask: u16, channel: usize) -> bool {
    channel < u16::BITS as usize && mask & (1u16 << channel) != 0
}

/// Return the only portion of a callback that is safe to read from the wet
/// timeline. The callback's input is submitted immediately before this
/// request, so the requested interval must end no later than its start.
#[inline]
fn safe_wet_request(
    callback_start: u64,
    frames: u32,
    scheduling_frames: usize,
) -> (u64, u32, usize) {
    let prefix = (scheduling_frames as u64)
        .saturating_sub(callback_start)
        .min(frames as u64) as usize;
    let start = callback_start.saturating_sub(scheduling_frames as u64);
    let requested_frames = frames - prefix as u32;
    debug_assert!(start.saturating_add(requested_frames as u64) <= callback_start);
    (start, requested_frames, prefix)
}

fn descriptor() -> EffectDescriptor {
    EffectDescriptor {
        id: HUSH_NOISE_SUPPRESSOR_ID.into(),
        name: "Hush Neural Noise Suppressor".into(),
        vendor: "qpwgraph-rs / Hush".into(),
        version: "1.0.0".into(),
        parameters: vec![
            EffectParameter {
                id: HUSH_NOISE_SUPPRESSOR_REDUCTION.into(),
                name: "Reduction".into(),
                minimum: 0.0,
                maximum: 60.0,
                default: 25.0,
                unit: "dB".into(),
            },
            EffectParameter {
                id: HUSH_NOISE_SUPPRESSOR_BYPASS.into(),
                name: "Bypass".into(),
                minimum: 0.0,
                maximum: 1.0,
                default: 0.0,
                unit: "boolean".into(),
            },
        ],
    }
}

struct HushNoiseSuppressorFactory;

impl EffectFactory for HushNoiseSuppressorFactory {
    fn descriptor(&self) -> &EffectDescriptor {
        static DESCRIPTOR: OnceLock<EffectDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(descriptor)
    }

    fn create(&self) -> Box<dyn EffectProcessor> {
        Box::new(HushNoiseSuppressor::default())
    }
}

/// A Hush processor with a shared immutable model and one worker per effect.
pub struct HushNoiseSuppressor {
    descriptor: EffectDescriptor,
    spec: Option<AudioSpec>,
    runtime: Option<HushRuntime>,
    diagnostics: Option<Arc<HushDiagnostics>>,
    reduction_db: f32,
    bypass: bool,
    host_bypass: bool,
    channel_mask: u16,
    generation: u64,
    input_frame_position: u64,
    scheduling_frames: usize,
    synthesis_delay_frames: usize,
    dry_delay: Option<DryDelay>,
    fallback: Vec<f32>,
    wet: Vec<f32>,
}

impl Default for HushNoiseSuppressor {
    fn default() -> Self {
        Self {
            descriptor: descriptor(),
            spec: None,
            runtime: None,
            diagnostics: None,
            reduction_db: 25.0,
            bypass: false,
            host_bypass: false,
            channel_mask: 0,
            generation: 0,
            input_frame_position: 0,
            scheduling_frames: 0,
            synthesis_delay_frames: 0,
            dry_delay: None,
            fallback: Vec::new(),
            wet: Vec::new(),
        }
    }
}

impl HushNoiseSuppressor {
    pub(crate) fn factory() -> Box<dyn EffectFactory> {
        Box::new(HushNoiseSuppressorFactory)
    }

    fn parameter(&self, id: &str) -> Option<(f32, f32)> {
        self.descriptor
            .parameters
            .iter()
            .find(|parameter| parameter.id == id)
            .map(|parameter| (parameter.minimum, parameter.maximum))
    }

    fn next_generation(&mut self, reset_delay: bool) {
        self.generation = self.generation.wrapping_add(1);
        self.input_frame_position = 0;
        if let Some(runtime) = &self.runtime {
            runtime.set_generation(self.generation);
            runtime.request_generation_retry();
        }
        if reset_delay {
            if let Some(delay) = &mut self.dry_delay {
                delay.reset();
            }
        }
        self.fallback.fill(0.0);
        self.wet.fill(0.0);
    }
}

impl EffectProcessor for HushNoiseSuppressor {
    fn descriptor(&self) -> &EffectDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, spec: AudioSpec) -> Result<(), EffectError> {
        spec.validate()?;
        if !matches!(spec.channels, 1 | 2) {
            return Err(EffectError::WorkerUnavailable(format!(
                "Hush supports one or two channels, got {}",
                spec.channels
            )));
        }
        if let Some(diagnostics) = &self.diagnostics {
            self.reduction_db = f32::from_bits(
                diagnostics
                    .attenuation_bits
                    .load(std::sync::atomic::Ordering::Acquire),
            );
            self.bypass = diagnostics
                .bypass
                .load(std::sync::atomic::Ordering::Acquire);
        }
        let model = shared_hush_model().map_err(EffectError::ModelUnavailable)?;
        let channels = spec.channels as usize;
        let channel_mask = if channels >= 16 {
            u16::MAX
        } else {
            (1u16 << channels) - 1
        };
        let generation = self
            .generation
            .wrapping_add(u64::from(self.runtime.is_some()));
        let runtime = HushRuntime::spawn(
            model,
            spec.sample_rate,
            spec.channels,
            spec.max_frames,
            self.reduction_db,
            generation,
        )
        .map_err(EffectError::WorkerUnavailable)?;
        self.channel_mask = channel_mask;
        self.generation = generation;
        // Resamplers center sample zero on source sample zero; their
        // lookahead delays availability, not sample position. Streaming Hush
        // contains 160 native samples of synthesis delay. The scheduling
        // allowance starts at 40 ms and grows from the observed host quantum;
        // worker cost is measured independently for overload/recovery. Reserve
        // enough capacity for any callback up to max_frames without making
        // that capacity the logical latency.
        let synthesis_delay_frames =
            (spec.sample_rate as usize * HUSH_SYNTHESIS_DELAY_SAMPLES).div_ceil(16_000);
        self.scheduling_frames =
            (spec.sample_rate as usize * HUSH_SCHEDULING_MS as usize).div_ceil(1000);
        let logical_delay_frames = synthesis_delay_frames + self.scheduling_frames;
        let delay_capacity_frames = logical_delay_frames
            + self.scheduling_frames.max(spec.max_frames as usize)
            + (spec.sample_rate as usize * HUSH_MAX_BACKLOG_MS as usize).div_ceil(1000);
        self.fallback = vec![0.0; spec.max_frames as usize * channels];
        self.wet = vec![0.0; spec.max_frames as usize * channels];
        self.dry_delay = Some(DryDelay::new(
            delay_capacity_frames.saturating_mul(channels),
        ));
        self.dry_delay
            .as_mut()
            .unwrap()
            .set_delay(logical_delay_frames * channels);
        self.input_frame_position = 0;
        self.synthesis_delay_frames = synthesis_delay_frames;
        runtime.diagnostics().scheduling_frames.store(
            self.scheduling_frames as u64,
            std::sync::atomic::Ordering::Release,
        );
        runtime.diagnostics().synthesis_delay_frames.store(
            synthesis_delay_frames as u64,
            std::sync::atomic::Ordering::Release,
        );
        runtime.diagnostics().total_latency_frames.store(
            (self.scheduling_frames + synthesis_delay_frames) as u64,
            std::sync::atomic::Ordering::Release,
        );
        runtime.set_generation(self.generation);
        runtime
            .diagnostics()
            .bypass
            .store(self.bypass, std::sync::atomic::Ordering::Release);
        runtime
            .diagnostics()
            .host_disabled
            .store(self.host_bypass, std::sync::atomic::Ordering::Release);
        self.diagnostics = Some(runtime.diagnostics().clone());
        self.spec = Some(spec);
        self.runtime = Some(runtime);
        Ok(())
    }

    fn process(&mut self, buffer: &mut [f32], frames: u32) -> Result<(), EffectError> {
        // `AudioSpec` is a small copyable value. Taking an owned snapshot
        // keeps the subsequent generation/quantum update independent of the
        // processor's mutable lifecycle fields and does not allocate.
        let Some(spec) = self.spec.clone() else {
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
        let Some(runtime) = &self.runtime else {
            return Err(EffectError::NotPrepared);
        };
        // The first callback establishes the active host quantum. Later
        // callbacks may vary, but scheduling only grows so the wet timeline
        // never moves earlier and never asks the worker for the callback it
        // has just received.
        self.scheduling_frames = runtime.observe_quantum(frames);
        if let Some(delay) = &mut self.dry_delay {
            let delay_frames = self.scheduling_frames + self.synthesis_delay_frames;
            delay.set_delay_preserve(delay_frames * spec.channels as usize);
        }
        for sample in buffer.iter_mut() {
            if !sample.is_finite() {
                *sample = 0.0;
            }
        }
        // Preserve the sanitized live input for the worker before the dry
        // delay overwrites the callback buffer.
        self.wet[..expected].copy_from_slice(buffer);

        if let Some(delay) = &mut self.dry_delay {
            delay.process(buffer, &mut self.fallback[..expected]);
        } else {
            self.fallback[..expected].copy_from_slice(buffer);
        }
        for frame in 0..frames as usize {
            for channel in 0..spec.channels as usize {
                if !channel_is_connected(self.channel_mask, channel) {
                    self.fallback[frame * spec.channels as usize + channel] = 0.0;
                }
            }
        }
        buffer.copy_from_slice(&self.fallback[..expected]);

        runtime.push(
            self.generation,
            self.input_frame_position,
            frames,
            spec.channels,
            self.channel_mask,
            &self.wet[..expected],
        );
        self.wet[..expected].copy_from_slice(&self.fallback[..expected]);
        // DeepFilterNet short-circuits synthesis below 0.01 dB, changing
        // its signal delay. Select exact aligned dry at that setting while
        // the worker runs at >=0.01 dB to keep synthesis/recurrent state warm.
        let bypassed = self.host_bypass
            || runtime
                .diagnostics()
                .bypass
                .load(std::sync::atomic::Ordering::Acquire)
            || f32::from_bits(
                runtime
                    .diagnostics()
                    .attenuation_bits
                    .load(std::sync::atomic::Ordering::Acquire),
            ) < 0.01;
        let (wet_start, wet_frames, prefix) =
            safe_wet_request(self.input_frame_position, frames, self.scheduling_frames);
        let wet_frames_available = if !runtime
            .diagnostics()
            .worker_failed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            runtime.pop_ready(
                self.generation,
                wet_start,
                wet_frames,
                spec.channels,
                &mut self.wet[prefix * spec.channels as usize..expected],
            )
        } else {
            0
        };
        let mut wet_frames_output = 0usize;
        if wet_frames_available > 0 && !bypassed {
            for channel in 0..spec.channels as usize {
                if !channel_is_connected(self.channel_mask, channel) {
                    for frame in 0..frames as usize {
                        self.wet[frame * spec.channels as usize + channel] = 0.0;
                    }
                }
            }
            buffer.copy_from_slice(&self.wet[..expected]);
            runtime
                .diagnostics()
                .wet_blocks_output
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            wet_frames_output = wet_frames_available;
        }
        let worker_failed = runtime
            .diagnostics()
            .worker_failed
            .load(std::sync::atomic::Ordering::Acquire);
        let resyncing = runtime.is_resyncing();
        let dry_frames = frames as usize - wet_frames_output;
        runtime.diagnostics().wet_frames_output.fetch_add(
            wet_frames_output as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        runtime
            .diagnostics()
            .dry_frames_output
            .fetch_add(dry_frames as u64, std::sync::atomic::Ordering::Relaxed);
        if bypassed || wet_frames_output < frames as usize {
            runtime
                .diagnostics()
                .dry_fallback_blocks
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if self.host_bypass {
                runtime
                    .diagnostics()
                    .dry_host_disabled_blocks
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else if bypassed {
                runtime
                    .diagnostics()
                    .dry_bypass_blocks
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else if worker_failed {
                runtime
                    .diagnostics()
                    .dry_worker_failure_blocks
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else if resyncing {
                runtime
                    .diagnostics()
                    .dry_resync_blocks
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else if self.input_frame_position
                < (self.scheduling_frames + self.synthesis_delay_frames) as u64
            {
                runtime
                    .diagnostics()
                    .dry_startup_blocks
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else {
                runtime
                    .diagnostics()
                    .dry_underrun_blocks
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            if !bypassed
                && !worker_failed
                && !resyncing
                && self.channel_mask != 0
                && self.input_frame_position
                    >= (self.scheduling_frames + self.synthesis_delay_frames) as u64
            {
                runtime
                    .diagnostics()
                    .underruns
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        self.input_frame_position += frames as u64;
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
            HUSH_NOISE_SUPPRESSOR_REDUCTION => {
                self.reduction_db = value;
                if let Some(runtime) = &self.runtime {
                    runtime.set_attenuation(value);
                }
            }
            HUSH_NOISE_SUPPRESSOR_BYPASS => {
                let bypass = value >= 0.5;
                self.bypass = bypass;
                if let Some(diagnostics) = &self.diagnostics {
                    diagnostics
                        .bypass
                        .store(bypass, std::sync::atomic::Ordering::Release);
                }
            }
            _ => unreachable!("descriptor and parameter match are kept together"),
        }
        Ok(())
    }

    fn set_host_bypass(&mut self, bypassed: bool) -> bool {
        let changed = self.host_bypass != bypassed;
        self.host_bypass = bypassed;
        if let Some(diagnostics) = &self.diagnostics {
            diagnostics
                .host_disabled
                .store(bypassed, std::sync::atomic::Ordering::Release);
        }
        if changed && !bypassed {
            if let Some(runtime) = &self.runtime {
                runtime.request_retry();
            }
        }
        true
    }

    fn set_channel_mask(&mut self, mask: u16) {
        if self.channel_mask != mask {
            self.channel_mask = mask;
            self.next_generation(true);
        }
    }

    fn hush_diagnostics(&self) -> Option<Arc<HushDiagnostics>> {
        self.diagnostics.clone()
    }

    fn has_failed(&self) -> bool {
        self.diagnostics.as_ref().is_some_and(|diagnostics| {
            diagnostics
                .worker_failed
                .load(std::sync::atomic::Ordering::Acquire)
        })
    }

    fn reset(&mut self) {
        self.next_generation(true);
    }
}

/// Load the model once, on the caller's setup/control thread.  The audio
/// callback never reads the filesystem or initializes Tract.
pub(crate) fn shared_hush_model() -> Result<Arc<HushModel>, String> {
    shared_hush_model_load().model.clone()
}

/// Return the model provenance captured by the one-time load attempt. Calling
/// this also performs the load if no effect has prepared yet, so callers must
/// keep it off the realtime path.
pub(crate) fn hush_model_load_info() -> HushModelLoadInfo {
    shared_hush_model_load().info.clone()
}

struct SharedHushModelLoad {
    model: Result<Arc<HushModel>, String>,
    info: HushModelLoadInfo,
}

fn shared_hush_model_load() -> &'static SharedHushModelLoad {
    static MODEL: OnceLock<SharedHushModelLoad> = OnceLock::new();
    MODEL.get_or_init(load_hush_model)
}

fn load_hush_model() -> SharedHushModelLoad {
    let override_path = non_empty_env_path("QPWGRAPH_HUSH_MODEL")
        .map(|path| ("QPWGRAPH_HUSH_MODEL", path))
        .or_else(|| non_empty_env_path("HUSH_MODEL").map(|path| ("HUSH_MODEL", path)));
    load_hush_model_with_override(override_path)
}

fn load_hush_model_with_override(override_path: Option<(&str, PathBuf)>) -> SharedHushModelLoad {
    let started = Instant::now();
    let (source, name, path, embedded) = match override_path.as_ref() {
        Some((variable, path)) => (
            format!("environment override ({variable})"),
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("external Hush model")
                .to_owned(),
            Some(path.display().to_string()),
            false,
        ),
        None => (
            "embedded".to_owned(),
            EMBEDDED_HUSH_MODEL_NAME.to_owned(),
            None,
            true,
        ),
    };
    let mut info = HushModelLoadInfo {
        source,
        name,
        path,
        embedded,
        load_started: true,
        ..HushModelLoadInfo::default()
    };
    eprintln!(
        "INFO hush: loading model source={} name={}{}",
        info.source,
        info.name,
        info.path
            .as_deref()
            .map(|path| format!(" path={path}"))
            .unwrap_or_default()
    );

    let bytes = if let Some((_, path)) = override_path {
        match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                let message = format!("could not read Hush model {}: {error}", path.display());
                return failed_model_load(info, started, message);
            }
        }
    } else {
        EMBEDDED_HUSH_MODEL.to_vec()
    };
    info.compressed_bytes = bytes.len() as u64;
    info.decompressed_bytes = gzip_uncompressed_size(&bytes);
    info.load_completed = true;
    let checksum = Sha256::digest(&bytes);
    info.checksum = format!("{checksum:x}");
    // The embedded artifact is pinned and can therefore be verified against a
    // known digest. An external override is intentionally allowed to have a
    // different digest: its checksum is still reported, while parsing and
    // denoiser initialization validate that it is actually a compatible Hush
    // bundle. Never silently fall back to the embedded artifact after an
    // override fails.
    if info.embedded {
        if let Err(error) = validate_model_checksum(&bytes, &info.source) {
            return failed_model_load(info, started, error);
        }
    }

    info.parse_started = true;
    let parsed = if info.embedded {
        HushModel::from_static_bytes(EMBEDDED_HUSH_MODEL)
    } else {
        HushModel::from_bytes(&bytes)
    };
    let model = match parsed {
        Ok(model) => {
            info.parse_completed = true;
            Ok(Arc::new(model))
        }
        Err(error) => Err(format!("could not parse Hush model: {error}")),
    };
    info.load_duration_ms = elapsed_ms(started);
    if let Err(error) = &model {
        info.error = Some(error.clone());
        eprintln!("ERROR hush: {error}");
    } else {
        eprintln!(
            "INFO hush: model parsed source={} checksum={} compressed_bytes={} decompressed_bytes={} load_ms={}",
            info.source,
            info.checksum,
            info.compressed_bytes,
            info.decompressed_bytes
                .map_or_else(|| "unknown".to_owned(), |bytes| bytes.to_string()),
            info.load_duration_ms
        );
    }
    SharedHushModelLoad { model, info }
}

fn failed_model_load(
    mut info: HushModelLoadInfo,
    started: Instant,
    error: String,
) -> SharedHushModelLoad {
    info.load_duration_ms = elapsed_ms(started);
    info.error = Some(error.clone());
    eprintln!("ERROR hush: {error}");
    SharedHushModelLoad {
        model: Err(error),
        info,
    }
}

fn non_empty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn gzip_uncompressed_size(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < 18 || bytes.get(..2) != Some(&[0x1f, 0x8b]) {
        return None;
    }
    // The gzip ISIZE footer is the input size modulo 2^32. Hush bundles are
    // far below that limit; reporting None for a future larger archive is
    // safer than presenting a wrapped size as exact.
    let size = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into().ok()?);
    Some(u64::from(size))
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

fn validate_model_checksum(bytes: &[u8], source: &str) -> Result<(), String> {
    let checksum = Sha256::digest(bytes);
    let checksum = format!("{checksum:x}");
    if checksum != HUSH_MODEL_SHA256 {
        return Err(format!(
            "Hush model checksum mismatch for {source} (expected {HUSH_MODEL_SHA256}, got {checksum})"
        ));
    }
    Ok(())
}

struct DryDelay {
    samples: Vec<f32>,
    position: usize,
    delay: usize,
}

impl DryDelay {
    fn new(samples: usize) -> Self {
        Self {
            samples: vec![0.0; samples.max(1)],
            position: 0,
            delay: 0,
        }
    }

    fn reset(&mut self) {
        self.samples.fill(0.0);
        self.position = 0;
    }

    fn set_delay(&mut self, delay: usize) {
        let delay = delay.min(self.samples.len());
        if self.delay != delay {
            self.reset();
            self.delay = delay;
        }
    }

    /// Change the logical delay without clearing the history. The ring is
    /// preallocated during prepare(), so quantum-aware latency growth remains
    /// realtime safe and the dry timeline stays continuous during worker-only
    /// recovery.
    fn set_delay_preserve(&mut self, delay: usize) {
        self.delay = delay.min(self.samples.len());
        if self.delay > 0 {
            self.position %= self.samples.len();
        } else {
            self.position = 0;
        }
    }

    fn process(&mut self, input: &[f32], output: &mut [f32]) {
        if self.delay == 0 {
            output.copy_from_slice(input);
            return;
        }
        let capacity = self.samples.len();
        for (sample, delayed) in input.iter().zip(output.iter_mut()) {
            let read = (self.position + capacity - self.delay % capacity) % capacity;
            *delayed = self.samples[read];
            self.samples[self.position] = *sample;
            self.position = (self.position + 1) % capacity;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioSpec, EffectProcessor};
    use std::thread;
    use std::time::{Duration, Instant};

    thread_local! {
        static COUNT_ALLOCATIONS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static ALLOCATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
    struct TestAllocator;
    #[global_allocator]
    static ALLOCATOR: TestAllocator = TestAllocator;
    unsafe impl std::alloc::GlobalAlloc for TestAllocator {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            COUNT_ALLOCATIONS.with(|enabled| {
                if enabled.get() {
                    ALLOCATIONS.with(|n| n.set(n.get() + 1));
                }
            });
            std::alloc::GlobalAlloc::alloc(&std::alloc::System, layout)
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            std::alloc::GlobalAlloc::dealloc(&std::alloc::System, ptr, layout)
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
            COUNT_ALLOCATIONS.with(|enabled| {
                if enabled.get() {
                    ALLOCATIONS.with(|n| n.set(n.get() + 1));
                }
            });
            std::alloc::GlobalAlloc::realloc(&std::alloc::System, ptr, layout, size)
        }
    }

    #[test]
    fn invalid_hush_model_override_reports_path_without_falling_back() {
        let path = PathBuf::from("/nonexistent/qpwgraph-hush-model.tar.gz");
        let loaded = load_hush_model_with_override(Some(("QPWGRAPH_HUSH_MODEL", path.clone())));
        assert!(loaded.model.is_err());
        assert!(!loaded.info.embedded);
        assert_eq!(
            loaded.info.source,
            "environment override (QPWGRAPH_HUSH_MODEL)"
        );
        assert_eq!(
            loaded.info.path.as_deref(),
            Some(path.to_string_lossy().as_ref())
        );
        let error = loaded
            .info
            .error
            .expect("override error should be retained");
        assert!(error.contains(&path.display().to_string()));
        assert!(!error.contains("embedded"));
        assert!(!loaded.info.parse_started);
    }

    #[test]
    fn embedded_hush_model_provenance_is_complete() {
        let loaded = load_hush_model_with_override(None);
        assert!(loaded.model.is_ok());
        let info = loaded.info;
        assert!(info.embedded);
        assert!(info.load_started);
        assert!(info.load_completed);
        assert!(info.parse_started);
        assert!(info.parse_completed);
        assert_eq!(info.checksum, HUSH_MODEL_SHA256);
        assert!(info.compressed_bytes > 0);
        assert!(info.decompressed_bytes.is_some());
    }

    #[test]
    fn callback_allocates_nothing_through_wet_bypass_and_reset() {
        let mut p = HushNoiseSuppressor::default();
        p.prepare(AudioSpec {
            sample_rate: 48_000,
            channels: 2,
            max_frames: 1024,
        })
        .unwrap();
        let mut audio = [0.1; 2048];
        for i in 0..40 {
            let frames = [128, 256, 480, 512, 1024][i % 5];
            ALLOCATIONS.with(|n| n.set(0));
            COUNT_ALLOCATIONS.with(|flag| flag.set(true));
            p.set_host_bypass(i % 3 == 0);
            if i == 20 {
                p.reset();
            }
            let result = p.process(&mut audio[..frames * 2], frames as u32);
            COUNT_ALLOCATIONS.with(|flag| flag.set(false));
            result.unwrap();
            assert_eq!(ALLOCATIONS.with(|n| n.get()), 0);
            p.runtime.as_ref().unwrap().wait_idle();
        }
    }

    #[test]
    fn hush_worker_eventually_publishes_processed_audio() {
        use std::sync::atomic::Ordering::Relaxed;
        for (rate, channels, quantums) in [
            (48_000, 1, vec![128]),
            (48_000, 1, vec![256]),
            (48_000, 1, vec![480]),
            (48_000, 1, vec![512]),
            (48_000, 1, vec![1024]),
            (48_000, 2, vec![256, 256, 128, 128, 512, 256, 480, 128]),
            (44_100, 1, vec![128, 512]),
            (96_000, 2, vec![256, 1024]),
        ] {
            let mut p = HushNoiseSuppressor::default();
            p.prepare(AudioSpec {
                sample_rate: rate,
                channels,
                max_frames: 1024,
            })
            .unwrap();
            let mut dry =
                DryDelay::new((p.scheduling_frames + p.synthesis_delay_frames) * channels as usize);
            dry.set_delay(dry.samples.len());
            let mut changed = false;
            let mut position = 0;
            for index in 0..80 {
                let frames = quantums[index % quantums.len()];
                let mut audio: Vec<f32> = (0..frames)
                    .flat_map(|i| {
                        let t = (position + i) as f32 / rate as f32;
                        let v = 0.2 * (t * 1131.0).sin() + 0.07 * (t * 17321.0).sin();
                        std::iter::repeat_n(v, channels as usize)
                    })
                    .collect();
                let mut reference = vec![0.0; audio.len()];
                dry.process(&audio, &mut reference);
                p.process(&mut audio, frames).unwrap();
                p.runtime.as_ref().unwrap().wait_idle();
                if position > rate / 10 {
                    changed |= audio
                        .iter()
                        .zip(&reference)
                        .any(|(a, b)| (a - b).abs() > 0.01);
                }
                position += frames;
            }
            let d = p.diagnostics.as_ref().unwrap();
            println!("rate={rate} channels={channels} quantums={quantums:?} wet_blocks_output={} dry_fallback_blocks={} underruns={} input_overruns={} output_overruns={}", d.wet_blocks_output.load(Relaxed), d.dry_fallback_blocks.load(Relaxed), d.underruns.load(Relaxed), d.input_overruns.load(Relaxed), d.output_overruns.load(Relaxed));
            assert!(changed, "only aligned dry output at {rate}/{quantums:?}");
            assert!(d.wet_blocks_output.load(Relaxed) > 0);
            assert_eq!(d.underruns.load(Relaxed), 0);
            assert_eq!(d.generation_resets.load(Relaxed), 0);
        }
    }

    #[test]
    fn diagnostics_prove_mono_inference_wet_delivery_and_live_attenuation_updates() {
        use std::sync::atomic::Ordering::{Acquire, Relaxed};

        let mut processor = HushNoiseSuppressor::default();
        processor
            .prepare(AudioSpec {
                sample_rate: 48_000,
                channels: 1,
                max_frames: 256,
            })
            .unwrap();
        let diagnostics = processor.diagnostics.as_ref().unwrap().clone();
        let runtime_identity = std::sync::Arc::as_ptr(&diagnostics);

        for block_index in 0..80 {
            let mut block = (0..256)
                .map(|sample| {
                    let position = block_index * 256 + sample;
                    0.2 * (position as f32 * 0.071).sin() + 0.12 * (position as f32 * 0.913).sin()
                })
                .collect::<Vec<_>>();
            processor.process(&mut block, 256).unwrap();
            processor.runtime.as_ref().unwrap().wait_idle();
        }

        assert!(diagnostics.model_init_started.load(Acquire));
        assert!(diagnostics.model_init_completed.load(Acquire));
        assert!(diagnostics.model_initialized.load(Acquire));
        assert_eq!(diagnostics.channels.load(Relaxed), 1);
        assert_eq!(diagnostics.active_hush_channels.load(Relaxed), 1);
        assert!(diagnostics.hush_frames_processed.load(Relaxed) > 0);
        assert!(diagnostics.hush_channel_frames_processed.load(Relaxed) > 0);
        assert!(diagnostics.wet_frames_output.load(Relaxed) > 0);
        assert!(diagnostics.last_successful_inference_ms.load(Relaxed) > 0);
        let delta_db = f64::from_bits(diagnostics.wet_dry_delta_rms_db_bits.load(Relaxed));
        assert!(delta_db.is_finite() && delta_db > -119.0);
        assert!(diagnostics.status_text().contains("active_channels=1"));

        processor
            .set_parameter(HUSH_NOISE_SUPPRESSOR_REDUCTION, 40.0)
            .unwrap();
        let mut block = vec![0.2; 256];
        processor.process(&mut block, 256).unwrap();
        processor.runtime.as_ref().unwrap().wait_idle();
        assert_eq!(
            f32::from_bits(diagnostics.effective_attenuation_bits.load(Acquire)),
            40.0
        );
        assert_eq!(
            runtime_identity,
            std::sync::Arc::as_ptr(processor.runtime.as_ref().unwrap().diagnostics())
        );
    }

    #[test]
    fn unsupported_hush_channel_layout_fails_during_setup() {
        let mut processor = HushNoiseSuppressor::default();
        let error = processor
            .prepare(AudioSpec {
                sample_rate: 48_000,
                channels: 3,
                max_frames: 256,
            })
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Hush supports one or two channels"));
    }

    #[test]
    fn bypass_and_host_disable_are_aligned_dry_while_wet_stays_active() {
        let spec = AudioSpec {
            sample_rate: 48_000,
            channels: 1,
            max_frames: 256,
        };
        let mut wet = HushNoiseSuppressor::default();
        wet.set_parameter(HUSH_NOISE_SUPPRESSOR_REDUCTION, 40.0)
            .unwrap();
        wet.prepare(spec.clone()).unwrap();

        let mut parameter_bypass = HushNoiseSuppressor::default();
        parameter_bypass
            .set_parameter(HUSH_NOISE_SUPPRESSOR_REDUCTION, 40.0)
            .unwrap();
        parameter_bypass.prepare(spec.clone()).unwrap();
        parameter_bypass
            .set_parameter(HUSH_NOISE_SUPPRESSOR_BYPASS, 1.0)
            .unwrap();

        let mut host_disabled = HushNoiseSuppressor::default();
        host_disabled
            .set_parameter(HUSH_NOISE_SUPPRESSOR_REDUCTION, 40.0)
            .unwrap();
        host_disabled.prepare(spec).unwrap();
        assert!(host_disabled.set_host_bypass(true));

        let mut dry = DryDelay::new(wet.scheduling_frames + wet.synthesis_delay_frames);
        dry.set_delay(dry.samples.len());
        let mut wet_changed = false;
        for block_index in 0..70 {
            let input: Vec<f32> = (0..256)
                .map(|frame| {
                    let sample = block_index * 256 + frame;
                    0.18 * (sample as f32 * 0.071).sin() + 0.11 * (sample as f32 * 0.913).sin()
                })
                .collect();
            let mut expected = vec![0.0; input.len()];
            dry.process(&input, &mut expected);
            let mut wet_block = input.clone();
            let mut bypass_block = input.clone();
            let mut disabled_block = input.clone();
            wet.process(&mut wet_block, 256).unwrap();
            parameter_bypass.process(&mut bypass_block, 256).unwrap();
            host_disabled.process(&mut disabled_block, 256).unwrap();
            wet.runtime.as_ref().unwrap().wait_idle();
            parameter_bypass.runtime.as_ref().unwrap().wait_idle();
            host_disabled.runtime.as_ref().unwrap().wait_idle();
            assert_eq!(bypass_block, expected, "parameter bypass lost alignment");
            assert_eq!(disabled_block, expected, "host disable lost alignment");
            if block_index > 20 {
                wet_changed |= wet_block
                    .iter()
                    .zip(&expected)
                    .any(|(actual, dry)| (actual - dry).abs() > 0.01);
            }
        }
        assert!(wet_changed, "bypass OFF never emitted wet Hush audio");
        assert_ne!(
            parameter_bypass.diagnostics.as_ref().unwrap().health(),
            crate::hush_worker::HushHealth::Overloaded,
            "manual bypass must not be reported as CPU overload"
        );
        assert_ne!(
            host_disabled.diagnostics.as_ref().unwrap().health(),
            crate::hush_worker::HushHealth::Overloaded,
            "host disable must not be reported as CPU overload"
        );

        assert!(host_disabled.set_host_bypass(false));
        let deadline = Instant::now() + Duration::from_secs(30);
        while host_disabled
            .diagnostics
            .as_ref()
            .unwrap()
            .resync_completed
            .load(std::sync::atomic::Ordering::Acquire)
            == 0
        {
            let mut block = vec![0.2; 256];
            let mut expected = vec![0.0; 256];
            dry.process(&vec![0.2; 256], &mut expected);
            host_disabled.process(&mut block, 256).unwrap();
            assert!(Instant::now() < deadline, "host re-enable reset timed out");
            thread::yield_now();
        }
        host_disabled.runtime.as_ref().unwrap().wait_idle();
        let mut resumed_wet = false;
        for block_index in 0..30 {
            let mut block = vec![0.2; 256];
            let mut expected = vec![0.0; 256];
            dry.process(&vec![0.2; 256], &mut expected);
            host_disabled.process(&mut block, 256).unwrap();
            host_disabled.runtime.as_ref().unwrap().wait_idle();
            if block_index > 8 {
                resumed_wet |= block
                    .iter()
                    .zip(&expected)
                    .any(|(actual, dry)| (actual - dry).abs() > 0.01);
            }
        }
        assert!(
            resumed_wet,
            "wet output did not resume after host re-enable"
        );
    }

    #[test]
    fn worker_adapter_matches_direct_hush_and_resampler_reference() {
        use nnnoiseless::{HushDenoiser, Resampler, HUSH_FRAME_SIZE};
        let rate = 44_100;
        let input: Vec<f32> = (0..16_000)
            .map(|i| {
                if i == 2000 {
                    0.8
                } else {
                    0.15 * (i as f32 * 0.031).sin() + 0.05 * (i as f32 * 0.377).sin()
                }
            })
            .collect();
        let model = shared_hush_model().unwrap();
        let mut direct: HushDenoiser = model.denoiser_with_attenuation_db(25.0).unwrap();
        let mut down = Resampler::new(rate as f64, 16_000.0, 1);
        let mut up = Resampler::new(16_000.0, rate as f64, 1);
        let mut native = Vec::new();
        down.process(&input, &mut native);
        let mut reference = Vec::new();
        for frame in native.chunks_exact(HUSH_FRAME_SIZE) {
            let mut out = [0.0; HUSH_FRAME_SIZE];
            direct.process_frame(&mut out, frame).unwrap();
            up.process(&out, &mut reference);
        }
        let mut p = HushNoiseSuppressor::default();
        p.prepare(AudioSpec {
            sample_rate: rate,
            channels: 1,
            max_frames: 512,
        })
        .unwrap();
        let mut position = 0;
        for frames in [256usize, 128, 512, 480].into_iter().cycle() {
            if position + frames > input.len() {
                break;
            }
            let mut block = input[position..position + frames].to_vec();
            p.process(&mut block, frames as u32).unwrap();
            p.runtime.as_ref().unwrap().wait_idle();
            if position >= p.scheduling_frames + p.synthesis_delay_frames {
                let start = position - p.scheduling_frames;
                for (actual, expected) in block.iter().zip(&reference[start..start + frames]) {
                    assert!(
                        (actual - expected).abs() < 2e-5,
                        "direct={expected}, adapter={actual}, position={position}"
                    );
                }
            }
            position += frames;
        }
    }

    #[test]
    fn temporarily_late_worker_recovers_and_failure_keeps_aligned_dry() {
        use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
        let mut p = HushNoiseSuppressor::default();
        p.prepare(AudioSpec {
            sample_rate: 48_000,
            channels: 1,
            max_frames: 128,
        })
        .unwrap();
        let d = p.diagnostics.as_ref().unwrap().clone();
        d.paused.store(true, Release);
        // 64 ms of backlog: later than a callback and later than playout,
        // but still inside the bounded catch-up window.
        for _ in 0..24 {
            p.process(&mut [0.2; 128], 128).unwrap();
        }
        assert_eq!(d.wet_blocks_output.load(Relaxed), 0);
        assert!(d.underruns.load(Relaxed) > 0);
        d.paused.store(false, Release);
        p.runtime.as_ref().unwrap().wait_idle();
        for _ in 0..30 {
            p.process(&mut [0.2; 128], 128).unwrap();
            p.runtime.as_ref().unwrap().wait_idle();
        }
        assert!(d.wet_blocks_output.load(Relaxed) > 0);
        // A worker inference taking 8 ms is still usable with a 2.67 ms
        // callback. Synchronize publication, without wall-clock deadlines.
        d.frame_delay_ms.store(8, Release);
        let wet_before = d.wet_blocks_output.load(Relaxed);
        for _ in 0..20 {
            p.process(&mut [0.2; 128], 128).unwrap();
            p.runtime.as_ref().unwrap().wait_idle();
        }
        assert!(d.wet_blocks_output.load(Relaxed) > wet_before);
        d.inject_error.store(true, Release);
        p.process(&mut [0.2; 128], 128).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !d.worker_failed.load(Acquire) {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        let mut audio = [0.2; 128];
        p.process(&mut audio, 128).unwrap();
        assert_eq!(audio, [0.2; 128]);
        assert!(p.has_failed());
        assert!(d.status_text().contains("injected inference failure"));
        p.set_channel_mask(0);
        p.process(&mut audio, 128).unwrap();
        assert_eq!(audio, [0.0; 128]);
        assert!(d.worker_failed.load(Acquire));
    }

    #[test]
    fn backlog_and_queue_overflow_resynchronize_without_latency_drift() {
        use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
        let mut p = HushNoiseSuppressor::default();
        p.prepare(AudioSpec {
            sample_rate: 48_000,
            channels: 1,
            max_frames: 128,
        })
        .unwrap();
        let d = p.diagnostics.as_ref().unwrap().clone();
        d.paused.store(true, Release);
        for _ in 0..160 {
            p.process(&mut [0.2; 128], 128).unwrap();
        }
        assert!(d.input_overruns.load(Relaxed) > 0);
        d.paused.store(false, Release);
        let deadline = Instant::now() + Duration::from_secs(30);
        while d.resync_completed.load(Acquire) == 0 {
            assert!(Instant::now() < deadline, "worker resync did not complete");
            thread::yield_now();
        }
        p.runtime.as_ref().unwrap().wait_idle();
        // First fresh input triggers the off-RT state reset after the gap.
        for _ in 0..50 {
            p.process(&mut [0.2; 128], 128).unwrap();
            p.runtime.as_ref().unwrap().wait_idle();
        }
        assert!(d.wet_blocks_output.load(Relaxed) > 0);
        assert!(d.resync_completed.load(Relaxed) > 0);
        assert!(d.stale_input_blocks_dropped.load(Relaxed) > 0);
        assert_eq!(d.generation_resets.load(Relaxed), 0);
        assert!(d.max_input_queue_depth.load(Relaxed) <= 128);
    }

    #[test]
    fn partial_enqueue_failure_requests_one_resync_and_recovers() {
        use crate::hush_worker::HUSH_QUEUE_CAPACITY;
        use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
        let mut p = HushNoiseSuppressor::default();
        p.prepare(AudioSpec {
            sample_rate: 48_000,
            channels: 1,
            max_frames: 2_048,
        })
        .unwrap();
        let d = p.diagnostics.as_ref().unwrap().clone();
        d.paused.store(true, Release);
        for _ in 0..128 {
            p.process(&mut vec![0.2; 480], 480).unwrap();
        }
        // A large host callback is split into bounded worker slots. With the
        // queue full, a partial enqueue must invalidate the wet epoch rather
        // than leave a silent hole in its timeline.
        p.process(&mut vec![0.2; 2_048], 2_048).unwrap();
        assert!(d.input_frames_rejected.load(Relaxed) > 0);
        assert_eq!(d.resync_requests.load(Relaxed), 1);
        d.paused.store(false, Release);
        let deadline = Instant::now() + Duration::from_secs(30);
        while d.resync_completed.load(Acquire) == 0 {
            assert!(
                Instant::now() < deadline,
                "partial enqueue resync did not complete"
            );
            thread::yield_now();
        }
        for _ in 0..60 {
            p.process(&mut [0.2; 480], 480).unwrap();
            p.runtime.as_ref().unwrap().wait_idle();
        }
        assert!(d.wet_frames_output.load(Relaxed) > 0);
        assert!(d.max_input_queue_depth.load(Relaxed) <= HUSH_QUEUE_CAPACITY as u64);
    }

    #[test]
    fn repeated_overloads_resynchronize_without_restarting_worker() {
        use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
        let mut p = HushNoiseSuppressor::default();
        p.prepare(AudioSpec {
            sample_rate: 48_000,
            channels: 1,
            max_frames: 128,
        })
        .unwrap();
        let d = p.diagnostics.as_ref().unwrap().clone();
        for expected_resync in 1..=2 {
            d.paused.store(true, Release);
            for _ in 0..160 {
                p.process(&mut [0.2; 128], 128).unwrap();
            }
            d.paused.store(false, Release);
            let deadline = Instant::now() + Duration::from_secs(30);
            while d.resync_completed.load(Acquire) < expected_resync {
                assert!(Instant::now() < deadline, "worker resync did not complete");
                thread::yield_now();
            }
            for _ in 0..45 {
                p.process(&mut [0.2; 128], 128).unwrap();
                p.runtime.as_ref().unwrap().wait_idle();
            }
        }
        assert_eq!(d.resync_requests.load(Relaxed), 2);
        assert_eq!(d.resync_completed.load(Relaxed), 2);
        assert!(d.wet_frames_output.load(Relaxed) > 0);
        assert!(!d.worker_failed.load(Acquire));
    }

    #[test]
    fn permanently_slow_worker_stays_bounded_and_uses_dry_fallback() {
        use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
        let mut p = HushNoiseSuppressor::default();
        p.prepare(AudioSpec {
            sample_rate: 48_000,
            channels: 1,
            max_frames: 128,
        })
        .unwrap();
        let d = p.diagnostics.as_ref().unwrap().clone();
        // Prime a healthy worker first. The following phase then slows each
        // native inference to a sustained factor above realtime, allowing the
        // persistent estimator—not merely queue overflow—to classify it.
        for _ in 0..24 {
            p.process(&mut [0.2; 128], 128).unwrap();
            p.runtime.as_ref().unwrap().wait_idle();
        }
        // A 160-sample native frame represents 10 ms of 16 kHz audio. Add
        // enough artificial work to make the sustained service rate clearly
        // slower than realtime even on a fast release-build host.
        d.frame_delay_ms.store(14, Release);
        let overload_deadline = Instant::now() + Duration::from_secs(30);
        while d.worker_state.load(Acquire)
            != crate::hush_worker::HushWorkerState::Overloaded.as_u32()
        {
            p.process(&mut [0.2; 128], 128).unwrap();
            assert!(
                Instant::now() < overload_deadline,
                "slow worker never became overloaded"
            );
            thread::sleep(Duration::from_micros(2_700));
        }
        p.process(&mut [0.2; 128], 128).unwrap();
        assert!(d.input_blocks_skipped_overloaded.load(Relaxed) > 0);
        assert!(d.dry_frames_output.load(Relaxed) > 0);
        assert!(d.max_input_queue_depth.load(Relaxed) <= 128);
        assert!(!d.worker_failed.load(Acquire));
        assert_eq!(
            d.overload_reason.load(Acquire),
            crate::hush_worker::HushOverloadReason::CpuTooSlow as u32
        );
        assert!(
            d.resync_requests.load(Relaxed)
                <= 1 + crate::hush_worker::HUSH_MAX_AUTOMATIC_RETRIES as u64,
            "automatic overload retries must remain bounded"
        );
        assert_eq!(
            d.resync_completed.load(Relaxed),
            d.resync_requests.load(Relaxed)
        );
    }

    #[test]
    fn worker_resync_warmup_preserves_connected_aligned_dry() {
        let mut p = HushNoiseSuppressor::default();
        p.set_parameter(HUSH_NOISE_SUPPRESSOR_REDUCTION, 0.02)
            .unwrap();
        p.prepare(AudioSpec {
            sample_rate: 48_000,
            channels: 1,
            max_frames: 128,
        })
        .unwrap();
        for _ in 0..40 {
            p.process(&mut [0.2; 128], 128).unwrap();
            p.runtime.as_ref().unwrap().wait_idle();
        }
        // Model a real missing input range without clearing the RT dry ring.
        p.input_frame_position += 128;
        for _ in 0..40 {
            let mut audio = [0.2; 128];
            p.process(&mut audio, 128).unwrap();
            p.runtime.as_ref().unwrap().wait_idle();
            assert!(
                audio.iter().all(|sample| *sample > 0.05),
                "synthesis warmup replaced valid dry audio with silence"
            );
        }
    }

    #[test]
    fn reset_that_outlasts_backlog_does_not_trigger_reset_storm() {
        use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
        let mut p = HushNoiseSuppressor::default();
        p.prepare(AudioSpec {
            sample_rate: 48_000,
            channels: 1,
            max_frames: 128,
        })
        .unwrap();
        let d = p.diagnostics.as_ref().unwrap().clone();
        d.reset_paused.store(true, Release);
        p.reset();
        p.process(&mut [0.2; 128], 128).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while d.recovery_attempts.load(Acquire) == 0 {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        // Stream time advances by more than the maximum backlog while the
        // worker rebuild is held. This deterministically models a slow reset.
        for _ in 0..80 {
            p.process(&mut [0.2; 128], 128).unwrap();
        }
        d.reset_paused.store(false, Release);
        let deadline = Instant::now() + Duration::from_secs(30);
        while d.resync_completed.load(Acquire) == 0 {
            assert!(Instant::now() < deadline, "worker resync did not complete");
            thread::yield_now();
        }
        p.runtime.as_ref().unwrap().wait_idle();
        for _ in 0..40 {
            p.process(&mut [0.2; 128], 128).unwrap();
            p.runtime.as_ref().unwrap().wait_idle();
        }
        assert_eq!(d.generation_resets.load(Relaxed), 1);
        assert!(d.wet_blocks_output.load(Relaxed) > 0);
    }

    #[test]
    fn sinc_roundtrip_impulse_is_centered_without_extra_signal_delay() {
        use nnnoiseless::Resampler;
        for rate in [16_000, 44_100, 48_000, 96_000] {
            let mut input = vec![0.0; rate / 10];
            let impulse = rate / 20;
            input[impulse] = 1.0;
            let mut down = Resampler::new(rate as f64, 16_000.0, 1);
            let mut up = Resampler::new(16_000.0, rate as f64, 1);
            let mut native = Vec::new();
            let mut host = Vec::new();
            for chunk in input.chunks(128) {
                down.process(chunk, &mut native);
            }
            for chunk in native.chunks(160) {
                up.process(chunk, &mut host);
            }
            let peak = host
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                .unwrap()
                .0;
            assert!(
                peak.abs_diff(impulse) <= 1,
                "rate={rate} peak={peak} expected={impulse}"
            );
            let mut dry = DryDelay::new(rate / 20);
            dry.set_delay(rate / 20);
            let mut out = vec![0.0; input.len()];
            dry.process(&input, &mut out);
            assert_eq!(out.iter().position(|s| *s != 0.0), None); // impulse arrives at the next block boundary
            dry.process(&[0.0], &mut out[..1]);
            assert_eq!(out[0], 1.0);
        }
    }

    #[test]
    fn embedded_model_worker_preserves_silence_and_sanitizes_input() {
        let mut processor = HushNoiseSuppressor::default();
        processor
            .prepare(AudioSpec {
                sample_rate: 48_000,
                channels: 2,
                max_frames: 480,
            })
            .expect("pinned Hush model should prepare");
        for _ in 0..24 {
            let mut silence = vec![0.0; 960];
            processor.process(&mut silence, 480).unwrap();
            assert!(silence.iter().all(|sample| *sample == 0.0));
        }
        let mut invalid = vec![f32::NAN, f32::INFINITY, -f32::INFINITY, 0.0];
        processor.process(&mut invalid, 2).unwrap();
        assert!(invalid.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn realtime_process_keeps_prepared_buffer_capacities() {
        let mut processor = HushNoiseSuppressor::default();
        processor
            .prepare(AudioSpec {
                sample_rate: 48_000,
                channels: 2,
                max_frames: 480,
            })
            .unwrap();
        let fallback_capacity = processor.fallback.capacity();
        let wet_capacity = processor.wet.capacity();
        let delay_capacity = processor
            .dry_delay
            .as_ref()
            .expect("prepared dry delay")
            .samples
            .capacity();
        for _ in 0..16 {
            let mut block = vec![0.0; 960];
            processor.process(&mut block, 480).unwrap();
        }
        assert_eq!(processor.fallback.capacity(), fallback_capacity);
        assert_eq!(processor.wet.capacity(), wet_capacity);
        assert_eq!(
            processor
                .dry_delay
                .as_ref()
                .expect("prepared dry delay")
                .samples
                .capacity(),
            delay_capacity
        );
    }

    #[test]
    fn disconnect_generation_forces_both_channels_to_silence() {
        let mut processor = HushNoiseSuppressor::default();
        processor
            .prepare(AudioSpec {
                sample_rate: 16_000,
                channels: 2,
                max_frames: 160,
            })
            .unwrap();
        processor.set_channel_mask(1);
        let mut block = vec![0.0; 320];
        for frame in 0..160 {
            block[frame * 2] = 0.25;
            block[frame * 2 + 1] = -0.75;
        }
        processor.process(&mut block, 160).unwrap();
        assert!(block.chunks_exact(2).all(|stereo| stereo[1] == 0.0));
        processor.process(&mut block, 160).unwrap();
        assert!(block.chunks_exact(2).all(|stereo| stereo[1] == 0.0));
    }

    #[test]
    fn mono_and_partial_stereo_routes_only_run_connected_hush_channels() {
        use std::sync::atomic::Ordering::Relaxed;

        let mut mono = HushNoiseSuppressor::default();
        mono.prepare(AudioSpec {
            sample_rate: 48_000,
            channels: 1,
            max_frames: 480,
        })
        .unwrap();
        for _ in 0..12 {
            mono.process(&mut [0.2; 480], 480).unwrap();
            mono.runtime.as_ref().unwrap().wait_idle();
        }
        let mono_diagnostics = mono.diagnostics.as_ref().unwrap();
        assert_eq!(mono_diagnostics.active_hush_channels.load(Relaxed), 1);
        assert!(mono_diagnostics.hush_frames_processed.load(Relaxed) > 0);

        let mut partial = HushNoiseSuppressor::default();
        partial
            .prepare(AudioSpec {
                sample_rate: 48_000,
                channels: 2,
                max_frames: 480,
            })
            .unwrap();
        partial.set_channel_mask(1);
        let diagnostics = partial.diagnostics.as_ref().unwrap().clone();
        let reset_deadline = Instant::now() + Duration::from_secs(90);
        while diagnostics
            .resync_completed
            .load(std::sync::atomic::Ordering::Acquire)
            == 0
        {
            assert!(
                Instant::now() < reset_deadline,
                "mono channel-mask reset did not complete"
            );
            thread::yield_now();
        }
        for _ in 0..12 {
            let mut block = vec![0.2; 960];
            partial.process(&mut block, 480).unwrap();
            partial.runtime.as_ref().unwrap().wait_idle();
            assert!(block.chunks_exact(2).all(|stereo| stereo[1] == 0.0));
        }
        assert_eq!(diagnostics.active_hush_channels.load(Relaxed), 1);
        // `hush_frames_processed` counts native inference calls. With the
        // right input disconnected, it must not double as it would for two
        // independent stereo denoisers.
        assert!(diagnostics.hush_frames_processed.load(Relaxed) >= 8);
        assert_eq!(
            diagnostics.hush_channel_frames_processed.load(Relaxed),
            diagnostics.hush_frames_processed.load(Relaxed) * 160
        );
    }

    #[test]
    fn bypass_uses_fixed_fifty_ms_latency() {
        let mut processor = HushNoiseSuppressor::default();
        processor
            .prepare(AudioSpec {
                sample_rate: 16_000,
                channels: 1,
                max_frames: 1_024,
            })
            .unwrap();
        processor
            .set_parameter(HUSH_NOISE_SUPPRESSOR_BYPASS, 1.0)
            .unwrap();

        let mut first = vec![0.25; 160];
        processor.process(&mut first, 160).unwrap();
        assert!(first.iter().all(|sample| *sample == 0.0));

        let mut second = vec![0.5; 160];
        processor.process(&mut second, 160).unwrap();
        assert!(second.iter().all(|sample| *sample == 0.0));

        for _ in 0..3 {
            let mut warmup = vec![0.75; 160];
            processor.process(&mut warmup, 160).unwrap();
            assert!(warmup.iter().all(|sample| *sample == 0.0));
        }
        let mut third = vec![0.75; 160];
        processor.process(&mut third, 160).unwrap();
        assert!(third.iter().all(|sample| (*sample - 0.25).abs() < 1e-6));
    }

    #[test]
    fn zero_reduction_uses_fixed_fifty_ms_latency() {
        let mut processor = HushNoiseSuppressor::default();
        processor
            .prepare(AudioSpec {
                sample_rate: 16_000,
                channels: 1,
                max_frames: 1_024,
            })
            .unwrap();
        processor
            .diagnostics
            .as_ref()
            .unwrap()
            .set_control_parameter(HUSH_NOISE_SUPPRESSOR_REDUCTION, 0.0)
            .unwrap();

        let mut first = vec![0.25; 160];
        processor.process(&mut first, 160).unwrap();
        assert!(first.iter().all(|sample| *sample == 0.0));

        let mut second = vec![0.5; 160];
        processor.process(&mut second, 160).unwrap();
        assert!(second.iter().all(|sample| *sample == 0.0));

        for _ in 0..3 {
            let mut warmup = vec![0.75; 160];
            processor.process(&mut warmup, 160).unwrap();
            assert!(warmup.iter().all(|sample| *sample == 0.0));
        }
        let mut third = vec![0.75; 160];
        processor.process(&mut third, 160).unwrap();
        assert!(third.iter().all(|sample| (*sample - 0.25).abs() < 1e-6));
    }

    #[test]
    fn large_quantum_grows_scheduling_before_wet_read() {
        let mut processor = HushNoiseSuppressor::default();
        processor
            .set_parameter(HUSH_NOISE_SUPPRESSOR_REDUCTION, 40.0)
            .unwrap();
        processor
            .prepare(AudioSpec {
                sample_rate: 48_000,
                channels: 2,
                max_frames: 16_384,
            })
            .unwrap();
        let mut block = vec![0.1; 2_048 * 2];
        processor.process(&mut block, 2_048).unwrap();
        assert!(processor.scheduling_frames >= 2_048);
        let diagnostics = processor.diagnostics.as_ref().unwrap();
        assert!(
            diagnostics
                .scheduling_frames
                .load(std::sync::atomic::Ordering::Acquire)
                >= 2_048
        );
        assert!(
            diagnostics
                .max_host_quantum
                .load(std::sync::atomic::Ordering::Acquire)
                >= 2_048
        );
        assert!(
            diagnostics
                .total_latency_frames
                .load(std::sync::atomic::Ordering::Acquire)
                >= processor.scheduling_frames as u64 + processor.synthesis_delay_frames as u64
        );
    }

    #[test]
    fn quantum_latency_only_grows_during_variable_callbacks() {
        let mut processor = HushNoiseSuppressor::default();
        processor
            .prepare(AudioSpec {
                sample_rate: 48_000,
                channels: 1,
                max_frames: 2_048,
            })
            .unwrap();
        let mut block = vec![0.1; 256];
        processor.process(&mut block, 256).unwrap();
        let initial = processor.scheduling_frames;
        let mut large = vec![0.1; 2_048];
        processor.process(&mut large, 2_048).unwrap();
        let grown = processor.scheduling_frames;
        assert!(grown >= 2_048);
        let mut small = vec![0.1; 128];
        processor.process(&mut small, 128).unwrap();
        assert!(processor.scheduling_frames >= grown);
        assert!(grown >= initial);
    }

    #[test]
    fn wet_request_never_includes_the_callback_just_submitted() {
        for callback_start in [0, 1, 511, 512, 4_096, 48_000] {
            for frames in [1, 128, 512, 2_048] {
                for schedule in [40, 512, 2_048, 4_096] {
                    let schedule = schedule.max(frames as usize);
                    let (start, requested, _) = safe_wet_request(callback_start, frames, schedule);
                    assert!(
                        start + requested as u64 <= callback_start,
                        "callback_start={callback_start} frames={frames} schedule={schedule}"
                    );
                }
            }
        }
    }

    #[test]
    #[ignore = "30-second release real-time throughput measurement"]
    fn sustained_48k_stereo_256_keeps_wet_audio_dominant() {
        if cfg!(debug_assertions) {
            eprintln!("run this sustained throughput check in release mode");
            return;
        }
        use std::sync::atomic::Ordering::Relaxed;
        let mut processor = HushNoiseSuppressor::default();
        processor
            .set_parameter(HUSH_NOISE_SUPPRESSOR_REDUCTION, 40.0)
            .unwrap();
        processor
            .prepare(AudioSpec {
                sample_rate: 48_000,
                channels: 2,
                max_frames: 256,
            })
            .unwrap();
        let callbacks = 30 * 48_000 / 256;
        let period = Duration::from_secs_f64(256.0 / 48_000.0);
        let started = Instant::now();
        for index in 0..callbacks {
            let mut block = vec![0.0; 512];
            for frame in 0..256 {
                let sample = ((index * 256 + frame) as f32 * 0.071).sin() * 0.18
                    + ((index * 256 + frame) as f32 * 0.913).sin() * 0.11;
                block[frame * 2] = sample;
                block[frame * 2 + 1] = sample;
            }
            processor.process(&mut block, 256).unwrap();
            let next = started + period * (index as u32 + 1);
            if let Some(remaining) = next.checked_duration_since(Instant::now()) {
                thread::sleep(remaining);
            }
        }
        processor.runtime.as_ref().unwrap().wait_idle();
        let diagnostics = processor.diagnostics.as_ref().unwrap();
        let wet = diagnostics.wet_frames_output.load(Relaxed);
        let dry = diagnostics.dry_frames_output.load(Relaxed);
        let ratio = wet as f64 / (wet + dry).max(1) as f64;
        println!(
            "sustained 48k/stereo/256: wet_frames={wet} dry_frames={dry} wet_ratio={:.3} input_overruns={} output_overruns={} underruns={} queue_peak={} resync={}/{}",
            ratio,
            diagnostics.input_overruns.load(Relaxed),
            diagnostics.output_overruns.load(Relaxed),
            diagnostics.underruns.load(Relaxed),
            diagnostics.max_input_queue_depth.load(Relaxed),
            diagnostics.resync_requests.load(Relaxed),
            diagnostics.resync_completed.load(Relaxed),
        );
        assert!(ratio > 0.99);
        assert_eq!(diagnostics.input_overruns.load(Relaxed), 0);
        assert_eq!(diagnostics.output_overruns.load(Relaxed), 0);
    }

    #[test]
    #[ignore = "manual Hush performance measurement"]
    fn benchmark_hush_rates() {
        let cases = [
            (16_000, 1, 160),
            (44_100, 1, 441),
            (48_000, 1, 480),
            (48_000, 2, 480),
            (96_000, 1, 960),
            (96_000, 2, 960),
            (48_000, 1, 64),
            (48_000, 1, 128),
            (48_000, 1, 256),
            (48_000, 1, 512),
            (48_000, 1, 1024),
            (48_000, 2, 64),
            (48_000, 2, 128),
            (48_000, 2, 256),
            (48_000, 2, 512),
            (48_000, 2, 1024),
            (48_000, 1, 2048),
            (48_000, 2, 2048),
            (96_000, 1, 256),
            (96_000, 1, 480),
            (96_000, 1, 1024),
            (96_000, 2, 256),
            (96_000, 2, 480),
            (96_000, 2, 1024),
        ];
        for (sample_rate, channels, frames) in cases {
            let mut processor = HushNoiseSuppressor::default();
            processor
                .prepare(AudioSpec {
                    sample_rate,
                    channels,
                    max_frames: frames,
                })
                .expect("embedded Hush model should prepare");
            let diagnostics = processor
                .diagnostics
                .as_ref()
                .expect("worker diagnostics")
                .clone();
            for _ in 0..500 {
                if diagnostics
                    .worker_ready
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            let mut callback_us = Vec::with_capacity(120);
            for block_index in 0..120 {
                let mut block = vec![0.0; frames as usize * channels as usize];
                for frame in 0..frames as usize {
                    let sample = (2.0
                        * std::f32::consts::PI
                        * 180.0
                        * (block_index * frames as usize + frame) as f32
                        / sample_rate as f32)
                        .sin()
                        * 0.1;
                    for channel in 0..channels as usize {
                        block[frame * channels as usize + channel] = sample;
                    }
                }
                let started = Instant::now();
                processor.process(&mut block, frames).unwrap();
                callback_us.push(started.elapsed().as_micros());
                thread::sleep(Duration::from_secs_f64(frames as f64 / sample_rate as f64));
            }
            // Keep consuming the delayed output while the worker drains the
            // final input. Ending the producer immediately would make the
            // output queue look full even though a real host callback keeps
            // reading it at the configured cadence.
            let tail_callbacks = (sample_rate / frames * 100 / 1000).max(1);
            for _ in 0..tail_callbacks {
                let mut tail = vec![0.0; frames as usize * channels as usize];
                processor.process(&mut tail, frames).unwrap();
                thread::sleep(Duration::from_secs_f64(frames as f64 / sample_rate as f64));
            }
            processor.runtime.as_ref().unwrap().wait_idle();
            callback_us.sort_unstable();
            let processed = diagnostics
                .processed_blocks
                .load(std::sync::atomic::Ordering::Relaxed);
            let average = callback_us.iter().sum::<u128>() as f64 / callback_us.len() as f64;
            let p95 = callback_us[callback_us.len() * 95 / 100];
            let p99 = callback_us[callback_us.len() * 99 / 100];
            let inference_total = diagnostics
                .inference_ns
                .load(std::sync::atomic::Ordering::Relaxed);
            let inference_average = if processed == 0 {
                0.0
            } else {
                inference_total as f64 / processed as f64 / 1_000.0
            };
            let inference_max = diagnostics
                .max_inference_ns
                .load(std::sync::atomic::Ordering::Relaxed)
                / 1_000;
            let mut timings: Vec<u64> = diagnostics
                .timings
                .lock()
                .unwrap()
                .iter()
                .copied()
                .collect();
            timings.sort_unstable();
            let percentile =
                |pct: usize| timings.get(timings.len() * pct / 100).copied().unwrap_or(0) / 1000;
            let wet_frames = diagnostics
                .wet_frames_output
                .load(std::sync::atomic::Ordering::Relaxed);
            let dry_frames = diagnostics
                .dry_frames_output
                .load(std::sync::atomic::Ordering::Relaxed);
            let latency_frames = diagnostics
                .total_latency_frames
                .load(std::sync::atomic::Ordering::Relaxed);
            let realtime_factor = f64::from_bits(
                diagnostics
                    .realtime_factor_bits
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
            println!("rate={sample_rate} channels={channels} quantum={frames} callback_audio_ms={:.3} latency_ms={:.3} worker_p95_us={} worker_p99_us={} wet_blocks_output={} dry_fallback_blocks={} wet_frames_output={} dry_frames_output={} wet_ratio={:.3} resync={}/{} stale_input/wet={}/{} lifetime_rt_factor={:.3} ewma_rt_factor={:.3} input_frames_rejected={} skipped_overload={} queue_overruns={} output_overruns={}", frames as f64 * 1000.0 / sample_rate as f64, latency_frames as f64 * 1000.0 / sample_rate as f64, percentile(95), percentile(99), diagnostics.wet_blocks_output.load(std::sync::atomic::Ordering::Relaxed), diagnostics.dry_fallback_blocks.load(std::sync::atomic::Ordering::Relaxed), wet_frames, dry_frames, wet_frames as f64 / (wet_frames + dry_frames).max(1) as f64, diagnostics.resync_requests.load(std::sync::atomic::Ordering::Relaxed), diagnostics.resync_completed.load(std::sync::atomic::Ordering::Relaxed), diagnostics.stale_input_blocks_dropped.load(std::sync::atomic::Ordering::Relaxed), diagnostics.stale_output_blocks_dropped.load(std::sync::atomic::Ordering::Relaxed), realtime_factor, f64::from_bits(diagnostics.ewma_realtime_factor_bits.load(std::sync::atomic::Ordering::Relaxed)), diagnostics.input_frames_rejected.load(std::sync::atomic::Ordering::Relaxed), diagnostics.input_blocks_skipped_overloaded.load(std::sync::atomic::Ordering::Relaxed), diagnostics.input_queue_overruns.load(std::sync::atomic::Ordering::Relaxed), diagnostics.output_overruns.load(std::sync::atomic::Ordering::Relaxed));
            println!(
                "rate={sample_rate} channels={channels} quantum={frames} callback_avg_us={average:.2} callback_p95_us={p95} callback_p99_us={p99} input_blocks={processed} hush_inferences={} hush_channel_samples={} active_hush_channels={} worker_avg_us={inference_average:.2} worker_max_us={inference_max} stable_p95_us={} stable_p99_us={} max_input_queue_depth={} max_output_queue_depth={} max_backlog_frames={} underruns={} input_overruns={} input_queue_overruns={} output_overruns={} input_blocks_pushed={} input_blocks_rejected={} skipped_recovery={} skipped_overloaded={} input_frames_rejected={} resets={} recovery_attempts={} recovery_successes={}",
                diagnostics
                    .hush_frames_processed
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .hush_channel_frames_processed
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics.active_hush_channels.load(std::sync::atomic::Ordering::Relaxed),
                diagnostics.stable_worker_p95_ns.load(std::sync::atomic::Ordering::Relaxed) / 1_000,
                diagnostics.stable_worker_p99_ns.load(std::sync::atomic::Ordering::Relaxed) / 1_000,
                diagnostics
                    .max_input_queue_depth
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .max_output_queue_depth
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .max_worker_backlog_frames
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .underruns
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .input_overruns
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .input_queue_overruns
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .output_overruns
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .input_blocks_pushed
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .input_blocks_rejected
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .input_blocks_skipped_recovery
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .input_blocks_skipped_overloaded
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .input_frames_rejected
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .generation_resets
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .recovery_attempts
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .recovery_successes
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
        }
    }

    #[test]
    #[ignore = "manual release baseline for native nnnoiseless Hush"]
    fn benchmark_direct_hush_rates() {
        use nnnoiseless::{HushDenoiser, HUSH_FRAME_SIZE};

        if cfg!(debug_assertions) {
            eprintln!("run this native Hush baseline in release mode");
            return;
        }
        let model = shared_hush_model().expect("embedded Hush model should prepare");
        let input_frames: Vec<[f32; HUSH_FRAME_SIZE]> = (0..320)
            .map(|frame_index| {
                std::array::from_fn(|sample_index| {
                    let sample = frame_index * HUSH_FRAME_SIZE + sample_index;
                    0.18 * (sample as f32 * 0.071).sin() + 0.11 * (sample as f32 * 0.913).sin()
                })
            })
            .collect();
        for channels in [1_usize, 2] {
            for attenuation_db in [6.0_f32, 25.0, 40.0] {
                for native_frames_per_block in [1_usize, 2, 4, 8] {
                    let mut denoisers: Vec<HushDenoiser> = (0..channels)
                        .map(|_| {
                            model
                                .denoiser_with_attenuation_db(attenuation_db)
                                .expect("native Hush denoiser should initialize")
                        })
                        .collect();
                    let mut outputs = vec![[0.0; HUSH_FRAME_SIZE]; channels];
                    let mut inference_timings = Vec::with_capacity(input_frames.len() * channels);
                    let mut block_timings =
                        Vec::with_capacity(input_frames.len().div_ceil(native_frames_per_block));
                    let started = Instant::now();
                    for block in input_frames.chunks(native_frames_per_block) {
                        let block_started = Instant::now();
                        for input in block {
                            for (channel, denoiser) in denoisers.iter_mut().enumerate() {
                                let inference_started = Instant::now();
                                denoiser
                                    .process_frame(&mut outputs[channel], input)
                                    .expect("native Hush inference should succeed");
                                inference_timings.push(
                                    inference_started
                                        .elapsed()
                                        .as_nanos()
                                        .min(u128::from(u64::MAX))
                                        as u64,
                                );
                            }
                        }
                        block_timings.push(
                            block_started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
                        );
                    }
                    // The direct baseline measures the sustained processing
                    // loop only; denoiser/model initialization is reported by
                    // the qpwgraph diagnostics and is not realtime audio.
                    let wall_ns = started.elapsed().as_nanos() as u64;
                    inference_timings.sort_unstable();
                    block_timings.sort_unstable();
                    let percentile = |timings: &[u64], pct: usize| {
                        timings
                            .get(timings.len() * pct / 100)
                            .copied()
                            .unwrap_or_default()
                    };
                    let audio_seconds = (input_frames.len() * HUSH_FRAME_SIZE) as f64 / 16_000.0;
                    let factor = wall_ns as f64 / (audio_seconds * 1e9);
                    let inference_total: u64 = inference_timings.iter().sum();
                    let mean_inference_ms =
                        inference_total as f64 / inference_timings.len().max(1) as f64 / 1e6;
                    println!(
                        "direct sample_rate=16000 block_size={} channels={channels} attenuation_db={attenuation_db:.1} audio_duration_s={audio_seconds:.2} wall_clock_ms={:.2} rt_factor={factor:.3} mean_inference_ms={mean_inference_ms:.3} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3} max_ms={:.3} block_p95_ms={:.3}",
                        native_frames_per_block * HUSH_FRAME_SIZE,
                        wall_ns as f64 / 1e6,
                        percentile(&inference_timings, 50) as f64 / 1e6,
                        percentile(&inference_timings, 95) as f64 / 1e6,
                        percentile(&inference_timings, 99) as f64 / 1e6,
                        inference_timings.last().copied().unwrap_or_default() as f64 / 1e6,
                        percentile(&block_timings, 95) as f64 / 1e6,
                    );
                }
            }
        }
    }
}
