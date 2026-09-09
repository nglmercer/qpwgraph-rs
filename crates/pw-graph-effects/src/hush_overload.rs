//! Hush scheduling, realtime-factor, and health policy.
//!
//! This module contains only control/worker-side policy.  It deliberately has
//! no dependency on the PipeWire callback or the neural channel implementation
//! so the overload thresholds can be tested independently.

use std::time::Duration;

pub(crate) const HUSH_SCHEDULING_MS: u32 = 40;
pub(crate) const HUSH_MIN_WORKER_HEADROOM_MS: u32 = 10;
pub(crate) const HUSH_MAX_BACKLOG_MS: u32 = 80;
pub(crate) const HUSH_RECOVERY_LEAD_MS: u32 = 20;
pub(crate) const HUSH_OVERLOAD_COOLDOWN_MS: u64 = 500;
pub(crate) const HUSH_REALTIME_FACTOR_LIMIT: f64 = 1.10;
pub(crate) const HUSH_HEALTHY_REALTIME_FACTOR_LIMIT: f64 = 0.90;
pub(crate) const HUSH_PERFORMANCE_WINDOW_MS: u64 = 50;
pub(crate) const HUSH_OVERLOAD_OBSERVATIONS: u32 = 3;
pub(crate) const HUSH_HEALTHY_OBSERVATIONS: u32 = 3;
pub(crate) const HUSH_MAX_AUTOMATIC_RETRIES: u32 = 3;

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HushOverloadReason {
    None = 0,
    CpuTooSlow = 1,
    BacklogExceeded = 2,
    InputQueueFull = 3,
}

pub(crate) fn overload_cooldown_for_attempt(attempt: u32) -> Duration {
    if attempt >= 3 {
        Duration::from_millis(5_000)
    } else {
        Duration::from_millis(HUSH_OVERLOAD_COOLDOWN_MS.saturating_mul(1_u64 << attempt))
    }
}

impl HushOverloadReason {
    pub(crate) fn as_u32(self) -> u32 {
        self as u32
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::CpuTooSlow => "CPU throughput",
            Self::BacklogExceeded => "backlog",
            Self::InputQueueFull => "queue full",
        }
    }
}

/// Timing knowledge is independent from the mutable DSP pipeline.  Resetting
/// a denoiser must not erase the evidence that a worker is too slow.
#[derive(Clone, Debug)]
pub(crate) struct HushPerformanceState {
    pub(crate) ewma_realtime_factor: f64,
    pub(crate) total_audio_frames: u64,
    pub(crate) total_processing_ns: u64,
    pub(crate) recent_audio_frames: u64,
    pub(crate) recent_processing_ns: u64,
    pub(crate) overload_windows: u32,
    pub(crate) healthy_windows: u32,
    pub(crate) last_factor: f64,
    pub(crate) timing_samples: u64,
    pub(crate) performance_windows: u64,
    pub(crate) stable_p95_ns: f64,
    pub(crate) stable_p99_ns: f64,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PerformanceObservation {
    pub(crate) factor: f64,
    pub(crate) ewma: f64,
}

impl Default for HushPerformanceState {
    fn default() -> Self {
        Self {
            ewma_realtime_factor: 1.0,
            total_audio_frames: 0,
            total_processing_ns: 0,
            recent_audio_frames: 0,
            recent_processing_ns: 0,
            overload_windows: 0,
            healthy_windows: 0,
            last_factor: 1.0,
            timing_samples: 0,
            performance_windows: 0,
            stable_p95_ns: 0.0,
            stable_p99_ns: 0.0,
        }
    }
}

impl HushPerformanceState {
    pub(crate) fn observe(
        &mut self,
        audio_frames: u64,
        processing_ns: u64,
        sample_rate: u32,
    ) -> PerformanceObservation {
        if audio_frames == 0 {
            return PerformanceObservation {
                factor: self.last_factor,
                ewma: self.ewma_realtime_factor,
            };
        }
        self.total_audio_frames = self.total_audio_frames.saturating_add(audio_frames);
        self.total_processing_ns = self.total_processing_ns.saturating_add(processing_ns);
        self.recent_audio_frames = self.recent_audio_frames.saturating_add(audio_frames);
        self.recent_processing_ns = self.recent_processing_ns.saturating_add(processing_ns);
        self.observe_timing(processing_ns);

        let window_frames =
            (u64::from(sample_rate).saturating_mul(HUSH_PERFORMANCE_WINDOW_MS) / 1000).max(1);
        let mut observation = PerformanceObservation {
            factor: self.last_factor,
            ewma: self.ewma_realtime_factor,
        };
        if self.recent_audio_frames >= window_frames {
            let factor = self.recent_processing_ns as f64 * f64::from(sample_rate.max(1))
                / (self.recent_audio_frames as f64 * 1e9);
            self.last_factor = factor;
            if self.performance_windows == 0 {
                self.ewma_realtime_factor = factor;
            } else {
                const ALPHA: f64 = 0.75;
                self.ewma_realtime_factor =
                    ALPHA * self.ewma_realtime_factor + (1.0 - ALPHA) * factor;
            }
            self.recent_audio_frames = 0;
            self.recent_processing_ns = 0;
            if factor.is_finite()
                && factor > HUSH_REALTIME_FACTOR_LIMIT
                && self.ewma_realtime_factor > HUSH_REALTIME_FACTOR_LIMIT
            {
                self.overload_windows = self.overload_windows.saturating_add(1);
                self.healthy_windows = 0;
            } else if factor.is_finite()
                && factor < HUSH_HEALTHY_REALTIME_FACTOR_LIMIT
                && self.ewma_realtime_factor < HUSH_HEALTHY_REALTIME_FACTOR_LIMIT
            {
                self.healthy_windows = self.healthy_windows.saturating_add(1);
                self.overload_windows = 0;
            } else {
                self.overload_windows = 0;
                self.healthy_windows = 0;
            }
            self.performance_windows = self.performance_windows.saturating_add(1);
            observation = PerformanceObservation {
                factor,
                ewma: self.ewma_realtime_factor,
            };
        }
        observation
    }

    fn observe_timing(&mut self, duration_ns: u64) {
        let duration = duration_ns as f64;
        if self.timing_samples == 0 {
            self.stable_p95_ns = duration;
            self.stable_p99_ns = duration;
            self.timing_samples = 1;
            return;
        }
        let p95_rate = if duration > self.stable_p95_ns {
            0.05
        } else {
            0.001
        };
        let p99_rate = if duration > self.stable_p99_ns {
            0.02
        } else {
            0.0005
        };
        self.stable_p95_ns += p95_rate * (duration - self.stable_p95_ns);
        self.stable_p99_ns += p99_rate * (duration - self.stable_p99_ns);
        self.timing_samples = self.timing_samples.saturating_add(1);
    }

    pub(crate) fn lifetime_factor(&self, sample_rate: u32) -> f64 {
        if self.total_audio_frames == 0 {
            0.0
        } else {
            self.total_processing_ns as f64 * f64::from(sample_rate.max(1))
                / (self.total_audio_frames as f64 * 1e9)
        }
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HushWorkerState {
    Starting = 0,
    Running = 1,
    ResyncRequested = 2,
    Warming = 3,
    Failed = 4,
    Recovering = 5,
    Overloaded = 6,
}

impl HushWorkerState {
    pub(crate) fn as_u32(self) -> u32 {
        self as u32
    }
}

/// User-facing health classification derived from worker and delivery
/// telemetry. It never treats an alive-but-too-slow worker as healthy.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HushHealth {
    Starting = 0,
    Healthy = 1,
    Degraded = 2,
    Overloaded = 3,
    Failed = 4,
}

impl HushHealth {
    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "STARTING",
            Self::Healthy => "HEALTHY",
            Self::Degraded => "DEGRADED",
            Self::Overloaded => "OVERLOADED",
            Self::Failed => "FAILED",
        }
    }
}
