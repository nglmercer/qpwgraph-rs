//! Hush neural noise suppressor.
//!
//! Model loading and denoiser construction happen during `prepare()`, which
//! hosts call from their control/setup thread.  The realtime `process()` path
//! only sanitizes samples, advances a preallocated dry delay, and performs
//! bounded SPSC queue operations.

use crate::hush_worker::{HushDiagnostics, HushRuntime, HUSH_SCHEDULING_MS};
use crate::{
    AudioSpec, EffectDescriptor, EffectError, EffectFactory, EffectParameter, EffectProcessor,
};
use nnnoiseless::{HushModel, HUSH_SYNTHESIS_DELAY_SAMPLES};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

pub const HUSH_NOISE_SUPPRESSOR_ID: &str = "builtin.hush-noise-suppressor";
pub const DEFAULT_EFFECT_ID: &str = HUSH_NOISE_SUPPRESSOR_ID;
pub const HUSH_NOISE_SUPPRESSOR_REDUCTION: &str = "reduction-db";
pub const HUSH_NOISE_SUPPRESSOR_BYPASS: &str = "bypass";

const HUSH_MODEL_SHA256: &str = "45632ccaa82b71bb743d6caa7c78e983fe2f2790a3af7f6ec48e6ed7ba085df6";
const EMBEDDED_HUSH_MODEL: &[u8] =
    include_bytes!("../resources/hush/advanced_dfnet16k_model_best_onnx.tar.gz");

#[inline]
fn channel_is_connected(mask: u16, channel: usize) -> bool {
    channel < u16::BITS as usize && mask & (1u16 << channel) != 0
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
        // contains 160 native samples of synthesis delay. Reserve 40 ms for
        // assembly/lookahead/scheduling, then align dry by another 10 ms.
        // The 320-sample algorithmic metadata is NOT added again.
        let synthesis_delay_frames =
            (spec.sample_rate as usize * HUSH_SYNTHESIS_DELAY_SAMPLES).div_ceil(16_000);
        self.scheduling_frames =
            (spec.sample_rate as usize * HUSH_SCHEDULING_MS as usize).div_ceil(1000);
        let delay_capacity_frames = synthesis_delay_frames + self.scheduling_frames;
        self.fallback = vec![0.0; spec.max_frames as usize * channels];
        self.wet = vec![0.0; spec.max_frames as usize * channels];
        self.dry_delay = Some(DryDelay::new(
            delay_capacity_frames.saturating_mul(channels),
        ));
        self.dry_delay
            .as_mut()
            .unwrap()
            .set_delay(delay_capacity_frames * channels);
        self.input_frame_position = 0;
        self.synthesis_delay_frames = synthesis_delay_frames;
        runtime.set_generation(self.generation);
        runtime
            .diagnostics()
            .bypass
            .store(self.bypass, std::sync::atomic::Ordering::Release);
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

        let Some(runtime) = &self.runtime else {
            return Err(EffectError::NotPrepared);
        };
        if !runtime.push(
            self.generation,
            self.input_frame_position,
            frames,
            spec.channels,
            self.channel_mask,
            &self.wet[..expected],
        ) {
            runtime
                .diagnostics()
                .input_overruns
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
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
        let wet_frames = if !runtime
            .diagnostics()
            .worker_failed
            .load(std::sync::atomic::Ordering::Acquire)
        {
            let prefix = (self.scheduling_frames as u64)
                .saturating_sub(self.input_frame_position)
                .min(frames as u64) as usize;
            runtime.pop_ready(
                self.generation,
                self.input_frame_position
                    .saturating_sub(self.scheduling_frames as u64),
                frames - prefix as u32,
                spec.channels,
                &mut self.wet[prefix * spec.channels as usize..expected],
            )
        } else {
            0
        };
        if wet_frames > 0 && !bypassed {
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
        }
        if bypassed || wet_frames < frames as usize {
            runtime
                .diagnostics()
                .dry_fallback_blocks
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if !bypassed
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
        self.host_bypass = bypassed;
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
fn shared_hush_model() -> Result<Arc<HushModel>, String> {
    static MODEL: OnceLock<Result<Arc<HushModel>, String>> = OnceLock::new();
    MODEL
        .get_or_init(|| {
            if let Some(path) = std::env::var_os("QPWGRAPH_HUSH_MODEL")
                .or_else(|| std::env::var_os("HUSH_MODEL"))
                .map(PathBuf::from)
            {
                let bytes = std::fs::read(&path).map_err(|error| {
                    format!("could not read Hush model {}: {error}", path.display())
                })?;
                validate_model_checksum(&bytes, &path.display().to_string())?;
                return HushModel::from_bytes(&bytes)
                    .map(Arc::new)
                    .map_err(|error| format!("could not parse Hush model: {error}"));
            }

            validate_model_checksum(EMBEDDED_HUSH_MODEL, "embedded Hush model")?;
            HushModel::from_static_bytes(EMBEDDED_HUSH_MODEL)
                .map(Arc::new)
                .map_err(|error| format!("could not parse embedded Hush model: {error}"))
        })
        .clone()
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

    fn process(&mut self, input: &[f32], output: &mut [f32]) {
        if self.delay == 0 {
            output.copy_from_slice(input);
            return;
        }
        for (sample, delayed) in input.iter().zip(output.iter_mut()) {
            *delayed = self.samples[self.position];
            self.samples[self.position] = *sample;
            self.position = (self.position + 1) % self.delay;
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
        use std::sync::atomic::Ordering::{Relaxed, Release};
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
        p.runtime.as_ref().unwrap().wait_idle();
        // First fresh input triggers the off-RT state reset after the gap.
        for _ in 0..50 {
            p.process(&mut [0.2; 128], 128).unwrap();
            p.runtime.as_ref().unwrap().wait_idle();
        }
        assert!(d.wet_blocks_output.load(Relaxed) > 0);
        assert!(d.generation_resets.load(Relaxed) > 0);
        assert!(d.max_input_queue_depth.load(Relaxed) <= 128);
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
        while d.generation_resets.load(Acquire) == 0 {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        // Stream time advances by more than the maximum backlog while the
        // worker rebuild is held. This deterministically models a slow reset.
        for _ in 0..80 {
            p.process(&mut [0.2; 128], 128).unwrap();
        }
        d.reset_paused.store(false, Release);
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
            thread::sleep(Duration::from_millis(100));
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
            let mut timings = diagnostics.timings.lock().unwrap().clone();
            timings.sort_unstable();
            let percentile =
                |pct: usize| timings.get(timings.len() * pct / 100).copied().unwrap_or(0) / 1000;
            println!("frames={frames} duration_ms={:.3} latency_ms=50 worker_p95_us={} worker_p99_us={} wet_blocks_output={} dry_fallback_blocks={}", frames as f64 * 1000.0 / sample_rate as f64, percentile(95), percentile(99), diagnostics.wet_blocks_output.load(std::sync::atomic::Ordering::Relaxed), diagnostics.dry_fallback_blocks.load(std::sync::atomic::Ordering::Relaxed));
            println!(
                "rate={sample_rate} channels={channels} callback_avg_us={average:.2} callback_p95_us={p95} callback_p99_us={p99} worker_blocks={processed} worker_avg_us={inference_average:.2} worker_max_us={inference_max} max_input_queue_depth={} max_output_queue_depth={} underruns={} input_overruns={} output_overruns={}",
                diagnostics
                    .max_input_queue_depth
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .max_output_queue_depth
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .underruns
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .input_overruns
                    .load(std::sync::atomic::Ordering::Relaxed),
                diagnostics
                    .output_overruns
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
        }
    }
}
