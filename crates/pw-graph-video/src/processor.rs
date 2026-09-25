//! Frame processing API and worker runtime.
//!
//! [`VideoProcessor`] is the video counterpart of the audio effect API, kept
//! separate because frame processing has different realtime constraints:
//! workers run on their own threads, consume from a bounded queue, and
//! publish only the latest output. A failing processor puts its pipeline into
//! a bypassed/failed state; it never panics the application.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::diagnostics::VideoDiagnostics;
use crate::format::{VideoError, VideoSpec};
use crate::frame::VideoFrame;
use crate::queue::{LatestFrame, VideoQueue};

/// Frame processor. Implementations must be deterministic for a given
/// input and must not block, allocate unboundedly, or touch UI, filesystem,
/// or network.
pub trait VideoProcessor: Send {
    /// Human-readable filter name for diagnostics and the UI.
    fn name(&self) -> &'static str;

    /// Prepare for `spec`. Called on the worker thread before the first
    /// frame and again on every renegotiation.
    fn prepare(&mut self, spec: &VideoSpec) -> Result<(), VideoError>;

    /// Transform `input` into `output`. Both frames share the prepared spec
    /// unless the filter documents an output-spec change (crop/scale publish
    /// their own output spec; see [`ProcessorOutput`]).
    fn process(&mut self, input: &VideoFrame, output: &mut VideoFrame) -> Result<(), VideoError>;

    /// Output spec for an input spec. Filters that preserve geometry return
    /// the input unchanged; crop/scale override this.
    fn output_spec(&self, input: &VideoSpec) -> Result<VideoSpec, VideoError> {
        input.validate()?;
        Ok(*input)
    }
}

/// Output of one worker pump step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessorState {
    /// Actively processing frames.
    Running,
    /// Processor failed; input is forwarded unchanged and the error is
    /// reported through diagnostics. Recovers on renegotiation or reset.
    Bypassed,
    /// No frames yet or the stream was reset.
    Idle,
}

/// Worker configuration.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// How long the worker parks when the queue is empty.
    pub idle_timeout: Duration,
    /// When true the worker processes every queued frame; when false (the
    /// default) it skips to the newest frame to minimize latency.
    pub process_every_frame: bool,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            idle_timeout: Duration::from_millis(50),
            process_every_frame: false,
        }
    }
}

/// Owns the queue consumer side, a processor, and the output slot.
///
/// `pump_once` is the unit-testable single step; [`VideoWorker::spawn`]
/// runs it on a dedicated thread.
pub struct VideoPipeline {
    queue: Arc<VideoQueue>,
    processor: Box<dyn VideoProcessor>,
    output: LatestFrame,
    diagnostics: VideoDiagnostics,
    prepared: Option<VideoSpec>,
    output_buffer: Option<VideoFrame>,
    state: ProcessorState,
    process_every_frame: bool,
}

impl VideoPipeline {
    pub fn new(
        queue: Arc<VideoQueue>,
        processor: Box<dyn VideoProcessor>,
        diagnostics: VideoDiagnostics,
    ) -> Self {
        Self {
            queue,
            processor,
            output: LatestFrame::new(),
            diagnostics,
            prepared: None,
            output_buffer: None,
            state: ProcessorState::Idle,
            process_every_frame: false,
        }
    }

    pub fn with_config(mut self, config: &WorkerConfig) -> Self {
        self.process_every_frame = config.process_every_frame;
        self
    }

    pub fn output_slot(&self) -> LatestFrame {
        self.output.clone()
    }

    pub fn state(&self) -> ProcessorState {
        self.state
    }

    pub fn reset(&mut self) {
        self.prepared = None;
        self.output_buffer = None;
        self.state = ProcessorState::Idle;
        self.diagnostics.clear_error();
    }

    /// Process at most one frame. Returns true when a frame was consumed.
    pub fn pump_once(&mut self) -> bool {
        let frame = if self.process_every_frame {
            self.queue.pop()
        } else {
            self.queue.pop_latest()
        };
        let Some(frame) = frame else {
            return false;
        };
        let started = Instant::now();
        let input_spec = frame.spec();

        // Renegotiation: a new spec re-prepares the processor before use.
        if self.prepared != Some(input_spec) {
            match self.processor.output_spec(&input_spec).and_then(|out| {
                self.processor.prepare(&input_spec)?;
                Ok(out)
            }) {
                Ok(output_spec) => match VideoFrame::allocate(output_spec) {
                    Ok(buffer) => {
                        self.output_buffer = Some(buffer);
                        self.prepared = Some(input_spec);
                        self.state = ProcessorState::Running;
                        self.diagnostics.clear_error();
                        self.diagnostics.note_spec(&output_spec);
                    }
                    Err(error) => {
                        self.fail(&error.to_string());
                        self.forward_bypass(&frame, started);
                        return true;
                    }
                },
                Err(error) => {
                    self.fail(&error.to_string());
                    self.forward_bypass(&frame, started);
                    return true;
                }
            }
        }

        let result = match self.output_buffer.as_mut() {
            Some(buffer) => self.processor.process(&frame, buffer).map(|()| {
                buffer.set_sequence(frame.sequence());
                buffer.set_captured_at(frame.captured_at().unwrap_or_else(Instant::now));
            }),
            None => Err(VideoError::NotPrepared),
        };
        match result {
            Ok(()) => {
                let output = self
                    .output_buffer
                    .as_ref()
                    .expect("buffer exists after successful prepare")
                    .clone();
                self.output.publish(output);
                self.state = ProcessorState::Running;
                self.diagnostics
                    .note_processed(started.elapsed(), &self.queue);
            }
            Err(error) => {
                self.fail(&error.to_string());
                self.forward_bypass(&frame, started);
            }
        }
        true
    }

    fn forward_bypass(&mut self, frame: &VideoFrame, started: Instant) {
        // Bypass publishes the input unchanged so downstream keeps showing
        // live video while the processor is failed.
        self.output.publish(frame.clone());
        self.diagnostics
            .note_bypassed(started.elapsed(), &self.queue);
    }

    fn fail(&mut self, message: &str) {
        self.state = ProcessorState::Bypassed;
        self.diagnostics.note_error(message);
    }
}

/// Background thread driving a [`VideoPipeline`].
pub struct VideoWorker {
    thread: Option<std::thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    queue: Arc<VideoQueue>,
}

impl VideoWorker {
    /// Spawn a worker thread named `name`. The thread exits when
    /// [`VideoWorker::stop`] is called or the worker is dropped.
    pub fn spawn(
        name: String,
        queue: Arc<VideoQueue>,
        processor: Box<dyn VideoProcessor>,
        diagnostics: VideoDiagnostics,
        config: WorkerConfig,
    ) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = shutdown.clone();
        let worker_queue = queue.clone();
        let idle_timeout = config.idle_timeout;
        let thread = std::thread::Builder::new()
            .name(name)
            .spawn(move || {
                let mut pipeline = VideoPipeline::new(worker_queue.clone(), processor, diagnostics)
                    .with_config(&config);
                loop {
                    if worker_shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    if !pipeline.pump_once()
                        && !worker_queue.wait_for_frame(&worker_shutdown, idle_timeout)
                        && worker_shutdown.load(Ordering::Acquire)
                    {
                        break;
                    }
                }
            })
            .expect("video worker thread should spawn");
        Self {
            thread: Some(thread),
            shutdown,
            queue,
        }
    }

    pub fn queue(&self) -> &Arc<VideoQueue> {
        &self.queue
    }

    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.queue.wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for VideoWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.queue.wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::Passthrough;
    use crate::format::VideoPixelFormat;

    fn spec_64() -> VideoSpec {
        VideoSpec::new(64, 64, 30, 1, VideoPixelFormat::Rgbx).unwrap()
    }

    #[test]
    fn pipeline_processes_queued_frames() {
        let queue = VideoQueue::new(4);
        let diagnostics = VideoDiagnostics::new();
        let mut pipeline = VideoPipeline::new(queue.clone(), Box::new(Passthrough), diagnostics);
        assert!(!pipeline.pump_once());
        let mut frame = VideoFrame::allocate(spec_64()).unwrap();
        frame.set_sequence(7);
        queue.push_overwrite(frame);
        assert!(pipeline.pump_once());
        assert_eq!(pipeline.state(), ProcessorState::Running);
        assert_eq!(pipeline.output_slot().latest().unwrap().sequence(), 7);
    }

    struct FailingProcessor;

    impl VideoProcessor for FailingProcessor {
        fn name(&self) -> &'static str {
            "failing-test"
        }

        fn prepare(&mut self, _spec: &VideoSpec) -> Result<(), VideoError> {
            Ok(())
        }

        fn process(
            &mut self,
            _input: &VideoFrame,
            _output: &mut VideoFrame,
        ) -> Result<(), VideoError> {
            Err(VideoError::ProcessorFailed("boom".into()))
        }
    }

    #[test]
    fn processor_failure_bypasses_without_crashing() {
        let queue = VideoQueue::new(4);
        let diagnostics = VideoDiagnostics::new();
        let mut pipeline = VideoPipeline::new(
            queue.clone(),
            Box::new(FailingProcessor),
            diagnostics.clone(),
        );
        let mut frame = VideoFrame::allocate(spec_64()).unwrap();
        frame.bytes_mut().fill(0xAB);
        frame.set_sequence(3);
        queue.push_overwrite(frame);
        assert!(pipeline.pump_once());
        assert_eq!(pipeline.state(), ProcessorState::Bypassed);
        // Bypass still publishes live video downstream.
        let output = pipeline.output_slot().latest().unwrap();
        assert_eq!(output.sequence(), 3);
        assert_eq!(output.bytes()[0], 0xAB);
        assert!(diagnostics.snapshot().last_error.is_some());
    }

    #[test]
    fn renegotiation_reprepares_on_spec_change() {
        let queue = VideoQueue::new(4);
        let diagnostics = VideoDiagnostics::new();
        let mut pipeline = VideoPipeline::new(queue.clone(), Box::new(Passthrough), diagnostics);
        queue.push_overwrite(VideoFrame::allocate(spec_64()).unwrap());
        assert!(pipeline.pump_once());
        let spec2 = VideoSpec::new(128, 128, 60, 1, VideoPixelFormat::Bgrx).unwrap();
        queue.push_overwrite(VideoFrame::allocate(spec2).unwrap());
        assert!(pipeline.pump_once());
        assert_eq!(pipeline.state(), ProcessorState::Running);
        assert_eq!(pipeline.output_slot().latest().unwrap().spec(), spec2);
    }
}
