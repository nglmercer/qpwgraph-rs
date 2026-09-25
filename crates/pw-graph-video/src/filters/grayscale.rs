use crate::format::{VideoError, VideoPixelFormat, VideoSpec};
use crate::frame::{PlanarLayout, VideoFrame};
use crate::processor::VideoProcessor;

/// Luminance conversion. Packed formats use integer BT.601 luma
/// (`(77R + 150G + 29B) >> 8`); planar formats keep Y and neutralize chroma,
/// which is exactly desaturation in YUV.
pub struct Grayscale;

impl VideoProcessor for Grayscale {
    fn name(&self) -> &'static str {
        "grayscale"
    }

    fn prepare(&mut self, spec: &VideoSpec) -> Result<(), VideoError> {
        spec.validate()
    }

    fn process(&mut self, input: &VideoFrame, output: &mut VideoFrame) -> Result<(), VideoError> {
        if input.spec() != output.spec() {
            return Err(VideoError::SpecMismatch {
                expected: output.spec().to_string(),
                actual: input.spec().to_string(),
            });
        }
        match input.format() {
            VideoPixelFormat::Rgbx | VideoPixelFormat::Bgrx | VideoPixelFormat::Rgba => {
                let red = input.format().red_offset().expect("packed format");
                let green = 1;
                let blue = if red == 0 { 2 } else { 0 };
                for (src, dst) in input
                    .bytes()
                    .chunks_exact(4)
                    .zip(output.bytes_mut().chunks_exact_mut(4))
                {
                    let r = u32::from(src[red]);
                    let g = u32::from(src[green]);
                    let b = u32::from(src[blue]);
                    let luma = ((77 * r + 150 * g + 29 * b) >> 8) as u8;
                    dst[red] = luma;
                    dst[green] = luma;
                    dst[blue] = luma;
                    dst[3] = src[3];
                }
                Ok(())
            }
            VideoPixelFormat::I420 => {
                let (y, u, v) = match input.planar_layout() {
                    Some(PlanarLayout::I420 { y, u, v, .. }) => (y, u, v),
                    _ => return Err(VideoError::ProcessorFailed("bad I420 layout".into())),
                };
                output.bytes_mut()[y.clone()].copy_from_slice(&input.bytes()[y]);
                output.bytes_mut()[u].fill(128);
                output.bytes_mut()[v].fill(128);
                Ok(())
            }
            VideoPixelFormat::Nv12 => {
                let (y, uv) = match input.planar_layout() {
                    Some(PlanarLayout::Nv12 { y, uv, .. }) => (y, uv),
                    _ => return Err(VideoError::ProcessorFailed("bad NV12 layout".into())),
                };
                output.bytes_mut()[y.clone()].copy_from_slice(&input.bytes()[y]);
                output.bytes_mut()[uv].fill(128);
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grayscale_uses_bt601_luma() {
        let spec = VideoSpec::new(2, 1, 30, 1, VideoPixelFormat::Rgbx).unwrap();
        // Pure red and pure green pixels.
        let mut input = VideoFrame::allocate(spec).unwrap();
        input
            .bytes_mut()
            .copy_from_slice(&[255, 0, 0, 255, 0, 255, 0, 255]);
        let mut filter = Grayscale;
        filter.prepare(&spec).unwrap();
        let mut output = VideoFrame::allocate(spec).unwrap();
        filter.process(&input, &mut output).unwrap();
        let out = output.bytes();
        assert_eq!(out[0], out[1]);
        assert_eq!(out[1], out[2]);
        assert_eq!(out[0], ((77 * 255) >> 8) as u8);
        assert_eq!(out[4], ((150 * 255) >> 8) as u8);
    }

    #[test]
    fn grayscale_neutralizes_planar_chroma() {
        for format in [VideoPixelFormat::Nv12, VideoPixelFormat::I420] {
            let spec = VideoSpec::new(4, 4, 30, 1, format).unwrap();
            let mut input = VideoFrame::allocate(spec).unwrap();
            input.bytes_mut().fill(200);
            let mut filter = Grayscale;
            filter.prepare(&spec).unwrap();
            let mut output = VideoFrame::allocate(spec).unwrap();
            filter.process(&input, &mut output).unwrap();
            // Luma preserved, chroma neutralized to 128.
            assert!(output.bytes()[..16].iter().all(|b| *b == 200));
            assert!(output.bytes()[16..].iter().all(|b| *b == 128));
        }
    }
}
