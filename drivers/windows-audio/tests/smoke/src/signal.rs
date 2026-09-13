//! Bounded, phase-independent tone detection for the live cable probe.
//! Ten-millisecond windows contain complete periods of both test tones.

pub const APP_TONE_HZ: f64 = 1000.0;
pub const RELAY_TONE_HZ: f64 = 2000.0;

/// Silence requires real packets from both live capture streams.
#[derive(Default)]
pub struct SilenceProbe {
    pub frames: [u64; 2],
    pub peaks: [f32; 2],
}

impl SilenceProbe {
    pub fn record(&mut self, packets: [(u32, f32); 2]) {
        for (index, (frames, peak)) in packets.into_iter().enumerate() {
            self.frames[index] += u64::from(frames);
            self.peaks[index] = self.peaks[index].max(if peak.is_finite() {
                peak.abs()
            } else {
                f32::INFINITY
            });
        }
    }

    pub fn passed(&self) -> bool {
        self.frames.iter().all(|frames| *frames > 0) && self.peaks.iter().all(|peak| *peak <= 0.001)
    }
}

pub struct ToneProbe {
    rate: u32,
    count: u32,
    sums: [[f64; 2]; 2],
    pub amplitudes: [f64; 2],
    pub invalid_samples: u64,
}

impl ToneProbe {
    pub fn new(rate: u32) -> Self {
        Self {
            rate,
            count: 0,
            sums: [[0.0; 2]; 2],
            amplitudes: [0.0; 2],
            invalid_samples: 0,
        }
    }

    pub fn push(&mut self, sample: f32) {
        if !sample.is_finite() {
            self.invalid_samples += 1;
            return;
        }
        for (index, frequency) in [APP_TONE_HZ, RELAY_TONE_HZ].into_iter().enumerate() {
            let phase =
                std::f64::consts::TAU * frequency * f64::from(self.count) / f64::from(self.rate);
            self.sums[index][0] += f64::from(sample) * phase.cos();
            self.sums[index][1] += f64::from(sample) * phase.sin();
        }
        self.count += 1;
        if self.count >= (self.rate / 100).max(1) {
            for index in 0..2 {
                let amplitude =
                    2.0 * self.sums[index][0].hypot(self.sums[index][1]) / f64::from(self.count);
                self.amplitudes[index] = self.amplitudes[index].max(amplitude);
            }
            self.count = 0;
            self.sums = [[0.0; 2]; 2];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_silence_requires_packets_from_both_cables() {
        for packets in [
            [(0, 0.0), (0, 0.0)],
            [(480, 0.0), (0, 0.0)],
            [(0, 0.0), (480, 0.0)],
        ] {
            let mut probe = SilenceProbe::default();
            probe.record(packets);
            assert!(!probe.passed());
        }
        let mut probe = SilenceProbe::default();
        probe.record([(480, 0.0), (0, 0.0)]);
        probe.record([(0, 0.0), (480, 0.0)]);
        assert!(probe.passed());
        assert_eq!(probe.frames, [480, 480]);
    }

    #[test]
    fn stopped_silence_rejects_signal_and_invalid_samples_on_either_cable() {
        for index in 0..2 {
            for peak in [0.002, -0.002, f32::NAN, f32::INFINITY] {
                let mut packets = [(480, 0.0); 2];
                packets[index].1 = peak;
                let mut probe = SilenceProbe::default();
                probe.record(packets);
                probe.record([(480, 0.0); 2]);
                assert!(!probe.passed());
            }
        }
    }

    #[test]
    fn separates_current_tone_from_previous_cable_tone_at_any_phase() {
        for rate in [44100, 48000] {
            for (index, hz) in [APP_TONE_HZ, RELAY_TONE_HZ].into_iter().enumerate() {
                let mut probe = ToneProbe::new(rate);
                for n in 0..rate / 10 {
                    probe.push(
                        (0.25
                            * (std::f64::consts::TAU * hz * f64::from(n) / f64::from(rate) + 0.7)
                                .sin()) as f32,
                    );
                }
                assert!((probe.amplitudes[index] - 0.25).abs() < 0.001);
                assert!(probe.amplitudes[1 - index] < 0.001);
            }
        }
    }

    #[test]
    fn silence_and_dc_do_not_pass_as_test_tones() {
        for value in [0.0, 0.25] {
            let mut probe = ToneProbe::new(48000);
            for _ in 0..4800 {
                probe.push(value);
            }
            assert!(probe.amplitudes.iter().all(|value| *value < 0.001));
        }
        let mut probe = ToneProbe::new(48000);
        probe.push(f32::NAN);
        probe.push(f32::INFINITY);
        assert_eq!(probe.invalid_samples, 2);
    }
}
