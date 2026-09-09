//! Bounded, non-realtime Hush processing.
//!
//! The PipeWire and Windows callbacks only enqueue sanitized blocks and read
//! already-produced blocks from this module.  DeepFilterNet/Tract, model
//! state, resampling, and frame assembly all live on the worker thread.

use nnnoiseless::{HushDenoiser, HushModel, Resampler, HUSH_FRAME_SIZE, HUSH_SAMPLE_RATE};
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The queue is deliberately small and fixed.  A late worker loses a block
/// and the processor uses its aligned dry path; latency cannot grow without
/// bound.
pub(crate) const HUSH_QUEUE_CAPACITY: usize = 128;
/// Fixed scheduling allowance, including model-frame assembly and sinc lookahead.
/// Signal alignment adds the synthesis delay separately (see hush_noise).
pub(crate) const HUSH_SCHEDULING_MS: u32 = 40;
/// Maximum amount of input that is useful to process after the realtime
/// producer has advanced. Beyond this point the wet result cannot meet the
/// fixed playout timeline and the worker must resynchronize.
pub(crate) const HUSH_MAX_BACKLOG_MS: u32 = 80;
/// Hysteresis for clearing the overload/recovery state. This is deliberately
/// smaller than the overload threshold so a worker does not oscillate between
/// running and recovering around one boundary.
pub(crate) const HUSH_RECOVERY_LEAD_MS: u32 = 40;

#[repr(u32)]
#[derive(Clone, Copy)]
enum HushWorkerState {
    Starting = 0,
    Running = 1,
    ResyncRequested = 2,
    Warming = 3,
    Failed = 4,
}

impl HushWorkerState {
    fn as_u32(self) -> u32 {
        self as u32
    }
}

#[inline]
fn channel_is_connected(mask: u16, channel: usize) -> bool {
    channel < u16::BITS as usize && mask & (1u16 << channel) != 0
}

#[derive(Default)]
pub struct HushDiagnostics {
    pub sample_rate: AtomicU32,
    pub channels: AtomicU16,
    pub host_quantum: AtomicU32,
    pub(crate) attenuation_bits: Arc<AtomicU32>,
    pub(crate) bypass: AtomicBool,
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
    pub max_input_queue_depth: AtomicU64,
    pub max_output_queue_depth: AtomicU64,
    pub max_worker_backlog_frames: AtomicU64,
    pub generation_resets: AtomicU64,
    pub processed_blocks: AtomicU64,
    pub inference_ns: AtomicU64,
    pub max_inference_ns: AtomicU64,
    pub worker_ready: AtomicBool,
    pub worker_failed: AtomicBool,
    pub worker_overloaded: AtomicBool,
    pub worker_state: AtomicU32,
    pub worker_active: AtomicBool,
    pub latest_input_frame: AtomicU64,
    pub worker_input_frame: AtomicU64,
    pub latest_wet_frame: AtomicU64,
    pub playout_frame: AtomicU64,
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

    /// Control/UI thread only: locks the worker's bounded timing/error side
    /// channels and allocates display text. Never call from an audio callback.
    pub fn status_text(&self) -> String {
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let processed = load(&self.processed_blocks);
        let pushed = load(&self.input_blocks_pushed);
        let state = if self.worker_failed.load(Ordering::Acquire) {
            "failed"
        } else if self.worker_overloaded.load(Ordering::Acquire) {
            "recovering"
        } else if self.worker_ready.load(Ordering::Acquire) {
            "ready"
        } else {
            "starting"
        };
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
        let p95 = times.get(times.len() * 95 / 100).copied().unwrap_or(0) as f64 / 1e6;
        let p99 = times.get(times.len() * 99 / 100).copied().unwrap_or(0) as f64 / 1e6;
        let total_frames = load(&self.wet_frames_output) + load(&self.dry_frames_output);
        let wet_ratio = if total_frames == 0 {
            0.0
        } else {
            load(&self.wet_frames_output) as f64 * 100.0 / total_frames as f64
        };
        let backlog_frames = self
            .latest_input_frame
            .load(Ordering::Relaxed)
            .saturating_sub(self.worker_input_frame.load(Ordering::Relaxed));
        let max_backlog_ms = load(&self.max_worker_backlog_frames) as f64 * 1000.0
            / self.sample_rate.load(Ordering::Relaxed).max(1) as f64;
        format!("Hush: {state} {error} | {} Hz / {} ch / quantum {} | latency: 50 ms | wet: {} blocks / {} frames ({wet_ratio:.1}%) dry: {} blocks / {} frames [startup:{} bypass:{} host-off:{} underrun:{} resync:{} failure:{}] | underruns: {} | input/output overruns: {}/{} | input blocks pushed/rejected: {pushed}/{} ({} frames) | wet frames dropped: {} | resync: {}/{} | stale input/wet: {}/{} | queue peaks: {}/{} | backlog: {:.1}/{max_backlog_ms:.1} ms | processed: {processed} Hush frames: {} resets: {} | worker avg/p95/p99/max: {:.2}/{p95:.2}/{p99:.2}/{:.2} ms",
            self.sample_rate.load(Ordering::Relaxed), self.channels.load(Ordering::Relaxed), self.host_quantum.load(Ordering::Relaxed),
            load(&self.wet_blocks_output), load(&self.wet_frames_output), load(&self.dry_fallback_blocks), load(&self.dry_frames_output), load(&self.dry_startup_blocks), load(&self.dry_bypass_blocks), load(&self.dry_host_disabled_blocks), load(&self.dry_underrun_blocks), load(&self.dry_resync_blocks), load(&self.dry_worker_failure_blocks), load(&self.underruns), load(&self.input_overruns), load(&self.output_overruns), load(&self.input_blocks_rejected), load(&self.input_frames_rejected), load(&self.wet_frames_dropped), load(&self.resync_requests), load(&self.resync_completed), load(&self.stale_input_blocks_dropped), load(&self.stale_output_blocks_dropped), load(&self.max_input_queue_depth), load(&self.max_output_queue_depth), backlog_frames as f64 * 1000.0 / self.sample_rate.load(Ordering::Relaxed).max(1) as f64, load(&self.hush_frames_processed), load(&self.generation_resets), load(&self.inference_ns) as f64 / processed.max(1) as f64 / 1e6, load(&self.max_inference_ns) as f64 / 1e6)
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
    let end = start + frames as u64;
    let mut copied = 0;
    for _ in 0..HUSH_QUEUE_CAPACITY {
        let Some((epoch, pipeline_epoch, first, last)) = queue.peek_with(|b| {
            (
                b.generation,
                b.wet_epoch,
                b.start_frame,
                b.start_frame + b.frames as u64,
            )
        }) else {
            break;
        };
        if epoch != generation || pipeline_epoch != wet_epoch || last <= start {
            if let Some(diagnostics) = diagnostics {
                diagnostics
                    .stale_output_blocks_dropped
                    .fetch_add(1, Ordering::Relaxed);
                diagnostics
                    .wet_frames_dropped
                    .fetch_add(last.saturating_sub(first), Ordering::Relaxed);
            }
            queue.pop_with(|_| ());
            continue;
        }
        if first >= end {
            break;
        }
        queue.peek_with(|b| {
            if b.channels != channels {
                return;
            }
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
        // Tract contains Rc/OpState and is !Send. Construct on its owning
        // thread, but synchronously acknowledge initialization to prepare().
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let max_samples = max_frames as usize * channels as usize;
        let input = Arc::new(BlockQueue::new(HUSH_QUEUE_CAPACITY, max_samples));
        let output = Arc::new(BlockQueue::new(HUSH_QUEUE_CAPACITY, max_samples));
        let stop = Arc::new(AtomicBool::new(false));
        let signal = Arc::new(WorkerSignal::default());
        let generation = Arc::new(AtomicU64::new(initial_generation));
        let wet_epoch = Arc::new(AtomicU64::new(0));
        let resync_requested = Arc::new(AtomicBool::new(false));
        let accept_input = Arc::new(AtomicBool::new(true));
        let attenuation_bits = Arc::new(AtomicU32::new(attenuation_db.to_bits()));
        let diagnostics = Arc::new(HushDiagnostics {
            attenuation_bits: attenuation_bits.clone(),
            ..HushDiagnostics::default()
        });
        diagnostics
            .sample_rate
            .store(sample_rate, Ordering::Relaxed);
        diagnostics.channels.store(channels, Ordering::Relaxed);
        let latest_frame = Arc::new(AtomicU64::new(0));
        let worker_latest = latest_frame.clone();

        let worker_input = input.clone();
        let worker_output = output.clone();
        let worker_stop = stop.clone();
        let worker_signal = signal.clone();
        let worker_generation = generation.clone();
        let worker_wet_epoch = wet_epoch.clone();
        let worker_resync_requested = resync_requested.clone();
        let worker_accept_input = accept_input.clone();
        let worker_attenuation = attenuation_bits.clone();
        let worker_diagnostics = diagnostics.clone();
        let worker = thread::Builder::new()
            .name("qpwgraph-hush".into())
            .spawn(move || {
                let initialized = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    HushWorker::new(model, sample_rate, channels, max_frames, attenuation_db)
                }))
                .map_err(|payload| {
                    format!("Hush initialization panicked: {}", panic_reason(&*payload))
                })
                .and_then(|result| result);
                let mut worker = match initialized {
                    Ok(mut worker) => {
                        worker.generation = initial_generation;
                        let _ = ready_tx.send(Ok(()));
                        worker
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
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

        if let Err(error) = ready_rx
            .recv()
            .unwrap_or_else(|_| Err("Hush initialization channel closed".into()))
        {
            let _ = worker.join();
            return Err(error);
        }
        Ok(Self {
            input,
            output,
            stop,
            signal,
            generation,
            wet_epoch,
            resync_requested,
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
            .host_quantum
            .store(frames, Ordering::Relaxed);
        self.latest_frame
            .store(start_frame + frames as u64, Ordering::Release);
        self.diagnostics
            .latest_input_frame
            .store(start_frame + frames as u64, Ordering::Release);
        if self.diagnostics.worker_failed.load(Ordering::Acquire) {
            self.diagnostics
                .input_blocks_rejected
                .fetch_add(1, Ordering::Relaxed);
            self.diagnostics
                .input_frames_rejected
                .fetch_add(frames as u64, Ordering::Relaxed);
            return true;
        }
        if !self.accept_input.load(Ordering::Acquire) {
            self.diagnostics
                .input_blocks_rejected
                .fetch_add(1, Ordering::Relaxed);
            self.diagnostics
                .input_frames_rejected
                .fetch_add(frames as u64, Ordering::Relaxed);
            return false;
        }
        if channels == 0 || samples.len() != frames as usize * channels as usize {
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
                self.request_resync();
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
        !self.accept_input.load(Ordering::Acquire)
            || self.resync_requested.load(Ordering::Acquire)
            || self.diagnostics.worker_overloaded.load(Ordering::Acquire)
    }

    fn request_resync(&self) {
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
        while self.input.depth() != 0 || self.diagnostics.worker_active.load(Ordering::Acquire) {
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

    channels: usize,
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

        let converted_capacity = (max_frames as f64 * HUSH_SAMPLE_RATE as f64 / sample_rate as f64)
            .ceil() as usize
            + HUSH_FRAME_SIZE * 2;
        let output_capacity = (max_frames as f64 * sample_rate as f64 / HUSH_SAMPLE_RATE as f64)
            .ceil() as usize
            + HUSH_FRAME_SIZE * 2;
        for _ in 0..channels {
            denoisers.push(
                model
                    .denoiser_with_attenuation_db(attenuation_db.max(0.01))
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
        if frames > self.max_frames || block.channels as usize != self.channels {
            return Err("Hush worker received an invalid audio block".into());
        }
        self.input_position = block.start_frame + block.frames as u64;
        let inference_start = Instant::now();
        for channel in 0..self.channels {
            if !channel_is_connected(block.channel_mask, channel) {
                self.input_pending[channel].clear();
                self.output_pending[channel].clear();
                self.output_offsets[channel] = 0;
                self.input_resamplers[channel].reset();
                self.output_resamplers[channel].reset();

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
                #[cfg(test)]
                thread::sleep(Duration::from_millis(
                    diagnostics.frame_delay_ms.load(Ordering::Relaxed),
                ));
                self.denoisers[channel]
                    .process_frame(&mut self.frame_output, &self.frame_input)
                    .map_err(|error| error.to_string())?;
                diagnostics
                    .hush_frames_processed
                    .fetch_add(1, Ordering::Relaxed);
                if self.frame_output.iter().any(|sample| !sample.is_finite()) {
                    return Err("Hush produced a non-finite frame".into());
                }
                self.at_host_rate.clear();
                self.output_resamplers[channel].process(&self.frame_output, &mut self.at_host_rate);
                if self.at_host_rate.iter().any(|sample| !sample.is_finite()) {
                    return Err("Hush output resampler produced non-finite audio".into());
                }
                self.output_pending[channel].extend_from_slice(&self.at_host_rate);
            }
        }
        let inference_ns = inference_start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        if let Ok(mut timings) = diagnostics.timings.lock() {
            if timings.len() == 4096 {
                timings.pop_front();
            }
            timings.push_back(inference_ns);
        }
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
                    .wet_frames_dropped
                    .fetch_add(count as u64, Ordering::Relaxed);
            } else {
                record_maximum(&diagnostics.max_output_queue_depth, output.depth());
                diagnostics
                    .latest_wet_frame
                    .store(self.output_position, Ordering::Release);
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
    accept_input: Arc<AtomicBool>,
    attenuation: Arc<AtomicU32>,
    diagnostics: Arc<HushDiagnostics>,
    latest_frame: Arc<AtomicU64>,
    max_backlog_frames: u64,
    recovery_lead_frames: u64,
) {
    let mut state = HushWorkerState::Starting;
    diagnostics
        .worker_state
        .store(state.as_u32(), Ordering::Release);
    while !stop.load(Ordering::Acquire) && !diagnostics.worker_failed.load(Ordering::Acquire) {
        if resync_requested.load(Ordering::Acquire) {
            diagnostics.worker_active.store(true, Ordering::Release);
            state = HushWorkerState::ResyncRequested;
            diagnostics
                .worker_state
                .store(state.as_u32(), Ordering::Release);
            accept_input.store(false, Ordering::Release);

            // The worker is the sole consumer and can safely drain both
            // queues. The realtime producer is gated by accept_input; a race
            // that publishes an old-epoch block is rejected below by epoch.
            while input
                .pop_with(|_| {
                    diagnostics
                        .stale_input_blocks_dropped
                        .fetch_add(1, Ordering::Relaxed);
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
                })
                .is_some()
            {}

            let current_generation = generation.load(Ordering::Acquire);
            let next_epoch = wet_epoch.fetch_add(1, Ordering::AcqRel) + 1;
            if let Err(error) =
                worker.reset(current_generation, worker.last_mask, &diagnostics, false)
            {
                *diagnostics.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
                diagnostics.worker_failed.store(true, Ordering::Release);
                diagnostics.worker_active.store(false, Ordering::Release);
                diagnostics
                    .worker_state
                    .store(HushWorkerState::Failed.as_u32(), Ordering::Release);
                break;
            }
            worker.wet_epoch = next_epoch;
            // Re-anchor progress at the live producer position. Input that
            // arrives after this point belongs to the fresh wet epoch; using
            // the old progress value here would immediately trip the backlog
            // guard again and create a resync loop.
            diagnostics
                .worker_input_frame
                .store(latest_frame.load(Ordering::Acquire), Ordering::Release);
            diagnostics.resync_completed.fetch_add(1, Ordering::Relaxed);
            diagnostics
                .worker_state
                .store(HushWorkerState::Warming.as_u32(), Ordering::Release);
            state = HushWorkerState::Warming;
            resync_requested.store(false, Ordering::Release);
            accept_input.store(true, Ordering::Release);
            diagnostics.worker_active.store(false, Ordering::Release);
            continue;
        }
        #[cfg(test)]
        if diagnostics.paused.load(Ordering::Acquire) {
            thread::sleep(Duration::from_micros(500));
            continue;
        }
        let latest = latest_frame.load(Ordering::Acquire);
        let progress = diagnostics.worker_input_frame.load(Ordering::Acquire);
        let backlog = latest.saturating_sub(progress);
        record_maximum(&diagnostics.max_worker_backlog_frames, backlog as usize);
        if backlog > max_backlog_frames {
            accept_input.store(false, Ordering::Release);
            if !resync_requested.swap(true, Ordering::AcqRel) {
                diagnostics.resync_requests.fetch_add(1, Ordering::Relaxed);
                diagnostics.worker_overloaded.store(true, Ordering::Release);
                diagnostics
                    .worker_state
                    .store(HushWorkerState::ResyncRequested.as_u32(), Ordering::Release);
            }
            continue;
        }
        let mut did_work = false;
        for _ in 0..HUSH_QUEUE_CAPACITY {
            let popped = input.pop_with(|block| {
                did_work = true;
                diagnostics.worker_active.store(true, Ordering::Release);
                let block_end = block.start_frame + block.frames as u64;
                diagnostics
                    .worker_input_frame
                    .store(block_end, Ordering::Release);
                let backlog = latest_frame
                    .load(Ordering::Acquire)
                    .saturating_sub(block_end);
                record_maximum(&diagnostics.max_worker_backlog_frames, backlog as usize);
                if diagnostics.worker_failed.load(Ordering::Acquire) {
                    return;
                }
                if backlog > max_backlog_frames {
                    accept_input.store(false, Ordering::Release);
                    if !resync_requested.swap(true, Ordering::AcqRel) {
                        diagnostics.resync_requests.fetch_add(1, Ordering::Relaxed);
                        diagnostics.worker_overloaded.store(true, Ordering::Release);
                        diagnostics
                            .worker_state
                            .store(HushWorkerState::ResyncRequested.as_u32(), Ordering::Release);
                    }
                    diagnostics
                        .stale_input_blocks_dropped
                        .fetch_add(1, Ordering::Relaxed);
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
                        diagnostics
                            .worker_state
                            .store(HushWorkerState::ResyncRequested.as_u32(), Ordering::Release);
                    }
                    diagnostics
                        .stale_input_blocks_dropped
                        .fetch_add(1, Ordering::Relaxed);
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
                // The runtime parameter update is deliberately applied here,
                // off the realtime thread, without rebuilding Tract state.
                let requested = f32::from_bits(attenuation.load(Ordering::Acquire));
                if requested.is_finite() && (requested - worker.attenuation_db).abs() > f32::EPSILON
                {
                    for denoiser in &mut worker.denoisers {
                        if let Err(error) = denoiser.set_attenuation_limit_db(requested.max(0.01)) {
                            *diagnostics.error.lock().unwrap() = Some(error.to_string());
                            diagnostics.worker_failed.store(true, Ordering::Release);
                            return;
                        }
                    }
                    worker.attenuation_db = requested;
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
                    *diagnostics.error.lock().unwrap() = Some(error);
                    diagnostics.worker_failed.store(true, Ordering::Release);
                    diagnostics
                        .worker_state
                        .store(HushWorkerState::Failed.as_u32(), Ordering::Release);
                } else if matches!(state, HushWorkerState::Starting) {
                    diagnostics
                        .worker_state
                        .store(HushWorkerState::Running.as_u32(), Ordering::Release);
                    state = HushWorkerState::Running;
                } else if matches!(state, HushWorkerState::Warming)
                    && diagnostics
                        .latest_wet_frame
                        .load(Ordering::Acquire)
                        .saturating_sub(diagnostics.playout_frame.load(Ordering::Acquire))
                        >= recovery_lead_frames
                {
                    diagnostics
                        .worker_overloaded
                        .store(false, Ordering::Release);
                    diagnostics
                        .worker_state
                        .store(HushWorkerState::Running.as_u32(), Ordering::Release);
                    state = HushWorkerState::Running;
                }
            });
            diagnostics.worker_active.store(false, Ordering::Release);
            let Some(()) = popped else {
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
                .wait_timeout(guard, Duration::from_micros(500));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
