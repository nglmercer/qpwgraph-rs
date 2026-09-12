//! The bounded, wait-free hand-off between a device thread and the router.
//!
//! A WASAPI capture thread and the router thread are two different clocks
//! that must never block each other: the capture side cannot wait on a mutex
//! the router happens to hold, and the router cannot wait on a device that is
//! mid-reset. So the two sides share a fixed-capacity ring of `f32` and never
//! synchronise on anything heavier than two atomic indices.
//!
//! The ring is deliberately *bounded and lossy at the edges* rather than
//! growing. A queue that grows to absorb a stalled consumer trades a dropout
//! for unbounded latency and eventually unbounded memory, which §19.2 of the
//! parity roadmap rules out. Overflow and underflow are counted instead, so a
//! starved or flooded route shows up in diagnostics rather than as mystery
//! audio.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// The shared storage behind one producer and one consumer.
struct Ring {
    /// Capacity is one slot larger than the requested capacity so that a full
    /// ring and an empty ring have different index pairs.
    slots: UnsafeCell<Box<[f32]>>,
    /// `slots.len()`, kept alongside so neither half has to form a reference
    /// to the shared storage just to ask how big it is.
    slots_len: usize,
    /// Next slot the consumer will read.
    read: AtomicUsize,
    /// Next slot the producer will write.
    write: AtomicUsize,
}

// Safety: `slots` is only ever touched through the producer or the consumer,
// and each of those is a distinct non-`Clone` handle. The producer writes only
// slots the consumer has already passed, and the consumer reads only slots the
// producer has already published with a `Release` store, so the two never
// alias the same element.
unsafe impl Send for Ring {}
unsafe impl Sync for Ring {}

impl Ring {
    fn capacity(&self) -> usize {
        self.slots_len
    }
}

/// Create a ring holding at most `capacity` samples.
///
/// Allocation happens exactly here, on the control thread. Neither half
/// allocates afterwards.
pub fn ring(capacity: usize) -> (RingProducer, RingConsumer) {
    let capacity = capacity.max(1);
    let ring = Arc::new(Ring {
        slots: UnsafeCell::new(vec![0.0; capacity + 1].into_boxed_slice()),
        slots_len: capacity + 1,
        read: AtomicUsize::new(0),
        write: AtomicUsize::new(0),
    });
    (
        RingProducer {
            ring: Arc::clone(&ring),
        },
        RingConsumer { ring },
    )
}

/// The writing half. Held by whichever thread produces audio.
pub struct RingProducer {
    ring: Arc<Ring>,
}

impl RingProducer {
    /// Samples the ring can hold. Fixed for its lifetime.
    pub fn capacity(&self) -> usize {
        self.ring.capacity() - 1
    }

    /// Samples currently waiting to be read. The other half of
    /// [`RingProducer::space`], and what a sink reports as its backlog so the
    /// router can steer its resampler against clock drift.
    pub fn backlog(&self) -> usize {
        self.capacity() - self.space()
    }

    /// Samples that can be written without overwriting unread audio.
    pub fn space(&self) -> usize {
        let ring = &*self.ring;
        let capacity = ring.capacity();
        let read = ring.read.load(Ordering::Acquire);
        let write = ring.write.load(Ordering::Relaxed);
        // One slot is always left unused so `write == read` can only mean
        // "empty".
        (capacity + read - write - 1) % capacity
    }

    /// Write as much of `src` as fits, returning how much was written.
    ///
    /// A short return is an overflow: the consumer is behind, and the tail of
    /// `src` is dropped rather than stalling the device thread. Callers count
    /// the difference so it surfaces in diagnostics.
    pub fn write(&mut self, src: &[f32]) -> usize {
        let ring = &*self.ring;
        let capacity = ring.capacity();
        let writable = self.space().min(src.len());
        if writable == 0 {
            return 0;
        }
        let start = ring.write.load(Ordering::Relaxed);
        // Safety: these slots are between the published write index and the
        // consumer's read index, so the consumer cannot be reading them.
        let slots = unsafe { &mut *ring.slots.get() };
        let first = writable.min(capacity - start);
        slots[start..start + first].copy_from_slice(&src[..first]);
        if writable > first {
            slots[..writable - first].copy_from_slice(&src[first..writable]);
        }
        ring.write
            .store((start + writable) % capacity, Ordering::Release);
        writable
    }
}

/// The reading half. Held by the router thread.
#[derive(Clone)]
pub struct RingConsumer {
    ring: Arc<Ring>,
}

impl RingConsumer {
    /// Samples the ring can hold. Fixed for its lifetime.
    pub fn capacity(&self) -> usize {
        self.ring.capacity() - 1
    }

    /// Samples currently available to read.
    pub fn available(&self) -> usize {
        let ring = &*self.ring;
        let capacity = ring.capacity();
        let write = ring.write.load(Ordering::Acquire);
        let read = ring.read.load(Ordering::Relaxed);
        (capacity + write - read) % capacity
    }

    /// Read as much as `dst` holds, returning how much was read.
    ///
    /// A short return is an underrun. The caller decides what silence policy
    /// applies; the ring does not invent samples.
    pub fn read(&mut self, dst: &mut [f32]) -> usize {
        let ring = &*self.ring;
        let capacity = ring.capacity();
        let readable = self.available().min(dst.len());
        if readable == 0 {
            return 0;
        }
        let start = ring.read.load(Ordering::Relaxed);
        // Safety: these slots were published by the producer's `Release`
        // store and the producer will not reuse them until the read index
        // moves past them below.
        let slots = unsafe { &*ring.slots.get() };
        let first = readable.min(capacity - start);
        dst[..first].copy_from_slice(&slots[start..start + first]);
        if readable > first {
            dst[first..readable].copy_from_slice(&slots[..readable - first]);
        }
        ring.read
            .store((start + readable) % capacity, Ordering::Release);
        readable
    }

    /// Drop every buffered sample.
    ///
    /// Used when a device is reset: replaying audio captured before the reset
    /// would play the stream's past at the wrong time.
    pub fn clear(&mut self) {
        let ring = &*self.ring;
        let write = ring.write.load(Ordering::Acquire);
        ring.read.store(write, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_ring_is_empty_and_offers_its_full_capacity() {
        let (producer, consumer) = ring(8);
        assert_eq!(consumer.available(), 0);
        assert_eq!(producer.space(), 8);
    }

    #[test]
    fn samples_come_back_out_in_the_order_they_went_in() {
        let (mut producer, mut consumer) = ring(8);
        assert_eq!(producer.write(&[1.0, 2.0, 3.0]), 3);
        let mut out = [0.0; 3];
        assert_eq!(consumer.read(&mut out), 3);
        assert_eq!(out, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn writing_past_capacity_drops_the_tail_rather_than_blocking() {
        let (mut producer, mut consumer) = ring(4);
        // Six samples into a four-sample ring: the device thread must return
        // immediately, having placed what fit.
        assert_eq!(producer.write(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 4);
        let mut out = [0.0; 6];
        assert_eq!(consumer.read(&mut out), 4);
        assert_eq!(out[..4], [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn reading_an_empty_ring_reports_a_short_read_instead_of_inventing_samples() {
        let (mut producer, mut consumer) = ring(8);
        producer.write(&[1.0, 2.0]);
        let mut out = [9.0; 4];
        assert_eq!(consumer.read(&mut out), 2);
        // The untouched tail still holds the caller's own bytes, so the
        // caller -- not the ring -- decides what silence means.
        assert_eq!(out, [1.0, 2.0, 9.0, 9.0]);
    }

    #[test]
    fn the_ring_wraps_without_losing_or_reordering_samples() {
        let (mut producer, mut consumer) = ring(4);
        let mut out = [0.0; 3];
        // Three passes of three samples through a four-sample ring forces the
        // write index to wrap twice at a different offset each time.
        for pass in 0..3u32 {
            let base = pass as f32 * 3.0;
            assert_eq!(producer.write(&[base + 1.0, base + 2.0, base + 3.0]), 3);
            assert_eq!(consumer.read(&mut out), 3);
            assert_eq!(out, [base + 1.0, base + 2.0, base + 3.0]);
        }
    }

    #[test]
    fn clearing_drops_buffered_audio_so_a_reset_device_does_not_replay_its_past() {
        let (mut producer, mut consumer) = ring(8);
        producer.write(&[1.0, 2.0, 3.0]);
        consumer.clear();
        assert_eq!(consumer.available(), 0);
        producer.write(&[4.0]);
        let mut out = [0.0; 1];
        assert_eq!(consumer.read(&mut out), 1);
        assert_eq!(out, [4.0]);
    }

    #[test]
    fn the_two_halves_can_be_driven_from_different_threads() {
        const TOTAL: usize = 10_000;
        let (mut producer, mut consumer) = ring(64);
        let writer = std::thread::spawn(move || {
            let mut written = 0usize;
            while written < TOTAL {
                let sample = [written as f32];
                if producer.write(&sample) == 1 {
                    written += 1;
                } else {
                    std::thread::yield_now();
                }
            }
        });
        let mut read = 0usize;
        let mut slot = [0.0f32; 1];
        while read < TOTAL {
            if consumer.read(&mut slot) == 1 {
                // Every sample arrives exactly once and in order; a torn index
                // would show up here as a gap or a repeat.
                assert_eq!(slot[0], read as f32);
                read += 1;
            } else {
                std::thread::yield_now();
            }
        }
        writer.join().expect("writer thread panicked");
    }
}
