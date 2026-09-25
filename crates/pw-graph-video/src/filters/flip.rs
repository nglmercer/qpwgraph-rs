use crate::format::{VideoError, VideoPixelFormat, VideoSpec};
use crate::frame::{PlanarLayout, VideoFrame};
use crate::processor::VideoProcessor;

/// Mirror left-right.
pub struct HorizontalFlip;
/// Mirror top-bottom.
pub struct VerticalFlip;

impl VideoProcessor for HorizontalFlip {
    fn name(&self) -> &'static str {
        "hflip"
    }

    fn prepare(&mut self, spec: &VideoSpec) -> Result<(), VideoError> {
        spec.validate()
    }

    fn process(&mut self, input: &VideoFrame, output: &mut VideoFrame) -> Result<(), VideoError> {
        check_specs(input, output)?;
        match input.format() {
            VideoPixelFormat::Rgbx | VideoPixelFormat::Bgrx | VideoPixelFormat::Rgba => {
                flip_packed_horizontal(input, output);
                Ok(())
            }
            VideoPixelFormat::I420 => flip_planar_horizontal(input, output, true),
            VideoPixelFormat::Nv12 => flip_planar_horizontal(input, output, false),
        }
    }
}

impl VideoProcessor for VerticalFlip {
    fn name(&self) -> &'static str {
        "vflip"
    }

    fn prepare(&mut self, spec: &VideoSpec) -> Result<(), VideoError> {
        spec.validate()
    }

    fn process(&mut self, input: &VideoFrame, output: &mut VideoFrame) -> Result<(), VideoError> {
        check_specs(input, output)?;
        match input.format() {
            VideoPixelFormat::Rgbx | VideoPixelFormat::Bgrx | VideoPixelFormat::Rgba => {
                flip_packed_vertical(input, output);
                Ok(())
            }
            VideoPixelFormat::I420 | VideoPixelFormat::Nv12 => flip_planar_vertical(input, output),
        }
    }
}

fn check_specs(input: &VideoFrame, output: &VideoFrame) -> Result<(), VideoError> {
    if input.spec() != output.spec() {
        return Err(VideoError::SpecMismatch {
            expected: output.spec().to_string(),
            actual: input.spec().to_string(),
        });
    }
    Ok(())
}

fn flip_packed_horizontal(input: &VideoFrame, output: &mut VideoFrame) {
    let width = input.width() as usize;
    let stride = width * 4;
    for y in 0..input.height() as usize {
        let src_row = &input.bytes()[y * stride..(y + 1) * stride];
        let dst_row = &mut output.bytes_mut()[y * stride..(y + 1) * stride];
        for x in 0..width {
            let src = &src_row[x * 4..(x + 1) * 4];
            let dst = &mut dst_row[(width - 1 - x) * 4..(width - x) * 4];
            dst.copy_from_slice(src);
        }
    }
}

fn flip_packed_vertical(input: &VideoFrame, output: &mut VideoFrame) {
    let height = input.height() as usize;
    let stride = input.width() as usize * 4;
    for y in 0..height {
        let src = &input.bytes()[y * stride..(y + 1) * stride];
        // Copy through a temp row to keep borrows disjoint.
        let mut row = vec![0u8; stride];
        row.copy_from_slice(src);
        let dst_y = height - 1 - y;
        output.bytes_mut()[dst_y * stride..(dst_y + 1) * stride].copy_from_slice(&row);
    }
}

/// Plane byte offset, plane width, plane height, plus the luma dimensions.
type PlaneRanges = (Vec<(usize, usize, usize)>, usize, usize);

fn plane_ranges(frame: &VideoFrame, separate_chroma: bool) -> Option<PlaneRanges> {
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    if separate_chroma {
        match frame.planar_layout()? {
            PlanarLayout::I420 { y, u, v, .. } => Some((
                vec![
                    (y.start, w, h),
                    (u.start, w / 2, h / 2),
                    (v.start, w / 2, h / 2),
                ],
                w,
                h,
            )),
            _ => None,
        }
    } else {
        match frame.planar_layout()? {
            PlanarLayout::Nv12 { y, uv, .. } => {
                // NV12 chroma is interleaved pairs; each pair flips as a unit.
                Some((vec![(y.start, w, h), (uv.start, w / 2, h / 2)], w, h))
            }
            _ => None,
        }
    }
}

fn flip_planar_horizontal(
    input: &VideoFrame,
    output: &mut VideoFrame,
    separate_chroma: bool,
) -> Result<(), VideoError> {
    let Some((planes, _, _)) = plane_ranges(input, separate_chroma) else {
        return Err(VideoError::ProcessorFailed("bad planar layout".into()));
    };
    // NV12 chroma plane: work in 2-byte pairs.
    for (index, (base, pw, ph)) in planes.iter().enumerate() {
        let pair = if !separate_chroma && index == 1 { 2 } else { 1 };
        let row_len = pw * pair;
        for y in 0..*ph {
            for x in 0..*pw {
                let src_off = base + y * row_len + x * pair;
                let dst_off = base + y * row_len + (pw - 1 - x) * pair;
                let tmp: Vec<u8> = input.bytes()[src_off..src_off + pair].to_vec();
                output.bytes_mut()[dst_off..dst_off + pair].copy_from_slice(&tmp);
            }
        }
    }
    Ok(())
}

fn flip_planar_vertical(input: &VideoFrame, output: &mut VideoFrame) -> Result<(), VideoError> {
    let separate = matches!(input.format(), VideoPixelFormat::I420);
    let Some((planes, _, _)) = plane_ranges(input, separate) else {
        return Err(VideoError::ProcessorFailed("bad planar layout".into()));
    };
    for (index, (base, pw, ph)) in planes.iter().enumerate() {
        let pair = if !separate && index == 1 { 2 } else { 1 };
        let row_len = pw * pair;
        for y in 0..*ph {
            let src_off = base + y * row_len;
            let dst_off = base + (ph - 1 - y) * row_len;
            let tmp = input.bytes()[src_off..src_off + row_len].to_vec();
            output.bytes_mut()[dst_off..dst_off + row_len].copy_from_slice(&tmp);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered_frame(width: u32, height: u32) -> VideoFrame {
        let spec = VideoSpec::new(width, height, 30, 1, VideoPixelFormat::Rgbx).unwrap();
        let mut frame = VideoFrame::allocate(spec).unwrap();
        for (i, px) in frame.bytes_mut().chunks_exact_mut(4).enumerate() {
            px[0] = i as u8;
            px[1] = 0;
            px[2] = 0;
            px[3] = 255;
        }
        frame
    }

    #[test]
    fn hflip_mirrors_columns() {
        let input = numbered_frame(4, 1);
        let spec = input.spec();
        let mut output = VideoFrame::allocate(spec).unwrap();
        HorizontalFlip.process(&input, &mut output).unwrap();
        let reds: Vec<u8> = output.bytes().chunks_exact(4).map(|px| px[0]).collect();
        assert_eq!(reds, vec![3, 2, 1, 0]);
    }

    #[test]
    fn vflip_mirrors_rows() {
        let input = numbered_frame(1, 4);
        let spec = input.spec();
        let mut output = VideoFrame::allocate(spec).unwrap();
        VerticalFlip.process(&input, &mut output).unwrap();
        let reds: Vec<u8> = output.bytes().chunks_exact(4).map(|px| px[0]).collect();
        assert_eq!(reds, vec![3, 2, 1, 0]);
    }

    #[test]
    fn double_flip_is_identity_for_packed_and_planar() {
        for format in VideoPixelFormat::ALL {
            let spec = VideoSpec::new(8, 8, 30, 1, format).unwrap();
            let mut input = VideoFrame::allocate(spec).unwrap();
            for (i, b) in input.bytes_mut().iter_mut().enumerate() {
                *b = (i % 251) as u8;
            }
            let mut mid = VideoFrame::allocate(spec).unwrap();
            let mut back = VideoFrame::allocate(spec).unwrap();
            HorizontalFlip.process(&input, &mut mid).unwrap();
            HorizontalFlip.process(&mid, &mut back).unwrap();
            assert_eq!(input.bytes(), back.bytes(), "hflip x2 {format}");
            VerticalFlip.process(&input, &mut mid).unwrap();
            VerticalFlip.process(&mid, &mut back).unwrap();
            assert_eq!(input.bytes(), back.bytes(), "vflip x2 {format}");
        }
    }
}
