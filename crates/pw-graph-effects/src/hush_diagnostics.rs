//! Hush diagnostics and health reporting.
//!
//! This module is control-plane only. It contains atomic counters and
//! formatting helpers; the audio callback only updates the counters and never
//! calls the display-oriented methods below.

use crate::hush_noise::HushModelLoadInfo;
use crate::hush_overload::{HushHealth, HushOverloadReason, HushWorkerState};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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

    /// Whether the current dry output is intentional rather than caused by a
    /// worker deadline miss. Hosts use this to render a distinct bypass state
    /// without parsing the formatted diagnostic text.
    pub fn is_bypassed(&self) -> bool {
        self.bypass.load(Ordering::Acquire) || self.host_disabled.load(Ordering::Acquire)
    }
}

impl crate::EffectDiagnostics for HushDiagnostics {
    fn snapshot(&self) -> crate::EffectDiagnosticsSnapshot {
        let health = self.health();
        let lifecycle = if matches!(health, HushHealth::Failed) {
            crate::EffectLifecycle::Failed
        } else if !self.worker_ready.load(Ordering::Acquire) {
            crate::EffectLifecycle::Preparing
        } else {
            crate::EffectLifecycle::Active
        };
        let total = self.wet_frames_output.load(Ordering::Relaxed)
            + self.dry_frames_output.load(Ordering::Relaxed);
        let wet_percent = if total == 0 {
            0.0
        } else {
            self.wet_frames_output.load(Ordering::Relaxed) as f64 * 100.0 / total as f64
        };
        let mut metrics = std::collections::BTreeMap::new();
        metrics.insert(
            "physical_channels".into(),
            self.channels.load(Ordering::Relaxed).to_string(),
        );
        metrics.insert(
            "active_channels".into(),
            self.active_hush_channels
                .load(Ordering::Relaxed)
                .to_string(),
        );
        metrics.insert(
            "active_mask".into(),
            format!("0x{:02x}", self.active_channel_mask.load(Ordering::Relaxed)),
        );
        metrics.insert("wet_percent".into(), format!("{wet_percent:.1}"));
        metrics.insert(
            "rt_factor".into(),
            format!(
                "{:.2}",
                f64::from_bits(self.ewma_realtime_factor_bits.load(Ordering::Relaxed))
            ),
        );
        crate::EffectDiagnosticsSnapshot {
            lifecycle,
            health: if self.bypass.load(Ordering::Acquire)
                || self.host_disabled.load(Ordering::Acquire)
            {
                crate::EffectHealth::Bypassed
            } else {
                health.into()
            },
            message: Some(self.status_text()),
            metrics,
        }
    }
}

#[inline]
pub(crate) fn record_maximum(counter: &AtomicU64, value: usize) {
    let value = value as u64;
    let mut maximum = counter.load(Ordering::Relaxed);
    while maximum < value {
        match counter.compare_exchange_weak(maximum, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(observed) => maximum = observed,
        }
    }
}
