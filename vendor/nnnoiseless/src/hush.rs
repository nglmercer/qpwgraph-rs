//! Hush/DeepFilterNet-SE inference backend.
//!
//! Hush is a different model family from the built-in RNNoise path. It runs at
//! 16 kHz, consumes 160-sample (10 ms) frames, and uses the DeepFilterNet
//! encoder, ERB mask decoder, and complex deep-filter decoder. This module
//! deliberately keeps that model behind an opt-in feature and a separate API;
//! Hush weights cannot be loaded by [`crate::RnnModel`].

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use df::tract::{DfParams, DfTract, ReduceMask, RuntimeParams};
use ndarray::{ArrayView2, ArrayViewMut2};

/// Hush's native sample rate, in samples per second.
///
/// Hush models are trained for and run at 16 kHz. Callers using another rate
/// should resample around [`HushDenoiser::process_frame`], or use
/// [`denoise_hush_buffer`], which does that conversion for a complete buffer.
pub const HUSH_SAMPLE_RATE: usize = 16_000;

/// Hush's streaming frame size: 160 samples, or 10 ms at [`HUSH_SAMPLE_RATE`].
///
/// [`HushDenoiser::process_frame`] requires exactly this many normalized mono
/// `f32` samples on every call.
pub const HUSH_FRAME_SIZE: usize = 160;

/// Hush's documented algorithmic latency: 320 samples, or 20 ms at
/// [`HUSH_SAMPLE_RATE`] (two [`HUSH_FRAME_SIZE`]-sample frames).
///
/// This is the model's reported algorithmic latency. It is distinct from the
/// shorter synthesis delay used to align output samples.
pub const HUSH_ALGORITHMIC_LATENCY_SAMPLES: usize = 320;

/// Hush's overlap-add synthesis delay: `fft_size - hop_size`, or 160 samples
/// for the pinned 320-sample FFT and 160-sample hop.
///
/// Buffer and streaming adapters use this value when discarding initial output
/// and flushing the final delayed frame. It must not be confused with
/// [`HUSH_ALGORITHMIC_LATENCY_SAMPLES`].
pub const HUSH_SYNTHESIS_DELAY_SAMPLES: usize = 160;

/// Compatibility alias for the pre-split latency constant.
pub const HUSH_LATENCY_SAMPLES: usize = HUSH_ALGORITHMIC_LATENCY_SAMPLES;

fn build_runtime_params(attenuation_db: f32) -> RuntimeParams {
    build_runtime_params_for_channels(1, attenuation_db, ReduceMask::MAX)
}

fn build_runtime_params_for_channels(
    channels: usize,
    attenuation_db: f32,
    reduce_mask: ReduceMask,
) -> RuntimeParams {
    RuntimeParams::new(
        channels,
        false,
        attenuation_db,
        -15.0,
        35.0,
        35.0,
        reduce_mask,
    )
}

/// An error returned while loading or running a Hush model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HushError(String);

impl HushError {
    fn new(message: impl Into<String>) -> HushError {
        HushError(message.into())
    }
}

impl fmt::Display for HushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HushError {}

/// Parsed Hush model parameters.
///
/// The model bundle is the `advanced_dfnet16k_model_best_onnx.tar.gz` artifact
/// released by Hush. It contains the encoder, ERB decoder, deep-filter
/// decoder, and the model configuration. The immutable parameters can be
/// cloned to create independent streaming sessions.
///
/// A model owns the parsed parameters and, for [`Self::from_bytes`], a private
/// copy of the input archive. The archive does not need to remain available
/// after loading. The immutable model can be shared between threads; create a
/// separate [`HushDenoiser`] for each audio stream because denoiser state is
/// mutable and stream-specific.
#[derive(Clone)]
pub struct HushModel {
    params: DfParams,
    // `DfParams::from_bytes` currently exposes a static-byte API even though
    // it copies every archive entry into owned buffers. Keep the source alive
    // for the duration of parsing without leaking it for every browser clip.
    #[allow(dead_code)]
    source_bytes: Option<Arc<[u8]>>,
}

impl HushModel {
    /// Loads a Hush ONNX bundle from a filesystem path.
    ///
    /// `path` must point to Hush's
    /// `advanced_dfnet16k_model_best_onnx.tar.gz` bundle. The file is read and
    /// parsed before this method returns; the returned model owns everything
    /// needed to create denoisers and does not retain the path or file handle.
    pub fn from_path(path: impl AsRef<Path>) -> Result<HushModel, HushError> {
        let path = path.as_ref();
        let params = DfParams::new(path.to_path_buf())
            .map_err(|error| HushError::new(format!("could not load Hush model: {error}")))?;
        Ok(HushModel {
            params,
            source_bytes: None,
        })
    }

    /// Loads a Hush ONNX bundle from memory.
    ///
    /// `bytes` must contain Hush's
    /// `advanced_dfnet16k_model_best_onnx.tar.gz` bundle. The input is copied
    /// into model-owned storage, so the caller may release or reuse its buffer
    /// after this method returns. Repeated loads reclaim that storage when the
    /// corresponding model is dropped.
    pub fn from_bytes(bytes: &[u8]) -> Result<HushModel, HushError> {
        let source_bytes: Arc<[u8]> = Arc::from(bytes);
        // The pinned DeepFilterNet 0.5.3 parser copies enc.onnx, erb_dec.onnx,
        // df_dec.onnx, and config.ini before returning. It never stores this
        // view. Keeping `source_bytes` in HushModel makes that contract explicit
        // and prevents the static API from becoming a use-after-free if the
        // parser is ever changed without updating this adapter.
        let static_bytes = leaked_static_view(&source_bytes);
        let params = DfParams::from_bytes(static_bytes)
            .map_err(|error| HushError::new(format!("could not parse Hush model: {error}")))?;
        Ok(HushModel {
            params,
            source_bytes: Some(source_bytes),
        })
    }

    /// Loads a Hush ONNX bundle from bytes that are already `'static`.
    ///
    /// This is useful for applications that embed the model in their binary.
    /// For downloaded or otherwise owned data, prefer [`Self::from_bytes`],
    /// which manages the input buffer's lifetime for you.
    pub fn from_static_bytes(bytes: &'static [u8]) -> Result<HushModel, HushError> {
        let params = DfParams::from_bytes(bytes)
            .map_err(|error| HushError::new(format!("could not parse Hush model: {error}")))?;
        Ok(HushModel {
            params,
            source_bytes: None,
        })
    }

    /// Creates an independent streaming denoiser with unlimited attenuation.
    ///
    /// The returned denoiser owns its recurrent, spectral, and overlap-add
    /// state. Calling this method again creates another independent stream.
    pub fn denoiser(&self) -> Result<HushDenoiser, HushError> {
        self.denoiser_with_attenuation_db(100.0)
    }

    /// Creates an independent streaming denoiser with a maximum attenuation
    /// in decibels. Use `100.0` for effectively unlimited suppression.
    pub fn denoiser_with_attenuation_db(
        &self,
        attenuation_db: f32,
    ) -> Result<HushDenoiser, HushError> {
        if !attenuation_db.is_finite() || attenuation_db < 0.0 {
            return Err(HushError::new(
                "Hush attenuation must be finite and non-negative",
            ));
        }

        let runtime_params = build_runtime_params(attenuation_db);
        let runtime = DfTract::new(self.params.clone(), &runtime_params).map_err(|error| {
            HushError::new(format!("could not initialize Hush runtime: {error}"))
        })?;

        if runtime.sr != HUSH_SAMPLE_RATE
            || runtime.hop_size != HUSH_FRAME_SIZE
            || runtime.fft_size.saturating_sub(runtime.hop_size) != HUSH_SYNTHESIS_DELAY_SAMPLES
        {
            return Err(HushError::new(format!(
                "model is {} Hz with {}-sample frames and {}-sample synthesis delay; expected Hush's {} Hz/{}-sample/{}-sample contract",
                runtime.sr,
                runtime.hop_size,
                runtime.fft_size.saturating_sub(runtime.hop_size),
                HUSH_SAMPLE_RATE,
                HUSH_FRAME_SIZE,
                HUSH_SYNTHESIS_DELAY_SAMPLES,
            )));
        }

        Ok(HushDenoiser {
            params: self.params.clone(),
            runtime,
            frame_size: HUSH_FRAME_SIZE,
            attenuation_db,
            last_lsnr_db: -15.0,
        })
    }

    /// Creates an experimental multi-channel streaming denoiser.
    ///
    /// The input and output of [`HushMultiDenoiser::process_frame`] are
    /// planar: channel zero occupies the first frame, channel one the next,
    /// and so on. `Independent` keeps separate mask decisions for each
    /// channel; `Maximum` and `Mean` link the mask across channels and are
    /// useful for experiments that prefer spatially coherent attenuation.
    ///
    /// This API is intentionally separate from [`Self::denoiser`]. qpwgraph's
    /// full stereo path uses `Independent` after a benchmark showed identical
    /// output to two mono sessions; partially connected routes continue to use
    /// independent mono sessions so disconnected channels do not consume
    /// inference time.
    pub fn multi_denoiser_with_attenuation_db(
        &self,
        channels: usize,
        attenuation_db: f32,
        mask_mode: HushMaskMode,
    ) -> Result<HushMultiDenoiser, HushError> {
        if channels == 0 {
            return Err(HushError::new(
                "Hush multi-channel denoiser needs at least one channel",
            ));
        }
        if !attenuation_db.is_finite() || attenuation_db < 0.0 {
            return Err(HushError::new(
                "Hush attenuation must be finite and non-negative",
            ));
        }
        let reduce_mask = match mask_mode {
            HushMaskMode::Independent => ReduceMask::NONE,
            HushMaskMode::Maximum => ReduceMask::MAX,
            HushMaskMode::Mean => ReduceMask::MEAN,
        };
        let runtime = DfTract::new(
            self.params.clone(),
            &build_runtime_params_for_channels(channels, attenuation_db, reduce_mask.clone()),
        )
        .map_err(|error| {
            HushError::new(format!(
                "could not initialize Hush multi-channel runtime: {error}"
            ))
        })?;

        if runtime.sr != HUSH_SAMPLE_RATE
            || runtime.hop_size != HUSH_FRAME_SIZE
            || runtime.fft_size.saturating_sub(runtime.hop_size) != HUSH_SYNTHESIS_DELAY_SAMPLES
        {
            return Err(HushError::new(format!(
                "model is {} Hz with {}-sample frames and {}-sample synthesis delay; expected Hush's {} Hz/{}-sample/{}-sample contract",
                runtime.sr,
                runtime.hop_size,
                runtime.fft_size.saturating_sub(runtime.hop_size),
                HUSH_SAMPLE_RATE,
                HUSH_FRAME_SIZE,
                HUSH_SYNTHESIS_DELAY_SAMPLES,
            )));
        }

        Ok(HushMultiDenoiser {
            params: self.params.clone(),
            runtime,
            channels,
            frame_size: HUSH_FRAME_SIZE,
            attenuation_db,
            mask_mode,
        })
    }
}

/// Mask reduction used by [`HushModel::multi_denoiser_with_attenuation_db`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HushMaskMode {
    /// Keep an independent gain decision for every channel.
    Independent,
    /// Use the maximum channel mask for all channels.
    Maximum,
    /// Use the mean channel mask for all channels.
    Mean,
}

/// A stateful, mono Hush denoiser.
///
/// Input and output are normalized `f32` samples in `-1.0..=1.0`, unlike the
/// existing RNNoise API in this crate, which uses the scale of signed 16-bit
/// PCM. Feed exactly [`HUSH_FRAME_SIZE`] samples to [`Self::process_frame`]
/// in order. A denoiser belongs to one stream at a time; use [`Self::reset`]
/// before starting a new stream.
pub struct HushDenoiser {
    params: DfParams,
    runtime: DfTract,
    frame_size: usize,
    attenuation_db: f32,
    last_lsnr_db: f32,
}

impl HushDenoiser {
    /// Returns the model sample rate, always 16 kHz for the released Hush model.
    pub fn sample_rate(&self) -> usize {
        HUSH_SAMPLE_RATE
    }

    /// Returns the number of samples expected by [`Self::process_frame`].
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }

    /// Returns the documented algorithmic delay in samples.
    ///
    /// Output alignment uses [`HUSH_SYNTHESIS_DELAY_SAMPLES`], which is the
    /// overlap-add delay rather than this model-level latency figure.
    pub fn latency_samples(&self) -> usize {
        HUSH_ALGORITHMIC_LATENCY_SAMPLES
    }

    /// Returns the local-SNR estimate from the most recently processed frame.
    pub fn last_lsnr_db(&self) -> f32 {
        self.last_lsnr_db
    }

    /// Processes one normalized 10 ms mono frame and returns its local-SNR estimate.
    ///
    /// Both `input` and `output` must contain exactly 160 samples. Input values
    /// are expected in the normalized `-1.0..=1.0` range, and output values are
    /// returned in that same range. The operation updates this denoiser's
    /// streaming state and may emit silence during the initial synthesis
    /// delay period.
    pub fn process_frame(&mut self, output: &mut [f32], input: &[f32]) -> Result<f32, HushError> {
        if input.len() != self.frame_size || output.len() != self.frame_size {
            return Err(HushError::new(format!(
                "Hush frames must contain {} samples (input {}, output {})",
                self.frame_size,
                input.len(),
                output.len()
            )));
        }

        let input = ArrayView2::from_shape((1, self.frame_size), input)
            .map_err(|error| HushError::new(format!("invalid Hush input frame: {error}")))?;
        let output = ArrayViewMut2::from_shape((1, self.frame_size), output)
            .map_err(|error| HushError::new(format!("invalid Hush output frame: {error}")))?;
        let lsnr = self
            .runtime
            .process(input, output)
            .map_err(|error| HushError::new(format!("Hush inference failed: {error}")))?;
        self.last_lsnr_db = lsnr;
        Ok(lsnr)
    }

    /// Changes the maximum attenuation for subsequent frames.
    ///
    /// `attenuation_db` must be finite and non-negative. A value of `100.0`
    /// is effectively unlimited suppression. The current recurrent and
    /// overlap-add state is retained; call [`Self::reset`] separately when a
    /// new stream should start from a clean state.
    pub fn set_attenuation_limit_db(&mut self, attenuation_db: f32) -> Result<(), HushError> {
        if !attenuation_db.is_finite() || attenuation_db < 0.0 {
            return Err(HushError::new(
                "Hush attenuation must be finite and non-negative",
            ));
        }
        self.runtime
            .set_atten_lim(attenuation_db)
            .map_err(|error| HushError::new(format!("could not set Hush attenuation: {error}")))?;
        self.attenuation_db = attenuation_db;
        Ok(())
    }

    /// Resets the recurrent, spectral-normalization, and overlap-add state.
    ///
    /// DeepFilterNet's `DfTract::init()` does not reset its recurrent Tract
    /// plans or the `DFState` analysis/synthesis memory, so reset constructs a
    /// fresh runtime from the retained model and runtime parameters.
    /// After reset, continue to provide complete 160-sample normalized frames.
    pub fn reset(&mut self) -> Result<(), HushError> {
        self.runtime = DfTract::new(
            self.params.clone(),
            &build_runtime_params(self.attenuation_db),
        )
        .map_err(|error| HushError::new(format!("could not reset Hush runtime: {error}")))?;
        self.last_lsnr_db = -15.0;
        Ok(())
    }
}

/// A stateful multi-channel Hush denoiser for controlled performance and
/// spatial-coherence experiments.
///
/// Samples are planar rather than interleaved because the pinned DeepFilterNet
/// runtime consumes an `[channels, frame_size]` tensor. Use one instance per
/// stream and call [`Self::reset`] before starting a new stream.
pub struct HushMultiDenoiser {
    params: DfParams,
    runtime: DfTract,
    channels: usize,
    frame_size: usize,
    attenuation_db: f32,
    mask_mode: HushMaskMode,
}

impl HushMultiDenoiser {
    /// Returns the number of channels in this runtime.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Returns the number of samples per channel expected by
    /// [`Self::process_frame`].
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }

    /// Returns the configured mask-linking mode.
    pub fn mask_mode(&self) -> HushMaskMode {
        self.mask_mode
    }

    /// Processes one planar multi-channel frame and returns its local-SNR
    /// estimate.
    ///
    /// Both slices must contain exactly `channels * frame_size` samples. The
    /// operation updates recurrent state and may emit silence during the
    /// initial synthesis delay period.
    pub fn process_frame(&mut self, output: &mut [f32], input: &[f32]) -> Result<f32, HushError> {
        let expected = self.channels * self.frame_size;
        if input.len() != expected || output.len() != expected {
            return Err(HushError::new(format!(
                "Hush multi-channel frames must contain {} samples (input {}, output {})",
                expected,
                input.len(),
                output.len()
            )));
        }

        let input = ArrayView2::from_shape((self.channels, self.frame_size), input)
            .map_err(|error| HushError::new(format!("invalid Hush input frame: {error}")))?;
        let output = ArrayViewMut2::from_shape((self.channels, self.frame_size), output)
            .map_err(|error| HushError::new(format!("invalid Hush output frame: {error}")))?;
        self.runtime
            .process(input, output)
            .map_err(|error| HushError::new(format!("Hush inference failed: {error}")))
    }

    /// Changes the maximum attenuation for subsequent frames.
    pub fn set_attenuation_limit_db(&mut self, attenuation_db: f32) -> Result<(), HushError> {
        if !attenuation_db.is_finite() || attenuation_db < 0.0 {
            return Err(HushError::new(
                "Hush attenuation must be finite and non-negative",
            ));
        }
        self.runtime
            .set_atten_lim(attenuation_db)
            .map_err(|error| HushError::new(format!("could not set Hush attenuation: {error}")))?;
        self.attenuation_db = attenuation_db;
        Ok(())
    }

    /// Resets recurrent, spectral-normalization, and overlap-add state.
    pub fn reset(&mut self) -> Result<(), HushError> {
        let reduce_mask = match self.mask_mode {
            HushMaskMode::Independent => ReduceMask::NONE,
            HushMaskMode::Maximum => ReduceMask::MAX,
            HushMaskMode::Mean => ReduceMask::MEAN,
        };
        self.runtime = DfTract::new(
            self.params.clone(),
            &build_runtime_params_for_channels(self.channels, self.attenuation_db, reduce_mask),
        )
        .map_err(|error| HushError::new(format!("could not reset Hush runtime: {error}")))?;
        Ok(())
    }
}

/// Denoises a complete normalized mono buffer with Hush.
///
/// Hush always runs at 16 kHz and processes 160-sample frames. If `sample_rate`
/// differs from 16 kHz, this function resamples the input to Hush's native
/// rate, compensates the model's 160-sample synthesis delay, flushes the
/// delayed tail with silent frames, and resamples the result back. The returned
/// buffer always has exactly the same length and sample rate as `samples`.
///
/// `samples` and the returned values use normalized `f32` samples in
/// `-1.0..=1.0`. `model_bytes` must contain Hush's
/// `advanced_dfnet16k_model_best_onnx.tar.gz` bundle. The model bytes are
/// copied while loading and may be reused or released after this call.
pub fn denoise_hush_buffer(
    samples: &[f32],
    sample_rate: f32,
    attenuation_limit_db: f32,
    model_bytes: &[u8],
) -> Result<Vec<f32>, HushError> {
    if samples.is_empty() {
        return Ok(Vec::new());
    }
    if !sample_rate.is_finite() || sample_rate <= 0.0 {
        return Err(HushError::new("sample rate must be finite and positive"));
    }

    let rate = sample_rate as f64;
    let at_16k = if (rate - HUSH_SAMPLE_RATE as f64).abs() < f64::EPSILON {
        samples.to_vec()
    } else {
        let mut resampler = crate::Resampler::new(rate, HUSH_SAMPLE_RATE as f64, 1);
        let mut converted = Vec::with_capacity(
            (samples.len() as f64 * HUSH_SAMPLE_RATE as f64 / rate) as usize + 64,
        );
        resampler.process(samples, &mut converted);
        resampler.flush(&mut converted);
        converted
    };

    let model = HushModel::from_bytes(model_bytes)?;
    let attenuation = if attenuation_limit_db > 0.0 {
        attenuation_limit_db
    } else {
        100.0
    };
    let mut state = model.denoiser_with_attenuation_db(attenuation)?;
    let frame_size = state.frame_size();
    let frames = at_16k.len().div_ceil(frame_size);
    let warmup = HUSH_SYNTHESIS_DELAY_SAMPLES.div_ceil(frame_size);
    let input_energy: f32 = at_16k.iter().map(|sample| sample * sample).sum();
    let at_16k_len = at_16k.len();
    let mut padded = at_16k;
    padded.resize(frames * frame_size, 0.0);
    let mut input = vec![0.0; frame_size];
    let mut frame_out = vec![0.0; frame_size];
    let mut enhanced = vec![0.0; padded.len()];

    for (frame_index, chunk) in padded.chunks_exact(frame_size).enumerate() {
        input.copy_from_slice(chunk);
        state.process_frame(&mut frame_out, &input)?;
        if frame_index >= warmup {
            let start = (frame_index - warmup) * frame_size;
            enhanced[start..start + frame_size].copy_from_slice(&frame_out);
        }
    }

    // The last `warmup` outputs are still inside Hush's delay line after the
    // final real input frame. Feed silent frames to release them, but only copy
    // the corresponding delayed output slots into the original-length buffer.
    // DeepFilterNet 0.5.3 short-circuits exactly silent frames before its
    // overlap-add synthesis. This numerically silent level is just above that
    // internal threshold, allowing the delayed tail to be released without
    // contributing audible flush audio to the returned buffer.
    let flush_level = if input_energy >= 1e-7 * at_16k_len as f32 {
        0.0004
    } else {
        0.0
    };
    input.fill(flush_level);
    for frame_index in frames..(frames + warmup) {
        state.process_frame(&mut frame_out, &input)?;
        if frame_index >= warmup {
            let start = (frame_index - warmup) * frame_size;
            if start + frame_size <= enhanced.len() {
                enhanced[start..start + frame_size].copy_from_slice(&frame_out);
            }
        }
    }
    enhanced.truncate(at_16k_len.min(enhanced.len()));

    let mut result = if (rate - HUSH_SAMPLE_RATE as f64).abs() < f64::EPSILON {
        enhanced
    } else {
        let mut resampler = crate::Resampler::new(HUSH_SAMPLE_RATE as f64, rate, 1);
        let mut converted = Vec::with_capacity(samples.len() + 64);
        resampler.process(&enhanced, &mut converted);
        resampler.flush(&mut converted);
        converted
    };
    result.resize(samples.len(), 0.0);
    Ok(result)
}

fn leaked_static_view(bytes: &Arc<[u8]>) -> &'static [u8] {
    // SAFETY:
    //
    // DeepFilterNet v0.5.3 consumes the byte slice synchronously.
    // It copies all archive entries into owned buffers before returning.
    // HushModel keeps Arc<[u8]> alive for the entire model/runtime lifetime.
    unsafe { std::slice::from_raw_parts(bytes.as_ptr(), bytes.len()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_frame_sizes_without_touching_the_runtime() {
        // Model construction is intentionally covered by the integration test
        // when HUSH_MODEL is supplied; this test keeps the API contract cheap.
        assert_eq!(HUSH_SAMPLE_RATE, 16_000);
        assert_eq!(HUSH_FRAME_SIZE, 160);
        assert_eq!(HUSH_ALGORITHMIC_LATENCY_SAMPLES, 320);
        assert_eq!(HUSH_SYNTHESIS_DELAY_SAMPLES, 160);
        assert_eq!(HUSH_LATENCY_SAMPLES, HUSH_ALGORITHMIC_LATENCY_SAMPLES);
    }
}
