//! Video filter bridges: graph node + worker + streams as one unit.
//!
//! One bridge is one PipeWire-visible processing node:
//!
//! ```text
//! upstream video ---> [capture stream] ---> queue ---> worker ---> output slot
//!                                                                  |      |
//!                                                               preview  [output stream] ---> downstream
//! ```
//!
//! The bridge owns no PipeWire objects itself; the driver creates and
//! destroys streams under the thread-loop lock and parks the handles here.
//! Worker lifecycle, renegotiation, failure bypass, and diagnostics are all
//! handled on the control thread or the worker thread — never in a realtime
//! callback.

use std::sync::{Arc, Mutex};

use pw_graph_core::{NodeId, PortId};
use pw_graph_video::diagnostics::VideoCounterSnapshot;
use pw_graph_video::filters::FilterParams;
use pw_graph_video::format::{VideoError, VideoSpec};
use pw_graph_video::preview::VideoPreview;
use pw_graph_video::{VideoDiagnostics, VideoProcessor, VideoQueue, VideoWorker, WorkerConfig};

use super::capture::VideoCaptureHandle;
use super::output::VideoOutputHandle;
use crate::video::{VideoFilterInstance, VideoNodeInfo, VideoNodeState};

/// Control-plane state for one video filter node.
pub struct VideoBridge {
    pub instance: VideoFilterInstance,
    pub filter_id: String,
    pub params: FilterParams,
    pub queue: Arc<VideoQueue>,
    pub diagnostics: VideoDiagnostics,
    pub preview: VideoPreview,
    worker: Mutex<Option<VideoWorker>>,
    output_spec: Mutex<Option<VideoSpec>>,
    state: Mutex<VideoNodeState>,
    last_error: Mutex<Option<String>>,
    /// Live PipeWire handles, present once the driver attaches streams.
    /// Created and destroyed under the thread-loop lock.
    pub capture: Option<VideoCaptureHandle>,
    pub output: Option<VideoOutputHandle>,
}

impl VideoBridge {
    pub fn new(instance: VideoFilterInstance, filter_id: String, params: FilterParams) -> Self {
        Self {
            instance,
            filter_id,
            params,
            queue: VideoQueue::new(crate::pipewire::DEFAULT_VIDEO_QUEUE_DEPTH),
            diagnostics: VideoDiagnostics::new(),
            preview: VideoPreview::new(),
            worker: Mutex::new(None),
            output_spec: Mutex::new(None),
            state: Mutex::new(VideoNodeState::Idle),
            last_error: Mutex::new(None),
            capture: None,
            output: None,
        }
    }

    pub fn node_id(&self) -> NodeId {
        self.instance.node_id
    }

    pub fn input_port(&self) -> PortId {
        self.instance.input_port
    }

    pub fn output_port(&self) -> PortId {
        self.instance.output_port
    }

    /// Start the worker thread for `processor`. Idempotent while running.
    /// The wrapper feeds the preview slot from every processed frame.
    pub fn ensure_worker(&self, processor: Box<dyn VideoProcessor>) {
        let mut worker = self.worker.lock().expect("video bridge mutex poisoned");
        if worker.is_none() {
            let diagnostics = self.diagnostics.clone();
            let queue = self.queue.clone();
            let name = format!("qpwgraph-video-{}", self.instance.instance_id);
            *worker = Some(VideoWorker::spawn(
                name,
                queue,
                ProcessorWithPreview::wrap(processor, self.preview.clone()),
                diagnostics,
                WorkerConfig::default(),
            ));
            *self.state.lock().expect("video bridge mutex poisoned") = VideoNodeState::Live;
        }
    }

    /// Stop the worker thread. The graph node stays; streams keep their last
    /// state so a restart resumes cleanly.
    pub fn stop_worker(&self) {
        let mut worker = self.worker.lock().expect("video bridge mutex poisoned");
        if worker.take().is_some() {
            *self.state.lock().expect("video bridge mutex poisoned") = VideoNodeState::Idle;
        }
    }

    pub fn worker_running(&self) -> bool {
        self.worker.lock().map(|w| w.is_some()).unwrap_or(false)
    }

    /// Note a negotiated input spec (from the capture stream). Mismatches
    /// against the filter's output spec mark the node for stream rebuild.
    pub fn note_input_spec(&self, spec: &VideoSpec) {
        self.diagnostics.note_spec(spec);
        if self.state() == VideoNodeState::Idle {
            *self.state.lock().expect("video bridge mutex poisoned") = VideoNodeState::Live;
        }
    }

    pub fn note_output_spec(&self, spec: &VideoSpec) {
        *self
            .output_spec
            .lock()
            .expect("video bridge mutex poisoned") = Some(*spec);
    }

    pub fn output_spec(&self) -> Option<VideoSpec> {
        self.output_spec.lock().ok().and_then(|s| *s)
    }

    pub fn mark_bypassed(&self, reason: &str) {
        *self.state.lock().expect("video bridge mutex poisoned") = VideoNodeState::Bypassed;
        let mut last_error = self.last_error.lock().expect("video bridge mutex poisoned");
        if last_error.is_none() {
            *last_error = Some(reason.to_owned());
        }
        self.diagnostics.note_error_if_empty(reason);
    }

    pub fn mark_failed(&self, reason: &str) {
        *self.state.lock().expect("video bridge mutex poisoned") = VideoNodeState::Failed;
        *self.last_error.lock().expect("video bridge mutex poisoned") = Some(reason.to_owned());
        self.diagnostics.note_error(reason);
        self.preview.mark_error(reason);
    }

    pub fn clear_failure(&self) {
        let mut state = self.state.lock().expect("video bridge mutex poisoned");
        if matches!(*state, VideoNodeState::Failed | VideoNodeState::Bypassed) {
            *state = VideoNodeState::Live;
        }
        *self.last_error.lock().expect("video bridge mutex poisoned") = None;
        self.diagnostics.clear_error();
    }

    /// The input link was removed: no source, no failure. The worker stays
    /// parked so a re-link resumes without a thread restart.
    pub fn note_input_detached(&self) {
        *self.state.lock().expect("video bridge mutex poisoned") = VideoNodeState::Idle;
        *self.last_error.lock().expect("video bridge mutex poisoned") = None;
        *self
            .output_spec
            .lock()
            .expect("video bridge mutex poisoned") = None;
        self.queue.clear();
        self.diagnostics.clear_error();
        self.preview.restart();
    }

    pub fn state(&self) -> VideoNodeState {
        self.state
            .lock()
            .map(|s| *s)
            .unwrap_or(VideoNodeState::Idle)
    }

    /// Reconcile liveness from the attached streams: a disconnected capture
    /// or output stream surfaces as failed, never as silent still video.
    pub fn reconcile_streams(&self) {
        let capture_down = self.capture.as_ref().is_some_and(|capture| {
            !capture
                .shared
                .connected
                .load(std::sync::atomic::Ordering::Relaxed)
        });
        let output_down = self.output.as_ref().is_some_and(|output| {
            !output
                .shared
                .connected
                .load(std::sync::atomic::Ordering::Relaxed)
        });
        let output_mismatch = self.output.as_ref().is_some_and(|output| {
            output
                .shared
                .format_mismatch
                .load(std::sync::atomic::Ordering::Relaxed)
        });
        if output_mismatch {
            self.mark_bypassed("output format mismatch; stream rebuild required");
        } else if capture_down || output_down {
            // Only downgrade a live node; an idle node with detached streams
            // is the normal pre-attachment state.
            if self.state() == VideoNodeState::Live {
                self.mark_bypassed("video stream stalled");
            }
        } else if self.state() == VideoNodeState::Bypassed {
            // Streams recovered (or were never the problem): a bypassed
            // node with healthy streams returns to live. Processor-level
            // failures surface through diagnostics, not this state, so
            // clearing here cannot mask a failed filter.
            self.clear_failure();
        }
    }

    pub fn counters(&self) -> VideoCounterSnapshot {
        self.diagnostics.snapshot_with_queue(&self.queue)
    }

    pub fn info(&self) -> VideoNodeInfo {
        VideoNodeInfo {
            node_id: self.instance.node_id,
            instance_id: Some(self.instance.instance_id.clone()),
            spec: self.output_spec().or_else(|| {
                self.capture
                    .as_ref()
                    .and_then(|capture| capture.shared.spec.lock().ok().and_then(|s| *s))
            }),
            state: self.state(),
            counters: Some(self.counters()),
            preview_state: self.preview.state(),
        }
    }
}

impl Drop for VideoBridge {
    fn drop(&mut self) {
        // Dropping the worker joins its thread; streams are destroyed by
        // their own handle drops under the caller's loop lock discipline.
        let _ = self.worker.lock().map(|mut worker| worker.take());
    }
}

/// Wraps a filter processor so every output frame also feeds the preview
/// slot. Preview publication never fails processing: a contended slot drops
/// the preview copy and keeps the pipeline frame.
struct ProcessorWithPreview {
    inner: Box<dyn VideoProcessor>,
    preview: VideoPreview,
    prepared: Option<VideoSpec>,
}

impl ProcessorWithPreview {
    fn wrap(inner: Box<dyn VideoProcessor>, preview: VideoPreview) -> Box<dyn VideoProcessor> {
        Box::new(Self {
            inner,
            preview,
            prepared: None,
        })
    }
}

impl VideoProcessor for ProcessorWithPreview {
    fn name(&self) -> &'static str {
        // Delegate name is dynamic; report the wrapper. The inner name is
        // visible through bridge metadata instead.
        "video-filter"
    }

    fn prepare(&mut self, spec: &VideoSpec) -> Result<(), VideoError> {
        self.inner.prepare(spec)?;
        self.prepared = Some(*spec);
        Ok(())
    }

    fn process(
        &mut self,
        input: &pw_graph_video::VideoFrame,
        output: &mut pw_graph_video::VideoFrame,
    ) -> Result<(), VideoError> {
        self.inner.process(input, output)?;
        self.preview.publish(output);
        Ok(())
    }

    fn output_spec(&self, input: &VideoSpec) -> Result<VideoSpec, VideoError> {
        self.inner.output_spec(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pw_graph_core::{NodeId, PortId};
    use pw_graph_video::filters::{FilterParams, Passthrough};

    fn test_bridge() -> VideoBridge {
        VideoBridge::new(
            VideoFilterInstance {
                instance_id: "test".into(),
                node_id: NodeId(1),
                input_port: PortId(2),
                output_port: PortId(3),
                filter_id: "passthrough".into(),
            },
            "passthrough".into(),
            FilterParams::default(),
        )
    }

    #[test]
    fn bridge_lifecycle_and_failure_states() {
        let bridge = test_bridge();
        assert_eq!(bridge.state(), VideoNodeState::Idle);
        bridge.ensure_worker(Box::new(Passthrough));
        assert!(bridge.worker_running());
        assert_eq!(bridge.state(), VideoNodeState::Live);
        bridge.mark_bypassed("test stall");
        assert_eq!(bridge.state(), VideoNodeState::Bypassed);
        bridge.clear_failure();
        assert_eq!(bridge.state(), VideoNodeState::Live);
        bridge.mark_failed("boom");
        assert_eq!(bridge.state(), VideoNodeState::Failed);
        assert!(matches!(
            bridge.preview.state(),
            pw_graph_video::preview::PreviewState::Error(_)
        ));
        bridge.stop_worker();
        assert!(!bridge.worker_running());
    }

    #[test]
    fn bridge_info_reports_spec_and_counters() {
        let bridge = test_bridge();
        let spec = VideoSpec::new(640, 480, 30, 1, pw_graph_video::VideoPixelFormat::Rgbx).unwrap();
        bridge.note_output_spec(&spec);
        let info = bridge.info();
        assert_eq!(info.spec, Some(spec));
        assert!(info.counters.is_some());
    }
}
