//! Bounded, non-realtime Hush processing.
//!
//! The PipeWire and Windows callbacks only enqueue sanitized blocks and read
//! already-produced blocks from this module.  DeepFilterNet/Tract, model
//! state, resampling, and frame assembly all live on the worker thread.

use nnnoiseless::{HushDenoiser, HushModel, Resampler, HUSH_FRAME_SIZE, HUSH_SAMPLE_RATE};
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The queue is deliberately small and fixed.  A late worker loses a block
/// and the processor uses its aligned dry path; latency cannot grow without
/// bound.
pub(crate) const HUSH_QUEUE_CAPACITY: usize = 8;
/// One callback quantum is reserved for the worker to finish the block. The
/// dry fallback carries this same quantum in addition to Hush's synthesis
/// delay.
const HUSH_WORKER_PIPELINE_BLOCKS: u64 = 1;

#[inline]
fn channel_is_connected(mask: u16, channel: usize) -> bool {
    channel < u16::BITS as usize && mask & (1u16 << channel) != 0
}

#[derive(Default)]
pub(crate) struct HushDiagnostics {
    pub(crate) underruns: AtomicU64,
    pub(crate) input_overruns: AtomicU64,
    pub(crate) output_overruns: AtomicU64,
    pub(crate) max_input_queue_depth: AtomicU64,
    pub(crate) max_output_queue_depth: AtomicU64,
    pub(crate) generation_resets: AtomicU64,
    pub(crate) processed_blocks: AtomicU64,
    pub(crate) inference_ns: AtomicU64,
    pub(crate) max_inference_ns: AtomicU64,
    pub(crate) worker_ready: AtomicBool,
    pub(crate) worker_failed: AtomicBool,
}

struct AudioBlock {
    generation: u64,
    sequence: u64,
    frames: u32,
    channels: u16,
    channel_mask: u16,
    samples: Vec<f32>,
}

impl AudioBlock {
    fn new(max_samples: usize) -> Self {
        Self {
            generation: 0,
            sequence: 0,
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

    fn try_push_with(
        &self,
        generation: u64,
        sequence: u64,
        frames: u32,
        channels: u16,
        channel_mask: u16,
        fill: impl FnOnce(&mut [f32]),
    ) -> bool {
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
        block.sequence = sequence;
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
    channel_mask: Arc<AtomicU16>,
    attenuation_bits: Arc<AtomicU32>,
    diagnostics: Arc<HushDiagnostics>,
    worker: Option<JoinHandle<()>>,
}

impl HushRuntime {
    pub(crate) fn spawn(
        model: Arc<HushModel>,
        sample_rate: u32,
        channels: u16,
        max_frames: u32,
        attenuation_db: f32,
    ) -> Result<Self, String> {
        let max_samples = max_frames as usize * channels as usize;
        let input = Arc::new(BlockQueue::new(HUSH_QUEUE_CAPACITY, max_samples));
        let output = Arc::new(BlockQueue::new(HUSH_QUEUE_CAPACITY, max_samples));
        let stop = Arc::new(AtomicBool::new(false));
        let signal = Arc::new(WorkerSignal::default());
        let generation = Arc::new(AtomicU64::new(0));
        let channel_mask = Arc::new(AtomicU16::new(if channels >= 16 {
            u16::MAX
        } else {
            (1u16 << channels) - 1
        }));
        let attenuation_bits = Arc::new(AtomicU32::new(attenuation_db.to_bits()));
        let diagnostics = Arc::new(HushDiagnostics::default());

        let worker_input = input.clone();
        let worker_output = output.clone();
        let worker_stop = stop.clone();
        let worker_signal = signal.clone();
        let worker_generation = generation.clone();
        let worker_mask = channel_mask.clone();
        let worker_attenuation = attenuation_bits.clone();
        let worker_diagnostics = diagnostics.clone();
        let worker = thread::Builder::new()
            .name("qpwgraph-hush".into())
            .spawn(move || {
                let initialized = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    HushWorker::new(model, sample_rate, channels, max_frames, attenuation_db)
                }));
                let mut worker = match initialized {
                    Ok(Ok(worker)) => worker,
                    Ok(Err(_)) | Err(_) => {
                        worker_diagnostics
                            .worker_failed
                            .store(true, Ordering::Release);
                        return;
                    }
                };
                worker_diagnostics
                    .worker_ready
                    .store(true, Ordering::Release);
                worker_loop(
                    &mut worker,
                    worker_input,
                    worker_output,
                    worker_stop,
                    worker_signal,
                    worker_generation,
                    worker_mask,
                    worker_attenuation,
                    worker_diagnostics,
                );
            })
            .map_err(|error| format!("could not start Hush worker: {error}"))?;

        Ok(Self {
            input,
            output,
            stop,
            signal,
            generation,
            channel_mask,
            attenuation_bits,
            diagnostics,
            worker: Some(worker),
        })
    }

    pub(crate) fn push(
        &self,
        generation: u64,
        sequence: u64,
        frames: u32,
        channels: u16,
        channel_mask: u16,
        samples: &[f32],
    ) -> bool {
        if samples.len() > self.input.max_samples() {
            return false;
        }
        let pushed = self.input.try_push_with(
            generation,
            sequence,
            frames,
            channels,
            channel_mask,
            |destination| destination[..samples.len()].copy_from_slice(samples),
        );
        if pushed {
            record_maximum(&self.diagnostics.max_input_queue_depth, self.input.depth());
            self.signal.condition.notify_one();
        }
        pushed
    }

    pub(crate) fn pop_ready(
        &self,
        generation: u64,
        sequence: u64,
        frames: u32,
        channels: u16,
        destination: &mut [f32],
    ) -> bool {
        // Consume the oldest block that is ready for this callback. The
        // worker is intentionally asynchronous, so the target is the
        // previous callback's sequence rather than the current one. Discard
        // old generations and blocks that missed that target; preserve a
        // future block for the next callback. The bounded loop is important:
        // malformed worker output cannot make the realtime callback spin
        // forever.
        let Some(target_sequence) = sequence.checked_sub(HUSH_WORKER_PIPELINE_BLOCKS) else {
            return false;
        };
        for _ in 0..HUSH_QUEUE_CAPACITY {
            let Some((block_generation, block_sequence)) = self
                .output
                .peek_with(|block| (block.generation, block.sequence))
            else {
                break;
            };
            if block_generation == generation && block_sequence == target_sequence {
                return self
                    .output
                    .pop_with(|block| {
                        let valid = block.frames == frames && block.channels == channels;
                        let samples = if valid {
                            block.frames as usize * block.channels as usize
                        } else {
                            0
                        };
                        let copy = samples.min(destination.len());
                        if valid {
                            destination[..copy].copy_from_slice(&block.samples[..copy]);
                        }
                        valid
                    })
                    .unwrap_or(false);
            }
            if block_generation == generation && block_sequence > target_sequence {
                // The worker got ahead of this callback. Leave the future
                // block queued so a later callback can consume it; dropping
                // it here would turn a temporary underrun into permanent
                // loss of synchronization.
                return false;
            }
            // Old generations and late blocks cannot be used by this
            // callback. Discard at most the queue capacity worth of them.
            let _ = self.output.pop_with(|_| ());
        }
        false
    }

    pub(crate) fn set_generation(&self, generation: u64, channel_mask: u16) {
        self.generation.store(generation, Ordering::Release);
        self.channel_mask.store(channel_mask, Ordering::Release);
        self.signal.condition.notify_one();
    }

    pub(crate) fn set_attenuation(&self, attenuation_db: f32) {
        self.attenuation_bits
            .store(attenuation_db.to_bits(), Ordering::Release);
        self.signal.condition.notify_one();
    }

    pub(crate) fn diagnostics(&self) -> &Arc<HushDiagnostics> {
        &self.diagnostics
    }
}

impl Drop for HushRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.signal.condition.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
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
    delayed: Vec<f32>,
    wet_delays: Vec<WetDelay>,
    channels: usize,
    max_frames: usize,
    attenuation_db: f32,
    generation: u64,
    last_mask: u16,
}

impl HushWorker {
    fn new(
        model: Arc<HushModel>,
        sample_rate: u32,
        channels: u16,
        max_frames: u32,
        attenuation_db: f32,
    ) -> Result<Self, String> {
        let channels = channels as usize;
        let max_frames = max_frames as usize;
        let mut denoisers = Vec::with_capacity(channels);
        let mut input_resamplers = Vec::with_capacity(channels);
        let mut output_resamplers = Vec::with_capacity(channels);
        let mut input_pending = Vec::with_capacity(channels);
        let mut output_pending = Vec::with_capacity(channels);
        let mut output_offsets = Vec::with_capacity(channels);
        let mut wet_delays = Vec::with_capacity(channels);
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
            wet_delays.push(WetDelay::new(0));
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
            delayed: Vec::with_capacity(output_capacity + HUSH_FRAME_SIZE),
            wet_delays,
            channels,
            max_frames,
            attenuation_db,
            generation: 0,
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
    ) -> Result<(), String> {
        self.generation = generation;
        self.last_mask = mask;
        diagnostics
            .generation_resets
            .fetch_add(1, Ordering::Relaxed);
        for channel in 0..self.channels {
            self.denoisers[channel]
                .reset()
                .map_err(|error| error.to_string())?;
            self.input_resamplers[channel].reset();
            self.output_resamplers[channel].reset();
            self.input_pending[channel].clear();
            self.output_pending[channel].clear();
            self.output_offsets[channel] = 0;
            self.wet_delays[channel].reset();
        }
        Ok(())
    }

    fn process_block(
        &mut self,
        block: &AudioBlock,
        output: &Arc<BlockQueue>,
        generation: &AtomicU64,
        mask: &AtomicU16,
        diagnostics: &HushDiagnostics,
    ) -> Result<(), String> {
        let current_generation = generation.load(Ordering::Acquire);
        let current_mask = mask.load(Ordering::Acquire);
        if block.generation != self.generation || block.generation != current_generation {
            self.reset(block.generation, current_mask, diagnostics)?;
        }
        if block.channel_mask != self.last_mask {
            self.reset(block.generation, block.channel_mask, diagnostics)?;
        }
        let frames = block.frames as usize;
        if frames > self.max_frames || block.channels as usize != self.channels {
            return Err("Hush worker received an invalid audio block".into());
        }
        let inference_start = Instant::now();
        for channel in 0..self.channels {
            if !channel_is_connected(block.channel_mask, channel) {
                self.input_pending[channel].clear();
                self.output_pending[channel].clear();
                self.output_offsets[channel] = 0;
                self.input_resamplers[channel].reset();
                self.output_resamplers[channel].reset();
                self.wet_delays[channel].reset();
                continue;
            }
            for frame in 0..frames {
                self.input_mono[frame] = block.samples[frame * self.channels + channel];
            }
            self.at_model_rate.clear();
            self.input_resamplers[channel]
                .process(&self.input_mono[..frames], &mut self.at_model_rate);
            self.input_pending[channel].extend_from_slice(&self.at_model_rate);
            while self.input_pending[channel].len() >= HUSH_FRAME_SIZE {
                self.frame_input
                    .copy_from_slice(&self.input_pending[channel][..HUSH_FRAME_SIZE]);
                let pending_len = self.input_pending[channel].len();
                self.input_pending[channel].copy_within(HUSH_FRAME_SIZE.., 0);
                self.input_pending[channel].truncate(pending_len - HUSH_FRAME_SIZE);
                self.frame_output.fill(0.0);
                self.denoisers[channel]
                    .process_frame(&mut self.frame_output, &self.frame_input)
                    .map_err(|error| error.to_string())?;
                if self.frame_output.iter().any(|sample| !sample.is_finite()) {
                    return Err("Hush produced a non-finite frame".into());
                }
                self.at_host_rate.clear();
                self.output_resamplers[channel].process(&self.frame_output, &mut self.at_host_rate);
                self.delayed.clear();
                self.wet_delays[channel].process(&self.at_host_rate, &mut self.delayed);
                self.output_pending[channel].extend_from_slice(&self.delayed);
            }
        }
        let inference_ns = inference_start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        diagnostics.processed_blocks.fetch_add(1, Ordering::Relaxed);
        diagnostics
            .inference_ns
            .fetch_add(inference_ns, Ordering::Relaxed);
        let mut maximum = diagnostics.max_inference_ns.load(Ordering::Relaxed);
        while maximum < inference_ns {
            match diagnostics.max_inference_ns.compare_exchange_weak(
                maximum,
                inference_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => maximum = observed,
            }
        }

        let pushed = output.try_push_with(
            block.generation,
            block.sequence,
            block.frames,
            block.channels,
            block.channel_mask,
            |destination| {
                let samples = frames * self.channels;
                destination[..samples].fill(0.0);
                for frame in 0..frames {
                    for channel in 0..self.channels {
                        if channel_is_connected(block.channel_mask, channel) {
                            let offset = self.output_offsets[channel];
                            if offset < self.output_pending[channel].len() {
                                destination[frame * self.channels + channel] =
                                    self.output_pending[channel][offset];
                                self.output_offsets[channel] = offset + 1;
                                if self.output_offsets[channel]
                                    == self.output_pending[channel].len()
                                {
                                    self.output_pending[channel].clear();
                                    self.output_offsets[channel] = 0;
                                }
                            }
                        }
                    }
                }
            },
        );
        if !pushed {
            diagnostics.output_overruns.fetch_add(1, Ordering::Relaxed);
        } else {
            record_maximum(&diagnostics.max_output_queue_depth, output.depth());
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
    mask: Arc<AtomicU16>,
    attenuation: Arc<AtomicU32>,
    diagnostics: Arc<HushDiagnostics>,
) {
    while !stop.load(Ordering::Acquire) {
        let mut did_work = false;
        for _ in 0..HUSH_QUEUE_CAPACITY {
            let Some(()) = input.pop_with(|block| {
                did_work = true;
                if block.generation != generation.load(Ordering::Acquire) {
                    // A reset/disconnect invalidates queued input before it
                    // reaches the denoiser. This keeps obsolete speech from
                    // consuming worker time or creating a tail block that
                    // could otherwise compete with the new generation.
                    return;
                }
                // The runtime parameter update is deliberately applied here,
                // off the realtime thread, without rebuilding Tract state.
                let requested = f32::from_bits(attenuation.load(Ordering::Acquire));
                if requested.is_finite() && (requested - worker.attenuation_db).abs() > f32::EPSILON
                {
                    for denoiser in &mut worker.denoisers {
                        if denoiser.set_attenuation_limit_db(requested).is_err() {
                            diagnostics.worker_failed.store(true, Ordering::Release);
                            return;
                        }
                    }
                    worker.attenuation_db = requested;
                }
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker.process_block(block, &output, &generation, &mask, &diagnostics)
                }));
                if !matches!(result, Ok(Ok(()))) {
                    diagnostics.worker_failed.store(true, Ordering::Release);
                }
            }) else {
                break;
            };
        }
        if !did_work {
            let guard = match signal.mutex.lock() {
                Ok(guard) => guard,
                Err(_) => break,
            };
            if stop.load(Ordering::Acquire) {
                break;
            }
            let _ = signal
                .condition
                .wait_timeout(guard, Duration::from_millis(2));
        }
    }
}

struct WetDelay {
    samples: Vec<f32>,
    position: usize,
}

impl WetDelay {
    fn new(delay_samples: usize) -> Self {
        Self {
            samples: vec![0.0; delay_samples],
            position: 0,
        }
    }

    fn reset(&mut self) {
        self.samples.fill(0.0);
        self.position = 0;
    }

    fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        output.clear();
        if self.samples.is_empty() {
            output.extend_from_slice(input);
            return;
        }
        if output.capacity() < input.len() {
            output.reserve(input.len() - output.capacity());
        }
        for &sample in input {
            let delayed = self.samples[self.position];
            self.samples[self.position] = sample;
            self.position = (self.position + 1) % self.samples.len();
            output.push(delayed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_is_bounded_and_fifo() {
        let queue = BlockQueue::new(2, 4);
        assert!(queue.try_push_with(1, 0, 1, 1, 1, |samples| samples[0] = 0.25));
        assert!(queue.try_push_with(1, 1, 1, 1, 1, |samples| samples[0] = 0.5));
        assert!(!queue.try_push_with(1, 2, 1, 1, 1, |_| {}));
        let first = queue.pop_with(|block| (block.sequence, block.samples[0]));
        let second = queue.pop_with(|block| (block.sequence, block.samples[0]));
        assert_eq!(first, Some((0, 0.25)));
        assert_eq!(second, Some((1, 0.5)));
        assert!(queue.pop_with(|_| ()).is_none());
    }

    #[test]
    fn future_output_is_not_consumed_by_an_earlier_sequence() {
        let queue = BlockQueue::new(4, 1);
        assert!(queue.try_push_with(7, 2, 1, 1, 1, |samples| { samples[0] = 0.75 }));
        assert_eq!(
            queue.peek_with(|block| (block.generation, block.sequence)),
            Some((7, 2))
        );
        assert_eq!(queue.pop_with(|block| block.samples[0]), Some(0.75));
    }

    #[test]
    fn wet_delay_starts_with_exact_silence() {
        let mut delay = WetDelay::new(3);
        let mut output = Vec::new();
        delay.process(&[0.5, 0.25, -0.5], &mut output);
        assert_eq!(output, vec![0.0, 0.0, 0.0]);
        delay.process(&[1.0], &mut output);
        assert_eq!(output, vec![0.5]);
    }
}
