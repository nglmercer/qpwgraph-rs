//! Sources and sinks that need no operating system.
//!
//! Two jobs. The first is the ring-backed pair, [`RingSource`] and
//! [`RingSink`], which is how a real device attaches: the WASAPI capture
//! thread owns a [`RingProducer`] and the router owns a [`RingSource`], so
//! neither ever blocks the other and neither knows the other's clock. The
//! second is the deterministic pair, [`BufferSource`] and [`CaptureSink`],
//! which is how the routing semantics are tested without a driver at all --
//! the Phase 3 exit criterion in the parity roadmap.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::buffer::{ring, RingConsumer, RingProducer};
use super::engine::{AudioSink, AudioSource, Backlog, SinkWrite, SourceRead, StreamHealth};
use super::format::AudioFormat;

/// A source fed by another thread through a bounded ring.
pub struct RingSource {
    format: AudioFormat,
    consumer: RingConsumer,
    /// Set by the producing side when its device goes away, so the router
    /// stops the route instead of treating a dead device as a quiet one.
    lost: Arc<std::sync::atomic::AtomicBool>,
}

impl RingSource {
    /// Make a second handle for the same single-consumer ring.
    ///
    /// The process-capture manager uses this only as a lease: the template
    /// handle remains idle while the returned handle is the active router
    /// consumer. Keeping the template lets a later route recovery obtain a
    /// fresh handle without opening another operating-system capture.
    #[cfg(target_os = "windows")]
    pub(crate) fn lease(&self) -> Self {
        Self {
            format: self.format,
            consumer: self.consumer.clone(),
            lost: Arc::clone(&self.lost),
        }
    }
}

/// The writing end of a [`RingSource`], held by the device thread.
pub struct RingSourceFeed {
    producer: RingProducer,
    lost: Arc<std::sync::atomic::AtomicBool>,
}

impl RingSourceFeed {
    /// Hand captured audio to the router. Returns how many samples fit;
    /// anything short was dropped rather than allowed to grow the queue.
    pub fn push(&mut self, samples: &[f32]) -> usize {
        self.producer.write(samples)
    }

    /// Report that the device behind this source is gone.
    pub fn mark_lost(&self) {
        self.lost.store(true, std::sync::atomic::Ordering::Release);
    }

    pub fn space(&self) -> usize {
        self.producer.space()
    }
}

/// Create a ring-backed source and the feed that fills it.
///
/// `capacity_frames` is the whole latency budget between the device and the
/// router, and it is fixed here: §19.2 forbids a queue that grows to hide a
/// stalled consumer.
pub fn ring_source(format: AudioFormat, capacity_frames: usize) -> (RingSource, RingSourceFeed) {
    let (producer, consumer) = ring(format.samples(capacity_frames));
    let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
    (
        RingSource {
            format,
            consumer,
            lost: Arc::clone(&lost),
        },
        RingSourceFeed { producer, lost },
    )
}

/// The capture side of a fixed-capacity process-loopback fan-out.
///
/// The feed vector is fixed before the capture worker starts. The worker only
/// loads an atomic enable bit and writes to already allocated rings; adding or
/// removing a consumer on the control thread therefore never takes a lock or
/// allocates in the audio loop.
pub struct RingSourceFanout {
    feeds: Vec<RingSourceFeed>,
    active: Arc<[AtomicBool]>,
}

/// Control handle for the active slots in [`RingSourceFanout`].
#[derive(Clone)]
pub struct RingSourceFanoutControl {
    active: Arc<[AtomicBool]>,
}

impl RingSourceFanoutControl {
    pub fn set_active(&self, slot: usize, active: bool) {
        if let Some(flag) = self.active.get(slot) {
            flag.store(active, Ordering::Release);
        }
    }

    pub fn is_active(&self, slot: usize) -> bool {
        self.active
            .get(slot)
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }

    pub fn slots(&self) -> usize {
        self.active.len()
    }
}

impl RingSourceFanout {
    /// Push one captured block to every currently active consumer.
    ///
    /// A full individual ring does not prevent another consumer from
    /// receiving the block. Each consumer has its own bounded overflow
    /// boundary, which is exactly what a slow Recorder or meter needs.
    pub fn push(&mut self, samples: &[f32]) {
        for (slot, feed) in self.feeds.iter_mut().enumerate() {
            if self.active[slot].load(Ordering::Acquire) {
                let _ = feed.push(samples);
            }
        }
    }

    pub fn mark_lost(&self) {
        for feed in &self.feeds {
            feed.mark_lost();
        }
    }
}

/// Allocate a bounded ring and producer for each fan-out slot.
///
/// `slots` is intentionally fixed for the lifetime of the capture. It is a
/// control-plane capacity limit, not an audio queue: unused slots are
/// disabled and consume only their preallocated bounded ring.
pub fn ring_source_fanout(
    format: AudioFormat,
    capacity_frames: usize,
    slots: usize,
) -> (Vec<RingSource>, RingSourceFanout, RingSourceFanoutControl) {
    let slots = slots.max(1);
    let mut sources = Vec::with_capacity(slots);
    let mut feeds = Vec::with_capacity(slots);
    for _ in 0..slots {
        let (source, feed) = ring_source(format, capacity_frames);
        sources.push(source);
        feeds.push(feed);
    }
    let active: Arc<[AtomicBool]> = Arc::from(
        (0..slots)
            .map(|_| AtomicBool::new(false))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );
    let fanout = RingSourceFanout {
        feeds,
        active: Arc::clone(&active),
    };
    let control = RingSourceFanoutControl { active };
    (sources, fanout, control)
}

impl AudioSource for RingSource {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn read(&mut self, dst: &mut [f32]) -> SourceRead {
        if self.lost.load(std::sync::atomic::Ordering::Acquire) {
            return SourceRead::lost();
        }
        let channels = self.format.channels as usize;
        // Read whole frames only. Handing the router a partial frame would
        // rotate every channel by one for the rest of the stream.
        let wanted = (dst.len() / channels) * channels;
        let read = self.consumer.read(&mut dst[..wanted]);
        let frames = read / channels;
        SourceRead {
            frames,
            health: if frames * channels == wanted {
                StreamHealth::Ok
            } else {
                StreamHealth::Starved
            },
        }
    }

    fn reset(&mut self) {
        self.consumer.clear();
    }
}

/// A sink that hands audio to another thread through a bounded ring.
pub struct RingSink {
    format: AudioFormat,
    producer: RingProducer,
    lost: Arc<std::sync::atomic::AtomicBool>,
}

/// The reading end of a [`RingSink`], held by the device thread.
pub struct RingSinkDrain {
    consumer: RingConsumer,
    lost: Arc<std::sync::atomic::AtomicBool>,
}

impl RingSinkDrain {
    /// Take audio for the device. A short read is silence the device must
    /// fill in; the ring never repeats the previous buffer.
    pub fn pull(&mut self, samples: &mut [f32]) -> usize {
        self.consumer.read(samples)
    }

    pub fn mark_lost(&self) {
        self.lost.store(true, std::sync::atomic::Ordering::Release);
    }

    pub fn available(&self) -> usize {
        self.consumer.available()
    }
}

/// Create a ring-backed sink and the drain that empties it.
pub fn ring_sink(format: AudioFormat, capacity_frames: usize) -> (RingSink, RingSinkDrain) {
    let (producer, consumer) = ring(format.samples(capacity_frames));
    let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
    (
        RingSink {
            format,
            producer,
            lost: Arc::clone(&lost),
        },
        RingSinkDrain { consumer, lost },
    )
}

impl AudioSink for RingSink {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn write(&mut self, src: &[f32]) -> SinkWrite {
        if self.lost.load(std::sync::atomic::Ordering::Acquire) {
            return SinkWrite::lost();
        }
        let channels = self.format.channels as usize;
        let written = self.producer.write(src);
        SinkWrite {
            frames: written / channels,
            health: if written == src.len() {
                StreamHealth::Ok
            } else {
                StreamHealth::Starved
            },
        }
    }

    /// How full the device's queue is running, in frames, which is what the
    /// router's drift controller steers towards half.
    fn backlog(&self) -> Option<Backlog> {
        let channels = self.format.channels as usize;
        Some(Backlog {
            frames: self.producer.backlog() / channels,
            capacity: self.producer.capacity() / channels,
        })
    }
}

/// A source that plays a fixed buffer.
///
/// Deterministic by construction: the same buffer through the same route
/// produces the same output every run, which is what lets the routing rules be
/// asserted rather than listened to.
pub struct BufferSource {
    format: AudioFormat,
    samples: Vec<f32>,
    position: usize,
    looping: bool,
}

impl BufferSource {
    pub fn new(format: AudioFormat, samples: Vec<f32>) -> Self {
        Self {
            format,
            samples,
            position: 0,
            looping: false,
        }
    }

    /// A source that never runs out, for tests about steady state rather
    /// than about the end of a stream.
    pub fn looping(format: AudioFormat, samples: Vec<f32>) -> Self {
        Self {
            looping: true,
            ..Self::new(format, samples)
        }
    }

    /// A sine at `frequency`, the same on every channel.
    ///
    /// The reference signal the parity roadmap's tone-source harness needs:
    /// its RMS, peak, and frequency are all known in advance, so an analyzer
    /// can assert on what came out the other end of a route.
    pub fn tone(format: AudioFormat, frequency: f32, amplitude: f32, frames: usize) -> Self {
        let channels = format.channels as usize;
        let mut samples = Vec::with_capacity(frames * channels);
        for frame in 0..frames {
            let phase =
                std::f32::consts::TAU * frequency * frame as f32 / format.sample_rate as f32;
            let value = phase.sin() * amplitude;
            samples.extend(std::iter::repeat_n(value, channels));
        }
        Self::looping(format, samples)
    }

    /// Frames not yet handed to the router.
    pub fn remaining_frames(&self) -> usize {
        self.format
            .frames(self.samples.len().saturating_sub(self.position))
    }
}

impl AudioSource for BufferSource {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn read(&mut self, dst: &mut [f32]) -> SourceRead {
        let channels = self.format.channels as usize;
        let wanted = (dst.len() / channels) * channels;
        let mut filled = 0;
        while filled < wanted {
            if self.position >= self.samples.len() {
                if !self.looping || self.samples.is_empty() {
                    break;
                }
                self.position = 0;
            }
            let take = (wanted - filled).min(self.samples.len() - self.position);
            dst[filled..filled + take]
                .copy_from_slice(&self.samples[self.position..self.position + take]);
            self.position += take;
            filled += take;
        }
        SourceRead {
            frames: filled / channels,
            health: if filled == wanted {
                StreamHealth::Ok
            } else {
                StreamHealth::Starved
            },
        }
    }

    fn reset(&mut self) {
        self.position = 0;
    }
}

/// Everything a [`CaptureSink`] received, readable from the test thread.
pub type Captured = Arc<Mutex<Vec<f32>>>;

/// A sink that records what it is given.
///
/// The mutex here would be wrong in a device adapter -- §8.1 forbids blocking
/// on the audio path -- but this sink exists to be asserted against, and the
/// router under test is driven synchronously by the same thread that reads
/// the recording.
pub struct CaptureSink {
    format: AudioFormat,
    captured: Captured,
    /// Frames accepted per write, for testing what the router does when a
    /// destination cannot keep up. `None` accepts everything.
    accept_frames: Option<usize>,
    lost: bool,
}

impl CaptureSink {
    pub fn new(format: AudioFormat) -> (Self, Captured) {
        let captured: Captured = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                format,
                captured: Arc::clone(&captured),
                accept_frames: None,
                lost: false,
            },
            captured,
        )
    }

    /// A sink that refuses to take more than `frames` per block.
    pub fn throttled(format: AudioFormat, frames: usize) -> (Self, Captured) {
        let (mut sink, captured) = Self::new(format);
        sink.accept_frames = Some(frames);
        (sink, captured)
    }

    /// A sink whose device is already gone.
    pub fn lost(format: AudioFormat) -> (Self, Captured) {
        let (mut sink, captured) = Self::new(format);
        sink.lost = true;
        (sink, captured)
    }
}

impl AudioSink for CaptureSink {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn write(&mut self, src: &[f32]) -> SinkWrite {
        if self.lost {
            return SinkWrite::lost();
        }
        let channels = self.format.channels as usize;
        let offered = src.len() / channels;
        let taken = self.accept_frames.map_or(offered, |cap| cap.min(offered));
        if let Ok(mut captured) = self.captured.lock() {
            captured.extend_from_slice(&src[..taken * channels]);
        }
        SinkWrite {
            frames: taken,
            health: if taken == offered {
                StreamHealth::Ok
            } else {
                StreamHealth::Starved
            },
        }
    }
}

/// A source whose device has gone away.
///
/// Exists so the "device invalidation" path can be tested without unplugging
/// anything.
pub struct LostSource {
    format: AudioFormat,
}

impl LostSource {
    pub fn new(format: AudioFormat) -> Self {
        Self { format }
    }
}

impl AudioSource for LostSource {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn read(&mut self, _dst: &mut [f32]) -> SourceRead {
        SourceRead::lost()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEREO: AudioFormat = AudioFormat::new(48_000, 2);

    #[test]
    fn a_buffer_source_hands_over_its_audio_then_reports_starvation() {
        let mut source = BufferSource::new(STEREO, vec![1.0, 2.0, 3.0, 4.0]);
        let mut block = [0.0; 8];
        let read = source.read(&mut block);
        assert_eq!(read.frames, 2);
        assert_eq!(read.health, StreamHealth::Starved);
        assert_eq!(block[..4], [1.0, 2.0, 3.0, 4.0]);
        // Running dry is starvation, not device loss: the route stays alive.
        assert_eq!(source.read(&mut block).frames, 0);
        assert_eq!(source.remaining_frames(), 0);
    }

    #[test]
    fn a_looping_source_never_starves() {
        let mut source = BufferSource::looping(STEREO, vec![1.0, 2.0]);
        let mut block = [0.0; 6];
        let read = source.read(&mut block);
        assert_eq!(read.health, StreamHealth::Ok);
        assert_eq!(block, [1.0, 2.0, 1.0, 2.0, 1.0, 2.0]);
    }

    #[test]
    fn a_source_only_ever_returns_whole_frames() {
        let mut source = BufferSource::looping(STEREO, vec![1.0, 2.0]);
        // Seven samples is three and a half stereo frames; the half is not
        // handed over, because it would rotate every later frame's channels.
        let mut block = [0.0; 7];
        assert_eq!(source.read(&mut block).frames, 3);
        assert_eq!(block[6], 0.0);
    }

    #[test]
    fn a_tone_source_has_the_amplitude_it_was_asked_for() {
        let mut source = BufferSource::tone(AudioFormat::new(48_000, 1), 1_000.0, 0.5, 480);
        let mut block = [0.0; 480];
        source.read(&mut block);
        let peak = block.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.01, "peak was {peak}");
    }

    #[test]
    fn a_capture_sink_records_exactly_what_it_accepted() {
        let (mut sink, captured) = CaptureSink::new(STEREO);
        assert_eq!(sink.write(&[1.0, 2.0, 3.0, 4.0]).frames, 2);
        assert_eq!(*captured.lock().unwrap(), vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn a_throttled_sink_takes_what_it_can_and_says_so() {
        let (mut sink, captured) = CaptureSink::throttled(STEREO, 1);
        let write = sink.write(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(write.frames, 1);
        assert_eq!(write.health, StreamHealth::Starved);
        // The refused frame is not silently recorded as if it had played.
        assert_eq!(*captured.lock().unwrap(), vec![1.0, 2.0]);
    }

    #[test]
    fn a_ring_source_carries_audio_from_a_device_thread() {
        let (mut source, mut feed) = ring_source(STEREO, 64);
        assert_eq!(feed.push(&[1.0, 2.0, 3.0, 4.0]), 4);
        let mut block = [0.0; 4];
        let read = source.read(&mut block);
        assert_eq!(read.frames, 2);
        assert_eq!(block, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn a_fanout_delivers_one_capture_block_to_each_active_consumer() {
        let (mut sources, mut fanout, control) = ring_source_fanout(STEREO, 64, 3);
        control.set_active(0, true);
        control.set_active(2, true);
        fanout.push(&[1.0, 2.0, 3.0, 4.0]);

        for slot in [0, 2] {
            let mut block = [0.0; 4];
            assert_eq!(sources[slot].read(&mut block).frames, 2);
            assert_eq!(block, [1.0, 2.0, 3.0, 4.0]);
        }
        let mut inactive = [9.0; 4];
        assert_eq!(sources[1].read(&mut inactive).frames, 0);
        assert_eq!(inactive, [9.0; 4]);
    }

    #[test]
    fn a_ring_source_reports_loss_rather_than_silence_when_its_device_goes() {
        let (mut source, feed) = ring_source(STEREO, 64);
        feed.mark_lost();
        let mut block = [0.0; 4];
        assert_eq!(source.read(&mut block).health, StreamHealth::Lost);
    }

    #[test]
    fn a_ring_sink_reports_its_fill_level_for_drift_correction() {
        let (mut sink, _drain) = ring_sink(STEREO, 100);
        sink.write(&[0.0; 50]);
        let backlog = sink.backlog().expect("a ring sink knows its own fill");
        assert_eq!(backlog.capacity, 100);
        assert_eq!(backlog.frames, 25);
    }

    #[test]
    fn a_ring_sink_that_is_full_reports_a_short_write() {
        let (mut sink, _drain) = ring_sink(STEREO, 2);
        let write = sink.write(&[0.0; 8]);
        assert_eq!(write.frames, 2);
        assert_eq!(write.health, StreamHealth::Starved);
    }
}
