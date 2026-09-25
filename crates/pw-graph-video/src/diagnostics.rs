//! Lock-free video diagnostics.
//!
//! Counters use atomics so the realtime callback, worker, and UI threads can
//! all update and read without blocking each other. Per-frame logging is
//! forbidden; overload surfaces as counters instead.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::format::VideoSpec;
use crate::queue::VideoQueue;

/// Shared diagnostics handle. Cloneable across callback, worker, and UI.
#[derive(Clone, Debug, Default)]
pub struct VideoDiagnostics {
    inner: Arc<DiagnosticsInner>,
}

#[derive(Debug, Default)]
struct DiagnosticsInner {
    received: AtomicU64,
    processed: AtomicU64,
    output: AtomicU64,
    dropped: AtomicU64,
    bypassed: AtomicU64,
    rejected: AtomicU64,
    last_processing_time_us: AtomicU64,
    max_processing_time_us: AtomicU64,
    state: Mutex<DiagnosticsState>,
}

#[derive(Debug, Default)]
struct DiagnosticsState {
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
    format: String,
    last_error: Option<String>,
}

/// Point-in-time snapshot for the UI and status reports. Cheap to clone.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VideoCounterSnapshot {
    pub frames_received: u64,
    pub frames_processed: u64,
    pub frames_output: u64,
    pub frames_dropped: u64,
    pub frames_bypassed: u64,
    pub frames_rejected: u64,
    pub queue_depth: usize,
    pub queue_capacity: usize,
    pub last_processing_time_us: u64,
    pub max_processing_time_us: u64,
    pub width: u32,
    pub height: u32,
    pub framerate_num: u32,
    pub framerate_den: u32,
    pub format: String,
    pub last_error: Option<String>,
}

impl VideoDiagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one received frame (callback side).
    pub fn note_received(&self) {
        self.inner.received.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one successfully processed frame (worker side).
    pub fn note_processed(&self, elapsed: std::time::Duration, queue: &VideoQueue) {
        self.inner.processed.fetch_add(1, Ordering::Relaxed);
        self.inner.output.fetch_add(1, Ordering::Relaxed);
        self.note_timing(elapsed);
        self.sync_queue(queue);
    }

    /// Record one bypassed frame after a processor failure.
    pub fn note_bypassed(&self, elapsed: std::time::Duration, queue: &VideoQueue) {
        self.inner.bypassed.fetch_add(1, Ordering::Relaxed);
        self.inner.output.fetch_add(1, Ordering::Relaxed);
        self.note_timing(elapsed);
        self.sync_queue(queue);
    }

    /// Record the currently negotiated output spec.
    pub fn note_spec(&self, spec: &VideoSpec) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.width = spec.width;
            state.height = spec.height;
            state.fps_num = spec.framerate_num;
            state.fps_den = spec.framerate_den;
            state.format = spec.format.as_str().to_owned();
        }
    }

    pub fn note_error(&self, message: &str) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.last_error = Some(message.to_owned());
        }
    }

    /// Record an error only when none is present, so a specific failure
    /// (unsupported format, stream error) is never overwritten by a generic
    /// downstream observation like "stalled".
    pub fn note_error_if_empty(&self, message: &str) {
        if let Ok(mut state) = self.inner.state.lock() {
            if state.last_error.is_none() {
                state.last_error = Some(message.to_owned());
            }
        }
    }

    /// Record a received buffer that could not become a frame (short,
    /// ragged, or unmappable). Distinct from overload drops: rejections
    /// indicate a negotiation or peer problem worth investigating.
    pub fn note_rejected(&self) {
        self.inner.rejected.fetch_add(1, Ordering::Relaxed);
    }

    pub fn clear_error(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.last_error = None;
        }
    }

    /// Reconcile queue counters (queue owns drops/receives on the hot path).
    pub fn sync_queue(&self, queue: &VideoQueue) {
        self.inner
            .received
            .store(queue.received(), Ordering::Relaxed);
        self.inner.dropped.store(queue.dropped(), Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> VideoCounterSnapshot {
        let state = self.inner.state.lock().ok();
        VideoCounterSnapshot {
            frames_received: self.inner.received.load(Ordering::Relaxed),
            frames_processed: self.inner.processed.load(Ordering::Relaxed),
            frames_output: self.inner.output.load(Ordering::Relaxed),
            frames_dropped: self.inner.dropped.load(Ordering::Relaxed),
            frames_bypassed: self.inner.bypassed.load(Ordering::Relaxed),
            frames_rejected: self.inner.rejected.load(Ordering::Relaxed),
            queue_depth: 0,
            queue_capacity: 0,
            last_processing_time_us: self.inner.last_processing_time_us.load(Ordering::Relaxed),
            max_processing_time_us: self.inner.max_processing_time_us.load(Ordering::Relaxed),
            width: state.as_ref().map(|s| s.width).unwrap_or(0),
            height: state.as_ref().map(|s| s.height).unwrap_or(0),
            framerate_num: state.as_ref().map(|s| s.fps_num).unwrap_or(0),
            framerate_den: state.as_ref().map(|s| s.fps_den).unwrap_or(0),
            format: state.as_ref().map(|s| s.format.clone()).unwrap_or_default(),
            last_error: state.as_ref().and_then(|s| s.last_error.clone()),
        }
    }

    /// Snapshot including live queue depth/capacity.
    pub fn snapshot_with_queue(&self, queue: &VideoQueue) -> VideoCounterSnapshot {
        let mut snapshot = self.snapshot();
        snapshot.queue_depth = queue.len();
        snapshot.queue_capacity = queue.capacity();
        snapshot.frames_received = queue.received();
        snapshot.frames_dropped = queue.dropped();
        snapshot
    }

    fn note_timing(&self, elapsed: std::time::Duration) {
        let us = elapsed.as_micros().min(u128::from(u64::MAX)) as u64;
        self.inner
            .last_processing_time_us
            .store(us, Ordering::Relaxed);
        self.inner
            .max_processing_time_us
            .fetch_max(us, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reports_counters_and_spec() {
        let diagnostics = VideoDiagnostics::new();
        let queue = VideoQueue::new(3);
        diagnostics.note_received();
        diagnostics.note_processed(std::time::Duration::from_micros(120), &queue);
        let spec = VideoSpec::new(1280, 720, 60, 1, crate::format::VideoPixelFormat::Nv12).unwrap();
        diagnostics.note_spec(&spec);
        let snapshot = diagnostics.snapshot_with_queue(&queue);
        assert_eq!(snapshot.frames_processed, 1);
        assert_eq!(snapshot.frames_output, 1);
        assert_eq!(snapshot.width, 1280);
        assert_eq!(snapshot.format, "NV12");
        assert_eq!(snapshot.queue_capacity, 3);
        assert!(snapshot.last_processing_time_us >= 120);
    }
}
