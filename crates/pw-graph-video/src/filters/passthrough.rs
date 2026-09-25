use crate::format::{VideoError, VideoSpec};
use crate::frame::VideoFrame;
use crate::processor::VideoProcessor;

/// Copies input to output unchanged. Also the health-check processor for new
/// PipeWire video streams.
pub struct Passthrough;

impl VideoProcessor for Passthrough {
    fn name(&self) -> &'static str {
        "passthrough"
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
        output.bytes_mut().copy_from_slice(input.bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::VideoPixelFormat;

    #[test]
    fn passthrough_copies_bytes() {
        let spec = VideoSpec::new(16, 16, 30, 1, VideoPixelFormat::Rgba).unwrap();
        let mut input = VideoFrame::allocate(spec).unwrap();
        for (i, byte) in input.bytes_mut().iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }
        let mut filter = Passthrough;
        filter.prepare(&spec).unwrap();
        let mut output = VideoFrame::allocate(spec).unwrap();
        filter.process(&input, &mut output).unwrap();
        assert_eq!(input.bytes(), output.bytes());
    }
}
