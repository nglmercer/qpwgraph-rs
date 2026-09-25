//! Preview slots: newest-frame display without blocking capture.
//!
//! Rendering stays isolated from capture and processing. The UI polls
//! [`VideoPreview`] on its own cadence, receives the newest frame (or the
//! current error state), and never touches the PipeWire thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::format::VideoSpec;
use crate::frame::VideoFrame;
use crate::queue::LatestFrame;

/// State shown by a preview surface.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PreviewState {
    /// No stream attached yet.
    #[default]
    Idle,
    /// Actively receiving frames.
    Live,
    /// Stream ended or was detached; the last good frame (if any) is kept.
    Ended,
    /// An error occurred; the message is displayable.
    Error(String),
}

/// Newest-frame preview fed by a capture or filter pipeline.
///
/// Cheap to clone; all clones observe the same stream. Survives stream
/// restart: a new spec replaces the old one and stale frames are discarded.
#[derive(Clone, Debug)]
pub struct VideoPreview {
    slot: LatestFrame,
    state: Arc<Mutex<PreviewState>>,
    frames_shown: Arc<AtomicU64>,
    spec: Arc<Mutex<Option<VideoSpec>>>,
}

impl Default for VideoPreview {
    fn default() -> Self {
        Self {
            slot: LatestFrame::new(),
            state: Arc::new(Mutex::new(PreviewState::Idle)),
            frames_shown: Arc::new(AtomicU64::new(0)),
            spec: Arc::new(Mutex::new(None)),
        }
    }
}

impl VideoPreview {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a frame from a worker thread. Never blocks the caller for
    /// longer than a short mutex acquisition; on contention the frame is
    /// dropped in favor of staying realtime-safe.
    pub fn publish(&self, frame: &VideoFrame) {
        if self.slot.try_publish(frame.clone()) {
            self.set_state(PreviewState::Live);
            if let Ok(mut spec) = self.spec.lock() {
                *spec = Some(frame.spec());
            }
        }
    }

    /// Poll the newest frame. Returns `None` when no frame is available yet.
    pub fn latest(&self) -> Option<Arc<VideoFrame>> {
        let frame = self.slot.latest()?;
        self.frames_shown.fetch_add(1, Ordering::Relaxed);
        Some(frame)
    }

    /// Raw newest-frame slot shared with output streams. A filter's output
    /// stream and its preview consume the same slot, so preview and sink
    /// never diverge.
    pub fn frame_slot(&self) -> LatestFrame {
        self.slot.clone()
    }

    pub fn state(&self) -> PreviewState {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn spec(&self) -> Option<VideoSpec> {
        self.spec.lock().ok().and_then(|s| *s)
    }

    pub fn frames_shown(&self) -> u64 {
        self.frames_shown.load(Ordering::Relaxed)
    }

    pub fn set_state(&self, state: PreviewState) {
        if let Ok(mut current) = self.state.lock() {
            *current = state;
        }
    }

    /// Mark the stream ended, keeping the last frame for display.
    pub fn mark_ended(&self) {
        self.set_state(PreviewState::Ended);
    }

    /// Record a displayable error. The last good frame is kept.
    pub fn mark_error(&self, message: impl Into<String>) {
        self.set_state(PreviewState::Error(message.into()));
    }

    /// Restart with a fresh stream: discard stale frames and error state.
    pub fn restart(&self) {
        self.slot.clear();
        if let Ok(mut spec) = self.spec.lock() {
            *spec = None;
        }
        self.set_state(PreviewState::Idle);
    }
}

/// Convert a frame to tightly packed RGBA8 pixels for display surfaces.
///
/// Packed formats are swizzled; planar formats use integer BT.601. Returns
/// `None` only for internally inconsistent frames, which validated
/// construction makes unreachable.
pub fn video_frame_to_rgba_bytes(frame: &VideoFrame) -> Option<Vec<u8>> {
    use crate::format::VideoPixelFormat;

    let (width, height) = (frame.width() as usize, frame.height() as usize);
    let pixels = width.checked_mul(height)?;
    let mut out = vec![0u8; pixels.checked_mul(4)?];
    match frame.format() {
        VideoPixelFormat::Rgba => {
            out.copy_from_slice(frame.bytes());
        }
        VideoPixelFormat::Rgbx => {
            for (src, dst) in frame.bytes().chunks_exact(4).zip(out.chunks_exact_mut(4)) {
                dst[0] = src[0];
                dst[1] = src[1];
                dst[2] = src[2];
                dst[3] = 255;
            }
        }
        VideoPixelFormat::Bgrx => {
            for (src, dst) in frame.bytes().chunks_exact(4).zip(out.chunks_exact_mut(4)) {
                dst[0] = src[2];
                dst[1] = src[1];
                dst[2] = src[0];
                dst[3] = 255;
            }
        }
        VideoPixelFormat::Nv12 | VideoPixelFormat::I420 => {
            yuv_to_rgba(frame, &mut out)?;
        }
    }
    Some(out)
}

fn yuv_to_rgba(frame: &VideoFrame, out: &mut [u8]) -> Option<()> {
    use crate::frame::PlanarLayout;

    let (width, height) = (frame.width() as usize, frame.height() as usize);
    let bytes = frame.bytes();
    // Per-pixel chroma lookup for both planar layouts.
    let chroma = |x: usize, y: usize| -> Option<(u8, u8)> {
        match frame.planar_layout()? {
            PlanarLayout::I420 { u, v, .. } => {
                let (cw, cx, cy) = (width / 2, x / 2, y / 2);
                Some((
                    *bytes.get(u.start + cy * cw + cx)?,
                    *bytes.get(v.start + cy * cw + cx)?,
                ))
            }
            PlanarLayout::Nv12 { uv, .. } => {
                let (cw, cx, cy) = (width / 2, x / 2, y / 2);
                let base = uv.start + cy * cw * 2 + cx * 2;
                Some((*bytes.get(base)?, *bytes.get(base + 1)?))
            }
        }
    };
    let y_plane_len = width * height;
    let y_plane = bytes.get(..y_plane_len)?;
    for y in 0..height {
        for x in 0..width {
            let y_value = i32::from(y_plane[y * width + x]);
            let (u, v) = chroma(x, y)?;
            let (u, v) = (i32::from(u) - 128, i32::from(v) - 128);
            // Integer BT.601 with rounding.
            let r = (y_value + ((359 * v) >> 8)).clamp(0, 255) as u8;
            let g = (y_value - ((88 * u + 183 * v) >> 8)).clamp(0, 255) as u8;
            let b = (y_value + ((454 * u) >> 8)).clamp(0, 255) as u8;
            let dst = (y * width + x) * 4;
            out.get_mut(dst..dst + 4)?.copy_from_slice(&[r, g, b, 255]);
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::VideoPixelFormat;

    #[test]
    fn preview_serves_newest_frame_and_survives_restart() {
        let preview = VideoPreview::new();
        assert_eq!(preview.state(), PreviewState::Idle);
        assert!(preview.latest().is_none());

        let spec = VideoSpec::new(32, 32, 30, 1, VideoPixelFormat::Rgbx).unwrap();
        let mut a = VideoFrame::allocate(spec).unwrap();
        a.set_sequence(1);
        preview.publish(&a);
        let mut b = VideoFrame::allocate(spec).unwrap();
        b.set_sequence(2);
        preview.publish(&b);
        assert_eq!(preview.state(), PreviewState::Live);
        assert_eq!(preview.latest().unwrap().sequence(), 2);
        assert_eq!(preview.spec(), Some(spec));

        preview.mark_error("stream lost");
        assert!(matches!(preview.state(), PreviewState::Error(_)));
        // Last good frame is retained through errors.
        assert!(preview.latest().is_some());

        preview.restart();
        assert_eq!(preview.state(), PreviewState::Idle);
        assert!(preview.latest().is_none());
    }

    #[test]
    fn rgba_conversion_covers_all_formats() {
        use crate::format::VideoPixelFormat;
        use crate::frame::VideoFrame;

        for format in VideoPixelFormat::ALL {
            let spec = VideoSpec::new(8, 8, 30, 1, format).unwrap();
            let mut frame = VideoFrame::allocate(spec).unwrap();
            // Neutral content: mid gray everywhere.
            match format {
                VideoPixelFormat::Rgbx => frame.bytes_mut().chunks_exact_mut(4).for_each(|px| {
                    px[0] = 128;
                    px[1] = 128;
                    px[2] = 128;
                    px[3] = 255;
                }),
                VideoPixelFormat::Bgrx => frame.bytes_mut().chunks_exact_mut(4).for_each(|px| {
                    px[0] = 128;
                    px[1] = 128;
                    px[2] = 128;
                    px[3] = 255;
                }),
                VideoPixelFormat::Rgba => frame.bytes_mut().chunks_exact_mut(4).for_each(|px| {
                    px[0] = 128;
                    px[1] = 128;
                    px[2] = 128;
                    px[3] = 255;
                }),
                VideoPixelFormat::Nv12 | VideoPixelFormat::I420 => {
                    frame.bytes_mut()[..64].fill(128);
                    frame.bytes_mut()[64..].fill(128);
                }
            }
            let rgba = video_frame_to_rgba_bytes(&frame).expect("conversion should succeed");
            assert_eq!(rgba.len(), 8 * 8 * 4);
            // Neutral gray stays gray (planar rounds within 1).
            assert!((rgba[0] as i32 - 128).abs() <= 1, "{format}: {}", rgba[0]);
            assert_eq!(rgba[3], 255);
        }
    }

    #[test]
    fn bgrx_swizzles_to_rgba() {
        use crate::format::VideoPixelFormat;
        use crate::frame::VideoFrame;

        let spec = VideoSpec::new(1, 1, 30, 1, VideoPixelFormat::Bgrx).unwrap();
        let frame = VideoFrame::new(spec, vec![10, 20, 30, 255]).unwrap();
        assert_eq!(
            video_frame_to_rgba_bytes(&frame).unwrap(),
            vec![30, 20, 10, 255]
        );
    }
}
