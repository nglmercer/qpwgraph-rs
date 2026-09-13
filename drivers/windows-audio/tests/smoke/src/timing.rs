//! Unit-independent WASAPI clock checks. QPC values from IAudioClock are in
//! 100 ns units; device positions must be divided by GetFrequency, not Hz.

pub const HNS_PER_SECOND: u64 = 10_000_000;
pub const MAX_ERROR_HNS: u128 = 500_000; // 50 ms smoke bound, not an HLK limit.

#[derive(Clone, Copy, Debug)]
pub struct ClockReading {
    pub position: u64,
    pub qpc_hns: u64,
}

pub struct ClockWindow {
    frequency: u64,
    first: ClockReading,
    previous: ClockReading,
    pub samples: u64,
    pub max_error_hns: u128,
}

impl ClockWindow {
    pub fn new(frequency: u64, first: ClockReading) -> Result<Self, String> {
        if frequency == 0 || first.qpc_hns == 0 {
            return Err("clock frequency or running QPC timestamp is zero".into());
        }
        Ok(Self {
            frequency,
            first,
            previous: first,
            samples: 1,
            max_error_hns: 0,
        })
    }

    pub fn observe(&mut self, reading: ClockReading) -> Result<(), String> {
        if reading.position < self.previous.position || reading.qpc_hns < self.previous.qpc_hns {
            return Err(format!(
                "clock moved backwards: {:?} -> {reading:?}",
                self.previous
            ));
        }
        let audio_hns = u128::from(reading.position - self.first.position)
            * u128::from(HNS_PER_SECOND)
            / u128::from(self.frequency);
        let qpc_hns = u128::from(reading.qpc_hns - self.first.qpc_hns);
        self.max_error_hns = self.max_error_hns.max(audio_hns.abs_diff(qpc_hns));
        self.previous = reading;
        self.samples += 1;
        Ok(())
    }

    pub fn finish(&self) -> Result<(), String> {
        if self.samples < 10 || self.previous.qpc_hns - self.first.qpc_hns < HNS_PER_SECOND / 2 {
            return Err("clock window has insufficient samples or elapsed time".into());
        }
        if self.previous.position == self.first.position || self.max_error_hns > MAX_ERROR_HNS {
            return Err(format!("clock failed to advance at its declared frequency: maximum error {} us (limit 50000 us)", self.max_error_hns / 10));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(frequency: u64, speed_percent: u64) -> ClockWindow {
        let mut window = ClockWindow::new(
            frequency,
            ClockReading {
                position: 0,
                qpc_hns: 1,
            },
        )
        .unwrap();
        for n in 1..=100 {
            window
                .observe(ClockReading {
                    position: frequency * n * speed_percent / 10_000,
                    qpc_hns: 1 + HNS_PER_SECOND * n / 100,
                })
                .unwrap();
        }
        window
    }

    #[test]
    fn position_units_are_not_assumed_to_be_audio_frames() {
        for frequency in [48_000, 192_000, 384_000, 10_000_000] {
            assert!(window(frequency, 100).finish().is_ok());
        }
    }

    #[test]
    fn stalled_fast_and_slow_clocks_fail() {
        for speed in [0, 50, 90, 110, 200] {
            assert!(window(48_000, speed).finish().is_err());
        }
    }

    #[test]
    fn invalid_frequency_short_window_and_backward_readings_fail() {
        let first = ClockReading {
            position: 100,
            qpc_hns: 100,
        };
        assert!(ClockWindow::new(0, first).is_err());
        assert!(ClockWindow::new(
            48_000,
            ClockReading {
                position: 0,
                qpc_hns: 0
            }
        )
        .is_err());
        assert!(ClockWindow::new(48_000, first).unwrap().finish().is_err());
        for reading in [
            ClockReading {
                position: 99,
                qpc_hns: 101,
            },
            ClockReading {
                position: 101,
                qpc_hns: 99,
            },
        ] {
            assert!(ClockWindow::new(48_000, first)
                .unwrap()
                .observe(reading)
                .is_err());
        }
    }

    #[test]
    fn large_positions_do_not_overflow_and_transient_errors_are_retained() {
        let mut span = ClockWindow::new(
            1,
            ClockReading {
                position: 0,
                qpc_hns: 1,
            },
        )
        .unwrap();
        span.observe(ClockReading {
            position: u64::MAX,
            qpc_hns: u64::MAX,
        })
        .unwrap();
        assert!(span.max_error_hns > u128::from(u64::MAX));
        let mut span = window(48_000, 100);
        span.observe(ClockReading {
            position: 96_000,
            qpc_hns: 11_000_001,
        })
        .unwrap();
        span.observe(ClockReading {
            position: 96_000,
            qpc_hns: 20_000_001,
        })
        .unwrap();
        assert!(span.finish().is_err());
    }
}
