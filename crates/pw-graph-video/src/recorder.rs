//! Local video recording abstraction.
//!
//! Full encoded recording (container + codec) is out of scope for this phase;
//! this module defines the stable handoff (`VideoFrame` -> recorder) so an
//! encoder can be attached later without changing capture, processing, or the
//! graph. Nothing here runs on the realtime thread: implementations must
//! ingest from a worker or a bounded queue.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::format::{VideoError, VideoSpec};
use crate::frame::VideoFrame;

/// Recording lifecycle state.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoRecorderState {
    #[default]
    Idle,
    Recording,
    Finalizing,
    Error,
}

/// Status snapshot for the UI.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct VideoRecorderStatus {
    pub state: VideoRecorderState,
    pub frames_accepted: u64,
    pub frames_dropped: u64,
    pub current_path: Option<PathBuf>,
    pub error: Option<String>,
}

/// Sink for decoded frames. A future encoded recorder implements this trait
/// behind a worker thread.
pub trait VideoRecorder: Send {
    /// Negotiate or renegotiate the incoming spec. Called before the first
    /// frame and on every spec change.
    fn prepare(&mut self, spec: &VideoSpec) -> Result<(), VideoError>;

    /// Accept one frame. Must not block indefinitely; on overload it should
    /// drop and count rather than stall the pipeline.
    fn record(&mut self, frame: &VideoFrame) -> Result<(), VideoError>;

    /// Finish the current output and report where it landed, if anywhere.
    fn finish(&mut self) -> Result<Option<PathBuf>, VideoError>;

    fn status(&self) -> VideoRecorderStatus;
}

/// Null recorder used for API validation and tests. Accepts and counts
/// frames without writing anything.
#[derive(Debug, Default)]
pub struct NullVideoRecorder {
    spec: Option<VideoSpec>,
    accepted: u64,
    dropped: u64,
    state: VideoRecorderState,
}

impl NullVideoRecorder {
    pub fn new() -> Self {
        Self::default()
    }
}

impl VideoRecorder for NullVideoRecorder {
    fn prepare(&mut self, spec: &VideoSpec) -> Result<(), VideoError> {
        spec.validate()?;
        self.spec = Some(*spec);
        self.state = VideoRecorderState::Recording;
        Ok(())
    }

    fn record(&mut self, frame: &VideoFrame) -> Result<(), VideoError> {
        let Some(spec) = self.spec else {
            self.dropped += 1;
            return Err(VideoError::NotPrepared);
        };
        if frame.spec() != spec {
            // Renegotiation is the caller's cue to re-prepare.
            self.dropped += 1;
            return Err(VideoError::SpecMismatch {
                expected: spec.to_string(),
                actual: frame.spec().to_string(),
            });
        }
        self.accepted += 1;
        Ok(())
    }

    fn finish(&mut self) -> Result<Option<PathBuf>, VideoError> {
        self.state = VideoRecorderState::Idle;
        Ok(None)
    }

    fn status(&self) -> VideoRecorderStatus {
        VideoRecorderStatus {
            state: self.state,
            frames_accepted: self.accepted,
            frames_dropped: self.dropped,
            current_path: None,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::VideoPixelFormat;

    #[test]
    fn null_recorder_counts_and_detects_renegotiation() {
        let mut recorder = NullVideoRecorder::new();
        let spec = VideoSpec::new(64, 64, 30, 1, VideoPixelFormat::Rgbx).unwrap();
        assert!(recorder
            .record(&VideoFrame::allocate(spec).unwrap())
            .is_err());
        recorder.prepare(&spec).unwrap();
        recorder
            .record(&VideoFrame::allocate(spec).unwrap())
            .unwrap();
        let other = VideoSpec::new(128, 128, 30, 1, VideoPixelFormat::Rgbx).unwrap();
        assert!(recorder
            .record(&VideoFrame::allocate(other).unwrap())
            .is_err());
        let status = recorder.status();
        assert_eq!(status.frames_accepted, 1);
        assert_eq!(status.frames_dropped, 2);
    }
}
