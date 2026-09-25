//! Bounded realtime queues.
//!
//! The PipeWire data thread must never block, allocate unboundedly, or wait
//! on a worker. [`VideoQueue`] is the only handoff between the callback and
//! the processing thread: fixed capacity, overwrite-oldest on overflow, and
//! `try_lock` ingestion so a contended lock drops a frame instead of stalling
//! the graph.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::format::{normalize_queue_depth, MAX_QUEUE_DEPTH};
use crate::frame::VideoFrame;

/// Fixed-capacity frame queue with drop-oldest overflow.
///
/// `push_overwrite` is the callback path: it never blocks and never grows.
/// `pop` / `pop_latest` are the worker path.
#[derive(Debug)]
pub struct VideoQueue {
    capacity: usize,
    inner: Mutex<VecDeque<VideoFrame>>,
    /// Notified on every successful push so a parked worker wakes promptly.
    notify: Condvar,
    dropped: AtomicU64,
    received: AtomicU64,
}

impl VideoQueue {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            capacity: normalize_queue_depth(capacity).min(MAX_QUEUE_DEPTH),
            inner: Mutex::new(VecDeque::new()),
            notify: Condvar::new(),
            dropped: AtomicU64::new(0),
            received: AtomicU64::new(0),
        })
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|q| q.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// Callback path. On a contended lock or a full queue the oldest frame is
    /// dropped and the new one is kept, so latency stays bounded.
    pub fn push_from_callback(&self, frame: VideoFrame) {
        self.received.fetch_add(1, Ordering::Relaxed);
        let Ok(mut queue) = self.inner.try_lock() else {
            // Never block the realtime thread: count the frame as dropped.
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if queue.len() >= self.capacity && queue.pop_front().is_some() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_back(frame);
        self.notify.notify_one();
    }

    /// Worker/control path with a blocking lock. Same overwrite-oldest
    /// policy; returns the number of frames dropped by this push (0 or 1).
    pub fn push_overwrite(&self, frame: VideoFrame) -> usize {
        self.received.fetch_add(1, Ordering::Relaxed);
        let mut queue = self.inner.lock().expect("video queue mutex poisoned");
        let mut dropped = 0;
        if queue.len() >= self.capacity && queue.pop_front().is_some() {
            dropped = 1;
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        queue.push_back(frame);
        self.notify.notify_one();
        dropped
    }

    /// Pop the oldest frame.
    pub fn pop(&self) -> Option<VideoFrame> {
        self.inner
            .lock()
            .expect("video queue mutex poisoned")
            .pop_front()
    }

    /// Pop the newest frame, discarding everything older. The worker uses this
    /// when it only cares about the latest image; every skipped frame counts
    /// as dropped so overload stays visible in diagnostics.
    pub fn pop_latest(&self) -> Option<VideoFrame> {
        let mut queue = self.inner.lock().expect("video queue mutex poisoned");
        let latest = queue.pop_back()?;
        let skipped = queue.len() as u64;
        if skipped > 0 {
            self.dropped.fetch_add(skipped, Ordering::Relaxed);
            queue.clear();
        }
        Some(latest)
    }

    /// Block until a frame arrives, a shutdown flag trips, or the timeout
    /// elapses. Only the worker thread waits; callbacks never do.
    pub fn wait_for_frame(
        &self,
        shutdown: &std::sync::atomic::AtomicBool,
        timeout: std::time::Duration,
    ) -> bool {
        let mut queue = self.inner.lock().expect("video queue mutex poisoned");
        if !queue.is_empty() || shutdown.load(Ordering::Acquire) {
            return !queue.is_empty();
        }
        let (guard, _) = self
            .notify
            .wait_timeout(queue, timeout)
            .expect("video queue condvar poisoned");
        queue = guard;
        !queue.is_empty() || shutdown.load(Ordering::Acquire)
    }

    pub fn wake(&self) {
        self.notify.notify_all();
    }

    pub fn clear(&self) {
        if let Ok(mut queue) = self.inner.lock() {
            self.dropped
                .fetch_add(queue.len() as u64, Ordering::Relaxed);
            queue.clear();
        }
    }
}

/// Latest-frame slot for previews and filter outputs.
///
/// Writers replace the whole frame; readers clone the [`Arc`], so the newest
/// image is always available without blocking either side. Stale frames are
/// simply overwritten.
#[derive(Clone, Debug, Default)]
pub struct LatestFrame {
    inner: Arc<Mutex<Option<Arc<VideoFrame>>>>,
    updates: Arc<AtomicU64>,
}

impl LatestFrame {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publish(&self, frame: VideoFrame) {
        if let Ok(mut slot) = self.inner.lock() {
            *slot = Some(Arc::new(frame));
            self.updates.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Control-plane publish that reports whether the slot was updated.
    pub fn try_publish(&self, frame: VideoFrame) -> bool {
        if let Ok(mut slot) = self.inner.try_lock() {
            *slot = Some(Arc::new(frame));
            self.updates.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    pub fn latest(&self) -> Option<Arc<VideoFrame>> {
        self.inner.lock().ok().and_then(|slot| slot.clone())
    }

    pub fn updates(&self) -> u64 {
        self.updates.load(Ordering::Relaxed)
    }

    pub fn clear(&self) {
        if let Ok(mut slot) = self.inner.lock() {
            *slot = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{VideoPixelFormat, VideoSpec};

    fn frame_64() -> VideoFrame {
        let spec = VideoSpec::new(64, 64, 30, 1, VideoPixelFormat::Rgbx).unwrap();
        VideoFrame::allocate(spec).unwrap()
    }

    #[test]
    fn full_queue_drops_oldest_and_keeps_newest() {
        let queue = VideoQueue::new(2);
        let mut a = frame_64();
        a.set_sequence(1);
        let mut b = frame_64();
        b.set_sequence(2);
        let mut c = frame_64();
        c.set_sequence(3);
        assert_eq!(queue.push_overwrite(a), 0);
        assert_eq!(queue.push_overwrite(b), 0);
        assert_eq!(queue.push_overwrite(c), 1);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.dropped(), 1);
        assert_eq!(queue.pop().unwrap().sequence(), 2);
        assert_eq!(queue.pop().unwrap().sequence(), 3);
        assert!(queue.pop().is_none());
    }

    #[test]
    fn pop_latest_discards_stale_frames() {
        let queue = VideoQueue::new(4);
        for sequence in 1..=4 {
            let mut frame = frame_64();
            frame.set_sequence(sequence);
            queue.push_overwrite(frame);
        }
        let latest = queue.pop_latest().unwrap();
        assert_eq!(latest.sequence(), 4);
        // Three stale frames were skipped and must be visible as drops.
        assert_eq!(queue.dropped(), 3);
        assert!(queue.pop().is_none());
    }

    #[test]
    fn capacity_is_clamped_to_bounds() {
        assert_eq!(VideoQueue::new(0).capacity(), 1);
        assert_eq!(VideoQueue::new(100).capacity(), MAX_QUEUE_DEPTH);
    }

    #[test]
    fn latest_frame_slot_keeps_newest() {
        let slot = LatestFrame::new();
        assert!(slot.latest().is_none());
        let mut a = frame_64();
        a.set_sequence(1);
        slot.publish(a);
        let mut b = frame_64();
        b.set_sequence(2);
        slot.publish(b);
        assert_eq!(slot.latest().unwrap().sequence(), 2);
        assert_eq!(slot.updates(), 2);
    }
}
