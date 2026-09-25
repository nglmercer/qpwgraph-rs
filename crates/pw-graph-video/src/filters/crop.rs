use serde::{Deserialize, Serialize};

use crate::format::{VideoError, VideoPixelFormat, VideoSpec};
use crate::frame::{PlanarLayout, VideoFrame};
use crate::processor::VideoProcessor;

/// Crop rectangle in input pixels. For planar formats x/y/width/height must
/// be even (chroma subsampling).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct CropRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl CropRect {
    pub fn validate_for(&self, spec: &VideoSpec) -> Result<(), VideoError> {
        if self.width == 0 || self.height == 0 {
            return Err(VideoError::InvalidDimensions {
                width: self.width,
                height: self.height,
            });
        }
        let right = self
            .x
            .checked_add(self.width)
            .ok_or(VideoError::FrameSizeOverflow)?;
        let bottom = self
            .y
            .checked_add(self.height)
            .ok_or(VideoError::FrameSizeOverflow)?;
        if right > spec.width || bottom > spec.height {
            return Err(VideoError::Unsupported(format!(
                "crop {self:?} exceeds input {}x{}",
                spec.width, spec.height
            )));
        }
        if !spec.format.is_packed()
            && (!self.x.is_multiple_of(2)
                || !self.y.is_multiple_of(2)
                || !self.width.is_multiple_of(2)
                || !self.height.is_multiple_of(2))
        {
            return Err(VideoError::Unsupported(
                "planar crop coordinates must be even".into(),
            ));
        }
        Ok(())
    }
}

pub struct Crop {
    rect: CropRect,
}

impl Crop {
    pub fn new(rect: CropRect) -> Result<Self, VideoError> {
        if rect.width == 0 || rect.height == 0 {
            return Err(VideoError::InvalidDimensions {
                width: rect.width,
                height: rect.height,
            });
        }
        Ok(Self { rect })
    }

    pub fn rect(&self) -> CropRect {
        self.rect
    }
}

impl VideoProcessor for Crop {
    fn name(&self) -> &'static str {
        "crop"
    }

    fn prepare(&mut self, spec: &VideoSpec) -> Result<(), VideoError> {
        spec.validate()?;
        self.rect.validate_for(spec)?;
        self.output_spec(spec)?.validate()
    }

    fn output_spec(&self, input: &VideoSpec) -> Result<VideoSpec, VideoError> {
        input.validate()?;
        self.rect.validate_for(input)?;
        VideoSpec::new(
            self.rect.width,
            self.rect.height,
            input.framerate_num,
            input.framerate_den,
            input.format,
        )
    }

    fn process(&mut self, input: &VideoFrame, output: &mut VideoFrame) -> Result<(), VideoError> {
        let expected = self.output_spec(&input.spec())?;
        if output.spec() != expected {
            return Err(VideoError::SpecMismatch {
                expected: expected.to_string(),
                actual: output.spec().to_string(),
            });
        }
        match input.format() {
            VideoPixelFormat::Rgbx | VideoPixelFormat::Bgrx | VideoPixelFormat::Rgba => {
                let in_stride = input.width() as usize * 4;
                let out_stride = self.rect.width as usize * 4;
                let x_off = self.rect.x as usize * 4;
                for row in 0..self.rect.height as usize {
                    let src_y = self.rect.y as usize + row;
                    let src = &input.bytes()
                        [src_y * in_stride + x_off..src_y * in_stride + x_off + out_stride];
                    output.bytes_mut()[row * out_stride..(row + 1) * out_stride]
                        .copy_from_slice(src);
                }
                Ok(())
            }
            VideoPixelFormat::I420 => {
                crop_planar(input, output, &self.rect, true)?;
                Ok(())
            }
            VideoPixelFormat::Nv12 => {
                crop_planar(input, output, &self.rect, false)?;
                Ok(())
            }
        }
    }
}

/// One input plane copy: byte offset, row stride in samples, bytes per sample.
type PlaneCopy = (usize, usize, usize);
/// One output plane: byte offset, bytes per sample.
type PlaneTarget = (usize, usize);

fn crop_planar(
    input: &VideoFrame,
    output: &mut VideoFrame,
    rect: &CropRect,
    separate_chroma: bool,
) -> Result<(), VideoError> {
    let in_w = input.width() as usize;
    let (in_planes, out_planes): (Vec<PlaneCopy>, Vec<PlaneTarget>) = if separate_chroma {
        let (
            Some(PlanarLayout::I420 { y, u, v, .. }),
            Some(PlanarLayout::I420 {
                y: oy,
                u: ou,
                v: ov,
                ..
            }),
        ) = (input.planar_layout(), output.planar_layout())
        else {
            return Err(VideoError::ProcessorFailed("bad I420 layout".into()));
        };
        (
            vec![
                (y.start, in_w, 1),
                (u.start, in_w / 2, 1),
                (v.start, in_w / 2, 1),
            ],
            vec![(oy.start, 1), (ou.start, 1), (ov.start, 1)],
        )
    } else {
        let (
            Some(PlanarLayout::Nv12 { y, uv, .. }),
            Some(PlanarLayout::Nv12 { y: oy, uv: ouv, .. }),
        ) = (input.planar_layout(), output.planar_layout())
        else {
            return Err(VideoError::ProcessorFailed("bad NV12 layout".into()));
        };
        (
            vec![(y.start, in_w, 1), (uv.start, in_w / 2, 2)],
            vec![(oy.start, 1), (ouv.start, 2)],
        )
    };
    let scale = [(1u32, 1u32), (2, 2), (2, 2)];
    for (i, ((in_base, in_stride_units, unit), (out_base, _))) in
        in_planes.iter().zip(out_planes.iter()).enumerate()
    {
        let (sx, sy) = scale[i.min(2)];
        let (rx, ry, rw, rh) = (
            rect.x as usize / sx as usize,
            rect.y as usize / sy as usize,
            rect.width as usize / sx as usize,
            rect.height as usize / sy as usize,
        );
        let in_stride = in_stride_units * unit;
        let copy_len = rw * unit;
        for row in 0..rh {
            let src_off = in_base + (ry + row) * in_stride + rx * unit;
            let dst_off = out_base + row * copy_len;
            let tmp = input.bytes()[src_off..src_off + copy_len].to_vec();
            output.bytes_mut()[dst_off..dst_off + copy_len].copy_from_slice(&tmp);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_extracts_packed_region() {
        let spec = VideoSpec::new(8, 8, 30, 1, VideoPixelFormat::Rgbx).unwrap();
        let mut input = VideoFrame::allocate(spec).unwrap();
        for (i, px) in input.bytes_mut().chunks_exact_mut(4).enumerate() {
            px[0] = i as u8;
            px[3] = 255;
        }
        let mut crop = Crop::new(CropRect {
            x: 2,
            y: 1,
            width: 4,
            height: 2,
        })
        .unwrap();
        crop.prepare(&spec).unwrap();
        let out_spec = crop.output_spec(&spec).unwrap();
        assert_eq!((out_spec.width, out_spec.height), (4, 2));
        let mut output = VideoFrame::allocate(out_spec).unwrap();
        crop.process(&input, &mut output).unwrap();
        // Top-left of crop is input pixel (2,1) = index 10.
        assert_eq!(output.bytes()[0], 10);
        assert_eq!(output.bytes()[4 * 4], 18);
    }

    #[test]
    fn crop_rejects_out_of_bounds_and_odd_planar() {
        let spec = VideoSpec::new(8, 8, 30, 1, VideoPixelFormat::Rgbx).unwrap();
        let mut crop = Crop::new(CropRect {
            x: 6,
            y: 0,
            width: 4,
            height: 4,
        })
        .unwrap();
        assert!(crop.prepare(&spec).is_err());
        let planar = VideoSpec::new(8, 8, 30, 1, VideoPixelFormat::Nv12).unwrap();
        let mut odd = Crop::new(CropRect {
            x: 1,
            y: 0,
            width: 4,
            height: 4,
        })
        .unwrap();
        assert!(odd.prepare(&planar).is_err());
        assert!(Crop::new(CropRect {
            x: 0,
            y: 0,
            width: 0,
            height: 4
        })
        .is_err());
    }
}
