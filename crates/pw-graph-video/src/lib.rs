//! Dedicated Linux video subsystem.
//!
//! This crate is intentionally separate from `pw-graph-effects`: audio DSP is
//! sample-based and runs inside the PipeWire realtime callback, while video
//! processing is frame-based, allocation-heavy, and must never run on the
//! realtime thread. The pipeline is always:
//!
//! ```text
//! PipeWire callback -> bounded queue -> worker thread -> latest frame slot
//! ```
//!
//! When overloaded the queue drops the oldest frame instead of growing
//! latency. Nothing here blocks, allocates unboundedly, or touches the UI,
//! filesystem, or network from a callback path.

pub mod diagnostics;
pub mod filters;
pub mod format;
pub mod frame;
pub mod preview;
pub mod processor;
pub mod queue;
pub mod recorder;

pub use diagnostics::{VideoCounterSnapshot, VideoDiagnostics};
pub use format::{
    VideoError, VideoPixelFormat, VideoSpec, MAX_FRAME_BYTES, MAX_QUEUE_DEPTH, MAX_VIDEO_FPS,
    MAX_VIDEO_HEIGHT, MAX_VIDEO_WIDTH,
};
pub use frame::{EncodedVideoFrame, VideoCodec, VideoFrame};
pub use processor::{ProcessorState, VideoProcessor, VideoWorker, WorkerConfig};
pub use queue::{LatestFrame, VideoQueue};
