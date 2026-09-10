//! Neural adaptive noise reduction backed by the vendored `nnnoiseless` model.
//!
//! `nnnoiseless` operates on 480-sample frames at 48 kHz and has an intrinsic
//! frame of latency. This module owns the frame assembly, channel deinterleaving,
//! sample-rate conversion, and fixed-capacity queues needed to expose that
//! streaming API through the effect SDK's in-place buffer contract.
//!
//! All buffers and model state are allocated by [`AdaptiveNoiseSuppressor::prepare`]
//! (or by a control-thread parameter update). The realtime `process` path only
//! mutates those buffers; it does not allocate, lock, or perform I/O.

use crate::{
    gain::{rms_db, GainCompensation},
    AudioSpec, EffectDescriptor, EffectError, EffectParameter, EffectProcessor, EffectProvider,
    NOISE_SUPPRESSOR_ADAPTATION, NOISE_SUPPRESSOR_AUTO_GAIN_COMPENSATION, NOISE_SUPPRESSOR_BYPASS,
    NOISE_SUPPRESSOR_ID, NOISE_SUPPRESSOR_OUTPUT_GAIN_DB, NOISE_SUPPRESSOR_REDUCTION,
    NOISE_SUPPRESSOR_VOICE_PRESERVE,
};
use nnnoiseless::{DenoiseParams, DenoiseState, Resampler};

const MODEL_SAMPLE_RATE: u32 = 48_000;
const MODEL_FRAME_SIZE: usize = DenoiseState::FRAME_SIZE;
const SAMPLE_SCALE: f32 = 32_768.0;
const SCHEDULER_LEAD_FRAMES: usize = 1;
const MIN_SUPPORTED_SAMPLE_RATE: u32 = 8_000;
const MAX_SUPPORTED_SAMPLE_RATE: u32 = 192_000;

pub(crate) fn descriptor() -> EffectDescriptor {
    EffectDescriptor {
        id: NOISE_SUPPRESSOR_ID.into(),
        name: "Adaptive Neural Noise Suppressor".into(),
        vendor: "qpwgraph-rs".into(),
        version: "3.1.0".into(),
        parameters: vec![
            EffectParameter {
                id: NOISE_SUPPRESSOR_REDUCTION.into(),
                name: "Reduction".into(),
                minimum: 0.0,
                maximum: 60.0,
                default: 20.0,
                unit: "dB".into(),
            },
            EffectParameter {
                id: NOISE_SUPPRESSOR_ADAPTATION.into(),
                name: "Adaptation".into(),
                minimum: 0.0,
                maximum: 100.0,
                default: 55.0,
                unit: "%".into(),
            },
            EffectParameter {
                id: NOISE_SUPPRESSOR_VOICE_PRESERVE.into(),
                name: "Voice Preserve".into(),
                minimum: 0.0,
                maximum: 100.0,
                default: 70.0,
                unit: "%".into(),
            },
            EffectParameter {
                id: NOISE_SUPPRESSOR_OUTPUT_GAIN_DB.into(),
                name: "Output Gain".into(),
                minimum: -12.0,
                maximum: 12.0,
                default: 0.0,
                unit: "dB".into(),
            },
            EffectParameter {
                id: NOISE_SUPPRESSOR_AUTO_GAIN_COMPENSATION.into(),
                name: "Automatic Gain Compensation".into(),
                minimum: 0.0,
                maximum: 1.0,
                default: 0.0,
                unit: "boolean".into(),
            },
            EffectParameter {
                id: NOISE_SUPPRESSOR_BYPASS.into(),
                name: "Bypass".into(),
                minimum: 0.0,
                maximum: 1.0,
                default: 0.0,
                unit: "boolean".into(),
            },
        ],
    }
}

pub(crate) struct AdaptiveNoiseSuppressorFactory;

impl EffectProvider for AdaptiveNoiseSuppressorFactory {
    fn descriptor(&self) -> &EffectDescriptor {
        static DESCRIPTOR: std::sync::OnceLock<EffectDescriptor> = std::sync::OnceLock::new();
        DESCRIPTOR.get_or_init(descriptor)
    }

    fn create(&self) -> Box<dyn EffectProcessor> {
        Box::new(AdaptiveNoiseSuppressor::default())
    }
}

/// A fixed-capacity single-producer/single-consumer sample queue.
///
/// The queue is used only by one processor call at a time, so it needs no
/// synchronization. Keeping it as a ring avoids `Vec::drain` and ensures that
/// the realtime path never has to move a potentially large tail of samples.
struct SampleRing {
    samples: Vec<f32>,
    read: usize,
    len: usize,
}

impl SampleRing {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            samples: vec![0.0; capacity.max(1)],
            read: 0,
            len: 0,
        }
    }

    fn clear(&mut self) {
        self.read = 0;
        self.len = 0;
    }

    fn available(&self) -> usize {
        self.len
    }

    fn push_slice(&mut self, input: &[f32]) -> Result<(), EffectError> {
        if input.len() > self.samples.len().saturating_sub(self.len) {
            return Err(EffectError::InternalBufferOverflow);
        }
        let capacity = self.samples.len();
        for (offset, &sample) in input.iter().enumerate() {
            self.samples[(self.read + self.len + offset) % capacity] = sample;
        }
        self.len += input.len();
        Ok(())
    }

    fn pop_or_zero(&mut self, output: &mut [f32]) {
        let take = output.len().min(self.len);
        let capacity = self.samples.len();
        for (offset, sample) in output[..take].iter_mut().enumerate() {
            *sample = self.samples[(self.read + offset) % capacity];
        }
        self.read = (self.read + take) % capacity;
        self.len -= take;
        output[take..].fill(0.0);
    }

    /// Keeps one full model frame queued so irregular host callback sizes do
    /// not expose a gap while the next neural frame is being assembled.
    fn pop_with_lead(&mut self) -> f32 {
        let lead = SCHEDULER_LEAD_FRAMES * MODEL_FRAME_SIZE;
        if self.len <= lead {
            0.0
        } else {
            let sample = self.samples[self.read];
            self.read = (self.read + 1) % self.samples.len();
            self.len -= 1;
            sample
        }
    }
}

/// State for one 48 kHz denoiser channel.
struct ChannelState {
    denoiser: Box<DenoiseState<'static>>,
    pending: Vec<f32>,
    frame_input: [f32; MODEL_FRAME_SIZE],
    frame_output: [f32; MODEL_FRAME_SIZE],
    ready: SampleRing,
}

impl ChannelState {
    fn new(params: DenoiseParams, max_model_frames: usize) -> Self {
        Self {
            denoiser: DenoiseState::with_params(params),
            pending: Vec::with_capacity(max_model_frames + MODEL_FRAME_SIZE),
            frame_input: [0.0; MODEL_FRAME_SIZE],
            frame_output: [0.0; MODEL_FRAME_SIZE],
            ready: SampleRing::with_capacity(
                max_model_frames + (SCHEDULER_LEAD_FRAMES + 2) * MODEL_FRAME_SIZE,
            ),
        }
    }

    fn reset(&mut self) {
        self.denoiser.reset();
        self.pending.clear();
        self.frame_input.fill(0.0);
        self.frame_output.fill(0.0);
        self.ready.clear();
    }

    fn process_pending(&mut self) -> Result<(), EffectError> {
        let frame_count = self.pending.len() / MODEL_FRAME_SIZE;
        let processed_samples = frame_count * MODEL_FRAME_SIZE;
        if self.ready.available().saturating_add(processed_samples) > self.ready.samples.len() {
            return Err(EffectError::InternalBufferOverflow);
        }

        for frame in self.pending[..processed_samples]
            .as_chunks::<MODEL_FRAME_SIZE>()
            .0
        {
            for (destination, &sample) in self.frame_input.iter_mut().zip(frame) {
                *destination = sample * SAMPLE_SCALE;
            }
            self.denoiser
                .process_frame(&mut self.frame_output, &self.frame_input);
            for sample in &mut self.frame_output {
                *sample = if sample.is_finite() {
                    (*sample / SAMPLE_SCALE).clamp(-1.0, 1.0)
                } else {
                    0.0
                };
            }
            self.ready.push_slice(&self.frame_output)?;
        }

        if processed_samples > 0 {
            let remaining = self.pending.len() - processed_samples;
            self.pending.copy_within(processed_samples.., 0);
            self.pending.truncate(remaining);
        }
        Ok(())
    }
}

/// A multi-channel 48 kHz neural stream with preallocated frame queues.
struct NeuralCore {
    channels: Vec<ChannelState>,
    channel_count: usize,
}

impl NeuralCore {
    fn new(
        channels: usize,
        max_model_frames: usize,
        params: DenoiseParams,
    ) -> Result<Self, EffectError> {
        if channels == 0 {
            return Err(EffectError::InvalidAudioSpec {
                sample_rate: MODEL_SAMPLE_RATE,
                channels: 0,
                max_frames: max_model_frames as u32,
            });
        }
        Ok(Self {
            channels: (0..channels)
                .map(|_| ChannelState::new(params, max_model_frames))
                .collect(),
            channel_count: channels,
        })
    }

    fn reset(&mut self) {
        for channel in &mut self.channels {
            channel.reset();
        }
    }

    fn process(&mut self, input: &[f32], output: &mut Vec<f32>) -> Result<(), EffectError> {
        if !input.len().is_multiple_of(self.channel_count) {
            return Err(EffectError::InvalidBufferLength {
                actual: input.len(),
                expected: input.len() - input.len() % self.channel_count,
            });
        }
        let frames = input.len() / self.channel_count;
        for channel in &self.channels {
            if channel.pending.len().saturating_add(frames) > channel.pending.capacity() {
                return Err(EffectError::InternalBufferOverflow);
            }
        }

        for frame in input.chunks_exact(self.channel_count) {
            for (channel, &sample) in self.channels.iter_mut().zip(frame) {
                let sample = if sample.is_finite() {
                    sample.clamp(-1.0, 1.0)
                } else {
                    0.0
                };
                channel.pending.push(sample);
            }
        }
        for channel in &mut self.channels {
            channel.process_pending()?;
        }

        output.clear();
        output.resize(input.len(), 0.0);
        for output_frame in output.chunks_exact_mut(self.channel_count) {
            for (sample, channel) in output_frame.iter_mut().zip(&mut self.channels) {
                *sample = channel.ready.pop_with_lead();
            }
        }
        Ok(())
    }
}

/// A complete stream adapter around the 48 kHz neural core.
struct NeuralStream {
    sample_rate: u32,
    channels: usize,
    core: NeuralCore,
    input_resampler: Option<Resampler>,
    output_resampler: Option<Resampler>,
    input_at_model_rate: Vec<f32>,
    output_at_model_rate: Vec<f32>,
    output_at_input_rate: Vec<f32>,
    output: SampleRing,
}

impl NeuralStream {
    fn new(spec: &AudioSpec, params: DenoiseParams) -> Result<Self, EffectError> {
        if !(MIN_SUPPORTED_SAMPLE_RATE..=MAX_SUPPORTED_SAMPLE_RATE).contains(&spec.sample_rate) {
            return Err(EffectError::UnsupportedSampleRate {
                sample_rate: spec.sample_rate,
                minimum: MIN_SUPPORTED_SAMPLE_RATE,
                maximum: MAX_SUPPORTED_SAMPLE_RATE,
            });
        }

        // Initialize FFT plans and SIMD dispatch on the preparation/control
        // thread. The first realtime callback must not pay this cost.
        let _ = nnnoiseless::active_isa();

        let channels = spec.channels as usize;
        let input_frames = spec.max_frames as usize;
        let model_frames = resampled_frame_capacity(input_frames, spec.sample_rate)
            .checked_add(2 * MODEL_FRAME_SIZE)
            .ok_or(EffectError::InternalBufferOverflow)?;
        let model_samples = model_frames
            .checked_mul(channels)
            .ok_or(EffectError::InternalBufferOverflow)?;
        let output_frames = input_frames
            .checked_add(4 * MODEL_FRAME_SIZE)
            .ok_or(EffectError::InternalBufferOverflow)?;
        let output_samples = output_frames
            .checked_mul(channels)
            .ok_or(EffectError::InternalBufferOverflow)?;

        let mut input_resampler = None;
        let mut output_resampler = None;
        if spec.sample_rate != MODEL_SAMPLE_RATE {
            let mut input =
                Resampler::new(spec.sample_rate as f64, MODEL_SAMPLE_RATE as f64, channels);
            input.reserve(input_frames);
            input_resampler = Some(input);

            let mut output =
                Resampler::new(MODEL_SAMPLE_RATE as f64, spec.sample_rate as f64, channels);
            output.reserve(model_frames);
            output_resampler = Some(output);
        }

        Ok(Self {
            sample_rate: spec.sample_rate,
            channels,
            core: NeuralCore::new(channels, model_frames, params)?,
            input_resampler,
            output_resampler,
            input_at_model_rate: Vec::with_capacity(model_samples),
            output_at_model_rate: Vec::with_capacity(model_samples),
            output_at_input_rate: Vec::with_capacity(output_samples),
            output: SampleRing::with_capacity(output_samples),
        })
    }

    fn reset(&mut self) {
        self.core.reset();
        if let Some(resampler) = &mut self.input_resampler {
            resampler.reset();
        }
        if let Some(resampler) = &mut self.output_resampler {
            resampler.reset();
        }
        self.input_at_model_rate.clear();
        self.output_at_model_rate.clear();
        self.output_at_input_rate.clear();
        self.output.clear();
    }

    fn process(&mut self, buffer: &mut [f32], frames: u32) -> Result<(), EffectError> {
        let expected = frames as usize * self.channels;
        if buffer.len() != expected {
            return Err(EffectError::InvalidBufferLength {
                actual: buffer.len(),
                expected,
            });
        }

        self.input_at_model_rate.clear();
        match &mut self.input_resampler {
            Some(resampler) => resampler.process(buffer, &mut self.input_at_model_rate),
            None => self.input_at_model_rate.extend_from_slice(buffer),
        }

        self.core
            .process(&self.input_at_model_rate, &mut self.output_at_model_rate)?;
        self.output_at_input_rate.clear();
        match &mut self.output_resampler {
            Some(resampler) => {
                resampler.process(&self.output_at_model_rate, &mut self.output_at_input_rate);
            }
            None => self
                .output_at_input_rate
                .extend_from_slice(&self.output_at_model_rate),
        }

        self.output.push_slice(&self.output_at_input_rate)?;
        self.output.pop_or_zero(buffer);
        Ok(())
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

fn resampled_frame_capacity(input_frames: usize, sample_rate: u32) -> usize {
    let ratio = MODEL_SAMPLE_RATE as f64 / sample_rate as f64;
    (input_frames as f64 * ratio).ceil() as usize
}

/// A neural adaptive noise suppressor exposed through the effect SDK.
pub struct AdaptiveNoiseSuppressor {
    descriptor: EffectDescriptor,
    spec: Option<AudioSpec>,
    stream: Option<NeuralStream>,
    reduction_db: f32,
    adaptation: f32,
    voice_preserve: f32,
    gain: GainCompensation,
    bypass: bool,
}

impl Default for AdaptiveNoiseSuppressor {
    fn default() -> Self {
        Self {
            descriptor: descriptor(),
            spec: None,
            stream: None,
            reduction_db: 20.0,
            adaptation: 55.0,
            voice_preserve: 70.0,
            gain: GainCompensation::default(),
            bypass: false,
        }
    }
}

impl AdaptiveNoiseSuppressor {
    fn parameter(&self, id: &str) -> Option<(f32, f32)> {
        self.descriptor
            .parameters
            .iter()
            .find(|parameter| parameter.id == id)
            .map(|parameter| (parameter.minimum, parameter.maximum))
    }

    fn denoise_params(&self) -> DenoiseParams {
        let adaptation = self.adaptation / 100.0;
        let voice_preserve = self.voice_preserve / 100.0;

        // The public controls retain their original meaning while mapping to
        // the neural model's smoothing and VAD controls:
        //
        // * adaptation controls how quickly suppression gains can change;
        // * voice preserve lowers the VAD gate and permits faster gain release.
        DenoiseParams::default()
            .max_attenuation_db(self.reduction_db)
            .gain_decay(0.92 - 0.42 * adaptation)
            .gain_rise(0.4 + 0.6 * voice_preserve)
            .vad_threshold(0.45 * (1.0 - voice_preserve))
            .silence_gain_decay(0.96 + 0.04 * adaptation)
    }

    fn rebuild_stream(&mut self) -> Result<(), EffectError> {
        let Some(spec) = self.spec else {
            return Ok(());
        };
        self.stream = Some(NeuralStream::new(&spec, self.denoise_params())?);
        Ok(())
    }
}

impl EffectProcessor for AdaptiveNoiseSuppressor {
    fn descriptor(&self) -> &EffectDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, spec: AudioSpec) -> Result<(), EffectError> {
        spec.validate()?;
        let stream = NeuralStream::new(&spec, self.denoise_params())?;
        self.spec = Some(spec);
        self.stream = Some(stream);
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
        let channels = spec.channels as usize;
        let expected = frames as usize * channels;
        if buffer.len() != expected {
            return Err(EffectError::InvalidBufferLength {
                actual: buffer.len(),
                expected,
            });
        }

        // Sanitize before either the model or a resampler sees the block. In
        // particular, allowing NaN/Inf into a resampler would poison its
        // history and make later finite blocks depend on the malformed one.
        for sample in buffer.iter_mut() {
            if !sample.is_finite() {
                *sample = 0.0;
            }
        }
        if self.bypass {
            return Ok(());
        }

        let input_rms_db = rms_db(buffer);
        let stream = self.stream.as_mut().ok_or(EffectError::NotPrepared)?;
        debug_assert_eq!(stream.sample_rate(), spec.sample_rate);
        stream.process(buffer, frames)?;
        let processed_rms_db = rms_db(buffer);
        self.gain.apply(
            buffer,
            channels,
            spec.sample_rate,
            input_rms_db,
            processed_rms_db,
        );
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
            NOISE_SUPPRESSOR_REDUCTION => self.reduction_db = value,
            NOISE_SUPPRESSOR_ADAPTATION => self.adaptation = value,
            NOISE_SUPPRESSOR_VOICE_PRESERVE => self.voice_preserve = value,
            NOISE_SUPPRESSOR_OUTPUT_GAIN_DB => self.gain.set_output_gain_db(value),
            NOISE_SUPPRESSOR_AUTO_GAIN_COMPENSATION => self.gain.set_automatic(value >= 0.5),
            NOISE_SUPPRESSOR_BYPASS => {
                let bypass = value >= 0.5;
                if self.bypass != bypass {
                    self.bypass = bypass;
                    self.reset();
                }
                return Ok(());
            }
            _ => unreachable!("descriptor and parameter match are kept together"),
        }
        self.rebuild_stream()
    }

    fn reset(&mut self) {
        if let Some(stream) = &mut self.stream {
            stream.reset();
        }
        self.gain.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared_suppressor(sample_rate: u32, channels: u16) -> AdaptiveNoiseSuppressor {
        let mut suppressor = AdaptiveNoiseSuppressor::default();
        suppressor
            .prepare(AudioSpec {
                sample_rate,
                channels,
                max_frames: 256,
            })
            .unwrap();
        suppressor
    }

    #[test]
    fn steady_noise_is_reduced_after_the_stream_warms_up() {
        let mut suppressor = prepared_suppressor(MODEL_SAMPLE_RATE, 1);
        suppressor
            .set_parameter(NOISE_SUPPRESSOR_REDUCTION, 36.0)
            .unwrap();
        for _ in 0..320 {
            let mut noise = vec![0.004; 128];
            suppressor.process(&mut noise, 128).unwrap();
        }
        let mut noise = vec![0.004; 128];
        suppressor.process(&mut noise, 128).unwrap();
        assert!(
            noise
                .iter()
                .all(|sample| sample.is_finite() && sample.abs() < 0.003),
            "learned steady noise should be attenuated"
        );
    }

    #[test]
    fn speech_and_invalid_input_are_safe_over_a_stream() {
        let mut suppressor = prepared_suppressor(MODEL_SAMPLE_RATE, 1);
        let mut saw_foreground = false;
        for block in 0..24 {
            let mut audio = vec![0.0; 128];
            if block == 0 {
                audio[0] = f32::NAN;
                audio[1] = f32::INFINITY;
            }
            for sample in audio.iter_mut().skip(2) {
                *sample = 0.5;
            }
            suppressor.process(&mut audio, 128).unwrap();
            assert!(audio.iter().all(|sample| sample.is_finite()));
            if audio.iter().any(|sample| sample.abs() > 0.1) {
                saw_foreground = true;
            }
        }
        assert!(saw_foreground, "foreground signal should remain audible");
    }

    #[test]
    fn common_audio_rates_are_resampled_without_allocating_in_process() {
        let mut suppressor = prepared_suppressor(16_000, 2);
        for _ in 0..80 {
            let mut audio = vec![0.01; 128 * 2];
            suppressor.process(&mut audio, 128).unwrap();
            assert!(audio.iter().all(|sample| sample.is_finite()));
        }
    }

    #[test]
    fn exact_silence_stays_finite_and_zero_at_48_khz() {
        let mut suppressor = prepared_suppressor(MODEL_SAMPLE_RATE, 2);
        for _ in 0..128 {
            let mut audio = vec![0.0; 128 * 2];
            suppressor.process(&mut audio, 128).unwrap();
            assert!(audio.iter().all(|sample| sample.is_finite()));
            assert!(audio.iter().all(|sample| *sample == 0.0));
        }
    }

    #[test]
    fn exact_silence_stays_finite_and_zero_through_resampling() {
        let mut suppressor = prepared_suppressor(44_100, 2);
        for _ in 0..128 {
            let mut audio = vec![0.0; 128 * 2];
            suppressor.process(&mut audio, 128).unwrap();
            assert!(audio.iter().all(|sample| sample.is_finite()));
            assert!(audio.iter().all(|sample| *sample == 0.0));
        }
    }

    #[test]
    fn reset_discards_queued_audio_before_silence_is_processed() {
        let mut suppressor = prepared_suppressor(MODEL_SAMPLE_RATE, 1);
        for _ in 0..12 {
            let mut signal = vec![0.5; 256];
            suppressor.process(&mut signal, 256).unwrap();
        }
        suppressor.reset();

        for _ in 0..16 {
            let mut silence = vec![0.0; 256];
            suppressor.process(&mut silence, 256).unwrap();
            assert!(silence.iter().all(|sample| sample.is_finite()));
            assert!(silence.iter().all(|sample| *sample == 0.0));
        }
    }

    #[test]
    fn bypass_is_transparent_for_valid_audio_and_silent_for_invalid_audio() {
        let mut suppressor = prepared_suppressor(MODEL_SAMPLE_RATE, 1);
        suppressor
            .set_parameter(NOISE_SUPPRESSOR_OUTPUT_GAIN_DB, 12.0)
            .unwrap();
        suppressor
            .set_parameter(NOISE_SUPPRESSOR_AUTO_GAIN_COMPENSATION, 1.0)
            .unwrap();
        suppressor
            .set_parameter(NOISE_SUPPRESSOR_BYPASS, 1.0)
            .unwrap();
        let mut audio = vec![0.25, f32::NAN, f32::INFINITY, -0.5];

        suppressor.process(&mut audio, 4).unwrap();

        assert_eq!(audio, vec![0.25, 0.0, 0.0, -0.5]);
    }
}
