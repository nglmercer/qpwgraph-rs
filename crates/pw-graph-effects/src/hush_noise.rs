//! Hush neural noise suppressor.
//!
//! Model loading and denoiser construction happen during `prepare()`, which
//! hosts call from their control/setup thread.  The realtime `process()` path
//! only sanitizes samples, advances a preallocated dry delay, and performs
//! bounded SPSC queue operations.

use crate::hush_worker::{HushDiagnostics, HushRuntime};
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
    channel_mask: u16,
    generation: u64,
    sequence: u64,
    callback_frames: Option<u32>,
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
            channel_mask: 0,
            generation: 0,
            sequence: 0,
            callback_frames: None,
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
        self.sequence = 0;
        if let Some(runtime) = &self.runtime {
            runtime.set_generation(self.generation, self.channel_mask);
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
        let model = shared_hush_model().map_err(EffectError::ModelUnavailable)?;
        let channels = spec.channels as usize;
        self.channel_mask = if channels >= 16 {
            u16::MAX
        } else {
            (1u16 << channels) - 1
        };
        let runtime = HushRuntime::spawn(
            model,
            spec.sample_rate,
            spec.channels,
            spec.max_frames,
            self.reduction_db,
        )
        .map_err(EffectError::WorkerUnavailable)?;
        // The streaming output already contains Hush's 160-sample
        // overlap-add synthesis delay. The worker normally returns one
        // callback quantum later, so the fallback reserves that bounded
        // quantum as well. At 16 kHz this is 160 + 160 = 320 samples, which
        // matches Hush's reported algorithmic latency without conflating the
        // two constants or adding the 320-sample metadata a second time.
        let synthesis_delay_frames =
            ((spec.sample_rate as usize * HUSH_SYNTHESIS_DELAY_SAMPLES).div_ceil(16_000)).max(1);
        // `max_frames` is only the preallocation ceiling. The actual worker
        // pipeline delay is one callback quantum, so the active delay is set
        // from the first observed `frames` value in `process()` below. This
        // keeps PipeWire's large maximum buffer from turning a 20 ms Hush
        // path into a hundreds-of-milliseconds bypass path.
        let delay_capacity_frames = synthesis_delay_frames.saturating_add(spec.max_frames as usize);
        self.fallback = vec![0.0; spec.max_frames as usize * channels];
        self.wet = vec![0.0; spec.max_frames as usize * channels];
        self.dry_delay = Some(DryDelay::new(
            delay_capacity_frames.saturating_mul(channels),
        ));
        self.callback_frames = None;
        self.synthesis_delay_frames = synthesis_delay_frames;
        runtime.set_generation(self.generation, self.channel_mask);
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
        // PipeWire and the Windows router normally use a fixed quantum, but
        // the effect contract permits smaller callback blocks. A quantum-size
        // change invalidates queued blocks and re-aligns the preallocated dry
        // delay without allocating or waiting on the realtime thread.
        if self.callback_frames != Some(frames) {
            self.next_generation(true);
            self.callback_frames = Some(frames);
            if let Some(delay) = &mut self.dry_delay {
                let delay_samples = self
                    .synthesis_delay_frames
                    .saturating_add(frames as usize)
                    .saturating_mul(spec.channels as usize);
                delay.set_delay(delay_samples);
            }
        }
        // Preserve the sanitized live input for the worker before the dry
        // delay overwrites the callback buffer.
        self.wet[..expected].copy_from_slice(buffer);

        // The dry path is deliberately delayed by Hush's 160-sample
        // overlap-add synthesis delay plus one bounded worker quantum. Hush
        // also reports 320 samples of algorithmic latency; that metadata is
        // represented by this combined alignment and is not added a second
        // time. The delayed path is used for bypass, startup, underrun, and
        // worker failure, so transitions remain time-aligned with wet audio.
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
        if self.bypass {
            return Ok(());
        }

        let Some(runtime) = &self.runtime else {
            return Err(EffectError::NotPrepared);
        };
        if !runtime.push(
            self.generation,
            self.sequence,
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
        self.wet[..expected].fill(0.0);
        if !runtime
            .diagnostics()
            .worker_failed
            .load(std::sync::atomic::Ordering::Acquire)
            && runtime.pop_ready(
                self.generation,
                self.sequence,
                frames,
                spec.channels,
                &mut self.wet[..expected],
            )
            && self.wet[..expected].iter().all(|sample| sample.is_finite())
        {
            for channel in 0..spec.channels as usize {
                if !channel_is_connected(self.channel_mask, channel) {
                    for frame in 0..frames as usize {
                        self.wet[frame * spec.channels as usize + channel] = 0.0;
                    }
                }
            }
            buffer.copy_from_slice(&self.wet[..expected]);
        } else {
            runtime
                .diagnostics()
                .underruns
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.sequence = self.sequence.wrapping_add(1);
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
                if self.bypass != bypass {
                    self.bypass = bypass;
                    // Bypass invalidates queued wet blocks, but the delayed
                    // dry line continues so the transition stays aligned.
                    self.next_generation(false);
                }
            }
            _ => unreachable!("descriptor and parameter match are kept together"),
        }
        Ok(())
    }

    fn set_channel_mask(&mut self, mask: u16) {
        if self.channel_mask != mask {
            self.channel_mask = mask;
            self.next_generation(true);
        }
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
    fn bypass_uses_synthesis_plus_one_actual_callback_quantum() {
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

        let mut third = vec![0.75; 160];
        processor.process(&mut third, 160).unwrap();
        assert!(third.iter().all(|sample| (*sample - 0.25).abs() < 1e-6));
    }

    #[test]
    #[ignore = "manual Hush performance measurement"]
    fn benchmark_hush_rates() {
        let cases = [
            (16_000, 1),
            (44_100, 1),
            (48_000, 1),
            (48_000, 2),
            (96_000, 1),
            (96_000, 2),
        ];
        for (sample_rate, channels) in cases {
            let frames = sample_rate / 100;
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
                thread::sleep(Duration::from_millis(10));
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
