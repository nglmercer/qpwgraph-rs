//! Video format negotiation types.
//!
//! Dimensions arriving from PipeWire are never trusted: every constructor
//! funnels through [`VideoSpec::validate`], which rejects zero, excessive, or
//! overflowed sizes before any allocation happens.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Largest accepted frame width (8K UHD). Anything wider is rejected before
/// allocation.
pub const MAX_VIDEO_WIDTH: u32 = 7680;
/// Largest accepted frame height (8K UHD).
pub const MAX_VIDEO_HEIGHT: u32 = 4320;
/// Largest accepted framerate in frames per second.
pub const MAX_VIDEO_FPS: u32 = 240;
/// Largest accepted single-frame payload in bytes (128 MiB, fits 8K RGBA).
pub const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;
/// Largest accepted bounded-queue depth. Queues drop old frames instead of
/// growing past their capacity.
pub const MAX_QUEUE_DEPTH: usize = 8;
/// Default queue depth: low latency with room for one scheduling hiccup.
pub const DEFAULT_QUEUE_DEPTH: usize = 3;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum VideoError {
    #[error("invalid video dimensions {width}x{height}")]
    InvalidDimensions { width: u32, height: u32 },
    #[error("invalid framerate {num}/{den}")]
    InvalidFramerate { num: u32, den: u32 },
    #[error("framerate {fps} exceeds maximum {MAX_VIDEO_FPS}")]
    FramerateTooHigh { fps: u32 },
    #[error("frame size {bytes} exceeds maximum {MAX_FRAME_BYTES}")]
    FrameTooLarge { bytes: usize },
    #[error("frame size computation overflowed")]
    FrameSizeOverflow,
    #[error("unsupported pixel format {0}")]
    UnsupportedFormat(String),
    #[error("frame payload is {actual} bytes, expected {expected}")]
    LengthMismatch { actual: usize, expected: usize },
    #[error("spec mismatch: expected {expected}, got {actual}")]
    SpecMismatch { expected: String, actual: String },
    #[error("processor failed: {0}")]
    ProcessorFailed(String),
    #[error("processor not prepared")]
    NotPrepared,
    #[error("stream closed")]
    StreamClosed,
    #[error("operation not supported: {0}")]
    Unsupported(String),
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoPixelFormat {
    /// 32-bit packed `R:G:B:x` (SPA `RGBx`).
    #[default]
    Rgbx,
    /// 32-bit packed `B:G:R:x` (SPA `BGRx`).
    Bgrx,
    /// 32-bit packed `R:G:B:A` (SPA `RGBA`).
    Rgba,
    /// Planar Y + interleaved UV (SPA `NV12`).
    Nv12,
    /// Planar Y + planar U + planar V (SPA `I420`).
    I420,
}

impl VideoPixelFormat {
    pub const ALL: [Self; 5] = [Self::Rgbx, Self::Bgrx, Self::Rgba, Self::Nv12, Self::I420];

    /// Stable short name used in diagnostics and the UI.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rgbx => "RGBx",
            Self::Bgrx => "BGRx",
            Self::Rgba => "RGBA",
            Self::Nv12 => "NV12",
            Self::I420 => "I420",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "rgbx" => Some(Self::Rgbx),
            "bgrx" => Some(Self::Bgrx),
            "rgba" => Some(Self::Rgba),
            "nv12" => Some(Self::Nv12),
            "i420" | "yu12" => Some(Self::I420),
            _ => None,
        }
    }

    /// True for single-plane 4-byte packed formats.
    pub const fn is_packed(self) -> bool {
        matches!(self, Self::Rgbx | Self::Bgrx | Self::Rgba)
    }

    /// Bytes per pixel for packed formats.
    pub const fn packed_bpp(self) -> Option<usize> {
        if self.is_packed() {
            Some(4)
        } else {
            None
        }
    }

    /// Offset of the red channel within one packed pixel.
    pub const fn red_offset(self) -> Option<usize> {
        match self {
            Self::Rgbx | Self::Rgba => Some(0),
            Self::Bgrx => Some(2),
            Self::Nv12 | Self::I420 => None,
        }
    }

    /// Exact payload size for `width`x`height`, or an error on overflow,
    /// zero dimensions, or sizes above [`MAX_FRAME_BYTES`].
    pub fn frame_bytes(self, width: u32, height: u32) -> Result<usize, VideoError> {
        if width == 0 || height == 0 {
            return Err(VideoError::InvalidDimensions { width, height });
        }
        let pixels = (width as usize)
            .checked_mul(height as usize)
            .ok_or(VideoError::FrameSizeOverflow)?;
        let bytes = match self {
            Self::Rgbx | Self::Bgrx | Self::Rgba => {
                pixels.checked_mul(4).ok_or(VideoError::FrameSizeOverflow)?
            }
            // Planar formats require even dimensions; odd sizes are rejected
            // at spec validation, but integer division here stays exact for
            // the validated even case.
            Self::Nv12 | Self::I420 => {
                let y = pixels;
                let uv = pixels.checked_div(2).unwrap_or(0);
                y.checked_add(uv).ok_or(VideoError::FrameSizeOverflow)?
            }
        };
        if bytes > MAX_FRAME_BYTES {
            return Err(VideoError::FrameTooLarge { bytes });
        }
        Ok(bytes)
    }
}

impl std::fmt::Display for VideoPixelFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Negotiated video stream parameters: size, framerate, and pixel format.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct VideoSpec {
    pub width: u32,
    pub height: u32,
    pub framerate_num: u32,
    pub framerate_den: u32,
    pub format: VideoPixelFormat,
}

impl VideoSpec {
    pub fn new(
        width: u32,
        height: u32,
        framerate_num: u32,
        framerate_den: u32,
        format: VideoPixelFormat,
    ) -> Result<Self, VideoError> {
        let spec = Self {
            width,
            height,
            framerate_num,
            framerate_den,
            format,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Validate every field, including checked frame-size computation.
    /// Untrusted PipeWire values must pass through here before use.
    pub fn validate(&self) -> Result<(), VideoError> {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_VIDEO_WIDTH
            || self.height > MAX_VIDEO_HEIGHT
        {
            return Err(VideoError::InvalidDimensions {
                width: self.width,
                height: self.height,
            });
        }
        if !matches!(
            self.format,
            VideoPixelFormat::Rgbx | VideoPixelFormat::Bgrx | VideoPixelFormat::Rgba
        ) && (!self.width.is_multiple_of(2) || !self.height.is_multiple_of(2))
        {
            return Err(VideoError::InvalidDimensions {
                width: self.width,
                height: self.height,
            });
        }
        if self.framerate_den == 0 || self.framerate_num == 0 {
            return Err(VideoError::InvalidFramerate {
                num: self.framerate_num,
                den: self.framerate_den,
            });
        }
        let fps = self.framerate_num / self.framerate_den.max(1);
        if fps > MAX_VIDEO_FPS {
            return Err(VideoError::FramerateTooHigh { fps });
        }
        // Checked size computation; rejects overflow and excessive payloads.
        self.format.frame_bytes(self.width, self.height)?;
        Ok(())
    }

    /// Frames per second as a float for display. Validation guarantees
    /// `framerate_den != 0`.
    pub fn fps_f64(self) -> f64 {
        f64::from(self.framerate_num) / f64::from(self.framerate_den.max(1))
    }

    /// Exact payload size; infallible for a validated spec.
    pub fn frame_bytes(self) -> Result<usize, VideoError> {
        self.format.frame_bytes(self.width, self.height)
    }
}

impl std::fmt::Display for VideoSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}x{}@{}/{} {}",
            self.width, self.height, self.framerate_num, self.framerate_den, self.format
        )
    }
}

/// Clamp a requested queue depth into `1..=MAX_QUEUE_DEPTH`.
pub fn normalize_queue_depth(requested: usize) -> usize {
    requested.clamp(1, MAX_QUEUE_DEPTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_validation_accepts_common_formats() {
        for format in VideoPixelFormat::ALL {
            VideoSpec::new(1920, 1080, 30, 1, format).expect("1080p30 should validate");
            VideoSpec::new(3840, 2160, 60, 1, format).expect("4K60 should validate");
        }
    }

    #[test]
    fn spec_rejects_zero_and_huge_dimensions() {
        assert!(VideoSpec::new(0, 1080, 30, 1, VideoPixelFormat::Rgbx).is_err());
        assert!(VideoSpec::new(1920, 0, 30, 1, VideoPixelFormat::Rgbx).is_err());
        assert!(VideoSpec::new(7681, 1080, 30, 1, VideoPixelFormat::Rgbx).is_err());
        assert!(VideoSpec::new(1920, 4321, 30, 1, VideoPixelFormat::Rgbx).is_err());
        assert!(VideoSpec::new(u32::MAX, u32::MAX, 30, 1, VideoPixelFormat::Rgba).is_err());
    }

    #[test]
    fn planar_formats_require_even_dimensions() {
        assert!(VideoSpec::new(1919, 1080, 30, 1, VideoPixelFormat::Nv12).is_err());
        assert!(VideoSpec::new(1920, 1079, 30, 1, VideoPixelFormat::I420).is_err());
        assert!(VideoSpec::new(1919, 1079, 30, 1, VideoPixelFormat::Rgbx).is_ok());
    }

    #[test]
    fn spec_rejects_bad_framerate() {
        assert!(VideoSpec::new(640, 480, 0, 1, VideoPixelFormat::Rgbx).is_err());
        assert!(VideoSpec::new(640, 480, 30, 0, VideoPixelFormat::Rgbx).is_err());
        assert!(VideoSpec::new(640, 480, 1000, 1, VideoPixelFormat::Rgbx).is_err());
    }

    #[test]
    fn frame_bytes_are_exact_and_checked() {
        assert_eq!(
            VideoPixelFormat::Rgbx.frame_bytes(1920, 1080),
            Ok(1920 * 1080 * 4)
        );
        assert_eq!(
            VideoPixelFormat::Nv12.frame_bytes(1920, 1080),
            Ok(1920 * 1080 * 3 / 2)
        );
        assert_eq!(
            VideoPixelFormat::I420.frame_bytes(640, 480),
            Ok(640 * 480 * 3 / 2)
        );
        // 8K RGBA is the largest representable payload and must still fit.
        assert!(VideoPixelFormat::Rgba.frame_bytes(7680, 4320).is_ok());
        assert!(matches!(
            VideoPixelFormat::Rgba.frame_bytes(u32::MAX, 2),
            Err(VideoError::FrameTooLarge { .. } | VideoError::FrameSizeOverflow)
        ));
    }
}
