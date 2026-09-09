//! Bounded audio block queues and wet-timeline matching for Hush.
//!
//! The queue is a fixed-capacity SPSC ring.  It is deliberately independent
//! from the worker loop so queue ownership, stale-output policy, and timeline
//! diagnostics can be tested without constructing a neural runtime.

use crate::hush_worker::HushDiagnostics;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) const HUSH_QUEUE_CAPACITY: usize = 128;

pub(crate) struct AudioBlock {
    pub(crate) generation: u64,
    pub(crate) wet_epoch: u64,
    pub(crate) start_frame: u64,
    pub(crate) frames: u32,
    pub(crate) channels: u16,
    pub(crate) channel_mask: u16,
    pub(crate) samples: Vec<f32>,
}

impl AudioBlock {
    pub(crate) fn new(max_samples: usize) -> Self {
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

/// A single-producer/single-consumer ring. Each side owns one index and a
/// slot is published only after its metadata and samples are filled.
pub(crate) struct BlockQueue {
    slots: Box<[UnsafeCell<AudioBlock>]>,
    capacity: usize,
    max_samples: usize,
    pub(crate) write: AtomicUsize,
    pub(crate) read: AtomicUsize,
}

unsafe impl Send for BlockQueue {}
unsafe impl Sync for BlockQueue {}

impl BlockQueue {
    pub(crate) fn new(capacity: usize, max_samples: usize) -> Self {
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
    pub(crate) fn try_push_with(
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
        // SAFETY: the producer owns this slot until it publishes `write`.
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

    pub(crate) fn pop_with<R>(&self, read: impl FnOnce(&AudioBlock) -> R) -> Option<R> {
        let read_index = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Acquire);
        if read_index == write {
            return None;
        }
        let slot = read_index % self.capacity;
        // SAFETY: the consumer owns the published front slot until read moves.
        let block = unsafe { &*self.slots[slot].get() };
        let result = read(block);
        self.read
            .store(read_index.wrapping_add(1), Ordering::Release);
        Some(result)
    }

    pub(crate) fn peek_with<R>(&self, read: impl FnOnce(&AudioBlock) -> R) -> Option<R> {
        let read_index = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Acquire);
        if read_index == write {
            return None;
        }
        let slot = read_index % self.capacity;
        // SAFETY: the consumer owns the front slot while it remains peeked.
        let block = unsafe { &*self.slots[slot].get() };
        Some(read(block))
    }

    pub(crate) fn max_samples(&self) -> usize {
        self.max_samples
    }

    pub(crate) fn depth(&self) -> usize {
        self.write
            .load(Ordering::Acquire)
            .wrapping_sub(self.read.load(Ordering::Acquire))
            .min(self.capacity)
    }
}

/// Copy wet blocks that overlap the requested interval.  Old generations,
/// epochs, malformed blocks, and channel/frame mismatches are discarded with
/// reason-specific diagnostics rather than being silently counted together.
#[allow(clippy::too_many_arguments)]
pub(crate) fn read_timeline(
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
            let channels = channels as usize;
            let src = (from - first) as usize * channels;
            let dst = (from - start) as usize * channels;
            let len = (to - from) as usize * channels;
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
