//! Sample-rate conversion.
//!
//! The denoiser only works at 48kHz, so anything else has to be converted first. This is a
//! windowed-sinc (Kaiser) interpolator: for downsampling it moves the filter cutoff down with
//! the ratio, which is what stops the material above the new Nyquist frequency from folding
//! back into the audible band.

/// Number of sinc lobes on each side of the centre, at unity ratio.
const HALF_TAPS: f64 = 16.0;
/// Resolution of the precomputed kernel, in table entries per input sample.
const OVERSAMPLE: usize = 128;
/// Kaiser window shape. About 90dB of stopband rejection.
const KAISER_BETA: f64 = 8.6;

/// Converts a stream of interleaved samples from one sample rate to another.
///
/// The converter is streaming: feed it whatever you have with [`Resampler::process`] and it
/// will emit as many output frames as it can, keeping the leftovers for next time.
///
/// # Example
///
/// ```rust
/// use nnnoiseless::Resampler;
///
/// // 16kHz mono up to the 48kHz the denoiser needs.
/// let mut r = Resampler::new(16_000.0, 48_000.0, 1);
/// let input: Vec<f32> = (0..1600).map(|i| (i as f32 * 0.05).sin()).collect();
/// let mut output = Vec::new();
/// r.process(&input, &mut output);
/// r.flush(&mut output);
/// assert!(output.len() > 4000);
/// ```
#[derive(Clone)]
pub struct Resampler {
    /// Right half of the symmetric kernel, sampled every `1 / OVERSAMPLE` input samples.
    kernel: Vec<f32>,
    /// Table entries per input sample.
    step: f64,
    /// Half the kernel width, in input samples.
    half_width: f64,
    /// Input samples consumed per output sample.
    ratio: f64,
    channels: usize,
    /// Input history, interleaved. Always holds at least `2 * taps` frames once running.
    history: Vec<f32>,
    /// Position of the next output sample, relative to the start of `history`, in frames.
    pos: f64,
    /// Number of input frames dropped off the front of `history` so far.
    taps: usize,
    finished: bool,
}

fn bessel_i0(x: f64) -> f64 {
    // Series expansion; converges quickly for the range of arguments a Kaiser window needs.
    let mut sum = 1.0;
    let mut term = 1.0;
    let half_x_sq = (x / 2.0) * (x / 2.0);
    for k in 1..64 {
        term *= half_x_sq / (k as f64 * k as f64);
        sum += term;
        if term < 1e-18 * sum {
            break;
        }
    }
    sum
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        let pix = std::f64::consts::PI * x;
        pix.sin() / pix
    }
}

impl Resampler {
    /// Creates a resampler from `in_rate` to `out_rate` for interleaved audio.
    ///
    /// # Panics
    ///
    /// Panics if either rate is not finite and positive, or if `channels` is zero.
    pub fn new(in_rate: f64, out_rate: f64, channels: usize) -> Resampler {
        assert!(
            in_rate.is_finite() && in_rate > 0.0,
            "input sample rate must be finite and positive"
        );
        assert!(
            out_rate.is_finite() && out_rate > 0.0,
            "output sample rate must be finite and positive"
        );
        assert!(channels > 0, "need at least one channel");

        let ratio = in_rate / out_rate;
        // When downsampling, the filter has to cut off at the *output* Nyquist frequency,
        // expressed relative to the input rate. When upsampling, the input's own Nyquist
        // frequency is the limit. The 0.95 leaves a little transition band.
        let cutoff = 0.95 * (1.0f64).min(1.0 / ratio);
        // Holding the number of lobes fixed means the kernel gets wider as the cutoff drops.
        let half_width = HALF_TAPS / cutoff;
        let taps = half_width.ceil() as usize + 1;

        let table_len = (half_width * OVERSAMPLE as f64).ceil() as usize + 2;
        let denom = bessel_i0(KAISER_BETA);
        let kernel: Vec<f32> = (0..table_len)
            .map(|i| {
                let t = i as f64 / OVERSAMPLE as f64;
                if t > half_width {
                    return 0.0;
                }
                let w = t / half_width;
                let window = bessel_i0(KAISER_BETA * (1.0 - w * w).max(0.0).sqrt()) / denom;
                (cutoff * sinc(cutoff * t) * window) as f32
            })
            .collect();

        Resampler {
            kernel,
            step: OVERSAMPLE as f64,
            half_width,
            ratio,
            channels,
            // Pre-load silence so that the first real sample is already centred in the kernel.
            history: vec![0.0; taps * channels],
            pos: taps as f64,
            taps,
            finished: false,
        }
    }

    /// A resampler that converts to the 48kHz the denoiser requires.
    pub fn to_denoiser_rate(in_rate: f64, channels: usize) -> Resampler {
        Resampler::new(in_rate, 48_000.0, channels)
    }

    /// The number of channels this resampler was built for.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Input samples consumed per output sample. Greater than one when downsampling.
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Whether this conversion is a no-op.
    pub fn is_identity(&self) -> bool {
        (self.ratio - 1.0).abs() < 1e-12
    }

    /// Reserves room for `additional_frames` of input without changing the
    /// converter's streaming state. Call this during setup when the converter
    /// will be fed from a realtime callback.
    pub fn reserve(&mut self, additional_frames: usize) {
        self.history
            .reserve(additional_frames.saturating_mul(self.channels));
    }

    /// Forgets buffered samples and returns the converter to its initial state.
    pub fn reset(&mut self) {
        self.history.clear();
        self.history
            .resize(self.taps.saturating_mul(self.channels), 0.0);
        self.pos = self.taps as f64;
        self.finished = false;
    }

    #[inline]
    fn tap(&self, t: f64) -> f32 {
        // Linear interpolation between kernel table entries. The table is dense enough that
        // this is far below the stopband.
        let x = t.abs() * self.step;
        let i = x as usize;
        if i + 1 >= self.kernel.len() {
            return 0.0;
        }
        let frac = (x - i as f64) as f32;
        self.kernel[i] + frac * (self.kernel[i + 1] - self.kernel[i])
    }

    /// Feeds interleaved input in, and appends whatever output is ready to `output`.
    ///
    /// # Panics
    ///
    /// Panics if `input.len()` is not a multiple of the channel count.
    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        assert_eq!(
            input.len() % self.channels,
            0,
            "input length must be a whole number of frames"
        );
        self.history.extend_from_slice(input);
        self.emit(output, false);
    }

    /// Flushes the tail of the stream, padding with silence so the last real samples are fully
    /// reconstructed. After this the resampler should not be fed any more input.
    pub fn flush(&mut self, output: &mut Vec<f32>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.history
            .extend(std::iter::repeat_n(0.0, self.taps * self.channels));
        self.emit(output, true);
    }

    fn emit(&mut self, output: &mut Vec<f32>, flushing: bool) {
        let ch = self.channels;
        let frames = self.history.len() / ch;
        // We can only produce an output sample once every input sample its kernel touches has
        // arrived.
        let limit = frames as f64 - self.half_width - 1.0;

        while self.pos < limit {
            let centre = self.pos;
            let first = ((centre - self.half_width).ceil()).max(0.0) as usize;
            let last = ((centre + self.half_width).floor() as usize).min(frames - 1);

            let start = output.len();
            output.resize(start + ch, 0.0);
            let mut norm = 0.0f32;
            for j in first..=last {
                let w = self.tap(centre - j as f64);
                if w == 0.0 {
                    continue;
                }
                norm += w;
                let base = j * ch;
                for c in 0..ch {
                    output[start + c] += w * self.history[base + c];
                }
            }
            // Normalizing by the actual tap sum keeps the gain flat regardless of where the
            // output sample happens to land between input samples.
            if norm.abs() > 1e-9 {
                for c in 0..ch {
                    output[start + c] /= norm;
                }
            }

            self.pos += self.ratio;
        }

        if flushing {
            return;
        }

        // Drop input we will never look at again, and move `pos` to match.
        let keep_from = ((self.pos - self.half_width).floor() as isize - 1).max(0) as usize;
        if keep_from > 0 {
            self.history.drain(..(keep_from * ch));
            self.pos -= keep_from as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    fn tone(n: usize, freq: f64, rate: f64) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * freq * i as f64 / rate).sin() as f32)
            .collect()
    }

    fn resample(input: &[f32], from: f64, to: f64, channels: usize) -> Vec<f32> {
        let mut r = Resampler::new(from, to, channels);
        let mut out = Vec::new();
        // Feed it in irregular chunks, to exercise the streaming buffering.
        for chunk in input.chunks(437 * channels) {
            r.process(chunk, &mut out);
        }
        r.flush(&mut out);
        out
    }

    #[test]
    fn output_length_follows_the_ratio() {
        for &(from, to) in &[(16000.0, 48000.0), (44100.0, 48000.0), (96000.0, 48000.0)] {
            let input = tone(24000, 440.0, from);
            let out = resample(&input, from, to, 1);
            let expected = input.len() as f64 * to / from;
            let err = (out.len() as f64 - expected).abs();
            assert!(
                err < 0.02 * expected,
                "{from} -> {to}: got {} want ~{expected}",
                out.len()
            );
        }
    }

    /// A tone well inside the passband must come through at the same amplitude and frequency.
    #[test]
    fn a_tone_survives_upsampling() {
        let from = 16000.0;
        let to = 48000.0;
        let input = tone(16000, 440.0, from);
        let out = resample(&input, from, to, 1);

        // Skip the edges, where the filter is still filling.
        let body = &out[2000..(out.len() - 2000)];
        let want = rms(&input[2000..(input.len() - 2000)]);
        assert!(
            (rms(body) - want).abs() < 0.05 * want,
            "amplitude drifted: {} vs {want}",
            rms(body)
        );

        // Count zero crossings to confirm the frequency was preserved.
        let crossings = body
            .windows(2)
            .filter(|w| w[0] <= 0.0 && w[1] > 0.0)
            .count();
        let expected = 440.0 * body.len() as f64 / to;
        assert!(
            (crossings as f64 - expected).abs() < 0.02 * expected,
            "frequency drifted: {crossings} crossings, expected ~{expected}"
        );
    }

    /// The reason the cutoff tracks the ratio: content above the new Nyquist frequency must be
    /// filtered out rather than folded back down into the audible band.
    #[test]
    fn downsampling_rejects_content_above_the_new_nyquist() {
        let from = 48000.0;
        let to = 16000.0;
        // 20kHz is far above the 8kHz Nyquist frequency of the output.
        let input = tone(48000, 20000.0, from);
        let out = resample(&input, from, to, 1);

        let body = &out[2000..(out.len() - 2000)];
        assert!(
            rms(body) < 0.02,
            "aliased energy leaked through: {}",
            rms(body)
        );
    }

    #[test]
    fn a_passband_tone_survives_downsampling() {
        let from = 48000.0;
        let to = 16000.0;
        let input = tone(48000, 440.0, from);
        let out = resample(&input, from, to, 1);
        let body = &out[2000..(out.len() - 2000)];
        assert!(
            (rms(body) - rms(&input[2000..40000])).abs() < 0.05,
            "amplitude drifted: {}",
            rms(body)
        );
    }

    /// Channels must not bleed into each other.
    #[test]
    fn channels_stay_separate() {
        let from = 24000.0;
        let to = 48000.0;
        let left = tone(12000, 300.0, from);
        let right: Vec<f32> = std::iter::repeat_n(0.0, 12000).collect();
        let interleaved: Vec<f32> = left
            .iter()
            .zip(&right)
            .flat_map(|(&l, &r)| [l, r])
            .collect();

        let out = resample(&interleaved, from, to, 2);
        assert_eq!(out.len() % 2, 0);
        let right_out: Vec<f32> = out.as_chunks::<2>().0.iter().map(|c| c[1]).collect();
        let left_out: Vec<f32> = out.as_chunks::<2>().0.iter().map(|c| c[0]).collect();
        assert!(
            rms(&right_out) < 1e-6,
            "silence leaked: {}",
            rms(&right_out)
        );
        assert!(rms(&left_out) > 0.5);
    }

    #[test]
    fn identity_ratio_is_reported_and_transparent() {
        let r = Resampler::new(48000.0, 48000.0, 1);
        assert!(r.is_identity());
        assert_eq!(r.channels(), 1);

        let input = tone(4800, 1000.0, 48000.0);
        let out = resample(&input, 48000.0, 48000.0, 1);
        let n = out.len().min(input.len());
        let body = 1000..(n - 1000);
        for i in body {
            assert!(
                (out[i] - input[i]).abs() < 0.02,
                "sample {i}: {} vs {}",
                out[i],
                input[i]
            );
        }
    }

    #[test]
    fn flush_is_idempotent() {
        let mut r = Resampler::new(16000.0, 48000.0, 1);
        let mut out = Vec::new();
        r.process(&tone(1600, 440.0, 16000.0), &mut out);
        r.flush(&mut out);
        let len = out.len();
        r.flush(&mut out);
        assert_eq!(out.len(), len);
    }

    #[test]
    #[should_panic(expected = "whole number of frames")]
    fn ragged_input_panics() {
        let mut r = Resampler::new(16000.0, 48000.0, 2);
        let mut out = Vec::new();
        r.process(&[0.0, 0.0, 0.0], &mut out);
    }
}
