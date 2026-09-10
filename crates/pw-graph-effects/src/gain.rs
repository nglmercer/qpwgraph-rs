//! Output gain and optional loudness compensation for denoising effects.
//!
//! The gain stage is deliberately kept separate from the denoiser.  It is
//! applied only after a processor has produced its output, which means a
//! user's boost cannot make the denoiser's input noise louder.

const AUTO_GAIN_MIN_DB: f32 = 0.0;
const AUTO_GAIN_MAX_DB: f32 = 6.0;
const AUTO_GAIN_ATTACK_MS: f32 = 200.0;
const AUTO_GAIN_RELEASE_MS: f32 = 700.0;
const RMS_FLOOR_DB: f32 = -120.0;

/// A post-DSP gain stage with an optional smoothed RMS-matching component.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GainCompensation {
    output_gain_db: f32,
    automatic: bool,
    automatic_gain_db: f32,
}

impl Default for GainCompensation {
    fn default() -> Self {
        Self {
            output_gain_db: 0.0,
            automatic: false,
            automatic_gain_db: 0.0,
        }
    }
}

impl GainCompensation {
    pub(crate) fn set_output_gain_db(&mut self, value: f32) {
        self.output_gain_db = value;
    }

    pub(crate) fn set_automatic(&mut self, enabled: bool) {
        if self.automatic != enabled {
            self.automatic = enabled;
            self.automatic_gain_db = 0.0;
        }
    }

    pub(crate) fn reset(&mut self) {
        self.automatic_gain_db = 0.0;
    }

    /// Apply the configured gain to post-DSP audio.
    ///
    /// `input_rms_db` must describe the signal before DSP and
    /// `processed_rms_db` the signal already written to `buffer`.  Automatic
    /// compensation is limited to 0..=6 dB and follows the target with a
    /// 200 ms attack and 700 ms release.  Smoothing is performed once per
    /// audio frame, not once per interleaved sample.
    pub(crate) fn apply(
        &mut self,
        buffer: &mut [f32],
        channels: usize,
        sample_rate: u32,
        input_rms_db: f32,
        processed_rms_db: f32,
    ) {
        if channels == 0 {
            return;
        }

        if !self.automatic {
            let gain = 10.0_f32.powf(self.output_gain_db / 20.0);
            for sample in buffer {
                *sample = if sample.is_finite() {
                    (*sample * gain).clamp(-1.0, 1.0)
                } else {
                    0.0
                };
            }
            return;
        }

        let target = automatic_target_db(input_rms_db, processed_rms_db);
        let sample_rate = sample_rate.max(1) as f32;
        let attack_step = smoothing_step(AUTO_GAIN_ATTACK_MS, sample_rate);
        let release_step = smoothing_step(AUTO_GAIN_RELEASE_MS, sample_rate);

        for frame in buffer.chunks_exact_mut(channels) {
            let step = if target > self.automatic_gain_db {
                attack_step
            } else {
                release_step
            };
            self.automatic_gain_db += (target - self.automatic_gain_db) * step;
            // Avoid a tiny amount of floating-point drift outside the
            // specified automatic compensation range.
            self.automatic_gain_db = self
                .automatic_gain_db
                .clamp(AUTO_GAIN_MIN_DB, AUTO_GAIN_MAX_DB);
            let gain = 10.0_f32.powf((self.output_gain_db + self.automatic_gain_db) / 20.0);

            for sample in frame {
                *sample = if sample.is_finite() {
                    (*sample * gain).clamp(-1.0, 1.0)
                } else {
                    0.0
                };
            }
        }
    }
}

/// Return the RMS level in dBFS for an interleaved buffer.
pub(crate) fn rms_db(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return RMS_FLOOR_DB;
    }

    let mut sum = 0.0_f64;
    for &sample in samples {
        let sample = if sample.is_finite() {
            f64::from(sample)
        } else {
            0.0
        };
        sum += sample * sample;
    }
    let rms = (sum / samples.len() as f64).sqrt() as f32;
    if rms > 0.0 && rms.is_finite() {
        (20.0 * rms.log10()).max(RMS_FLOOR_DB)
    } else {
        RMS_FLOOR_DB
    }
}

fn automatic_target_db(input_rms_db: f32, processed_rms_db: f32) -> f32 {
    if !input_rms_db.is_finite()
        || !processed_rms_db.is_finite()
        || input_rms_db <= RMS_FLOOR_DB
        || processed_rms_db <= RMS_FLOOR_DB
    {
        0.0
    } else {
        (input_rms_db - processed_rms_db).clamp(AUTO_GAIN_MIN_DB, AUTO_GAIN_MAX_DB)
    }
}

fn smoothing_step(milliseconds: f32, sample_rate: f32) -> f32 {
    let time_constant = (milliseconds * 0.001 * sample_rate).max(1.0);
    1.0 - (-1.0 / time_constant).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_gain_leaves_samples_unchanged() {
        let mut gain = GainCompensation::default();
        let mut audio = [0.5, -0.25];
        let rms = rms_db(&audio);

        gain.apply(&mut audio, 1, 48_000, rms, rms);

        assert_eq!(audio, [0.5, -0.25]);
    }

    #[test]
    fn positive_gain_is_applied_after_processing_and_clipped() {
        let mut gain = GainCompensation::default();
        gain.set_output_gain_db(6.0);
        let mut audio = [0.5, -0.75];
        let rms = rms_db(&audio);

        gain.apply(&mut audio, 1, 48_000, rms, rms);

        assert!((audio[0] - 0.99763113).abs() < 1e-5);
        assert_eq!(audio[1], -1.0);
    }

    #[test]
    fn automatic_compensation_is_limited_and_smoothed() {
        let mut gain = GainCompensation::default();
        gain.set_automatic(true);
        let input_db = rms_db(&[0.5; 256]);
        let processed_db = rms_db(&[0.25; 256]);
        let mut audio = [0.25; 256];

        gain.apply(&mut audio, 1, 48_000, input_db, processed_db);

        // The 200 ms attack means the first block is only part-way to +6 dB.
        assert!(audio[0] > 0.25);
        assert!(audio[0] < 0.5);
        assert!(gain.automatic_gain_db > 0.0);
        assert!(gain.automatic_gain_db <= AUTO_GAIN_MAX_DB);
    }

    #[test]
    fn rms_of_silence_is_safe() {
        assert_eq!(rms_db(&[0.0; 4]), RMS_FLOOR_DB);
        assert_eq!(rms_db(&[f32::NAN, f32::INFINITY]), RMS_FLOOR_DB);
    }
}
