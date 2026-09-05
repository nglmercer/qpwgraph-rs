#![no_std]

#[cfg(test)]
extern crate std;

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Overflow policy used by both virtual cables: keep the oldest queued audio
/// and drop the incoming tail. This bounds latency and makes overruns visible.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PushResult {
    Complete,
    DroppedNewest { samples: usize },
}

/// Lifecycle of a virtual audio stream.  Pausing preserves the virtual clock;
/// only an explicit reset while stopped can discard accumulated positions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamState {
    Stopped,
    Running,
    Paused,
}

/// Monotonic counters shared by the render and capture sides of a virtual
/// cable.  Counters are frames, not bytes, so format conversion cannot make a
/// timestamp jump backwards or appear to run faster than the stream.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StreamPosition {
    pub frames_started: u64,
    pub frames_presented: u64,
    pub frames_captured: u64,
    pub qpc_start: u64,
    pub packet_number: u64,
    pub discontinuity_generation: u64,
}

/// Allocation-free virtual stream clock used by both sides of a kernel cable.
///
/// The ACX adapter will call these methods from its stream callbacks.  The
/// core deliberately does not know about QPC frequency or WDK types: it only
/// preserves ordering and monotonicity, leaving clock-domain conversion to
/// the adapter wrapper.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamClock {
    sample_rate: u32,
    state: StreamState,
    position: StreamPosition,
    clock_origin_set: bool,
}

impl StreamClock {
    pub const fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            state: StreamState::Stopped,
            position: StreamPosition {
                frames_started: 0,
                frames_presented: 0,
                frames_captured: 0,
                qpc_start: 0,
                packet_number: 0,
                discontinuity_generation: 0,
            },
            clock_origin_set: false,
        }
    }

    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub const fn state(&self) -> StreamState {
        self.state
    }

    pub const fn position(&self) -> StreamPosition {
        self.position
    }

    /// Start or resume without resetting positions. The first QPC value is
    /// retained as the stream's clock origin for the lifetime of the stream.
    pub fn start(&mut self, qpc: u64) {
        if !self.clock_origin_set {
            self.position.qpc_start = qpc;
            self.clock_origin_set = true;
        }
        self.state = StreamState::Running;
    }

    pub fn pause(&mut self) {
        if self.state == StreamState::Running {
            self.state = StreamState::Paused;
        }
    }

    pub fn stop(&mut self) {
        self.state = StreamState::Stopped;
    }

    /// Reset only a stopped stream. Keeping this restriction in the core
    /// prevents an in-flight ACX callback from rewinding a live capture clock.
    pub fn reset(&mut self) -> bool {
        if self.state != StreamState::Stopped {
            return false;
        }
        self.position = StreamPosition::default();
        self.clock_origin_set = false;
        true
    }

    /// Mark a render packet as accepted by the stream. A zero-length packet
    /// still advances the packet number, which makes a callback discontinuity
    /// visible without inventing frames.
    pub fn begin_render_packet(&mut self, frames: u32) -> bool {
        if self.state != StreamState::Running {
            return false;
        }
        self.position.frames_started = self
            .position
            .frames_started
            .saturating_add(u64::from(frames));
        self.position.packet_number = self.position.packet_number.saturating_add(1);
        true
    }

    /// Advance the presented position after the packet has been consumed by
    /// the virtual transport. It can never outrun frames accepted by render.
    pub fn present_render(&mut self, frames: u32) -> bool {
        if self.state != StreamState::Running {
            return false;
        }
        let target = self
            .position
            .frames_presented
            .saturating_add(u64::from(frames));
        self.position.frames_presented = target.min(self.position.frames_started);
        true
    }

    /// Advance capture time. Silence produced during underflow is still real
    /// capture time, so the caller uses this method for both audio and silence.
    pub fn capture_frames(&mut self, frames: u32) -> bool {
        if self.state != StreamState::Running {
            return false;
        }
        self.position.frames_captured = self
            .position
            .frames_captured
            .saturating_add(u64::from(frames));
        true
    }

    pub fn mark_discontinuity(&mut self) {
        self.position.discontinuity_generation =
            self.position.discontinuity_generation.saturating_add(1);
    }
}

/// A timestamped packet boundary in the virtual clock domain.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PacketStamp {
    pub packet_number: u64,
    pub qpc: u64,
    pub frames: u32,
    pub discontinuity_generation: u64,
}

/// Small, allocation-free packet timeline. QPC values are clamped forward so
/// a clock adjustment cannot make a later packet appear older than its
/// predecessor.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PacketTimeline {
    next_packet_number: u64,
    last_qpc: u64,
    discontinuity_generation: u64,
}

impl PacketTimeline {
    pub const fn new() -> Self {
        Self {
            next_packet_number: 0,
            last_qpc: 0,
            discontinuity_generation: 0,
        }
    }

    pub const fn discontinuity_generation(&self) -> u64 {
        self.discontinuity_generation
    }

    pub fn mark_discontinuity(&mut self) {
        self.discontinuity_generation = self.discontinuity_generation.saturating_add(1);
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn push(&mut self, qpc: u64, frames: u32) -> PacketStamp {
        let qpc = qpc.max(self.last_qpc);
        let stamp = PacketStamp {
            packet_number: self.next_packet_number,
            qpc,
            frames,
            discontinuity_generation: self.discontinuity_generation,
        };
        self.next_packet_number = self.next_packet_number.saturating_add(1);
        self.last_qpc = qpc;
        stamp
    }
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
        Self::new()
    }
}

impl<const N: usize> SpscSampleRing<N> {
    /// Construct an empty ring in a `const` context so the kernel adapter can
    /// keep its bounded transport in static non-paged storage.
    pub const fn new() -> Self {
        Self {
            samples: UnsafeCell::new([0.0; N]),
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            underflows: AtomicU64::new(0),
            overflows: AtomicU64::new(0),
            discontinuities: AtomicU64::new(0),
        }
    }

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

    /// Record a tail that the cable intentionally did not submit to the ring.
    /// This keeps the cable's whole-frame admission policy visible through the
    /// same counters as a normal ring overflow.
    pub fn record_overflow(&self, samples: usize) {
        if samples > 0 {
            self.overflows.fetch_add(samples as u64, Ordering::Relaxed);
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

/// A complete render-to-capture virtual cable contract.
///
/// This is the driver-independent transport that the ACX adapter will wrap.
/// Windows shared-mode mixing is expected to produce one mixed render stream;
/// the cable therefore admits one render stream and one capture stream. The
/// ring remains SPSC and bounded, while the clocks make underflow/overflow
/// behavior observable without allowing time to rewind.
pub struct VirtualCable<const N: usize> {
    ring: SpscSampleRing<N>,
    render_clock: StreamClock,
    capture_clock: StreamClock,
    render_timeline: PacketTimeline,
    capture_timeline: PacketTimeline,
    last_render_packet: Option<PacketStamp>,
    last_capture_packet: Option<PacketStamp>,
    channels: u16,
    active_render_streams: AtomicU32,
    active_capture_streams: AtomicU32,
}

impl<const N: usize> VirtualCable<N> {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            ring: SpscSampleRing::default(),
            render_clock: StreamClock::new(sample_rate.max(1)),
            capture_clock: StreamClock::new(sample_rate.max(1)),
            render_timeline: PacketTimeline::new(),
            capture_timeline: PacketTimeline::new(),
            last_render_packet: None,
            last_capture_packet: None,
            channels: channels.max(1),
            active_render_streams: AtomicU32::new(0),
            active_capture_streams: AtomicU32::new(0),
        }
    }

    pub const fn channels(&self) -> u16 {
        self.channels
    }

    pub const fn render_clock(&self) -> StreamClock {
        self.render_clock
    }

    pub const fn capture_clock(&self) -> StreamClock {
        self.capture_clock
    }

    pub const fn last_render_packet(&self) -> Option<PacketStamp> {
        self.last_render_packet
    }

    pub const fn last_capture_packet(&self) -> Option<PacketStamp> {
        self.last_capture_packet
    }

    pub fn active_render_streams(&self) -> u32 {
        self.active_render_streams.load(Ordering::Acquire)
    }

    pub fn active_capture_streams(&self) -> u32 {
        self.active_capture_streams.load(Ordering::Acquire)
    }

    /// Start the single mixed render stream. A second client is refused until
    /// the first one stops, avoiding an uncontrolled multi-producer ring.
    pub fn start_render(&mut self, qpc: u64) -> bool {
        if self
            .active_render_streams
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.render_clock.start(qpc);
        true
    }

    pub fn stop_render(&mut self) {
        if self.active_render_streams.swap(0, Ordering::AcqRel) != 0 {
            self.render_clock.stop();
        }
    }

    /// Start the single capture stream exposed by the cable.
    pub fn start_capture(&mut self, qpc: u64) -> bool {
        if self
            .active_capture_streams
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.capture_clock.start(qpc);
        true
    }

    pub fn stop_capture(&mut self) {
        if self.active_capture_streams.swap(0, Ordering::AcqRel) != 0 {
            self.capture_clock.stop();
        }
    }

    /// Accept one render packet. The ring's drop-newest policy is preserved;
    /// a dropped tail advances the discontinuity generation but never rewinds
    /// the render clock.
    pub fn push_render_packet(&mut self, qpc: u64, frames: u32, samples: &[f32]) -> PushResult {
        let expected_samples = usize::try_from(frames)
            .ok()
            .and_then(|frames| frames.checked_mul(usize::from(self.channels)));
        if expected_samples != Some(samples.len()) {
            self.render_clock.mark_discontinuity();
            self.render_timeline.mark_discontinuity();
            return PushResult::DroppedNewest {
                samples: samples.len(),
            };
        }
        if self.active_render_streams() == 0 || !self.render_clock.begin_render_packet(frames) {
            return PushResult::DroppedNewest {
                samples: samples.len(),
            };
        }
        self.last_render_packet = Some(self.render_timeline.push(qpc, frames));
        let channels = usize::from(self.channels);
        let capacity_frames = N / channels;
        let queued_frames = self.ring.queued() / channels;
        let accepted_frames = (frames as usize).min(capacity_frames.saturating_sub(queued_frames));
        let accepted_samples = accepted_frames.saturating_mul(channels);
        let requested_drop = samples.len().saturating_sub(accepted_samples);
        if requested_drop > 0 {
            self.ring.record_overflow(requested_drop);
        }
        let ring_result = self.ring.push(&samples[..accepted_samples]);
        let ring_drop = match ring_result {
            PushResult::Complete => 0,
            PushResult::DroppedNewest { samples } => samples,
        };
        let presented_frames = accepted_samples
            .saturating_sub(ring_drop)
            .checked_div(channels)
            .unwrap_or(0);
        let dropped = requested_drop.saturating_add(ring_drop);
        if dropped > 0 {
            self.render_clock.mark_discontinuity();
            self.render_timeline.mark_discontinuity();
            let _ = self.render_clock.present_render(presented_frames as u32);
            PushResult::DroppedNewest { samples: dropped }
        } else {
            let _ = self.render_clock.present_render(frames);
            PushResult::Complete
        }
    }

    /// Fill one capture packet, padding starvation with silence. Capture time
    /// advances for both real samples and silence, and underflow marks a
    /// discontinuity rather than replaying stale ring contents.
    pub fn pop_capture_packet(&mut self, qpc: u64, samples: &mut [f32]) -> bool {
        if self.active_capture_streams() == 0 {
            samples.fill(0.0);
            return false;
        }
        let underflows = self.ring.underflows();
        self.ring.pop_or_silence(samples);
        let frames = samples.len() / usize::from(self.channels);
        let _ = self.capture_clock.capture_frames(frames as u32);
        self.last_capture_packet = Some(self.capture_timeline.push(qpc, frames as u32));
        if self.ring.underflows() != underflows {
            self.capture_clock.mark_discontinuity();
            self.capture_timeline.mark_discontinuity();
        }
        true
    }

    pub fn queued_samples(&self) -> usize {
        self.ring.queued()
    }

    pub fn underflows(&self) -> u64 {
        self.ring.underflows()
    }

    pub fn overflows(&self) -> u64 {
        self.ring.overflows()
    }

    pub fn discontinuities(&self) -> u64 {
        self.ring
            .discontinuities()
            .saturating_add(self.render_clock.position().discontinuity_generation)
            .saturating_add(self.capture_clock.position().discontinuity_generation)
    }

    /// Clear transport state only after both endpoints have stopped. The
    /// clocks reset as a documented stream transition; live callbacks cannot
    /// rewind a position underneath an active client.
    pub fn reset(&mut self) -> bool {
        if self.active_render_streams() != 0 || self.active_capture_streams() != 0 {
            return false;
        }
        self.ring.clear();
        let render_reset = self.render_clock.reset();
        let capture_reset = self.capture_clock.reset();
        self.render_timeline.reset();
        self.capture_timeline.reset();
        self.last_render_packet = None;
        self.last_capture_packet = None;
        render_reset && capture_reset
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

    #[test]
    fn stream_clock_keeps_positions_monotonic_across_pause_resume() {
        let mut clock = StreamClock::new(48_000);
        clock.start(100);
        assert!(clock.begin_render_packet(480));
        assert!(clock.present_render(480));
        assert!(clock.capture_frames(480));
        clock.pause();
        let paused = clock.position();
        assert!(!clock.capture_frames(480));
        clock.start(50);
        assert!(clock.begin_render_packet(240));
        assert!(clock.present_render(240));
        assert!(clock.capture_frames(240));
        let resumed = clock.position();
        assert_eq!(resumed.qpc_start, 100);
        assert!(resumed.frames_started >= paused.frames_started);
        assert!(resumed.frames_presented >= paused.frames_presented);
        assert!(resumed.frames_captured >= paused.frames_captured);
    }

    #[test]
    fn reset_is_rejected_while_live_and_allowed_after_stop() {
        let mut clock = StreamClock::new(48_000);
        clock.start(1);
        assert!(!clock.reset());
        clock.stop();
        assert!(clock.reset());
        assert_eq!(clock.position(), StreamPosition::default());
    }

    #[test]
    fn packet_timeline_clamps_backward_qpc_and_marks_discontinuity() {
        let mut timeline = PacketTimeline::new();
        let first = timeline.push(100, 480);
        timeline.mark_discontinuity();
        let second = timeline.push(90, 480);
        assert_eq!(first.packet_number, 0);
        assert_eq!(second.packet_number, 1);
        assert_eq!(second.qpc, 100);
        assert_eq!(second.discontinuity_generation, 1);
    }

    #[test]
    fn virtual_cable_enforces_one_stream_and_advances_through_silence() {
        let mut cable = VirtualCable::<4>::new(48_000, 2);
        assert!(cable.start_render(100));
        assert!(!cable.start_render(101));
        assert!(cable.start_capture(200));
        assert!(!cable.start_capture(201));

        assert_eq!(
            cable.push_render_packet(110, 2, &[1.0, 2.0, 3.0, 4.0]),
            PushResult::Complete
        );
        assert_eq!(
            cable.last_render_packet().map(|packet| packet.qpc),
            Some(110)
        );

        let mut output = [9.0; 6];
        assert!(cable.pop_capture_packet(210, &mut output));
        assert_eq!(output, [1.0, 2.0, 3.0, 4.0, 0.0, 0.0]);
        assert_eq!(
            cable.last_capture_packet().map(|packet| packet.qpc),
            Some(210)
        );
        assert_eq!(cable.capture_clock().position().frames_captured, 3);
        assert_eq!(cable.underflows(), 2);
        assert!(cable.discontinuities() > 0);
        assert!(!cable.reset());

        cable.stop_render();
        cable.stop_capture();
        assert!(cable.reset());
        assert_eq!(cable.queued_samples(), 0);
        assert_eq!(cable.render_clock().position(), StreamPosition::default());
        assert_eq!(cable.capture_clock().position(), StreamPosition::default());
    }

    #[test]
    fn virtual_cable_drops_newest_render_tail() {
        let mut cable = VirtualCable::<4>::new(48_000, 2);
        assert!(cable.start_render(1));
        assert_eq!(
            cable.push_render_packet(2, 3, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
            PushResult::DroppedNewest { samples: 2 }
        );
        assert_eq!(cable.overflows(), 2);
        assert_eq!(cable.queued_samples(), 4);
        assert_eq!(cable.render_clock().position().frames_started, 3);
        assert_eq!(cable.render_clock().position().frames_presented, 2);
    }

    #[test]
    fn virtual_cable_drops_only_complete_frames_for_unaligned_capacity() {
        let mut cable = VirtualCable::<5>::new(48_000, 2);
        assert!(cable.start_render(1));
        assert_eq!(
            cable.push_render_packet(2, 3, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
            PushResult::DroppedNewest { samples: 2 }
        );
        assert_eq!(cable.queued_samples(), 4);
        assert_eq!(cable.overflows(), 2);
        assert_eq!(cable.render_clock().position().frames_presented, 2);

        assert!(cable.start_capture(3));
        let mut output = [0.0; 4];
        assert!(cable.pop_capture_packet(4, &mut output));
        assert_eq!(output, [1.0, 2.0, 3.0, 4.0]);
    }
}
