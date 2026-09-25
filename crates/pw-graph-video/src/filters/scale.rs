use serde::{Deserialize, Serialize};

use crate::format::{VideoError, VideoPixelFormat, VideoSpec};
use crate::frame::{PlanarLayout, VideoFrame};
use crate::processor::VideoProcessor;

/// Target size for [`Scale`]. Must be a valid, bounded dimension pair.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ScaleSize {
    pub width: u32,
    pub height: u32,
}

impl ScaleSize {
    pub fn validate_for(&self, spec: &VideoSpec) -> Result<(), VideoError> {
        VideoSpec::new(
            self.width,
            self.height,
            spec.framerate_num,
            spec.framerate_den,
            spec.format,
        )?;
        if !spec.format.is_packed()
            && (!self.width.is_multiple_of(2) || !self.height.is_multiple_of(2))
        {
            return Err(VideoError::Unsupported(
                "planar scale targets must be even".into(),
            ));
        }
        Ok(())
    }
}

/// Nearest-neighbor resizer. Fast, deterministic, and dependency-free; quality
/// is acceptable for previews and filter chains in this phase.
pub struct Scale {
    size: ScaleSize,
}

impl Scale {
    pub fn new(size: ScaleSize) -> Result<Self, VideoError> {
        if size.width == 0 || size.height == 0 {
            return Err(VideoError::InvalidDimensions {
                width: size.width,
                height: size.height,
            });
        }
        if size.width > crate::format::MAX_VIDEO_WIDTH
            || size.height > crate::format::MAX_VIDEO_HEIGHT
        {
            return Err(VideoError::InvalidDimensions {
                width: size.width,
                height: size.height,
            });
        }
        Ok(Self { size })
    }

    pub fn size(&self) -> ScaleSize {
        self.size
    }
}

impl VideoProcessor for Scale {
    fn name(&self) -> &'static str {
        "scale"
    }

    fn prepare(&mut self, spec: &VideoSpec) -> Result<(), VideoError> {
        spec.validate()?;
        self.size.validate_for(spec)?;
        self.output_spec(spec)?.validate()
    }

    fn output_spec(&self, input: &VideoSpec) -> Result<VideoSpec, VideoError> {
        input.validate()?;
        self.size.validate_for(input)?;
        VideoSpec::new(
            self.size.width,
            self.size.height,
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
                scale_packed(input, output);
                Ok(())
            }
            VideoPixelFormat::I420 => scale_planar(input, output, true),
            VideoPixelFormat::Nv12 => scale_planar(input, output, false),
        }
    }
}

fn scale_packed(input: &VideoFrame, output: &mut VideoFrame) {
    let (in_w, in_h) = (input.width() as usize, input.height() as usize);
    let (out_w, out_h) = (output.width() as usize, output.height() as usize);
    let in_stride = in_w * 4;
    let out_stride = out_w * 4;
    for out_y in 0..out_h {
        let in_y = (out_y * in_h / out_h).min(in_h - 1);
        for out_x in 0..out_w {
            let in_x = (out_x * in_w / out_w).min(in_w - 1);
            let src = in_y * in_stride + in_x * 4;
            let dst = out_y * out_stride + out_x * 4;
            let px = [
                input.bytes()[src],
                input.bytes()[src + 1],
                input.bytes()[src + 2],
                input.bytes()[src + 3],
            ];
            output.bytes_mut()[dst..dst + 4].copy_from_slice(&px);
        }
    }
}

fn scale_planar(
    input: &VideoFrame,
    output: &mut VideoFrame,
    separate_chroma: bool,
) -> Result<(), VideoError> {
    // Resample each plane independently with nearest neighbor. For NV12 the
    // chroma "pixels" are 2-byte UV pairs.
    struct Plane {
        in_base: usize,
        out_base: usize,
        in_w: usize,
        in_h: usize,
        out_w: usize,
        out_h: usize,
        unit: usize,
    }
    let mut planes = Vec::new();
    if separate_chroma {
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
        let (iw, ih, ow, oh) = plane_dims(input, output);
        planes.push(Plane {
            in_base: y.start,
            out_base: oy.start,
            in_w: iw,
            in_h: ih,
            out_w: ow,
            out_h: oh,
            unit: 1,
        });
        planes.push(Plane {
            in_base: u.start,
            out_base: ou.start,
            in_w: iw / 2,
            in_h: ih / 2,
            out_w: ow / 2,
            out_h: oh / 2,
            unit: 1,
        });
        planes.push(Plane {
            in_base: v.start,
            out_base: ov.start,
            in_w: iw / 2,
            in_h: ih / 2,
            out_w: ow / 2,
            out_h: oh / 2,
            unit: 1,
        });
    } else {
        let (
            Some(PlanarLayout::Nv12 { y, uv, .. }),
            Some(PlanarLayout::Nv12 { y: oy, uv: ouv, .. }),
        ) = (input.planar_layout(), output.planar_layout())
        else {
            return Err(VideoError::ProcessorFailed("bad NV12 layout".into()));
        };
        let (iw, ih, ow, oh) = plane_dims(input, output);
        planes.push(Plane {
            in_base: y.start,
            out_base: oy.start,
            in_w: iw,
            in_h: ih,
            out_w: ow,
            out_h: oh,
            unit: 1,
        });
        planes.push(Plane {
            in_base: uv.start,
            out_base: ouv.start,
            in_w: iw / 2,
            in_h: ih / 2,
            out_w: ow / 2,
            out_h: oh / 2,
            unit: 2,
        });
    }
    for plane in planes {
        for out_y in 0..plane.out_h {
            let in_y = (out_y * plane.in_h / plane.out_h).min(plane.in_h - 1);
            for out_x in 0..plane.out_w {
                let in_x = (out_x * plane.in_w / plane.out_w).min(plane.in_w - 1);
                let src = plane.in_base + in_y * plane.in_w * plane.unit + in_x * plane.unit;
                let dst = plane.out_base + out_y * plane.out_w * plane.unit + out_x * plane.unit;
                let tmp = input.bytes()[src..src + plane.unit].to_vec();
                output.bytes_mut()[dst..dst + plane.unit].copy_from_slice(&tmp);
            }
        }
    }
    Ok(())
}

fn plane_dims(input: &VideoFrame, output: &VideoFrame) -> (usize, usize, usize, usize) {
    (
        input.width() as usize,
        input.height() as usize,
        output.width() as usize,
        output.height() as usize,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_down_picks_nearest_samples() {
        let spec = VideoSpec::new(4, 4, 30, 1, VideoPixelFormat::Rgbx).unwrap();
        let mut input = VideoFrame::allocate(spec).unwrap();
        for (i, px) in input.bytes_mut().chunks_exact_mut(4).enumerate() {
            px[0] = i as u8;
            px[3] = 255;
        }
        let mut scale = Scale::new(ScaleSize {
            width: 2,
            height: 2,
        })
        .unwrap();
        scale.prepare(&spec).unwrap();
        let out_spec = scale.output_spec(&spec).unwrap();
        let mut output = VideoFrame::allocate(out_spec).unwrap();
        scale.process(&input, &mut output).unwrap();
        let reds: Vec<u8> = output.bytes().chunks_exact(4).map(|px| px[0]).collect();
        assert_eq!(reds, vec![0, 2, 8, 10]);
    }

    #[test]
    fn scale_rejects_zero_and_oversize_targets() {
        assert!(Scale::new(ScaleSize {
            width: 0,
            height: 64
        })
        .is_err());
        assert!(Scale::new(ScaleSize {
            width: 9000,
            height: 64
        })
        .is_err());
        let spec = VideoSpec::new(64, 64, 30, 1, VideoPixelFormat::Nv12).unwrap();
        let mut odd = Scale::new(ScaleSize {
            width: 63,
            height: 64,
        })
        .unwrap();
        assert!(odd.prepare(&spec).is_err());
    }
}
