//! Bounded, non-realtime Hush processing.
//!
//! The PipeWire and Windows callbacks only enqueue sanitized blocks and read
//! already-produced blocks from this module.  DeepFilterNet/Tract, model
//! state, resampling, and frame assembly all live on the worker thread.

use crate::hush_noise::HushModelLoadInfo;
use nnnoiseless::{HushDenoiser, HushModel, Resampler, HUSH_FRAME_SIZE, HUSH_SAMPLE_RATE};
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The queue is deliberately small and fixed.  A late worker loses a block
/// and the processor uses its aligned dry path; latency cannot grow without
/// bound.
pub(crate) const HUSH_QUEUE_CAPACITY: usize = 128;
/// Initial scheduling allowance, including model-frame assembly and sinc
/// lookahead. The effective value grows when the host quantum or measured
/// worker cost is measured separately for health diagnostics. Signal
/// alignment adds the synthesis delay separately (see hush_noise).
pub(crate) const HUSH_SCHEDULING_MS: u32 = 40;
/// Minimum worker headroom added to the observed host quantum. Measured worker
/// cost is exposed separately and drives overload/recovery state.
pub(crate) const HUSH_MIN_WORKER_HEADROOM_MS: u32 = 10;
/// Maximum amount of input that is useful to process after the realtime
/// producer has advanced. Beyond this point the wet result cannot meet the
/// fixed playout timeline and the worker must resynchronize.
pub(crate) const HUSH_MAX_BACKLOG_MS: u32 = 80;
/// Hysteresis for clearing the overload/recovery state. This is deliberately
/// smaller than the overload threshold so a worker does not oscillate between
/// running and recovering around one boundary.
pub(crate) const HUSH_RECOVERY_LEAD_MS: u32 = 20;
/// A permanently slow worker must not be reset for every rejected callback.
/// Retry only after a control/worker-side cooldown, leaving the audio thread
/// on its continuous aligned-dry timeline in the meantime.
pub(crate) const HUSH_OVERLOAD_COOLDOWN_MS: u64 = 500;
/// Rolling realtime-factor margin used before declaring a worker overloaded.
pub(crate) const HUSH_REALTIME_FACTOR_LIMIT: f64 = 1.10;
/// A worker must report this factor for several persistent observations before
/// it is classified as permanently too slow.  The lower exit threshold gives
/// temporary scheduler stalls room to recover without state flapping.
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

fn overload_cooldown_for_attempt(attempt: u32) -> Duration {
    if attempt >= 3 {
        Duration::from_millis(5_000)
    } else {
        Duration::from_millis(HUSH_OVERLOAD_COOLDOWN_MS.saturating_mul(1_u64 << attempt))
    }
}

impl HushOverloadReason {
    fn as_u32(self) -> u32 {
        self as u32
    }

    fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::CpuTooSlow => "CPU throughput",
            Self::BacklogExceeded => "backlog",
            Self::InputQueueFull => "queue full",
        }
    }
}

/// Timing knowledge is deliberately independent from the mutable DSP
/// pipeline.  In particular, `reset_hush_dsp` must never call `reset` on this
/// value: a slow worker may need several wet resets before its service rate is
/// confidently classified.
#[derive(Clone, Debug)]
struct HushPerformanceState {
    ewma_realtime_factor: f64,
    total_audio_frames: u64,
    total_processing_ns: u64,
    recent_audio_frames: u64,
    recent_processing_ns: u64,
    overload_windows: u32,
    healthy_windows: u32,
    last_factor: f64,
    timing_samples: u64,
    performance_windows: u64,
    stable_p95_ns: f64,
    stable_p99_ns: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct PerformanceObservation {
    factor: f64,
    ewma: f64,
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
    fn observe(
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
            // Seed the EWMA with the first complete interval, but require
            // multiple high intervals before entering overload. This means a
            // single cold inference is visible in diagnostics without being a
            // state transition by itself.
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
        // An inexpensive exponentially weighted high-percentile estimate is
        // sufficient for scheduling. It responds quickly to a sustained slow
        // worker and decays slowly after a one-off cold outlier.
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

    fn lifetime_factor(&self, sample_rate: u32) -> f64 {
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

/// User-facing health classification derived from delivery and worker
/// telemetry.  It deliberately does not call an overloaded effect healthy
/// merely because its worker thread is still alive.
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
    fn label(self) -> &'static str {
        match self {
            Self::Starting => "STARTING",
            Self::Healthy => "HEALTHY",
            Self::Degraded => "DEGRADED",
            Self::Overloaded => "OVERLOADED",
            Self::Failed => "FAILED",
        }
    }
}

#[inline]
fn channel_is_connected(mask: u16, channel: usize) -> bool {
    channel < u16::BITS as usize && mask & (1u16 << channel) != 0
}

#[inline]
fn channel_mask_for_count(channels: usize) -> u16 {
    if channels >= u16::BITS as usize {
        u16::MAX
    } else {
        (1u16 << channels) - 1
    }
}

#[inline]
fn signal_db_from_energy(energy: f64, samples: u64) -> f64 {
    if samples == 0 {
        return -120.0;
    }
    let rms = (energy / samples as f64).sqrt().max(1.0e-12);
    (20.0 * rms.log10()).clamp(-120.0, 12.0)
}

#[inline]
fn signal_db_from_peak(peak: f64) -> f64 {
    (20.0 * peak.max(1.0e-12).log10()).clamp(-120.0, 12.0)
}

#[inline]
fn unix_time_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[derive(Default)]
pub struct HushDiagnostics {
    /// Provenance and one-time load status for the immutable model shared by
    /// this worker. The contents are control-plane metadata, never audio data.
    pub(crate) model: HushModelLoadInfo,
    pub model_init_started: AtomicBool,
    pub model_init_completed: AtomicBool,
    pub model_initialized: AtomicBool,
    pub model_init_duration_us: AtomicU64,
    pub(crate) model_error: Mutex<Option<String>>,
    pub sample_rate: AtomicU32,
    pub channels: AtomicU16,
    pub host_quantum: AtomicU32,
    pub max_host_quantum: AtomicU32,
    pub scheduling_frames: AtomicU64,
    pub synthesis_delay_frames: AtomicU64,
    pub total_latency_frames: AtomicU64,
    pub measured_worker_headroom_frames: AtomicU64,
    pub max_backlog_frames: AtomicU64,
    pub recovery_lead_frames: AtomicU64,
    pub(crate) attenuation_bits: Arc<AtomicU32>,
    pub effective_attenuation_bits: AtomicU32,
    pub(crate) bypass: AtomicBool,
    pub host_disabled: AtomicBool,
    pub wet_blocks_output: AtomicU64,
    pub dry_fallback_blocks: AtomicU64,
    pub wet_frames_output: AtomicU64,
    pub dry_frames_output: AtomicU64,
    pub dry_startup_blocks: AtomicU64,
    pub dry_bypass_blocks: AtomicU64,
    pub dry_underrun_blocks: AtomicU64,
    pub dry_worker_failure_blocks: AtomicU64,
    pub dry_resync_blocks: AtomicU64,
    pub dry_host_disabled_blocks: AtomicU64,
    pub(crate) error: Mutex<Option<String>>,
    pub(crate) timings: Mutex<VecDeque<u64>>,
    #[cfg(test)]
    pub(crate) frame_delay_ms: AtomicU64,
    #[cfg(test)]
    pub(crate) paused: AtomicBool,
    #[cfg(test)]
    pub(crate) reset_paused: AtomicBool,
    #[cfg(test)]
    pub(crate) inject_error: AtomicBool,
    pub underruns: AtomicU64,
    pub input_overruns: AtomicU64,
    pub output_overruns: AtomicU64,
    pub input_blocks_pushed: AtomicU64,
    pub input_blocks_rejected: AtomicU64,
    pub input_frames_rejected: AtomicU64,
    pub wet_frames_dropped: AtomicU64,
    pub stale_input_blocks_dropped: AtomicU64,
    pub stale_output_blocks_dropped: AtomicU64,
    pub resync_requests: AtomicU64,
    pub resync_completed: AtomicU64,
    pub hush_frames_processed: AtomicU64,
    pub inference_errors_total: AtomicU64,
    pub last_inference_duration_us: AtomicU64,
    pub inference_duration_us: AtomicU64,
    pub last_successful_inference_ms: AtomicU64,
    pub last_wet_output_ms: AtomicU64,
    pub input_rms_db_bits: AtomicU64,
    pub input_peak_db_bits: AtomicU64,
    pub output_rms_db_bits: AtomicU64,
    pub output_peak_db_bits: AtomicU64,
    pub wet_dry_delta_rms_db_bits: AtomicU64,
    pub no_audio_input_blocks: AtomicU64,
    pub channel_config_errors: AtomicU64,
    pub max_input_queue_depth: AtomicU64,
    pub max_output_queue_depth: AtomicU64,
    pub max_worker_backlog_frames: AtomicU64,
    pub generation_resets: AtomicU64,
    pub processed_blocks: AtomicU64,
    /// Total worker time, including resampling, buffer handling, and Hush.
    pub inference_ns: AtomicU64,
    pub max_inference_ns: AtomicU64,
    pub input_resample_ns: AtomicU64,
    pub hush_inference_ns: AtomicU64,
    pub output_resample_ns: AtomicU64,
    pub worker_audio_frames_processed: AtomicU64,
    pub worker_processing_ns: AtomicU64,
    pub realtime_factor_bits: AtomicU64,
    pub ewma_realtime_factor_bits: AtomicU64,
    pub last_realtime_factor_bits: AtomicU64,
    pub stable_worker_p95_ns: AtomicU64,
    pub stable_worker_p99_ns: AtomicU64,
    pub performance_windows: AtomicU64,
    pub overload_windows: AtomicU32,
    pub healthy_windows: AtomicU32,
    pub recovery_attempts: AtomicU64,
    pub recovery_successes: AtomicU64,
    pub recovery_failures: AtomicU64,
    pub input_blocks_consumed: AtomicU64,
    pub input_frames_consumed: AtomicU64,
    pub warmup_frames: AtomicU64,
    /// One count is one native 160-sample Hush inference for one channel.
    pub hush_channel_frames_processed: AtomicU64,
    /// Stable graph-derived channel state. `channels` remains the prepared
    /// physical capacity; this mask is the subset that may consume inference
    /// work for the current route.
    pub active_channel_mask: AtomicU16,
    pub active_hush_channels: AtomicU16,
    pub input_blocks_skipped_recovery: AtomicU64,
    pub input_frames_skipped_recovery: AtomicU64,
    pub input_blocks_skipped_overloaded: AtomicU64,
    pub input_frames_skipped_overloaded: AtomicU64,
    pub input_queue_overruns: AtomicU64,
    pub stale_wet_frames_dropped: AtomicU64,
    pub wet_frames_dropped_queue_full: AtomicU64,
    pub wet_frames_dropped_epoch: AtomicU64,
    pub wet_drop_too_old: AtomicU64,
    pub wet_drop_wrong_generation: AtomicU64,
    pub wet_drop_wrong_sequence: AtomicU64,
    pub wet_drop_wrong_channel_count: AtomicU64,
    pub wet_drop_wrong_frame_count: AtomicU64,
    pub wet_drop_other: AtomicU64,
    pub wet_drop_queue_full: AtomicU64,
    pub overload_reason: AtomicU32,
    pub worker_ready: AtomicBool,
    pub worker_failed: AtomicBool,
    pub worker_overloaded: AtomicBool,
    pub worker_state: AtomicU32,
    pub worker_active: AtomicBool,
    pub latest_input_frame: AtomicU64,
    pub worker_input_frame: AtomicU64,
    pub latest_wet_frame: AtomicU64,
    pub playout_frame: AtomicU64,
    pub latest_submitted_frame: AtomicU64,
    pub latest_completed_input_frame: AtomicU64,
}

fn panic_reason(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("unknown panic")
}

impl HushDiagnostics {
    /// Control-thread parameter side channel. PipeWire uses this handle so
    /// slider edits never contend with the callback's processor mutex.
    pub fn set_control_parameter(&self, id: &str, value: f32) -> Result<(), crate::EffectError> {
        use crate::hush_noise::{HUSH_NOISE_SUPPRESSOR_BYPASS, HUSH_NOISE_SUPPRESSOR_REDUCTION};
        if !value.is_finite() {
            return Err(crate::EffectError::InvalidParameter {
                id: id.into(),
                value,
            });
        }
        match id {
            HUSH_NOISE_SUPPRESSOR_REDUCTION => self
                .attenuation_bits
                .store(value.clamp(0.0, 60.0).to_bits(), Ordering::Release),
            HUSH_NOISE_SUPPRESSOR_BYPASS => self.bypass.store(value >= 0.5, Ordering::Release),
            _ => return Err(crate::EffectError::UnsupportedParameter(id.into())),
        }
        Ok(())
    }

    /// Derive a user-facing health state from the current worker and delivery
    /// telemetry. This is a control/UI-thread operation; it only reads
    /// atomics and never touches the worker queues.
    pub fn health(&self) -> HushHealth {
        let worker_state = self.worker_state.load(Ordering::Acquire);
        if self.worker_failed.load(Ordering::Acquire)
            || worker_state == HushWorkerState::Failed.as_u32()
        {
            return HushHealth::Failed;
        }
        if !self.worker_ready.load(Ordering::Acquire)
            || worker_state == HushWorkerState::Starting.as_u32()
        {
            return HushHealth::Starting;
        }
        // Intentional dry operation is not an overload signal. Keep it out of
        // the wet-ratio classifier so a user who enables bypass or the host
        // disables the effect does not get a false CPU failure diagnosis.
        if self.bypass.load(Ordering::Acquire) || self.host_disabled.load(Ordering::Acquire) {
            return HushHealth::Degraded;
        }
        // A live effect with no usable input is a routing/connection
        // condition, not an inference deadline miss. Keep it out of the wet
        // ratio classifier so the diagnostic state says “degraded/no input”
        // instead of falsely claiming CPU overload.
        if self.active_hush_channels.load(Ordering::Acquire) == 0
            && self.no_audio_input_blocks.load(Ordering::Relaxed) > 0
        {
            return HushHealth::Degraded;
        }
        let wet = self.wet_frames_output.load(Ordering::Relaxed);
        let dry = self.dry_frames_output.load(Ordering::Relaxed);
        let delivered = wet.saturating_add(dry);
        let wet_ratio = if delivered == 0 {
            0.0
        } else {
            wet as f64 / delivered as f64
        };
        let ewma = f64::from_bits(self.ewma_realtime_factor_bits.load(Ordering::Relaxed));
        if worker_state == HushWorkerState::Overloaded.as_u32()
            || (delivered > 0 && (wet_ratio < 0.90 || ewma >= 1.0))
        {
            return HushHealth::Overloaded;
        }
        if matches!(
            worker_state,
            state if state == HushWorkerState::ResyncRequested.as_u32()
                || state == HushWorkerState::Recovering.as_u32()
                || state == HushWorkerState::Warming.as_u32()
        ) {
            return HushHealth::Degraded;
        }
        if worker_state == HushWorkerState::Running.as_u32()
            && delivered > 0
            && wet_ratio >= 0.98
            && ewma < 0.90
        {
            HushHealth::Healthy
        } else {
            HushHealth::Degraded
        }
    }

    /// Control/UI thread only: locks the worker's bounded timing/error side
    /// channels and allocates display text. Never call from an audio callback.
    pub fn status_text(&self) -> String {
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let processed = load(&self.processed_blocks);
        let pushed = load(&self.input_blocks_pushed);
        let health = self.health();
        let error = self
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default();
        let mut times: Vec<u64> = self
            .timings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied()
            .collect();
        times.sort_unstable();
        let average = if processed == 0 {
            0.0
        } else {
            load(&self.inference_ns) as f64 / processed as f64 / 1e6
        };
        let p95 = times.get(times.len() * 95 / 100).copied().unwrap_or(0) as f64 / 1e6;
        let p99 = times.get(times.len() * 99 / 100).copied().unwrap_or(0) as f64 / 1e6;
        let p50 = times.get(times.len() * 50 / 100).copied().unwrap_or(0) as f64 / 1e6;
        let total_frames = load(&self.wet_frames_output) + load(&self.dry_frames_output);
        let wet_ratio = if total_frames == 0 {
            0.0
        } else {
            load(&self.wet_frames_output) as f64 * 100.0 / total_frames as f64
        };
        let backlog_frames = self
            .latest_submitted_frame
            .load(Ordering::Relaxed)
            .saturating_sub(self.latest_completed_input_frame.load(Ordering::Relaxed));
        let max_backlog_ms = load(&self.max_worker_backlog_frames) as f64 * 1000.0
            / self.sample_rate.load(Ordering::Relaxed).max(1) as f64;
        let sample_rate = self.sample_rate.load(Ordering::Relaxed).max(1);
        let scheduling_frames = self.scheduling_frames.load(Ordering::Relaxed);
        let total_latency_frames = self.total_latency_frames.load(Ordering::Relaxed);
        let realtime_factor = f64::from_bits(self.realtime_factor_bits.load(Ordering::Relaxed));
        let ewma_factor = f64::from_bits(self.ewma_realtime_factor_bits.load(Ordering::Relaxed));
        let last_factor = f64::from_bits(self.last_realtime_factor_bits.load(Ordering::Relaxed));
        let measured_headroom_frames = load(&self.measured_worker_headroom_frames);
        let overload_reason = match self.overload_reason.load(Ordering::Relaxed) {
            1 => HushOverloadReason::CpuTooSlow,
            2 => HushOverloadReason::BacklogExceeded,
            3 => HushOverloadReason::InputQueueFull,
            _ => HushOverloadReason::None,
        };
        let model_error = self
            .model_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .or_else(|| self.model.error.clone())
            .unwrap_or_default();
        let effective_attenuation =
            f32::from_bits(self.effective_attenuation_bits.load(Ordering::Acquire));
        let requested_attenuation = f32::from_bits(self.attenuation_bits.load(Ordering::Acquire));
        let inference_calls = load(&self.hush_frames_processed);
        let mean_inference_us = if inference_calls == 0 {
            0.0
        } else {
            load(&self.inference_duration_us) as f64 / inference_calls as f64
        };
        let f64_metric = |bits: &AtomicU64| f64::from_bits(bits.load(Ordering::Relaxed));
        let backlog_ms = backlog_frames as f64 * 1000.0 / sample_rate as f64;
        let current_wet_percent = wet_ratio;
        let decompressed_bytes = self
            .model
            .decompressed_bytes
            .map_or_else(|| "unknown".to_owned(), |bytes| bytes.to_string());
        let init_us = load(&self.model_init_duration_us);
        let dry_percent = 100.0 - current_wet_percent;
        let backlog_limit_ms = load(&self.max_backlog_frames) as f64 * 1000.0 / sample_rate as f64;
        let last_inference_ms = load(&self.last_successful_inference_ms);
        let last_wet_ms = load(&self.last_wet_output_ms);
        let overload_cause = if self.active_hush_channels.load(Ordering::Acquire) == 0
            && load(&self.no_audio_input_blocks) > 0
        {
            "no audio input"
        } else {
            overload_reason.label()
        };
        let wet_blocks = load(&self.wet_blocks_output);
        let fallback_blocks = load(&self.dry_fallback_blocks);
        let manual_bypass_blocks = load(&self.dry_bypass_blocks);
        let host_disabled_blocks = load(&self.dry_host_disabled_blocks);
        let wet_drop_other = load(&self.wet_drop_other);
        let wet_drop_queue_full = load(&self.wet_drop_queue_full);
        let model_path = self.model.path.as_deref().unwrap_or("-");
        let manual_bypass_now = self.bypass.load(Ordering::Acquire);
        let host_disabled_now = self.host_disabled.load(Ordering::Acquire);
        let mut status = format!(
            "Hush: {}\nModel: source={} name={} path={model_path} embedded={} load_started={} load_completed={} load_ms={} parse_started={} parse_completed={} initialized={} init_ms={} checksum={} compressed_bytes={} decompressed_bytes={}{}\nAudio: rate={} Hz configured_channels={} active_channels={} quantum={} max_quantum={} callback_budget_ms={:.3} latency_ms={:.1} schedule_frames={} measured_headroom_frames={}\nInference: calls={} native_samples={} mean_us={mean_inference_us:.1} last_us={} errors={} last_success_ms={last_inference_ms} worker_avg_ms={average:.2} p50_ms={:.2} p95_ms={p95:.2} p99_ms={p99:.2} worker_max_ms={:.2} rt_factor={ewma_factor:.2}x last_rt={last_factor:.2}x lifetime_rt={realtime_factor:.2}x input_rms_db={:.1} input_peak_db={:.1} output_rms_db={:.1} output_peak_db={:.1} wet_dry_delta_db={:.1} worker_blocks={} queued={}\nDelivery: wet={current_wet_percent:.1}% dry_fallback={dry_percent:.1}% wet_blocks={wet_blocks} fallback_blocks={fallback_blocks} manual_bypass_blocks={manual_bypass_blocks} manual_now={} host_disabled_blocks={host_disabled_blocks} host_now={} last_wet_ms={last_wet_ms} underruns={} no_input_blocks={}\nQueue: backlog_ms={backlog_ms:.1} max_backlog_ms={max_backlog_ms:.1} limit_ms={:.1} input_queue_peak={} output_queue_peak={} resync={}/{} dropped={} queue_full={wet_drop_queue_full} old={} generation={} sequence={} channels={} frames={} other={wet_drop_other}\nReduction: requested_db={requested_attenuation:.2} effective_db={effective_attenuation:.2}\nCause: {overload_cause}\nError: {error}",
            health.label(),
            self.model.source,
            self.model.name,
            self.model.embedded,
            self.model.load_started,
            self.model.load_completed,
            self.model.load_duration_ms,
            self.model.parse_started,
            self.model.parse_completed,
            self.model_initialized.load(Ordering::Acquire),
            init_us as f64 / 1000.0,
            self.model.checksum,
            self.model.compressed_bytes,
            decompressed_bytes,
            if model_error.is_empty() {
                String::new()
            } else {
                format!(" model_error={model_error}")
            },
            sample_rate,
            self.channels.load(Ordering::Relaxed),
            self.active_hush_channels.load(Ordering::Relaxed),
            self.host_quantum.load(Ordering::Relaxed),
            self.max_host_quantum.load(Ordering::Relaxed),
            self.host_quantum.load(Ordering::Relaxed) as f64 * 1000.0 / sample_rate as f64,
            total_latency_frames as f64 * 1000.0 / sample_rate as f64,
            scheduling_frames,
            measured_headroom_frames,
            inference_calls,
            load(&self.hush_channel_frames_processed),
            load(&self.last_inference_duration_us),
            load(&self.inference_errors_total),
            p50,
            load(&self.max_inference_ns) as f64 / 1e6,
            f64_metric(&self.input_rms_db_bits),
            f64_metric(&self.input_peak_db_bits),
            f64_metric(&self.output_rms_db_bits),
            f64_metric(&self.output_peak_db_bits),
            f64_metric(&self.wet_dry_delta_rms_db_bits),
            processed,
            pushed,
            manual_bypass_now,
            host_disabled_now,
            load(&self.underruns),
            load(&self.no_audio_input_blocks),
            backlog_limit_ms,
            load(&self.max_input_queue_depth),
            load(&self.max_output_queue_depth),
            load(&self.resync_requests),
            load(&self.resync_completed),
            load(&self.wet_frames_dropped),
            load(&self.wet_drop_too_old),
            load(&self.wet_drop_wrong_generation),
            load(&self.wet_drop_wrong_sequence),
            load(&self.wet_drop_wrong_channel_count),
            load(&self.wet_drop_wrong_frame_count),
        );
        status.push_str(&format!(
            "\nTopology: physical_channels={} active_channels={} active_mask=0x{:02x}",
            self.channels.load(Ordering::Relaxed),
            self.active_hush_channels.load(Ordering::Relaxed),
            self.active_channel_mask.load(Ordering::Relaxed),
        ));
        status.push_str(&format!(
            "\nRouting: channel_errors={} input_rejected={} input_overruns={} output_overruns={}",
            load(&self.channel_config_errors),
            load(&self.input_blocks_rejected),
            load(&self.input_overruns),
            load(&self.output_overruns),
        ));
        status
    }
    pub fn failure_reason(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

struct AudioBlock {
    generation: u64,
    wet_epoch: u64,
    start_frame: u64,
    frames: u32,
    channels: u16,
    channel_mask: u16,
    samples: Vec<f32>,
}

impl AudioBlock {
    fn new(max_samples: usize) -> Self {
        Self {
            generation: 0,
            wet_epoch: 0,
            start_frame: 0,
            frames: 0,
            channels: 0,
            channel_mask: 0,
            samples: vec![0.0; max_samples],
        }
    }
}

/// A single-producer/single-consumer ring.  The producer and consumer each
/// own one side of the index pair, so no mutex is involved in `process()`.
struct BlockQueue {
    slots: Box<[UnsafeCell<AudioBlock>]>,
    capacity: usize,
    max_samples: usize,
    write: AtomicUsize,
    read: AtomicUsize,
}

// Each slot is accessed by exactly one side of the SPSC queue while it is
// owned by that side.  Publication is ordered by the acquire/release indices.
unsafe impl Send for BlockQueue {}
unsafe impl Sync for BlockQueue {}

impl BlockQueue {
    fn new(capacity: usize, max_samples: usize) -> Self {
        assert!(capacity.is_power_of_two());
        let slots = (0..capacity)
            .map(|_| UnsafeCell::new(AudioBlock::new(max_samples)))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self {
            slots,
            capacity,
            max_samples,
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn try_push_with(
        &self,
        generation: u64,
        wet_epoch: u64,
        start_frame: u64,
        frames: u32,
        channels: u16,
        channel_mask: u16,
        fill: impl FnOnce(&mut [f32]),
    ) -> bool {
        if frames as usize * channels as usize > self.max_samples {
            return false;
        }
        let write = self.write.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        if write.wrapping_sub(read) >= self.capacity {
            return false;
        }
        let slot = write % self.capacity;
        // SAFETY: only the producer touches a slot between its read and the
        // release publication below.
        let block = unsafe { &mut *self.slots[slot].get() };
        block.generation = generation;
        block.wet_epoch = wet_epoch;
        block.start_frame = start_frame;
        block.frames = frames;
        block.channels = channels;
        block.channel_mask = channel_mask;
        fill(&mut block.samples);
        self.write.store(write.wrapping_add(1), Ordering::Release);
        true
    }

    fn pop_with<R>(&self, read: impl FnOnce(&AudioBlock) -> R) -> Option<R> {
        let read_index = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Acquire);
        if read_index == write {
            return None;
        }
        let slot = read_index % self.capacity;
        // SAFETY: only the consumer reads a slot after the producer's release
        // publication and before releasing it below.
        let block = unsafe { &*self.slots[slot].get() };
        let result = read(block);
        self.read
            .store(read_index.wrapping_add(1), Ordering::Release);
        Some(result)
    }

    fn peek_with<R>(&self, read: impl FnOnce(&AudioBlock) -> R) -> Option<R> {
        let read_index = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Acquire);
        if read_index == write {
            return None;
        }
        let slot = read_index % self.capacity;
        // SAFETY: the consumer owns the front slot until it advances `read`.
        // Peeking does not publish or mutate that slot.
        let block = unsafe { &*self.slots[slot].get() };
        Some(read(block))
    }

    fn max_samples(&self) -> usize {
        self.max_samples
    }

    fn depth(&self) -> usize {
        self.write
            .load(Ordering::Acquire)
            .wrapping_sub(self.read.load(Ordering::Acquire))
            .min(self.capacity)
    }
}

/// Copy overlaps, retaining a partially consumed front block. The destination
/// already contains aligned dry samples for any holes. Only the consumer
/// advances read; a producer must never drop the oldest slot itself.
#[allow(clippy::too_many_arguments)]
fn read_timeline(
    queue: &BlockQueue,
    generation: u64,
    wet_epoch: u64,
    start: u64,
    frames: u32,
    channels: u16,
    diagnostics: Option<&HushDiagnostics>,
    destination: &mut [f32],
) -> usize {
    if channels == 0 || destination.len() < frames as usize * channels as usize {
        if let Some(diagnostics) = diagnostics {
            diagnostics.wet_drop_other.fetch_add(1, Ordering::Relaxed);
        }
        return 0;
    }
    let end = start.saturating_add(frames as u64);
    let mut copied = 0;
    for _ in 0..HUSH_QUEUE_CAPACITY {
        let Some((epoch, pipeline_epoch, first, last)) = queue.peek_with(|b| {
            (
                b.generation,
                b.wet_epoch,
                b.start_frame,
                b.start_frame.saturating_add(b.frames as u64),
            )
        }) else {
            break;
        };
        let metadata = queue.peek_with(|b| {
            (
                b.channels,
                b.frames,
                b.frames as usize * b.channels as usize <= b.samples.len(),
            )
        });
        let Some((block_channels, block_frames, block_fits)) = metadata else {
            break;
        };
        let drop_reason = if epoch != generation {
            Some(0_u8)
        } else if pipeline_epoch != wet_epoch {
            Some(1_u8)
        } else if block_frames == 0 || !block_fits {
            Some(2_u8)
        } else if block_channels != channels {
            Some(3_u8)
        } else if last <= start {
            Some(4_u8)
        } else {
            None
        };
        if let Some(drop_reason) = drop_reason {
            if let Some(diagnostics) = diagnostics {
                diagnostics
                    .stale_output_blocks_dropped
                    .fetch_add(1, Ordering::Relaxed);
                diagnostics
                    .wet_frames_dropped
                    .fetch_add(last.saturating_sub(first), Ordering::Relaxed);
                match drop_reason {
                    0 => {
                        diagnostics
                            .wet_drop_wrong_generation
                            .fetch_add(1, Ordering::Relaxed);
                        diagnostics
                            .stale_wet_frames_dropped
                            .fetch_add(last.saturating_sub(first), Ordering::Relaxed);
                    }
                    1 => {
                        diagnostics
                            .wet_drop_wrong_sequence
                            .fetch_add(1, Ordering::Relaxed);
                        diagnostics
                            .wet_frames_dropped_epoch
                            .fetch_add(last.saturating_sub(first), Ordering::Relaxed);
                    }
                    2 => {
                        diagnostics
                            .wet_drop_wrong_frame_count
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    3 => {
                        diagnostics
                            .wet_drop_wrong_channel_count
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    4 => {
                        diagnostics.wet_drop_too_old.fetch_add(1, Ordering::Relaxed);
                        diagnostics
                            .stale_wet_frames_dropped
                            .fetch_add(last.saturating_sub(first), Ordering::Relaxed);
                    }
                    _ => {
                        diagnostics.wet_drop_other.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            queue.pop_with(|_| ());
            continue;
        }
        if first >= end {
            break;
        }
        queue.peek_with(|b| {
            let from = first.max(start);
            let to = last.min(end);
            let ch = channels as usize;
            let src = (from - first) as usize * ch;
            let dst = (from - start) as usize * ch;
            let len = (to - from) as usize * ch;
            destination[dst..dst + len].copy_from_slice(&b.samples[src..src + len]);
            copied += (to - from) as usize;
        });
        if last <= end {
            queue.pop_with(|_| ());
        } else {
            break;
        }
    }
    copied
}

#[inline]
fn record_maximum(counter: &AtomicU64, value: usize) {
    let value = value as u64;
    let mut maximum = counter.load(Ordering::Relaxed);
    while maximum < value {
        match counter.compare_exchange_weak(maximum, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(observed) => maximum = observed,
        }
    }
}

struct WorkerSignal {
    mutex: Mutex<()>,
    condition: Condvar,
}

impl Default for WorkerSignal {
    fn default() -> Self {
        Self {
            mutex: Mutex::new(()),
            condition: Condvar::new(),
        }
    }
}

pub(crate) struct HushRuntime {
    input: Arc<BlockQueue>,
    output: Arc<BlockQueue>,
    stop: Arc<AtomicBool>,
    signal: Arc<WorkerSignal>,
    generation: Arc<AtomicU64>,
    wet_epoch: Arc<AtomicU64>,
    resync_requested: Arc<AtomicBool>,
    retry_requested: Arc<AtomicBool>,
    retry_generation: Arc<AtomicBool>,
    accept_input: Arc<AtomicBool>,
    attenuation_bits: Arc<AtomicU32>,
    diagnostics: Arc<HushDiagnostics>,
    latest_frame: Arc<AtomicU64>,
    worker: Option<JoinHandle<()>>,
}

impl HushRuntime {
    pub fn spawn(
        model: Arc<HushModel>,
        sample_rate: u32,
        channels: u16,
        max_frames: u32,
        attenuation_db: f32,
        initial_generation: u64,
    ) -> Result<Self, String> {
        // Slot size is at most 10 ms, independent of the host's capacity
        // ceiling. Large callbacks are copied in bounded chunks on enqueue.
        let max_frames = max_frames.min(sample_rate.div_ceil(100).max(1));
        // Tract contains Rc/OpState and is !Send. Construct it on the worker
        // that owns it. Readiness is published through diagnostics so the
        // component loader can decide when to activate without making this
        // runtime constructor block its caller.
        let max_samples = max_frames as usize * channels as usize;
        let input = Arc::new(BlockQueue::new(HUSH_QUEUE_CAPACITY, max_samples));
        let output = Arc::new(BlockQueue::new(HUSH_QUEUE_CAPACITY, max_samples));
        let stop = Arc::new(AtomicBool::new(false));
        let signal = Arc::new(WorkerSignal::default());
        let generation = Arc::new(AtomicU64::new(initial_generation));
        let wet_epoch = Arc::new(AtomicU64::new(0));
        let resync_requested = Arc::new(AtomicBool::new(false));
        let retry_requested = Arc::new(AtomicBool::new(false));
        let retry_generation = Arc::new(AtomicBool::new(false));
        let accept_input = Arc::new(AtomicBool::new(true));
        let attenuation_bits = Arc::new(AtomicU32::new(attenuation_db.to_bits()));
        let model_info = crate::hush_noise::hush_model_load_info();
        let diagnostics = Arc::new(HushDiagnostics {
            model: model_info,
            attenuation_bits: attenuation_bits.clone(),
            effective_attenuation_bits: AtomicU32::new(attenuation_db.max(0.01).to_bits()),
            ..HushDiagnostics::default()
        });
        diagnostics
            .sample_rate
            .store(sample_rate, Ordering::Relaxed);
        diagnostics.channels.store(channels, Ordering::Relaxed);
        diagnostics
            .realtime_factor_bits
            .store(1.0_f64.to_bits(), Ordering::Relaxed);
        diagnostics
            .ewma_realtime_factor_bits
            .store(1.0_f64.to_bits(), Ordering::Relaxed);
        diagnostics
            .last_realtime_factor_bits
            .store(1.0_f64.to_bits(), Ordering::Relaxed);
        for metric in [
            &diagnostics.input_rms_db_bits,
            &diagnostics.input_peak_db_bits,
            &diagnostics.output_rms_db_bits,
            &diagnostics.output_peak_db_bits,
            &diagnostics.wet_dry_delta_rms_db_bits,
        ] {
            metric.store((-120.0_f64).to_bits(), Ordering::Relaxed);
        }
        let base_schedule_frames =
            (sample_rate as usize * HUSH_SCHEDULING_MS as usize).div_ceil(1000);
        let synthesis_delay_frames =
            (sample_rate as usize * nnnoiseless::HUSH_SYNTHESIS_DELAY_SAMPLES).div_ceil(16_000);
        diagnostics
            .scheduling_frames
            .store(base_schedule_frames as u64, Ordering::Relaxed);
        diagnostics
            .synthesis_delay_frames
            .store(synthesis_delay_frames as u64, Ordering::Relaxed);
        diagnostics.total_latency_frames.store(
            (base_schedule_frames + synthesis_delay_frames) as u64,
            Ordering::Relaxed,
        );
        let initial_backlog_frames = (sample_rate as u64 * HUSH_MAX_BACKLOG_MS as u64) / 1000;
        diagnostics
            .max_backlog_frames
            .store(initial_backlog_frames, Ordering::Relaxed);
        diagnostics.recovery_lead_frames.store(
            (sample_rate as u64 * HUSH_RECOVERY_LEAD_MS as u64) / 1000,
            Ordering::Relaxed,
        );
        let latest_frame = Arc::new(AtomicU64::new(0));
        let worker_latest = latest_frame.clone();

        let worker_input = input.clone();
        let worker_output = output.clone();
        let worker_stop = stop.clone();
        let worker_signal = signal.clone();
        let worker_generation = generation.clone();
        let worker_wet_epoch = wet_epoch.clone();
        let worker_resync_requested = resync_requested.clone();
        let worker_retry_requested = retry_requested.clone();
        let worker_retry_generation = retry_generation.clone();
        let worker_accept_input = accept_input.clone();
        let worker_attenuation = attenuation_bits.clone();
        let worker_diagnostics = diagnostics.clone();
        let worker = thread::Builder::new()
            .name("qpwgraph-hush".into())
            .spawn(move || {
                worker_diagnostics
                    .model_init_started
                    .store(true, Ordering::Release);
                let model_init_started = Instant::now();
                let initialized = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    HushWorker::new(model, sample_rate, channels, max_frames, attenuation_db)
                }))
                .map_err(|payload| {
                    format!("Hush initialization panicked: {}", panic_reason(&*payload))
                })
                .and_then(|result| result);
                worker_diagnostics.model_init_duration_us.store(
                    model_init_started
                        .elapsed()
                        .as_micros()
                        .min(u128::from(u64::MAX)) as u64,
                    Ordering::Release,
                );
                worker_diagnostics
                    .model_init_completed
                    .store(true, Ordering::Release);
                let mut worker = match initialized {
                    Ok(mut worker) => {
                        worker_diagnostics
                            .model_initialized
                            .store(true, Ordering::Release);
                        worker_diagnostics
                            .effective_attenuation_bits
                            .store(worker.attenuation_db.to_bits(), Ordering::Release);
                        eprintln!(
                            "INFO hush: model initialized channels={} rate={} init_us={}",
                            channels,
                            sample_rate,
                            worker_diagnostics
                                .model_init_duration_us
                                .load(Ordering::Acquire)
                        );
                        worker.generation = initial_generation;
                        worker
                    }
                    Err(error) => {
                        *worker_diagnostics
                            .model_error
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.clone());
                        *worker_diagnostics
                            .error
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.clone());
                        worker_diagnostics
                            .worker_failed
                            .store(true, Ordering::Release);
                        worker_diagnostics
                            .worker_state
                            .store(HushWorkerState::Failed.as_u32(), Ordering::Release);
                        eprintln!("ERROR hush: model initialization failed: {error}");
                        return;
                    }
                };
                worker_diagnostics
                    .worker_ready
                    .store(true, Ordering::Release);
                let failure_diagnostics = worker_diagnostics.clone();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_loop(
                        &mut worker,
                        worker_input,
                        worker_output,
                        worker_stop,
                        worker_signal,
                        worker_generation,
                        worker_wet_epoch,
                        worker_resync_requested,
                        worker_retry_requested,
                        worker_retry_generation,
                        worker_accept_input,
                        worker_attenuation,
                        worker_diagnostics,
                        worker_latest,
                        (sample_rate as u64 * HUSH_MAX_BACKLOG_MS as u64) / 1000,
                        (sample_rate as u64 * HUSH_RECOVERY_LEAD_MS as u64) / 1000,
                    );
                }));
                if let Err(payload) = result {
                    *failure_diagnostics
                        .error
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) =
                        Some(format!("Hush worker panicked: {}", panic_reason(&*payload)));
                    failure_diagnostics
                        .worker_failed
                        .store(true, Ordering::Release);
                }
            })
            .map_err(|error| format!("could not start Hush worker: {error}"))?;

        Ok(Self {
            input,
            output,
            stop,
            signal,
            generation,
            wet_epoch,
            resync_requested,
            retry_requested,
            retry_generation,
            accept_input,
            attenuation_bits,
            diagnostics,
            latest_frame,
            worker: Some(worker),
        })
    }

    pub fn push(
        &self,
        generation: u64,
        start_frame: u64,
        frames: u32,
        channels: u16,
        channel_mask: u16,
        samples: &[f32],
    ) -> bool {
        self.diagnostics
            .active_channel_mask
            .store(channel_mask, Ordering::Release);
        self.diagnostics.active_hush_channels.store(
            channel_mask.count_ones().min(u32::from(channels)) as u16,
            Ordering::Release,
        );
        self.diagnostics
            .host_quantum
            .store(frames, Ordering::Relaxed);
        self.latest_frame
            .store(start_frame + frames as u64, Ordering::Release);
        self.diagnostics
            .latest_input_frame
            .store(start_frame + frames as u64, Ordering::Release);
        self.diagnostics
            .latest_submitted_frame
            .store(start_frame + frames as u64, Ordering::Release);
        if self.diagnostics.worker_failed.load(Ordering::Acquire) {
            return true;
        }
        if channel_mask == 0 {
            self.diagnostics
                .no_audio_input_blocks
                .fetch_add(1, Ordering::Relaxed);
            return true;
        }
        if !self.accept_input.load(Ordering::Acquire) {
            match self.diagnostics.worker_state.load(Ordering::Acquire) {
                state if state == HushWorkerState::Overloaded.as_u32() => {
                    self.diagnostics
                        .input_blocks_skipped_overloaded
                        .fetch_add(1, Ordering::Relaxed);
                    self.diagnostics
                        .input_frames_skipped_overloaded
                        .fetch_add(frames as u64, Ordering::Relaxed);
                }
                _ => {
                    self.diagnostics
                        .input_blocks_skipped_recovery
                        .fetch_add(1, Ordering::Relaxed);
                    self.diagnostics
                        .input_frames_skipped_recovery
                        .fetch_add(frames as u64, Ordering::Relaxed);
                }
            }
            return true;
        }
        let expected_channels = self.diagnostics.channels.load(Ordering::Acquire);
        let valid_mask = channel_mask_for_count(channels as usize);
        if channels == 0
            || channels != expected_channels
            || channel_mask & !valid_mask != 0
            || samples.len() != frames as usize * channels as usize
        {
            self.diagnostics
                .channel_config_errors
                .fetch_add(1, Ordering::Relaxed);
            self.diagnostics
                .input_blocks_rejected
                .fetch_add(1, Ordering::Relaxed);
            self.diagnostics
                .input_frames_rejected
                .fetch_add(frames as u64, Ordering::Relaxed);
            return false;
        }
        let slot_frames = self.input.max_samples() / channels as usize;
        let mut complete = true;
        let wet_epoch = self.wet_epoch.load(Ordering::Acquire);
        let mut frame_offset = 0usize;
        while frame_offset < frames as usize {
            let chunk_frames = (frames as usize - frame_offset).min(slot_frames);
            let sample_start = frame_offset * channels as usize;
            let sample_end = (frame_offset + chunk_frames) * channels as usize;
            let chunk = &samples[sample_start..sample_end];
            let was_empty = self.input.depth() == 0;
            let pushed = self.input.try_push_with(
                generation,
                wet_epoch,
                start_frame + frame_offset as u64,
                chunk_frames as u32,
                channels,
                channel_mask,
                |destination| destination[..chunk.len()].copy_from_slice(chunk),
            );
            if pushed {
                self.diagnostics
                    .input_blocks_pushed
                    .fetch_add(1, Ordering::Relaxed);
                if was_empty {
                    self.signal.condition.notify_one();
                }
            } else {
                self.diagnostics
                    .input_blocks_rejected
                    .fetch_add(1, Ordering::Relaxed);
                self.diagnostics
                    .input_frames_rejected
                    .fetch_add(chunk_frames as u64, Ordering::Relaxed);
                self.diagnostics
                    .input_overruns
                    .fetch_add(1, Ordering::Relaxed);
                self.diagnostics
                    .input_queue_overruns
                    .fetch_add(1, Ordering::Relaxed);
                self.request_resync(HushOverloadReason::InputQueueFull);
                let remaining_frames = frames as usize - frame_offset - chunk_frames;
                if remaining_frames > 0 {
                    self.diagnostics.input_blocks_rejected.fetch_add(
                        remaining_frames.div_ceil(slot_frames) as u64,
                        Ordering::Relaxed,
                    );
                    self.diagnostics
                        .input_frames_rejected
                        .fetch_add(remaining_frames as u64, Ordering::Relaxed);
                }
                complete = false;
                break;
            }
            complete &= pushed;
            frame_offset += chunk_frames;
        }
        record_maximum(&self.diagnostics.max_input_queue_depth, self.input.depth());
        complete
    }

    /// Observe a host callback without making the callback size a stream
    /// discontinuity. The effective scheduling allowance only grows: a
    /// larger quantum or measured worker cost can never make the wet request
    /// move earlier in time. This is allocation-free and uses atomics only.
    pub fn observe_quantum(&self, frames: u32) -> usize {
        let frames = frames as usize;
        self.diagnostics
            .host_quantum
            .store(frames as u32, Ordering::Relaxed);
        let mut maximum = self.diagnostics.max_host_quantum.load(Ordering::Relaxed);
        while maximum < frames as u32 {
            match self.diagnostics.max_host_quantum.compare_exchange_weak(
                maximum,
                frames as u32,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => maximum = observed,
            }
        }
        let sample_rate = self.diagnostics.sample_rate.load(Ordering::Relaxed).max(1) as usize;
        let measured_ns = self.diagnostics.max_inference_ns.load(Ordering::Relaxed);
        let measured_headroom = ((measured_ns as u128 * sample_rate as u128)
            .div_ceil(1_000_000_000))
        .min(u64::MAX as u128) as u64;
        self.diagnostics
            .measured_worker_headroom_frames
            .store(measured_headroom, Ordering::Relaxed);
        let minimum_headroom = (sample_rate * HUSH_MIN_WORKER_HEADROOM_MS as usize).div_ceil(1000);
        // Once several worker observations exist, use the stable p99 rather
        // than one cold outlier as the timing budget. The schedule is
        // grow-only for the lifetime of this prepared stream, so the dry/wet
        // timeline cannot move earlier during normal operation.
        let stable_p99_frames = if self.diagnostics.performance_windows.load(Ordering::Acquire)
            >= HUSH_OVERLOAD_OBSERVATIONS as u64
        {
            (self
                .diagnostics
                .stable_worker_p99_ns
                .load(Ordering::Acquire) as u128
                * sample_rate as u128)
                .div_ceil(1_000_000_000)
                .min(usize::MAX as u128) as usize
        } else {
            0
        };
        let target = frames
            .saturating_add(minimum_headroom)
            .max(stable_p99_frames.saturating_add(frames))
            .max((sample_rate * HUSH_SCHEDULING_MS as usize).div_ceil(1000));
        let mut current = self.diagnostics.scheduling_frames.load(Ordering::Acquire) as usize;
        while current < target {
            match self.diagnostics.scheduling_frames.compare_exchange_weak(
                current as u64,
                target as u64,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    current = target;
                    break;
                }
                Err(observed) => current = observed as usize,
            }
        }
        let total = current.saturating_add(
            self.diagnostics
                .synthesis_delay_frames
                .load(Ordering::Relaxed) as usize,
        );
        self.diagnostics
            .total_latency_frames
            .store(total as u64, Ordering::Relaxed);
        current
    }

    pub fn pop_ready(
        &self,
        generation: u64,
        start_frame: u64,
        frames: u32,
        channels: u16,
        destination: &mut [f32],
    ) -> usize {
        self.diagnostics
            .playout_frame
            .store(start_frame, Ordering::Release);
        read_timeline(
            &self.output,
            generation,
            self.wet_epoch.load(Ordering::Acquire),
            start_frame,
            frames,
            channels,
            Some(&self.diagnostics),
            destination,
        )
    }

    pub fn is_resyncing(&self) -> bool {
        matches!(
            self.diagnostics.worker_state.load(Ordering::Acquire),
            state if state == HushWorkerState::ResyncRequested.as_u32()
                || state == HushWorkerState::Recovering.as_u32()
                || state == HushWorkerState::Warming.as_u32()
                || state == HushWorkerState::Overloaded.as_u32()
        )
    }

    fn request_resync(&self, reason: HushOverloadReason) {
        let state = self.diagnostics.worker_state.load(Ordering::Acquire);
        if state == HushWorkerState::Overloaded.as_u32()
            || state == HushWorkerState::Failed.as_u32()
        {
            return;
        }
        self.diagnostics
            .overload_reason
            .store(reason.as_u32(), Ordering::Release);
        if state == HushWorkerState::Warming.as_u32() {
            // A queue-full partial push while the fresh epoch is warming is
            // already a failed recovery attempt. Keep the dry timeline and
            // enter the worker's cooldown path; do not reset Hush a second
            // time for the same episode.
            self.accept_input.store(false, Ordering::Release);
            self.diagnostics
                .worker_overloaded
                .store(true, Ordering::Release);
            self.diagnostics
                .worker_state
                .store(HushWorkerState::Overloaded.as_u32(), Ordering::Release);
            return;
        }
        self.accept_input.store(false, Ordering::Release);
        if !self.resync_requested.swap(true, Ordering::AcqRel) {
            self.diagnostics
                .resync_requests
                .fetch_add(1, Ordering::Relaxed);
            self.diagnostics
                .worker_overloaded
                .store(true, Ordering::Release);
            self.diagnostics
                .worker_state
                .store(HushWorkerState::ResyncRequested.as_u32(), Ordering::Release);
        }
    }

    /// Retry request. It only flips atomics; all DSP reset and queue draining
    /// remains off the realtime path. The worker polls its bounded wait, so
    /// this method is also safe when a host delivers a bypass transition from
    /// its audio callback.
    pub fn request_retry(&self) {
        self.request_retry_with_generation(false);
    }

    /// Request a wet retry and record that the caller also advanced the host
    /// stream generation. This keeps generation-reset diagnostics distinct
    /// from worker-only retries such as host re-enable.
    pub fn request_generation_retry(&self) {
        self.request_retry_with_generation(true);
    }

    fn request_retry_with_generation(&self, count_generation: bool) {
        if self.diagnostics.worker_failed.load(Ordering::Acquire) {
            return;
        }
        self.retry_requested.store(true, Ordering::Release);
        if count_generation {
            self.retry_generation.store(true, Ordering::Release);
        }
        self.accept_input.store(false, Ordering::Release);
        if !self.resync_requested.swap(true, Ordering::AcqRel) {
            self.diagnostics
                .resync_requests
                .fetch_add(1, Ordering::Relaxed);
        }
        self.diagnostics
            .overload_reason
            .store(HushOverloadReason::None.as_u32(), Ordering::Release);
        self.diagnostics
            .worker_state
            .store(HushWorkerState::ResyncRequested.as_u32(), Ordering::Release);
    }

    pub fn set_generation(&self, generation: u64) {
        self.generation.store(generation, Ordering::Release);
    }

    pub fn set_attenuation(&self, attenuation_db: f32) {
        self.attenuation_bits
            .store(attenuation_db.to_bits(), Ordering::Release);
    }

    #[cfg(test)]
    pub fn wait_idle(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self
            .diagnostics
            .model_init_completed
            .load(Ordering::Acquire)
            || self.input.depth() != 0
            || self.diagnostics.worker_active.load(Ordering::Acquire)
        {
            assert!(
                !self.diagnostics.worker_failed.load(Ordering::Acquire),
                "{:?}",
                self.diagnostics.error.lock().unwrap()
            );
            assert!(Instant::now() < deadline, "Hush worker did not drain");
            thread::yield_now();
        }
    }

    pub fn diagnostics(&self) -> &Arc<HushDiagnostics> {
        &self.diagnostics
    }
}

impl Drop for HushRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.signal.condition.notify_one();

        if let Some(worker) = self.worker.take() {
            // A processor can be released by a UI/control operation while
            // the worker is still inside a model frame.  Joining here would
            // make effect removal wait on inference.  Hand the join handle to
            // the bounded reaper instead; if its queue is full, dropping the
            // handle detaches the worker after the stop request and remains
            // non-blocking.
            if let Some(reaper) = hush_worker_reaper() {
                match reaper.try_send(worker) {
                    Ok(()) => {}
                    Err(TrySendError::Full(worker) | TrySendError::Disconnected(worker)) => {
                        drop(worker);
                    }
                }
            } else {
                drop(worker);
            }
        }
    }
}

/// One bounded supervisor owns deferred Hush joins.  This avoids creating an
/// unbounded reaper thread per effect removal while keeping all join waits off
/// UI, control, and realtime threads.
fn hush_worker_reaper() -> Option<&'static SyncSender<JoinHandle<()>>> {
    static REAPER: OnceLock<Option<SyncSender<JoinHandle<()>>>> = OnceLock::new();
    REAPER
        .get_or_init(|| {
            let (sender, receiver) = mpsc::sync_channel::<JoinHandle<()>>(64);
            let spawned = thread::Builder::new()
                .name("qpwgraph-hush-reaper".into())
                .spawn(move || {
                    while let Ok(worker) = receiver.recv() {
                        let _ = worker.join();
                    }
                })
                .is_ok();
            spawned.then_some(sender)
        })
        .as_ref()
}

struct HushWorker {
    denoisers: Vec<HushDenoiser>,
    input_resamplers: Vec<Resampler>,
    output_resamplers: Vec<Resampler>,
    input_pending: Vec<Vec<f32>>,
    output_pending: Vec<Vec<f32>>,
    output_offsets: Vec<usize>,
    input_mono: Vec<f32>,
    at_model_rate: Vec<f32>,
    frame_input: Vec<f32>,
    frame_output: Vec<f32>,
    at_host_rate: Vec<f32>,

    channels: usize,
    sample_rate: u32,
    max_frames: usize,
    attenuation_db: f32,
    generation: u64,
    wet_epoch: u64,
    last_mask: u16,
    pristine: bool,
    synthesis_frames: usize,
    warmup_remaining: usize,
    input_position: u64,
    output_position: u64,
    performance: HushPerformanceState,
}

impl HushWorker {
    fn new(
        model: Arc<HushModel>,
        sample_rate: u32,
        channels: u16,
        max_frames: u32,
        attenuation_db: f32,
    ) -> Result<Self, String> {
        let attenuation_db = if attenuation_db.is_finite() {
            attenuation_db.max(0.01)
        } else {
            return Err(format!("Hush attenuation is not finite: {attenuation_db}"));
        };
        let channels = channels as usize;
        let max_frames = max_frames as usize;
        let mut denoisers = Vec::with_capacity(channels);
        let mut input_resamplers = Vec::with_capacity(channels);
        let mut output_resamplers = Vec::with_capacity(channels);
        let mut input_pending = Vec::with_capacity(channels);
        let mut output_pending = Vec::with_capacity(channels);
        let mut output_offsets = Vec::with_capacity(channels);

        let converted_capacity = (max_frames as f64 * HUSH_SAMPLE_RATE as f64 / sample_rate as f64)
            .ceil() as usize
            + HUSH_FRAME_SIZE * 2;
        let output_capacity = (max_frames as f64 * sample_rate as f64 / HUSH_SAMPLE_RATE as f64)
            .ceil() as usize
            + HUSH_FRAME_SIZE * 2;
        for _ in 0..channels {
            denoisers.push(
                model
                    .denoiser_with_attenuation_db(attenuation_db)
                    .map_err(|error| error.to_string())?,
            );
            let mut input_resampler =
                Resampler::new(sample_rate as f64, HUSH_SAMPLE_RATE as f64, 1);
            input_resampler.reserve(max_frames + HUSH_FRAME_SIZE);
            let mut output_resampler =
                Resampler::new(HUSH_SAMPLE_RATE as f64, sample_rate as f64, 1);
            output_resampler.reserve(converted_capacity + HUSH_FRAME_SIZE);
            input_resamplers.push(input_resampler);
            output_resamplers.push(output_resampler);
            let mut input = Vec::with_capacity(converted_capacity + HUSH_FRAME_SIZE);
            input.reserve(converted_capacity + HUSH_FRAME_SIZE);
            input_pending.push(input);
            output_pending.push(Vec::with_capacity(output_capacity + HUSH_FRAME_SIZE));
            output_offsets.push(0);
        }
        Ok(Self {
            denoisers,
            input_resamplers,
            output_resamplers,
            input_pending,
            output_pending,
            output_offsets,
            input_mono: vec![0.0; max_frames],
            at_model_rate: Vec::with_capacity(converted_capacity + HUSH_FRAME_SIZE),
            frame_input: vec![0.0; HUSH_FRAME_SIZE],
            frame_output: vec![0.0; HUSH_FRAME_SIZE],
            at_host_rate: Vec::with_capacity(output_capacity + HUSH_FRAME_SIZE),

            channels,
            sample_rate,
            max_frames,
            attenuation_db,
            generation: 0,
            wet_epoch: 0,
            pristine: true,
            synthesis_frames: (sample_rate as usize * nnnoiseless::HUSH_SYNTHESIS_DELAY_SAMPLES)
                .div_ceil(HUSH_SAMPLE_RATE),
            warmup_remaining: (sample_rate as usize * nnnoiseless::HUSH_SYNTHESIS_DELAY_SAMPLES)
                .div_ceil(HUSH_SAMPLE_RATE),
            input_position: 0,
            output_position: 0,
            performance: HushPerformanceState::default(),
            last_mask: if channels >= 16 {
                u16::MAX
            } else {
                (1u16 << channels) - 1
            },
        })
    }

    fn reset(
        &mut self,
        generation: u64,
        mask: u16,
        diagnostics: &HushDiagnostics,
        count_generation: bool,
    ) -> Result<(), String> {
        self.pristine = true;
        self.warmup_remaining = self.synthesis_frames;
        self.generation = generation;
        self.last_mask = mask;
        if count_generation {
            diagnostics
                .generation_resets
                .fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(test)]
        {
            let deadline = Instant::now() + Duration::from_secs(30);
            while diagnostics.reset_paused.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "test reset gate timed out");
                thread::yield_now();
            }
        }
        for channel in 0..self.channels {
            self.denoisers[channel]
                .reset()
                .map_err(|error| error.to_string())?;
            self.input_resamplers[channel].reset();
            self.output_resamplers[channel].reset();
            self.input_pending[channel].clear();
            self.output_pending[channel].clear();
            self.output_offsets[channel] = 0;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn process_block(
        &mut self,
        block: &AudioBlock,
        output: &Arc<BlockQueue>,
        generation: &AtomicU64,
        wet_epoch: &AtomicU64,
        diagnostics: &HushDiagnostics,
    ) -> Result<(), String> {
        #[cfg(test)]
        if diagnostics.inject_error.swap(false, Ordering::AcqRel) {
            diagnostics
                .inference_errors_total
                .fetch_add(1, Ordering::Relaxed);
            return Err("injected inference failure".into());
        }
        if block.generation != generation.load(Ordering::Acquire)
            || block.wet_epoch != wet_epoch.load(Ordering::Acquire)
        {
            diagnostics
                .stale_input_blocks_dropped
                .fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        // The producer controls the monotonic stream generation. Equality is
        // the only safe comparison here: it also handles the extremely rare
        // u64 wrap without treating the wrapped value as an old generation.
        if block.generation != self.generation || block.channel_mask != self.last_mask {
            self.reset(block.generation, block.channel_mask, diagnostics, true)?;
        }
        if block.wet_epoch != self.wet_epoch {
            diagnostics
                .stale_input_blocks_dropped
                .fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        if self.pristine {
            self.output_position = block.start_frame;
            self.pristine = false;
        }
        let frames = block.frames as usize;
        if frames == 0
            || frames > self.max_frames
            || block.channels as usize != self.channels
            || frames.saturating_mul(block.channels as usize) > block.samples.len()
        {
            diagnostics
                .channel_config_errors
                .fetch_add(1, Ordering::Relaxed);
            return Err(format!(
                "Hush worker received invalid audio block: frames={} channels={} expected_channels={} sample_capacity={}",
                block.frames, block.channels, self.channels, block.samples.len()
            ));
        }
        let valid_mask = channel_mask_for_count(self.channels);
        if block.channel_mask & !valid_mask != 0 {
            diagnostics
                .channel_config_errors
                .fetch_add(1, Ordering::Relaxed);
            return Err(format!(
                "Hush worker received invalid channel mask 0x{:x} for {} channels",
                block.channel_mask, self.channels
            ));
        }
        self.input_position = block.start_frame + block.frames as u64;
        let worker_start = Instant::now();
        let mut input_resample_ns = 0_u64;
        let mut hush_inference_ns = 0_u64;
        let mut output_resample_ns = 0_u64;
        let mut inference_calls = 0_u64;
        let mut input_energy = 0.0_f64;
        let mut output_energy = 0.0_f64;
        let mut delta_energy = 0.0_f64;
        let mut input_peak = 0.0_f64;
        let mut output_peak = 0.0_f64;
        let mut metric_samples = 0_u64;
        let connected_channels = (0..self.channels)
            .filter(|&channel| channel_is_connected(block.channel_mask, channel))
            .count();
        diagnostics
            .active_hush_channels
            .store(connected_channels as u16, Ordering::Release);
        if connected_channels == 0 {
            diagnostics
                .no_audio_input_blocks
                .fetch_add(1, Ordering::Relaxed);
        }
        for channel in 0..self.channels {
            if !channel_is_connected(block.channel_mask, channel) {
                self.input_pending[channel].clear();
                self.output_pending[channel].clear();
                self.output_offsets[channel] = 0;
                self.input_resamplers[channel].reset();
                self.output_resamplers[channel].reset();

                continue;
            }
            let input_started = Instant::now();
            for frame in 0..frames {
                self.input_mono[frame] = block.samples[frame * self.channels + channel];
            }
            self.at_model_rate.clear();
            self.input_resamplers[channel]
                .process(&self.input_mono[..frames], &mut self.at_model_rate);
            self.input_pending[channel].extend_from_slice(&self.at_model_rate);
            input_resample_ns = input_resample_ns
                .saturating_add(input_started.elapsed().as_nanos().min(u64::MAX as u128) as u64);
            while self.input_pending[channel].len() >= HUSH_FRAME_SIZE {
                self.frame_input
                    .copy_from_slice(&self.input_pending[channel][..HUSH_FRAME_SIZE]);
                let pending_len = self.input_pending[channel].len();
                self.input_pending[channel].copy_within(HUSH_FRAME_SIZE.., 0);
                self.input_pending[channel].truncate(pending_len - HUSH_FRAME_SIZE);
                self.frame_output.fill(0.0);
                let hush_started = Instant::now();
                #[cfg(test)]
                thread::sleep(Duration::from_millis(
                    diagnostics.frame_delay_ms.load(Ordering::Relaxed),
                ));
                if let Err(error) =
                    self.denoisers[channel].process_frame(&mut self.frame_output, &self.frame_input)
                {
                    diagnostics
                        .inference_errors_total
                        .fetch_add(1, Ordering::Relaxed);
                    return Err(error.to_string());
                }
                let inference_ns = hush_started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
                hush_inference_ns = hush_inference_ns.saturating_add(inference_ns);
                diagnostics
                    .last_inference_duration_us
                    .store(inference_ns / 1_000, Ordering::Relaxed);
                diagnostics
                    .inference_duration_us
                    .fetch_add(inference_ns / 1_000, Ordering::Relaxed);
                diagnostics
                    .hush_frames_processed
                    .fetch_add(1, Ordering::Relaxed);
                diagnostics
                    .hush_channel_frames_processed
                    .fetch_add(HUSH_FRAME_SIZE as u64, Ordering::Relaxed);
                if self.frame_output.iter().any(|sample| !sample.is_finite()) {
                    diagnostics
                        .inference_errors_total
                        .fetch_add(1, Ordering::Relaxed);
                    return Err("Hush produced a non-finite frame".into());
                }
                for (&input, &output) in self.frame_input.iter().zip(&self.frame_output) {
                    let input = f64::from(input);
                    let output = f64::from(output);
                    input_energy += input * input;
                    output_energy += output * output;
                    let delta = input - output;
                    delta_energy += delta * delta;
                    input_peak = input_peak.max(input.abs());
                    output_peak = output_peak.max(output.abs());
                }
                inference_calls = inference_calls.saturating_add(1);
                metric_samples = metric_samples.saturating_add(HUSH_FRAME_SIZE as u64);
                let output_started = Instant::now();
                self.at_host_rate.clear();
                self.output_resamplers[channel].process(&self.frame_output, &mut self.at_host_rate);
                output_resample_ns = output_resample_ns.saturating_add(
                    output_started.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                );
                if self.at_host_rate.iter().any(|sample| !sample.is_finite()) {
                    return Err("Hush output resampler produced non-finite audio".into());
                }
                self.output_pending[channel].extend_from_slice(&self.at_host_rate);
            }
        }
        let worker_total_ns = worker_start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        diagnostics
            .input_resample_ns
            .fetch_add(input_resample_ns, Ordering::Relaxed);
        diagnostics
            .hush_inference_ns
            .fetch_add(hush_inference_ns, Ordering::Relaxed);
        diagnostics
            .output_resample_ns
            .fetch_add(output_resample_ns, Ordering::Relaxed);
        if inference_calls > 0 {
            diagnostics.input_rms_db_bits.store(
                signal_db_from_energy(input_energy, metric_samples).to_bits(),
                Ordering::Relaxed,
            );
            diagnostics
                .input_peak_db_bits
                .store(signal_db_from_peak(input_peak).to_bits(), Ordering::Relaxed);
            diagnostics.output_rms_db_bits.store(
                signal_db_from_energy(output_energy, metric_samples).to_bits(),
                Ordering::Relaxed,
            );
            diagnostics.output_peak_db_bits.store(
                signal_db_from_peak(output_peak).to_bits(),
                Ordering::Relaxed,
            );
            diagnostics.wet_dry_delta_rms_db_bits.store(
                signal_db_from_energy(delta_energy, metric_samples).to_bits(),
                Ordering::Relaxed,
            );
            diagnostics
                .last_successful_inference_ms
                .store(unix_time_millis(), Ordering::Relaxed);
        }
        if connected_channels != 0 {
            diagnostics
                .worker_audio_frames_processed
                .fetch_add(block.frames as u64, Ordering::Relaxed);
            diagnostics
                .worker_processing_ns
                .fetch_add(worker_total_ns, Ordering::Relaxed);
            let observation =
                self.performance
                    .observe(block.frames as u64, worker_total_ns, self.sample_rate);
            diagnostics.realtime_factor_bits.store(
                self.performance.lifetime_factor(self.sample_rate).to_bits(),
                Ordering::Release,
            );
            diagnostics
                .ewma_realtime_factor_bits
                .store(observation.ewma.to_bits(), Ordering::Release);
            diagnostics
                .last_realtime_factor_bits
                .store(observation.factor.to_bits(), Ordering::Release);
            diagnostics
                .overload_windows
                .store(self.performance.overload_windows, Ordering::Release);
            diagnostics
                .healthy_windows
                .store(self.performance.healthy_windows, Ordering::Release);
            diagnostics
                .performance_windows
                .store(self.performance.performance_windows, Ordering::Release);
            diagnostics.stable_worker_p95_ns.store(
                self.performance.stable_p95_ns.max(0.0) as u64,
                Ordering::Relaxed,
            );
            diagnostics.stable_worker_p99_ns.store(
                self.performance.stable_p99_ns.max(0.0) as u64,
                Ordering::Relaxed,
            );
        }
        if let Ok(mut timings) = diagnostics.timings.lock() {
            if timings.len() == 4096 {
                timings.pop_front();
            }
            timings.push_back(worker_total_ns);
        }
        diagnostics.processed_blocks.fetch_add(1, Ordering::Relaxed);
        diagnostics
            .inference_ns
            .fetch_add(worker_total_ns, Ordering::Relaxed);
        let mut maximum = diagnostics.max_inference_ns.load(Ordering::Relaxed);
        while maximum < worker_total_ns {
            match diagnostics.max_inference_ns.compare_exchange_weak(
                maximum,
                worker_total_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => maximum = observed,
            }
        }

        // Publish only samples actually generated. Callback boundaries never
        // insert zeros or consume incomplete native frames.
        let available = (0..self.channels)
            .filter(|&ch| channel_is_connected(block.channel_mask, ch))
            .map(|ch| self.output_pending[ch].len() - self.output_offsets[ch])
            .min()
            .unwrap_or(0);
        // The first synthesis-delay samples after a fresh origin represent
        // time BEFORE that origin. Do not publish that invalid warmup as wet:
        // during recovery it would overwrite valid aligned dry with zeros.
        let skip = available.min(self.warmup_remaining);
        for channel in 0..self.channels {
            if channel_is_connected(block.channel_mask, channel) {
                self.output_offsets[channel] += skip;
                if self.output_offsets[channel] == self.output_pending[channel].len() {
                    self.output_pending[channel].clear();
                    self.output_offsets[channel] = 0;
                }
            }
        }
        self.output_position += skip as u64;
        self.warmup_remaining -= skip;
        let mut remaining = available - skip;
        while remaining > 0 {
            let count = remaining.min(self.max_frames);
            let pushed = output.try_push_with(
                block.generation,
                self.wet_epoch,
                self.output_position,
                count as u32,
                block.channels,
                block.channel_mask,
                |destination| {
                    destination[..count * self.channels].fill(0.0);
                    for channel in 0..self.channels {
                        if channel_is_connected(block.channel_mask, channel) {
                            let offset = self.output_offsets[channel];
                            for frame in 0..count {
                                destination[frame * self.channels + channel] =
                                    self.output_pending[channel][offset + frame];
                            }
                        }
                    }
                },
            );
            // Drop on overflow but still advance the absolute timeline. A
            // missing range can never shift subsequent wet samples in time.
            for channel in 0..self.channels {
                if channel_is_connected(block.channel_mask, channel) {
                    self.output_offsets[channel] += count;
                    if self.output_offsets[channel] == self.output_pending[channel].len() {
                        self.output_pending[channel].clear();
                        self.output_offsets[channel] = 0;
                    }
                }
            }
            self.output_position += count as u64;
            remaining -= count;
            if !pushed {
                diagnostics.output_overruns.fetch_add(1, Ordering::Relaxed);
                diagnostics
                    .wet_frames_dropped_queue_full
                    .fetch_add(count as u64, Ordering::Relaxed);
                diagnostics
                    .wet_drop_queue_full
                    .fetch_add(1, Ordering::Relaxed);
                diagnostics
                    .wet_frames_dropped
                    .fetch_add(count as u64, Ordering::Relaxed);
            } else {
                record_maximum(&diagnostics.max_output_queue_depth, output.depth());
                diagnostics
                    .latest_wet_frame
                    .store(self.output_position, Ordering::Release);
                diagnostics
                    .last_wet_output_ms
                    .store(unix_time_millis(), Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
    worker: &mut HushWorker,
    input: Arc<BlockQueue>,
    output: Arc<BlockQueue>,
    stop: Arc<AtomicBool>,
    signal: Arc<WorkerSignal>,
    generation: Arc<AtomicU64>,
    wet_epoch: Arc<AtomicU64>,
    resync_requested: Arc<AtomicBool>,
    retry_requested: Arc<AtomicBool>,
    retry_generation: Arc<AtomicBool>,
    accept_input: Arc<AtomicBool>,
    attenuation: Arc<AtomicU32>,
    diagnostics: Arc<HushDiagnostics>,
    latest_frame: Arc<AtomicU64>,
    max_backlog_frames: u64,
    _recovery_lead_frames: u64,
) {
    let mut state = HushWorkerState::Starting;
    let mut overload_attempts = 0_u32;
    let mut cooldown_until = Instant::now();
    diagnostics
        .worker_state
        .store(state.as_u32(), Ordering::Release);

    let set_state = |state: HushWorkerState, diagnostics: &HushDiagnostics| {
        let previous = diagnostics
            .worker_state
            .swap(state.as_u32(), Ordering::AcqRel);
        if previous == state.as_u32() {
            return;
        }
        if state == HushWorkerState::Overloaded {
            let wet = diagnostics.wet_frames_output.load(Ordering::Relaxed);
            let dry = diagnostics.dry_frames_output.load(Ordering::Relaxed);
            let wet_percent = if wet.saturating_add(dry) == 0 {
                0.0
            } else {
                wet as f64 * 100.0 / wet.saturating_add(dry) as f64
            };
            let factor = f64::from_bits(
                diagnostics
                    .ewma_realtime_factor_bits
                    .load(Ordering::Relaxed),
            );
            eprintln!(
                "WARN hush: entering overloaded state reason={} wet_delivery={wet_percent:.1}% rt_factor={factor:.2}x",
                match diagnostics.overload_reason.load(Ordering::Relaxed) {
                    1 => HushOverloadReason::CpuTooSlow.label(),
                    2 => HushOverloadReason::BacklogExceeded.label(),
                    3 => HushOverloadReason::InputQueueFull.label(),
                    _ => HushOverloadReason::None.label(),
                }
            );
        } else if previous == HushWorkerState::Overloaded.as_u32()
            && state == HushWorkerState::Running
        {
            eprintln!("INFO hush: recovered from overload");
        }
    };
    let wait_briefly = |signal: &WorkerSignal, stop: &AtomicBool| -> bool {
        let guard = match signal.mutex.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        if stop.load(Ordering::Acquire) {
            return false;
        }
        let _ = signal
            .condition
            .wait_timeout(guard, Duration::from_millis(1));
        true
    };

    while !stop.load(Ordering::Acquire) && !diagnostics.worker_failed.load(Ordering::Acquire) {
        // A realtime producer may enter the terminal state while reporting a
        // queue-full recovery failure. Mirror that atomic state into the
        // worker-local state machine before the next scheduling decision; the
        // worker must not remain in Warming with input acceptance disabled.
        if diagnostics.worker_state.load(Ordering::Acquire) == HushWorkerState::Overloaded.as_u32()
            && !matches!(state, HushWorkerState::Overloaded)
        {
            state = HushWorkerState::Overloaded;
            accept_input.store(false, Ordering::Release);
            cooldown_until = Instant::now() + overload_cooldown_for_attempt(overload_attempts);
        }
        // A resync is consumed by the worker exactly once. The producer is
        // gated while queues are drained, so no stale block can restart the
        // reset loop.
        if resync_requested.load(Ordering::Acquire) {
            let _manual_retry = retry_requested.swap(false, Ordering::AcqRel);
            let count_generation = retry_generation.swap(false, Ordering::AcqRel);
            diagnostics.worker_active.store(true, Ordering::Release);
            diagnostics
                .recovery_attempts
                .fetch_add(1, Ordering::Relaxed);
            state = HushWorkerState::Recovering;
            set_state(state, &diagnostics);
            accept_input.store(false, Ordering::Release);
            while input
                .pop_with(|block| {
                    diagnostics
                        .stale_input_blocks_dropped
                        .fetch_add(1, Ordering::Relaxed);
                    diagnostics
                        .wet_frames_dropped
                        .fetch_add(block.frames as u64, Ordering::Relaxed);
                    diagnostics
                        .wet_frames_dropped_epoch
                        .fetch_add(block.frames as u64, Ordering::Relaxed);
                })
                .is_some()
            {}
            while output
                .pop_with(|block| {
                    diagnostics
                        .stale_output_blocks_dropped
                        .fetch_add(1, Ordering::Relaxed);
                    diagnostics
                        .wet_frames_dropped
                        .fetch_add(block.frames as u64, Ordering::Relaxed);
                    diagnostics
                        .wet_frames_dropped_epoch
                        .fetch_add(block.frames as u64, Ordering::Relaxed);
                })
                .is_some()
            {}

            let current_generation = generation.load(Ordering::Acquire);
            let next_epoch = wet_epoch.fetch_add(1, Ordering::AcqRel) + 1;
            if let Err(error) = worker.reset(
                current_generation,
                worker.last_mask,
                &diagnostics,
                count_generation,
            ) {
                *diagnostics.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
                diagnostics.worker_failed.store(true, Ordering::Release);
                diagnostics.worker_active.store(false, Ordering::Release);
                set_state(HushWorkerState::Failed, &diagnostics);
                break;
            }
            worker.wet_epoch = next_epoch;
            diagnostics
                .latest_completed_input_frame
                .store(latest_frame.load(Ordering::Acquire), Ordering::Release);
            diagnostics
                .worker_input_frame
                .store(latest_frame.load(Ordering::Acquire), Ordering::Release);
            diagnostics.resync_completed.fetch_add(1, Ordering::Relaxed);
            set_state(HushWorkerState::Warming, &diagnostics);
            state = HushWorkerState::Warming;
            resync_requested.store(false, Ordering::Release);
            accept_input.store(true, Ordering::Release);
            diagnostics.worker_active.store(false, Ordering::Release);
            continue;
        }

        if matches!(state, HushWorkerState::Overloaded) {
            // Do not let a permanently slow worker retain seconds of queued
            // audio. Dropping here is intentionally cheap and does not reset
            // the denoiser again until the cooldown expires.
            accept_input.store(false, Ordering::Release);
            while input
                .pop_with(|block| {
                    diagnostics
                        .stale_input_blocks_dropped
                        .fetch_add(1, Ordering::Relaxed);
                    diagnostics
                        .wet_frames_dropped
                        .fetch_add(block.frames as u64, Ordering::Relaxed);
                    diagnostics
                        .wet_frames_dropped_epoch
                        .fetch_add(block.frames as u64, Ordering::Relaxed);
                })
                .is_some()
            {}
            // Rejected callbacks are intentionally outside the useful wet
            // timeline. Treat them as discarded progress so diagnostics and
            // the next retry do not report an ever-growing phantom backlog.
            diagnostics
                .worker_input_frame
                .store(latest_frame.load(Ordering::Acquire), Ordering::Release);
            diagnostics
                .latest_completed_input_frame
                .store(latest_frame.load(Ordering::Acquire), Ordering::Release);
            let manual_retry = retry_requested.swap(false, Ordering::AcqRel);
            if Instant::now() < cooldown_until && !manual_retry {
                if !wait_briefly(&signal, &stop) {
                    break;
                }
                continue;
            }
            // Retry on a bounded exponential cooldown. A temporary scheduler
            // stall can recover; after a small number of automatic attempts a
            // permanently slow model remains idle in stable dry fallback.
            if manual_retry || overload_attempts < HUSH_MAX_AUTOMATIC_RETRIES {
                overload_attempts = overload_attempts.saturating_add(1);
                diagnostics.resync_requests.fetch_add(1, Ordering::Relaxed);
                resync_requested.store(true, Ordering::Release);
                set_state(HushWorkerState::ResyncRequested, &diagnostics);
            } else if !wait_briefly(&signal, &stop) {
                break;
            }
            continue;
        }

        #[cfg(test)]
        if diagnostics.paused.load(Ordering::Acquire) {
            thread::sleep(Duration::from_micros(500));
            continue;
        }

        let latest = latest_frame.load(Ordering::Acquire);
        let progress = diagnostics.worker_input_frame.load(Ordering::Acquire);
        let schedule = diagnostics.scheduling_frames.load(Ordering::Acquire);
        let sample_rate = diagnostics.sample_rate.load(Ordering::Relaxed).max(1) as u64;
        let overload_margin = (sample_rate * 40 / 1000).max(1);
        let backlog_limit = max_backlog_frames.max(schedule.saturating_add(overload_margin));
        let quantum = diagnostics.max_host_quantum.load(Ordering::Relaxed) as u64;
        let recovery_lead = quantum
            .max((sample_rate * HUSH_MIN_WORKER_HEADROOM_MS as u64 / 1000).max(1))
            .min(schedule.saturating_sub(1).max(1));
        diagnostics
            .max_backlog_frames
            .store(backlog_limit, Ordering::Relaxed);
        diagnostics
            .recovery_lead_frames
            .store(recovery_lead, Ordering::Relaxed);
        let backlog = latest.saturating_sub(progress);
        record_maximum(&diagnostics.max_worker_backlog_frames, backlog as usize);
        if backlog > backlog_limit {
            accept_input.store(false, Ordering::Release);
            diagnostics.overload_reason.store(
                HushOverloadReason::BacklogExceeded.as_u32(),
                Ordering::Release,
            );
            if matches!(state, HushWorkerState::Warming) {
                // Warming already follows a completed wet-pipeline reset. If
                // the fresh worker cannot establish lead before the useful
                // backlog limit, enter stable overload directly; resetting
                // again would only create a storm.
                diagnostics.worker_overloaded.store(true, Ordering::Release);
                diagnostics
                    .recovery_failures
                    .fetch_add(1, Ordering::Relaxed);
                state = HushWorkerState::Overloaded;
                set_state(state, &diagnostics);
                cooldown_until = Instant::now() + overload_cooldown_for_attempt(overload_attempts);
            } else if !resync_requested.swap(true, Ordering::AcqRel) {
                diagnostics.resync_requests.fetch_add(1, Ordering::Relaxed);
                diagnostics.worker_overloaded.store(true, Ordering::Release);
                set_state(HushWorkerState::ResyncRequested, &diagnostics);
            }
            continue;
        }

        let mut did_work = false;
        for _ in 0..HUSH_QUEUE_CAPACITY {
            let popped = input.pop_with(|block| {
                did_work = true;
                diagnostics.worker_active.store(true, Ordering::Release);
                diagnostics
                    .input_blocks_consumed
                    .fetch_add(1, Ordering::Relaxed);
                let block_end = block.start_frame + block.frames as u64;
                diagnostics
                    .input_frames_consumed
                    .fetch_add(block.frames as u64, Ordering::Relaxed);
                let current_backlog = latest_frame
                    .load(Ordering::Acquire)
                    .saturating_sub(block_end);
                record_maximum(
                    &diagnostics.max_worker_backlog_frames,
                    current_backlog as usize,
                );
                if diagnostics.worker_failed.load(Ordering::Acquire) {
                    return;
                }
                if current_backlog > backlog_limit {
                    accept_input.store(false, Ordering::Release);
                    diagnostics.overload_reason.store(
                        HushOverloadReason::BacklogExceeded.as_u32(),
                        Ordering::Release,
                    );
                    if matches!(state, HushWorkerState::Warming) {
                        diagnostics.worker_overloaded.store(true, Ordering::Release);
                        diagnostics
                            .recovery_failures
                            .fetch_add(1, Ordering::Relaxed);
                        state = HushWorkerState::Overloaded;
                        set_state(state, &diagnostics);
                        cooldown_until =
                            Instant::now() + overload_cooldown_for_attempt(overload_attempts);
                    } else if !resync_requested.swap(true, Ordering::AcqRel) {
                        diagnostics.resync_requests.fetch_add(1, Ordering::Relaxed);
                        diagnostics.worker_overloaded.store(true, Ordering::Release);
                        set_state(HushWorkerState::ResyncRequested, &diagnostics);
                    }
                    diagnostics
                        .stale_input_blocks_dropped
                        .fetch_add(1, Ordering::Relaxed);
                    diagnostics
                        .wet_frames_dropped
                        .fetch_add(block.frames as u64, Ordering::Relaxed);
                    return;
                }
                if block.generation != generation.load(Ordering::Acquire)
                    || block.wet_epoch != wet_epoch.load(Ordering::Acquire)
                {
                    diagnostics
                        .stale_input_blocks_dropped
                        .fetch_add(1, Ordering::Relaxed);
                    return;
                }
                if !worker.pristine && block.start_frame != worker.input_position {
                    accept_input.store(false, Ordering::Release);
                    if !resync_requested.swap(true, Ordering::AcqRel) {
                        diagnostics.resync_requests.fetch_add(1, Ordering::Relaxed);
                        diagnostics.worker_overloaded.store(true, Ordering::Release);
                        set_state(HushWorkerState::ResyncRequested, &diagnostics);
                    }
                    diagnostics
                        .stale_input_blocks_dropped
                        .fetch_add(1, Ordering::Relaxed);
                    diagnostics
                        .wet_frames_dropped
                        .fetch_add(block.frames as u64, Ordering::Relaxed);
                    return;
                }
                if block.generation != generation.load(Ordering::Acquire)
                    || block.wet_epoch != worker.wet_epoch
                {
                    diagnostics
                        .stale_input_blocks_dropped
                        .fetch_add(1, Ordering::Relaxed);
                    return;
                }
                let requested = f32::from_bits(attenuation.load(Ordering::Acquire));
                if !requested.is_finite() {
                    let error = format!("Hush attenuation became non-finite: {requested}");
                    *diagnostics.error.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(error.clone());
                    diagnostics.worker_failed.store(true, Ordering::Release);
                    set_state(HushWorkerState::Failed, &diagnostics);
                    return;
                }
                let effective = requested.max(0.01);
                if (effective - worker.attenuation_db).abs() > f32::EPSILON {
                    for denoiser in &mut worker.denoisers {
                        if let Err(error) = denoiser.set_attenuation_limit_db(effective) {
                            *diagnostics.error.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(error.to_string());
                            diagnostics
                                .inference_errors_total
                                .fetch_add(1, Ordering::Relaxed);
                            diagnostics.worker_failed.store(true, Ordering::Release);
                            set_state(HushWorkerState::Failed, &diagnostics);
                            return;
                        }
                    }
                    worker.attenuation_db = effective;
                    diagnostics
                        .effective_attenuation_bits
                        .store(effective.to_bits(), Ordering::Release);
                }
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker.process_block(block, &output, &generation, &wet_epoch, &diagnostics)
                }));
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error),
                    Err(payload) => Some(format!(
                        "Hush worker panicked: {}",
                        payload
                            .downcast_ref::<String>()
                            .map(String::as_str)
                            .or_else(|| payload.downcast_ref::<&str>().copied())
                            .unwrap_or("unknown panic")
                    )),
                };
                if let Some(error) = error {
                    *diagnostics.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
                    diagnostics.worker_failed.store(true, Ordering::Release);
                    set_state(HushWorkerState::Failed, &diagnostics);
                    return;
                }
                // This is a completed horizon, not merely a block removed
                // from the input queue.  Keeping the distinction explicit is
                // what makes the wet timeline safe while inference is still
                // running.
                diagnostics
                    .worker_input_frame
                    .store(block_end, Ordering::Release);
                diagnostics
                    .latest_completed_input_frame
                    .store(block_end, Ordering::Release);

                if matches!(state, HushWorkerState::Starting) {
                    set_state(HushWorkerState::Running, &diagnostics);
                    state = HushWorkerState::Running;
                }
                let ewma_factor = f64::from_bits(
                    diagnostics
                        .ewma_realtime_factor_bits
                        .load(Ordering::Acquire),
                );
                let overload_confident = diagnostics.overload_windows.load(Ordering::Acquire)
                    >= HUSH_OVERLOAD_OBSERVATIONS;
                let live_backlog = latest_frame
                    .load(Ordering::Acquire)
                    .saturating_sub(block_end)
                    > recovery_lead
                    || input.depth() > HUSH_QUEUE_CAPACITY / 4;
                if overload_confident
                    && ewma_factor.is_finite()
                    && ewma_factor > HUSH_REALTIME_FACTOR_LIMIT
                    && live_backlog
                {
                    diagnostics
                        .overload_reason
                        .store(HushOverloadReason::CpuTooSlow.as_u32(), Ordering::Release);
                    diagnostics.worker_overloaded.store(true, Ordering::Release);
                    diagnostics
                        .recovery_failures
                        .fetch_add(1, Ordering::Relaxed);
                    accept_input.store(false, Ordering::Release);
                    state = HushWorkerState::Overloaded;
                    set_state(state, &diagnostics);
                    cooldown_until =
                        Instant::now() + overload_cooldown_for_attempt(overload_attempts);
                }
                let fresh_wet_lead = diagnostics
                    .latest_wet_frame
                    .load(Ordering::Acquire)
                    .saturating_sub(diagnostics.playout_frame.load(Ordering::Acquire));
                let healthy_confident = diagnostics.healthy_windows.load(Ordering::Acquire)
                    >= HUSH_HEALTHY_OBSERVATIONS;
                let post_backlog = latest_frame
                    .load(Ordering::Acquire)
                    .saturating_sub(block_end);
                if matches!(state, HushWorkerState::Warming)
                    && healthy_confident
                    && ewma_factor.is_finite()
                    && ewma_factor < HUSH_HEALTHY_REALTIME_FACTOR_LIMIT
                    && fresh_wet_lead >= recovery_lead
                    && post_backlog <= recovery_lead
                {
                    diagnostics
                        .worker_overloaded
                        .store(false, Ordering::Release);
                    diagnostics
                        .recovery_successes
                        .fetch_add(1, Ordering::Relaxed);
                    set_state(HushWorkerState::Running, &diagnostics);
                    state = HushWorkerState::Running;
                    overload_attempts = 0;
                    diagnostics
                        .overload_reason
                        .store(HushOverloadReason::None.as_u32(), Ordering::Release);
                }
            });
            diagnostics.worker_active.store(false, Ordering::Release);
            let Some(()) = popped else {
                break;
            };
        }
        if !did_work && !wait_briefly(&signal, &stop) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observe_windows(performance: &mut HushPerformanceState, factor: u64, count: usize) {
        for _ in 0..count {
            // 2,400 frames at 48 kHz are one 50 ms performance window.
            performance.observe(2_400, factor * 50_000_000, 48_000);
        }
    }

    #[test]
    fn performance_estimator_survives_a_dsp_reset_boundary() {
        let mut performance = HushPerformanceState::default();
        observe_windows(&mut performance, 2, HUSH_OVERLOAD_OBSERVATIONS as usize);
        assert!(performance.overload_windows >= HUSH_OVERLOAD_OBSERVATIONS);
        let ewma_before_reset = performance.ewma_realtime_factor;

        // Exercise the real reset boundary: only the worker's DSP-owned
        // state is rebuilt, while the persistent estimator remains intact.
        let model = crate::hush_noise::shared_hush_model().unwrap();
        let diagnostics = HushDiagnostics::default();
        let mut worker = HushWorker::new(model, 48_000, 1, 128, 25.0).unwrap();
        worker.performance = performance.clone();
        worker.reset(0, 1, &diagnostics, false).unwrap();

        assert!(performance.overload_windows >= HUSH_OVERLOAD_OBSERVATIONS);
        assert_eq!(worker.performance.ewma_realtime_factor, ewma_before_reset);
        assert!(worker.performance.overload_windows >= HUSH_OVERLOAD_OBSERVATIONS);
    }

    #[test]
    fn performance_state_distinguishes_healthy_temporary_and_permanent_load() {
        let mut healthy = HushPerformanceState::default();
        observe_windows(&mut healthy, 0, HUSH_HEALTHY_OBSERVATIONS as usize);
        assert!(healthy.healthy_windows >= HUSH_HEALTHY_OBSERVATIONS);
        assert!(healthy.overload_windows < HUSH_OVERLOAD_OBSERVATIONS);

        let mut temporary = HushPerformanceState::default();
        observe_windows(&mut temporary, 0, HUSH_HEALTHY_OBSERVATIONS as usize);
        // The EWMA deliberately filters a short spike. Five observations are
        // long enough to classify this as overload, while the healthy tail
        // below proves that confidence can recover without reconstructing the
        // performance state.
        observe_windows(&mut temporary, 2, 5);
        assert!(temporary.overload_windows >= HUSH_OVERLOAD_OBSERVATIONS);
        observe_windows(&mut temporary, 0, 8);
        assert!(temporary.healthy_windows >= HUSH_HEALTHY_OBSERVATIONS);
        assert!(temporary.overload_windows < HUSH_OVERLOAD_OBSERVATIONS);

        let mut permanent = HushPerformanceState::default();
        observe_windows(&mut permanent, 2, 12);
        assert!(permanent.overload_windows >= HUSH_OVERLOAD_OBSERVATIONS);
        assert_eq!(permanent.healthy_windows, 0);
    }

    #[test]
    fn no_audio_input_is_not_classified_as_cpu_overload() {
        let diagnostics = HushDiagnostics::default();
        diagnostics.worker_ready.store(true, Ordering::Release);
        diagnostics
            .worker_state
            .store(HushWorkerState::Running.as_u32(), Ordering::Release);
        diagnostics
            .no_audio_input_blocks
            .store(1, Ordering::Release);
        diagnostics.dry_frames_output.store(512, Ordering::Release);

        assert_eq!(diagnostics.health(), HushHealth::Degraded);
    }

    #[test]
    fn overload_cooldown_is_exponential_and_bounded() {
        assert_eq!(overload_cooldown_for_attempt(0), Duration::from_millis(500));
        assert_eq!(
            overload_cooldown_for_attempt(1),
            Duration::from_millis(1_000)
        );
        assert_eq!(
            overload_cooldown_for_attempt(2),
            Duration::from_millis(2_000)
        );
        assert_eq!(
            overload_cooldown_for_attempt(3),
            Duration::from_millis(5_000)
        );
        assert_eq!(
            overload_cooldown_for_attempt(20),
            Duration::from_millis(5_000)
        );
    }

    #[test]
    fn queue_is_bounded_and_fifo() {
        let queue = BlockQueue::new(2, 4);
        assert!(queue.try_push_with(1, 0, 0, 1, 1, 1, |samples: &mut [f32]| samples[0] = 0.25));
        assert!(queue.try_push_with(1, 0, 1, 1, 1, 1, |samples: &mut [f32]| samples[0] = 0.5));
        assert!(!queue.try_push_with(1, 0, 2, 1, 1, 1, |_samples: &mut [f32]| {}));
        let first = queue.pop_with(|block| (block.start_frame, block.samples[0]));
        let second = queue.pop_with(|block| (block.start_frame, block.samples[0]));
        assert_eq!(first, Some((0, 0.25)));
        assert_eq!(second, Some((1, 0.5)));
        assert!(queue.pop_with(|_| ()).is_none());
    }

    #[test]
    fn future_output_is_not_consumed_by_an_earlier_start_frame() {
        let queue = BlockQueue::new(4, 1);
        assert!(
            queue.try_push_with(7, 0, 2, 1, 1, 1, |samples: &mut [f32]| {
                samples[0] = 0.75
            })
        );
        assert_eq!(
            queue.peek_with(|block| (block.generation, block.start_frame)),
            Some((7, 2))
        );
        assert_eq!(queue.pop_with(|block| block.samples[0]), Some(0.75));
    }

    #[test]
    fn timeline_preserves_future_and_partial_blocks_and_discards_only_expired() {
        let q = BlockQueue::new(4, 8);
        q.try_push_with(3, 0, 10, 8, 1, 1, |s: &mut [f32]| {
            for (i, v) in s.iter_mut().enumerate() {
                *v = (10 + i) as f32;
            }
        });
        let mut out = [-1.0; 4];
        assert_eq!(read_timeline(&q, 3, 0, 0, 4, 1, None, &mut out), 0);
        assert_eq!(q.depth(), 1);
        assert_eq!(read_timeline(&q, 3, 0, 8, 4, 1, None, &mut out), 2);
        assert_eq!(out, [-1.0, -1.0, 10.0, 11.0]);
        assert_eq!(q.depth(), 1);
        assert_eq!(read_timeline(&q, 3, 0, 14, 4, 1, None, &mut out), 4);
        assert_eq!(out, [14.0, 15.0, 16.0, 17.0]);
        assert_eq!(q.depth(), 0);
    }

    #[test]
    fn timeline_discards_wrong_channel_count_and_keeps_reading() {
        let q = BlockQueue::new(4, 8);
        assert!(q.try_push_with(3, 0, 0, 4, 2, 3, |samples: &mut [f32]| {
            samples.fill(0.5);
        }));
        let diagnostics = HushDiagnostics::default();
        let mut output = [0.0; 4];
        assert_eq!(
            read_timeline(&q, 3, 0, 0, 4, 1, Some(&diagnostics), &mut output,),
            0
        );
        assert_eq!(q.depth(), 0, "a mismatched result must not stall the queue");
        assert_eq!(
            diagnostics
                .wet_drop_wrong_channel_count
                .load(Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn concurrent_queue_wraparound_preserves_samples_and_epochs() {
        let q = Arc::new(BlockQueue::new(8, 8));
        // Power-of-two capacity divides the usize index space, including wrap.
        q.read.store(usize::MAX - 7, Ordering::Relaxed);
        q.write.store(usize::MAX - 7, Ordering::Relaxed);
        let producer = q.clone();
        let writer = thread::spawn(move || {
            for i in 0..20_000u64 {
                while !producer
                    .try_push_with(i / 100, 0, i * 4, 4, 2, 3, |s: &mut [f32]| s.fill(i as f32))
                {
                    thread::yield_now();
                }
            }
        });
        for i in 0..20_000u64 {
            loop {
                if q.pop_with(|b| {
                    assert_eq!(b.generation, i / 100);
                    assert_eq!(b.start_frame, i * 4);
                    assert!(b.samples.iter().all(|v| *v == i as f32));
                })
                .is_some()
                {
                    break;
                }
                thread::yield_now();
            }
        }
        writer.join().unwrap();
    }
}
