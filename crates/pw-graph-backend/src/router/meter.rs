//! Level metering for audio the router owns.
//!
//! This is the piece that closes the RMS gap in the parity matrix. Windows'
//! `IAudioMeterInformation` is a *peak* meter and has no RMS reading, so the
//! Core Audio backend reports `rms: 0.0` and honestly declines the capability.
//! Once the router owns the PCM there is nothing stopping a real RMS, and this
//! module computes both from the same block.
//!
//! Meters are read from the UI thread and written from the audio thread, so
//! every field is an atomic and no reader can ever block a writer. `f32` is
//! carried through `AtomicU32` by its bit pattern, which is exact.
//!
//! The reading is defined as **post-effect**: what the sink is about to
//! receive, not what the source produced. That matches PipeWire, where a meter
//! sits on the port it is attached to, and it is the definition §12.3 of the
//! parity roadmap asks to fix cross-platform.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// One meter's current state.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterReading {
    /// Loudest absolute sample since the previous read, 0..=1 for audio that
    /// has not clipped.
    pub peak: f32,
    /// Root-mean-square level of the most recent block.
    pub rms: f32,
    /// Milliseconds of audio processed since this meter last saw a block.
    pub age_ms: u32,
    /// Whether the meter has ever been fed. A meter on a route that has not
    /// started is unavailable rather than silent, and the two must not look
    /// the same in the UI.
    pub available: bool,
}

/// A lock-free level meter for one point in the graph.
#[derive(Debug, Default)]
pub struct MeterCell {
    /// Peak held since the last read, so a transient between two polls is
    /// still seen. Single-consumer: [`MeterCell::read`] clears it.
    peak: AtomicU32,
    /// RMS of the most recent block. Not held, because an averaged level that
    /// never falls is not a level.
    rms: AtomicU32,
    /// Value of the router's frame clock when the last block arrived.
    observed_at: AtomicU64,
    available: AtomicBool,
}

impl MeterCell {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one block of interleaved audio.
    ///
    /// Called from the audio thread. Allocation-free, branch-light, and one
    /// pass over the block: peak and RMS share the same traversal because the
    /// block is already in cache.
    pub fn observe(&self, block: &[f32], frame_clock: u64) {
        let mut peak = 0.0f32;
        let mut sum = 0.0f64;
        for &sample in block {
            // A malformed source or processor must not turn an observational
            // meter into a NaN that survives until the UI reads it. The audio
            // path owns its own fallback; this only keeps the observation
            // deterministic.
            let sample = if sample.is_finite() { sample } else { 0.0 };
            let magnitude = sample.abs();
            if magnitude > peak {
                peak = magnitude;
            }
            sum += f64::from(sample) * f64::from(sample);
        }
        let rms = if block.is_empty() {
            0.0
        } else {
            (sum / block.len() as f64).sqrt() as f32
        };

        // Hold the loudest peak seen since the last read.
        let held = f32::from_bits(self.peak.load(Ordering::Relaxed));
        self.peak.store(peak.max(held).to_bits(), Ordering::Relaxed);
        self.rms.store(rms.to_bits(), Ordering::Relaxed);
        self.observed_at.store(frame_clock, Ordering::Relaxed);
        self.available.store(true, Ordering::Release);
    }

    /// Read the meter and clear the held peak.
    ///
    /// `frame_clock` and `sample_rate` are the router's, which is what makes
    /// `age_ms` deterministic in tests: it measures audio processed, not wall
    /// time, so a paused router reports a frozen age rather than a growing one
    /// that no audio caused.
    pub fn read(&self, frame_clock: u64, sample_rate: u32) -> MeterReading {
        if !self.available.load(Ordering::Acquire) {
            return MeterReading::default();
        }
        let peak = f32::from_bits(self.peak.swap(0.0f32.to_bits(), Ordering::Relaxed));
        let rms = f32::from_bits(self.rms.load(Ordering::Relaxed));
        let observed_at = self.observed_at.load(Ordering::Relaxed);
        let elapsed = frame_clock.saturating_sub(observed_at);
        let age_ms = if sample_rate == 0 {
            0
        } else {
            (elapsed * 1_000 / u64::from(sample_rate)).min(u64::from(u32::MAX)) as u32
        };
        MeterReading {
            peak,
            rms,
            age_ms,
            available: true,
        }
    }

    /// Forget everything, including that the meter was ever fed.
    ///
    /// Used when a route is torn down: a stale peak outliving the route it
    /// described would show a level for audio that is no longer flowing.
    pub fn clear(&self) {
        self.peak.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.rms.store(0.0f32.to_bits(), Ordering::Relaxed);
        self.observed_at.store(0, Ordering::Relaxed);
        self.available.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_meter_that_has_never_seen_audio_is_unavailable_not_silent() {
        let meter = MeterCell::new();
        let reading = meter.read(0, 48_000);
        assert!(!reading.available);
        assert_eq!(reading.peak, 0.0);
        assert_eq!(reading.rms, 0.0);
    }

    #[test]
    fn a_silent_block_makes_the_meter_available_and_reads_zero() {
        let meter = MeterCell::new();
        meter.observe(&[0.0; 16], 0);
        let reading = meter.read(0, 48_000);
        // Silence is a real measurement; the UI must be able to tell it apart
        // from "no meter here".
        assert!(reading.available);
        assert_eq!(reading.peak, 0.0);
        assert_eq!(reading.rms, 0.0);
    }

    #[test]
    fn peak_is_the_loudest_magnitude_regardless_of_sign() {
        let meter = MeterCell::new();
        meter.observe(&[0.25, -0.75, 0.5], 0);
        assert_eq!(meter.read(0, 48_000).peak, 0.75);
    }

    #[test]
    fn rms_of_full_scale_square_audio_is_one() {
        let meter = MeterCell::new();
        meter.observe(&[1.0, -1.0, 1.0, -1.0], 0);
        let reading = meter.read(0, 48_000);
        assert!((reading.rms - 1.0).abs() < 1e-6);
    }

    #[test]
    fn non_finite_samples_are_observed_as_silence() {
        let meter = MeterCell::new();
        meter.observe(&[f32::NAN, f32::INFINITY, -f32::INFINITY, 0.25], 0);
        let reading = meter.read(0, 48_000);

        assert_eq!(reading.peak, 0.25);
        assert!((reading.rms - 0.125).abs() < 1e-6);
    }

    #[test]
    fn rms_is_below_peak_for_a_sine_by_the_expected_root_two() {
        let meter = MeterCell::new();
        let block: Vec<f32> = (0..480)
            .map(|n| (n as f32 * std::f32::consts::TAU / 48.0).sin())
            .collect();
        meter.observe(&block, 0);
        let reading = meter.read(0, 48_000);
        assert!((reading.peak - 1.0).abs() < 1e-3);
        // This is the reading Core Audio cannot give: a sine's RMS is its peak
        // over root two, and a peak-only meter would report 1.0 for both.
        assert!((reading.rms - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3);
    }

    #[test]
    fn a_transient_between_two_reads_is_held_rather_than_missed() {
        let meter = MeterCell::new();
        meter.observe(&[0.9], 0);
        meter.observe(&[0.1], 1);
        // Without the hold, polling at 30 Hz would simply never see a click.
        assert_eq!(meter.read(1, 48_000).peak, 0.9);
    }

    #[test]
    fn the_held_peak_is_cleared_by_reading_it() {
        let meter = MeterCell::new();
        meter.observe(&[0.9], 0);
        assert_eq!(meter.read(0, 48_000).peak, 0.9);
        assert_eq!(meter.read(0, 48_000).peak, 0.0);
    }

    #[test]
    fn rms_follows_the_latest_block_down_instead_of_holding() {
        let meter = MeterCell::new();
        meter.observe(&[1.0, 1.0], 0);
        meter.observe(&[0.0, 0.0], 1);
        assert_eq!(meter.read(1, 48_000).rms, 0.0);
    }

    #[test]
    fn age_counts_audio_processed_since_the_last_block() {
        let meter = MeterCell::new();
        meter.observe(&[0.5], 0);
        // 24,000 frames at 48 kHz is half a second of audio.
        assert_eq!(meter.read(24_000, 48_000).age_ms, 500);
    }

    #[test]
    fn clearing_makes_the_meter_unavailable_again() {
        let meter = MeterCell::new();
        meter.observe(&[0.5], 0);
        meter.clear();
        assert!(!meter.read(0, 48_000).available);
    }
}
