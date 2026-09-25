//! Bounded frame representations.
//!
//! A [`VideoFrame`] is a validated spec plus an exactly-sized payload. Length
//! is checked at construction so every consumer can index planes without
//! further validation.

use serde::{Deserialize, Serialize};
use std::time::Instant;

use crate::format::{VideoError, VideoPixelFormat, VideoSpec};

/// One decoded video frame. Payload length always equals the spec's exact
/// frame size.
#[derive(Clone, Debug)]
pub struct VideoFrame {
    spec: VideoSpec,
    data: Vec<u8>,
    sequence: u64,
    captured_at: Option<Instant>,
}

impl VideoFrame {
    /// Build a frame, validating the spec and exact payload length.
    pub fn new(spec: VideoSpec, data: Vec<u8>) -> Result<Self, VideoError> {
        spec.validate()?;
        let expected = spec.frame_bytes()?;
        if data.len() != expected {
            return Err(VideoError::LengthMismatch {
                actual: data.len(),
                expected,
            });
        }
        Ok(Self {
            spec,
            data,
            sequence: 0,
            captured_at: None,
        })
    }

    /// Allocate a zeroed frame for `spec`.
    pub fn allocate(spec: VideoSpec) -> Result<Self, VideoError> {
        spec.validate()?;
        let len = spec.frame_bytes()?;
        Ok(Self {
            spec,
            data: vec![0; len],
            sequence: 0,
            captured_at: None,
        })
    }

    /// Adopt a reused buffer, checking spec and length. Workers use this to
    /// avoid per-frame allocation on the steady-state path.
    pub fn from_reused_buffer(
        spec: VideoSpec,
        mut buffer: Vec<u8>,
        filled: usize,
        sequence: u64,
    ) -> Result<Self, VideoError> {
        spec.validate()?;
        let expected = spec.frame_bytes()?;
        if filled != expected || buffer.len() < filled {
            return Err(VideoError::LengthMismatch {
                actual: filled.min(buffer.len()),
                expected,
            });
        }
        buffer.truncate(filled);
        Ok(Self {
            spec,
            data: buffer,
            sequence,
            captured_at: None,
        })
    }

    pub fn spec(&self) -> VideoSpec {
        self.spec
    }

    pub fn format(&self) -> VideoPixelFormat {
        self.spec.format
    }

    pub fn width(&self) -> u32 {
        self.spec.width
    }

    pub fn height(&self) -> u32 {
        self.spec.height
    }

    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn set_sequence(&mut self, sequence: u64) {
        self.sequence = sequence;
    }

    pub fn captured_at(&self) -> Option<Instant> {
        self.captured_at
    }

    pub fn set_captured_at(&mut self, at: Instant) {
        self.captured_at = Some(at);
    }

    /// Split a packed frame into rows. Returns `None` for planar formats.
    pub fn packed_rows(&self) -> Option<PackedRows<'_>> {
        let bpp = self.format().packed_bpp()?;
        let stride = self.width() as usize * bpp;
        Some(PackedRows {
            data: &self.data,
            stride,
            height: self.height() as usize,
        })
    }

    pub fn packed_rows_mut(&mut self) -> Option<PackedRowsMut<'_>> {
        let bpp = self.format().packed_bpp()?;
        let stride = self.width() as usize * bpp;
        let height = self.height() as usize;
        Some(PackedRowsMut {
            data: &mut self.data,
            stride,
            height,
        })
    }

    /// Split planar frames into their planes (Y, U, V) or (Y, UV).
    /// Returns `None` for packed formats.
    pub fn planar_layout(&self) -> Option<PlanarLayout> {
        let (w, h) = (self.width() as usize, self.height() as usize);
        match self.format() {
            VideoPixelFormat::I420 => {
                let y_len = w * h;
                let c_len = y_len / 4;
                Some(PlanarLayout::I420 {
                    y: 0..y_len,
                    u: y_len..y_len + c_len,
                    v: y_len + c_len..y_len + 2 * c_len,
                    width: w,
                    height: h,
                })
            }
            VideoPixelFormat::Nv12 => {
                let y_len = w * h;
                Some(PlanarLayout::Nv12 {
                    y: 0..y_len,
                    uv: y_len..y_len + y_len / 2,
                    width: w,
                    height: h,
                })
            }
            _ => None,
        }
    }
}

/// Row view over a packed frame.
pub struct PackedRows<'a> {
    data: &'a [u8],
    stride: usize,
    height: usize,
}

impl<'a> PackedRows<'a> {
    pub fn row(&self, y: usize) -> Option<&'a [u8]> {
        if y >= self.height {
            return None;
        }
        self.data.get(y * self.stride..(y + 1) * self.stride)
    }
}

pub struct PackedRowsMut<'a> {
    data: &'a mut [u8],
    stride: usize,
    height: usize,
}

impl<'a> PackedRowsMut<'a> {
    pub fn row_mut(&mut self, y: usize) -> Option<&mut [u8]> {
        if y >= self.height {
            return None;
        }
        // Borrow disjoint rows through raw slicing to satisfy the borrow
        // checker without per-row RefCell overhead.
        let stride = self.stride;
        self.data.get_mut(y * stride..(y + 1) * stride)
    }
}

/// Byte ranges of planar frames. All ranges are validated against the frame
/// length at construction time.
#[derive(Clone, Debug)]
pub enum PlanarLayout {
    I420 {
        y: std::ops::Range<usize>,
        u: std::ops::Range<usize>,
        v: std::ops::Range<usize>,
        width: usize,
        height: usize,
    },
    Nv12 {
        y: std::ops::Range<usize>,
        uv: std::ops::Range<usize>,
        width: usize,
        height: usize,
    },
}

/// Encoded frame placeholder for future relay transport (protocol v4).
///
/// No encoder ships in this phase; the type exists so the relay layer can
/// later consume encoded output without changing the graph architecture.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct EncodedVideoFrame {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub keyframe: bool,
    pub presentation_timestamp_ms: u64,
    pub data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    #[default]
    H264,
    Vp9,
    Av1,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_rejects_wrong_length() {
        let spec = VideoSpec::new(64, 64, 30, 1, VideoPixelFormat::Rgbx).unwrap();
        assert!(VideoFrame::new(spec, vec![0; 10]).is_err());
        assert!(VideoFrame::new(spec, vec![0; 64 * 64 * 4]).is_ok());
    }

    #[test]
    fn frame_rejects_huge_dimensions_without_allocating() {
        // u32::MAX dimensions must fail in validation, never in `vec!`.
        let spec = VideoSpec {
            width: u32::MAX,
            height: u32::MAX,
            framerate_num: 30,
            framerate_den: 1,
            format: VideoPixelFormat::Rgba,
        };
        assert!(VideoFrame::allocate(spec).is_err());
    }

    #[test]
    fn planar_layout_ranges_cover_payload() {
        for format in [VideoPixelFormat::Nv12, VideoPixelFormat::I420] {
            let spec = VideoSpec::new(64, 64, 30, 1, format).unwrap();
            let frame = VideoFrame::allocate(spec).unwrap();
            let end = match frame.planar_layout().unwrap() {
                PlanarLayout::I420 { v, .. } => v.end,
                PlanarLayout::Nv12 { uv, .. } => uv.end,
            };
            assert_eq!(end, frame.len());
            assert_eq!(frame.len(), 64 * 64 * 3 / 2);
        }
    }
}
