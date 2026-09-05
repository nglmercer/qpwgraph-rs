#![no_std]

#[cfg(test)]
extern crate std;

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Overflow policy used by both virtual cables: keep the oldest queued audio
/// and drop the incoming tail. This bounds latency and makes overruns visible.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PushResult {
    Complete,
    DroppedNewest { samples: usize },
}

/// Allocation-free single-owner ring for the driver's packet path.
///
/// Synchronization is supplied by the ACX stream wrapper: the ring itself has
/// one render-side writer and one capture-side reader and performs no locks,
/// allocation, logging, or formatting.
pub struct SampleRing<const N: usize> {
    samples: [f32; N],
    read: usize,
    len: usize,
    underflows: u64,
    overflows: u64,
    discontinuities: u64,
}

/// Lock-free single-producer/single-consumer transport for an ACX stream.
///
/// The render callback is the sole producer and the capture callback is the
/// sole consumer. Positions are monotonically wrapping counters; acquire and
/// release ordering publishes samples without a kernel mutex or an allocation.
/// The ring intentionally uses the same drop-newest/silence-on-underflow
/// policy as [`SampleRing`], so the policy is testable before WDK bindings are
/// available.
pub struct SpscSampleRing<const N: usize> {
    samples: UnsafeCell<[f32; N]>,
    write: AtomicUsize,
    read: AtomicUsize,
    underflows: AtomicU64,
    overflows: AtomicU64,
    discontinuities: AtomicU64,
}

// SAFETY: The API enforces one writer and one reader. The writer publishes a
// sample before advancing `write` with Release; the reader observes it with
// Acquire. No other method exposes the backing array.
unsafe impl<const N: usize> Sync for SpscSampleRing<N> {}

impl<const N: usize> Default for SpscSampleRing<N> {
    fn default() -> Self {
        Self {
            samples: UnsafeCell::new([0.0; N]),
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            underflows: AtomicU64::new(0),
            overflows: AtomicU64::new(0),
            discontinuities: AtomicU64::new(0),
        }
    }
}

impl<const N: usize> SpscSampleRing<N> {
    pub const fn capacity(&self) -> usize {
        N
    }

    pub fn queued(&self) -> usize {
        if N == 0 {
            return 0;
        }
        let write = self.write.load(Ordering::Acquire);
        let read = self.read.load(Ordering::Acquire);
        write.wrapping_sub(read).min(N)
    }

    pub fn push(&self, input: &[f32]) -> PushResult {
        if N == 0 || input.is_empty() {
            return if input.is_empty() {
                PushResult::Complete
            } else {
                self.overflows
                    .fetch_add(input.len() as u64, Ordering::Relaxed);
                self.discontinuities.fetch_add(1, Ordering::Relaxed);
                PushResult::DroppedNewest {
                    samples: input.len(),
                }
            };
        }
        let write = self.write.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        let queued = write.wrapping_sub(read).min(N);
        let accepted = input.len().min(N - queued);
        // SAFETY: only the producer writes these slots, and the consumer has
        // advanced `read` with Release before a slot is reused.
        let samples = unsafe { &mut *self.samples.get() };
        for (offset, sample) in input[..accepted].iter().copied().enumerate() {
            samples[(write.wrapping_add(offset)) % N] = sample;
        }
        self.write
            .store(write.wrapping_add(accepted), Ordering::Release);
        let dropped = input.len() - accepted;
        if dropped == 0 {
            PushResult::Complete
        } else {
            self.overflows.fetch_add(dropped as u64, Ordering::Relaxed);
            self.discontinuities.fetch_add(1, Ordering::Relaxed);
            PushResult::DroppedNewest { samples: dropped }
        }
    }

    /// Fill the complete output slice. Missing samples are explicit silence,
    /// never stale data left by a previous packet.
    pub fn pop_or_silence(&self, output: &mut [f32]) {
        if output.is_empty() {
            return;
        }
        let read = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Acquire);
        let available = write.wrapping_sub(read).min(N).min(output.len());
        // SAFETY: only the consumer reads these slots, and the producer has
        // published them with Release before the Acquire above.
        let samples = unsafe { &*self.samples.get() };
        for (offset, output) in output[..available].iter_mut().enumerate() {
            *output = samples[(read.wrapping_add(offset)) % N];
        }
        self.read
            .store(read.wrapping_add(available), Ordering::Release);
        let missing = output.len() - available;
        output[available..].fill(0.0);
        if missing > 0 {
            self.underflows.fetch_add(missing as u64, Ordering::Relaxed);
            self.discontinuities.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Reset positions while the stream is stopped. Calling this while both
    /// callbacks are active is a programming error because it discards the
    /// producer's unpublished samples.
    pub fn clear(&self) {
        let write = self.write.load(Ordering::Acquire);
        self.read.store(write, Ordering::Release);
        self.discontinuities.fetch_add(1, Ordering::Relaxed);
    }

    pub fn underflows(&self) -> u64 {
        self.underflows.load(Ordering::Relaxed)
    }

    pub fn overflows(&self) -> u64 {
        self.overflows.load(Ordering::Relaxed)
    }

    pub fn discontinuities(&self) -> u64 {
        self.discontinuities.load(Ordering::Relaxed)
    }
}

impl<const N: usize> Default for SampleRing<N> {
    fn default() -> Self {
        Self {
            samples: [0.0; N],
            read: 0,
            len: 0,
            underflows: 0,
            overflows: 0,
            discontinuities: 0,
        }
    }
}

impl<const N: usize> SampleRing<N> {
    pub const fn capacity(&self) -> usize {
        N
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn underflows(&self) -> u64 {
        self.underflows
    }

    pub const fn overflows(&self) -> u64 {
        self.overflows
    }

    pub const fn discontinuities(&self) -> u64 {
        self.discontinuities
    }

    pub fn mark_discontinuity(&mut self) {
        self.discontinuities = self.discontinuities.saturating_add(1);
    }

    pub fn push(&mut self, input: &[f32]) -> PushResult {
        let accepted = input.len().min(N.saturating_sub(self.len));
        for (offset, sample) in input[..accepted].iter().copied().enumerate() {
            let index = (self.read + self.len + offset) % N;
            self.samples[index] = sample;
        }
        self.len += accepted;
        let dropped = input.len() - accepted;
        if dropped == 0 {
            PushResult::Complete
        } else {
            self.overflows = self.overflows.saturating_add(dropped as u64);
            self.mark_discontinuity();
            PushResult::DroppedNewest { samples: dropped }
        }
    }

    /// Fill every requested capture sample. Starvation is padded with silence
    /// and counted, so no stale kernel buffer contents reach user mode.
    pub fn pop_or_silence(&mut self, output: &mut [f32]) {
        let available = output.len().min(self.len);
        for sample in &mut output[..available] {
            *sample = self.samples[self.read];
            self.read = (self.read + 1) % N;
        }
        self.len -= available;
        let missing = output.len() - available;
        output[available..].fill(0.0);
        if missing > 0 {
            self.underflows = self.underflows.saturating_add(missing as u64);
            self.mark_discontinuity();
        }
    }

    pub fn clear(&mut self) {
        self.read = 0;
        self.len = 0;
        self.mark_discontinuity();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn wraps_without_reordering() {
        let mut ring = SampleRing::<4>::default();
        ring.push(&[1.0, 2.0, 3.0]);
        let mut first = [0.0; 2];
        ring.pop_or_silence(&mut first);
        ring.push(&[4.0, 5.0]);
        let mut second = [0.0; 3];
        ring.pop_or_silence(&mut second);
        assert_eq!(first, [1.0, 2.0]);
        assert_eq!(second, [3.0, 4.0, 5.0]);
    }

    #[test]
    fn overflow_drops_newest_and_is_bounded() {
        let mut ring = SampleRing::<3>::default();
        assert_eq!(
            ring.push(&[1.0, 2.0, 3.0, 4.0]),
            PushResult::DroppedNewest { samples: 1 }
        );
        let mut output = [0.0; 3];
        ring.pop_or_silence(&mut output);
        assert_eq!(output, [1.0, 2.0, 3.0]);
        assert_eq!(ring.overflows(), 1);
    }

    #[test]
    fn underflow_is_silence_and_is_counted() {
        let mut ring = SampleRing::<3>::default();
        ring.push(&[0.25]);
        let mut output = [9.0; 3];
        ring.pop_or_silence(&mut output);
        assert_eq!(output, [0.25, 0.0, 0.0]);
        assert_eq!(ring.underflows(), 2);
    }

    #[test]
    fn spsc_ring_keeps_order_across_threads() {
        let ring = Arc::new(SpscSampleRing::<8>::default());
        let producer = Arc::clone(&ring);
        let writer = std::thread::spawn(move || {
            producer.push(&[1.0, 2.0, 3.0, 4.0]);
        });
        writer.join().unwrap();
        let mut output = [0.0; 4];
        ring.pop_or_silence(&mut output);
        assert_eq!(output, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn spsc_ring_has_bounded_overflow_and_silence_underflow() {
        let ring = SpscSampleRing::<2>::default();
        assert_eq!(
            ring.push(&[1.0, 2.0, 3.0]),
            PushResult::DroppedNewest { samples: 1 }
        );
        let mut output = [0.0; 3];
        ring.pop_or_silence(&mut output);
        assert_eq!(output, [1.0, 2.0, 0.0]);
        assert_eq!(ring.overflows(), 1);
        assert_eq!(ring.underflows(), 1);
    }
}
