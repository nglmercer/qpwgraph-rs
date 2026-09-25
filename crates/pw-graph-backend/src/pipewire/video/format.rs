//! SPA video format negotiation.
//!
//! This is the video-specific port negotiation: `EnumFormat` params offering
//! the pixel formats in [`SUPPORTED_SPA_FORMATS`] with daemon-fixated size
//! and framerate. Nothing is assumed about resolution or fps — every value
//! arriving from PipeWire is validated through [`VideoSpec`] before use.

use super::super::*;
use pw::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::{ChoiceValue, Object, Property, PropertyFlags, Value};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle, SpaTypes};
use pw_graph_video::{
    VideoError, VideoPixelFormat, VideoSpec, MAX_FRAME_BYTES, MAX_VIDEO_FPS, MAX_VIDEO_HEIGHT,
    MAX_VIDEO_WIDTH,
};

/// Pixel formats we negotiate, in preference order.
pub const SUPPORTED_PIXEL_FORMATS: [VideoPixelFormat; 5] = [
    VideoPixelFormat::Rgbx,
    VideoPixelFormat::Bgrx,
    VideoPixelFormat::Rgba,
    VideoPixelFormat::Nv12,
    VideoPixelFormat::I420,
];

/// Map an SPA video format to our pixel format. Anything else (YUY2, GRAY8,
/// DMA-BUF modifiers we do not handle, ...) is rejected so the daemon picks
/// a format we can actually process.
pub fn spa_format_to_pixel(format: VideoFormat) -> Option<VideoPixelFormat> {
    match format {
        VideoFormat::RGBx => Some(VideoPixelFormat::Rgbx),
        VideoFormat::BGRx => Some(VideoPixelFormat::Bgrx),
        VideoFormat::RGBA => Some(VideoPixelFormat::Rgba),
        VideoFormat::NV12 => Some(VideoPixelFormat::Nv12),
        VideoFormat::I420 => Some(VideoPixelFormat::I420),
        _ => None,
    }
}

pub fn pixel_to_spa(format: VideoPixelFormat) -> VideoFormat {
    match format {
        VideoPixelFormat::Rgbx => VideoFormat::RGBx,
        VideoPixelFormat::Bgrx => VideoFormat::BGRx,
        VideoPixelFormat::Rgba => VideoFormat::RGBA,
        VideoPixelFormat::Nv12 => VideoFormat::NV12,
        VideoPixelFormat::I420 => VideoFormat::I420,
    }
}

/// Convert negotiated SPA video info into a validated spec.
///
/// A zero framerate means "unspecified" (variable-rate sources); it falls
/// back to 30 fps rather than failing negotiation.
pub fn video_info_to_spec(info: &VideoInfoRaw) -> Result<VideoSpec, VideoError> {
    let format = spa_format_to_pixel(info.format()).ok_or_else(|| {
        VideoError::UnsupportedFormat(format!("unsupported SPA video format {:?}", info.format()))
    })?;
    let size = info.size();
    let framerate = info.framerate();
    let (num, den) = if framerate.num == 0 || framerate.denom == 0 {
        (30, 1)
    } else {
        (framerate.num, framerate.denom)
    };
    VideoSpec::new(size.width, size.height, num, den, format)
}

/// Parse a `Format` param pod into a validated spec.
pub fn parse_format_param(pod: &Pod) -> Result<VideoSpec, VideoError> {
    let mut info = VideoInfoRaw::new();
    info.parse(pod)
        .map_err(|_| VideoError::UnsupportedFormat("unparseable SPA video format".into()))?;
    video_info_to_spec(&info)
}

/// Build an `EnumFormat` pod offering `formats` with daemon-fixated size and
/// framerate. Used by capture streams, which accept the source's native
/// geometry instead of imposing their own.
pub fn build_enum_format_pod(formats: &[VideoPixelFormat]) -> BackendResult<Vec<u8>> {
    let formats: Vec<VideoFormat> = if formats.is_empty() {
        SUPPORTED_PIXEL_FORMATS
            .iter()
            .map(|f| pixel_to_spa(*f))
            .collect()
    } else {
        formats.iter().map(|f| pixel_to_spa(*f)).collect()
    };
    let (default, alternatives) = formats
        .split_first()
        .expect("format list is never empty after defaulting");
    let object = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: vec![
            Property {
                key: FormatProperties::MediaType.as_raw(),
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(MediaType::Video.as_raw())),
            },
            Property {
                key: FormatProperties::MediaSubtype.as_raw(),
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(MediaSubtype::Raw.as_raw())),
            },
            Property {
                key: FormatProperties::VideoFormat.as_raw(),
                flags: PropertyFlags::empty(),
                value: Value::Choice(ChoiceValue::Id(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: Id(default.as_raw()),
                        alternatives: alternatives.iter().map(|f| Id(f.as_raw())).collect(),
                    },
                ))),
            },
            Property {
                key: FormatProperties::VideoSize.as_raw(),
                flags: PropertyFlags::empty(),
                value: Value::Choice(ChoiceValue::Rectangle(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Rectangle {
                            width: 1920,
                            height: 1080,
                        },
                        min: Rectangle {
                            width: 32,
                            height: 32,
                        },
                        max: Rectangle {
                            width: MAX_VIDEO_WIDTH,
                            height: MAX_VIDEO_HEIGHT,
                        },
                    },
                ))),
            },
            Property {
                key: FormatProperties::VideoFramerate.as_raw(),
                flags: PropertyFlags::empty(),
                value: Value::Choice(ChoiceValue::Fraction(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Fraction { num: 30, denom: 1 },
                        min: Fraction { num: 1, denom: 1 },
                        max: Fraction {
                            num: MAX_VIDEO_FPS,
                            denom: 1,
                        },
                    },
                ))),
            },
        ],
    };
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map(|success| success.0.into_inner())
        .map_err(|error| native_error("PipeWire video format serialization", error))
}

/// Build a `Buffers` param describing acceptable buffer pools: `count`
/// buffers of one block each, with block sizes in
/// `min_size..=max_size` (defaulting to `default_size`).
///
/// Video streams must offer this: without it the daemon allocates
/// audio-sized (8 KiB) buffers and every frame arrives short. The negotiated
/// allocation is the intersection with the peer's offer, so a generous max
/// only widens acceptance — it never preallocates the max.
pub fn build_buffers_pod(
    min_size: u32,
    default_size: u32,
    max_size: u32,
    count: u32,
) -> BackendResult<Vec<u8>> {
    use pw::spa::sys::{
        SPA_PARAM_BUFFERS_blocks, SPA_PARAM_BUFFERS_buffers, SPA_PARAM_BUFFERS_size,
    };

    if min_size == 0 || default_size == 0 || max_size == 0 || count == 0 {
        return Err(BackendError::Native(
            "video buffer parameters must be nonzero".into(),
        ));
    }
    if !(min_size <= default_size && default_size <= max_size) {
        return Err(BackendError::Native(
            "video buffer sizes must order min <= default <= max".into(),
        ));
    }
    if max_size > MAX_FRAME_BYTES as u32 {
        return Err(BackendError::Native(
            "video buffer max exceeds the frame limit".into(),
        ));
    }
    let object = Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![
            Property {
                key: SPA_PARAM_BUFFERS_buffers,
                flags: PropertyFlags::empty(),
                value: Value::Int(count as i32),
            },
            Property {
                key: SPA_PARAM_BUFFERS_blocks,
                flags: PropertyFlags::empty(),
                value: Value::Int(1),
            },
            Property {
                key: SPA_PARAM_BUFFERS_size,
                flags: PropertyFlags::empty(),
                value: Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: default_size as i32,
                        min: min_size as i32,
                        max: max_size as i32,
                    },
                ))),
            },
        ],
    };
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map(|success| success.0.into_inner())
        .map_err(|error| native_error("PipeWire video buffers serialization", error))
}

/// Buffers offer for capture streams: accept anything up to the frame limit.
pub fn build_capture_buffers_pod() -> BackendResult<Vec<u8>> {
    build_buffers_pod(1, 1920 * 1080 * 4, MAX_FRAME_BYTES as u32, 4)
}

/// Buffers offer for output streams: exactly the produced frame size.
pub fn build_output_buffers_pod(spec: &VideoSpec) -> BackendResult<Vec<u8>> {
    let size = spec
        .frame_bytes()
        .map_err(|error| BackendError::native(format!("invalid video output spec: {error}")))?
        as u32;
    build_buffers_pod(size, size, size, 4)
}

/// Build an `EnumFormat` pod offering exactly `spec`. Used by output streams,
/// which produce a fixed geometry the downstream must accept.
pub fn build_fixed_format_pod(spec: &VideoSpec) -> BackendResult<Vec<u8>> {
    spec.validate()
        .map_err(|error| BackendError::native(format!("invalid video output spec: {error}")))?;
    let object = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: vec![
            Property {
                key: FormatProperties::MediaType.as_raw(),
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(MediaType::Video.as_raw())),
            },
            Property {
                key: FormatProperties::MediaSubtype.as_raw(),
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(MediaSubtype::Raw.as_raw())),
            },
            Property {
                key: FormatProperties::VideoFormat.as_raw(),
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(pixel_to_spa(spec.format).as_raw())),
            },
            Property {
                key: FormatProperties::VideoSize.as_raw(),
                flags: PropertyFlags::empty(),
                value: Value::Rectangle(Rectangle {
                    width: spec.width,
                    height: spec.height,
                }),
            },
            Property {
                key: FormatProperties::VideoFramerate.as_raw(),
                flags: PropertyFlags::empty(),
                value: Value::Fraction(Fraction {
                    num: spec.framerate_num,
                    denom: spec.framerate_den,
                }),
            },
        ],
    };
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map(|success| success.0.into_inner())
        .map_err(|error| native_error("PipeWire video fixed-format serialization", error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spa_format_mapping_covers_supported_set() {
        for pixel in SUPPORTED_PIXEL_FORMATS {
            assert_eq!(spa_format_to_pixel(pixel_to_spa(pixel)), Some(pixel));
        }
        assert_eq!(spa_format_to_pixel(VideoFormat::YUY2), None);
        assert_eq!(spa_format_to_pixel(VideoFormat::Unknown), None);
    }

    #[test]
    fn enum_format_pod_serializes() {
        let bytes = build_enum_format_pod(&[]).expect("pod should build");
        assert!(!bytes.is_empty());
        assert!(Pod::from_bytes(&bytes).is_some());
        let bytes = build_enum_format_pod(&[VideoPixelFormat::Nv12]).expect("pod should build");
        assert!(Pod::from_bytes(&bytes).is_some());
    }

    #[test]
    fn buffers_pods_serialize_and_validate() {
        let capture = build_capture_buffers_pod().expect("capture buffers should build");
        assert!(Pod::from_bytes(&capture).is_some());
        let spec = VideoSpec::new(320, 240, 30, 1, VideoPixelFormat::Rgbx).unwrap();
        let output = build_output_buffers_pod(&spec).expect("output buffers should build");
        assert!(Pod::from_bytes(&output).is_some());
        assert!(build_buffers_pod(0, 1, 1, 1).is_err());
        assert!(build_buffers_pod(10, 1, 100, 1).is_err());
        assert!(build_buffers_pod(1, 1, MAX_FRAME_BYTES as u32 + 1, 1).is_err());
    }

    #[test]
    fn fixed_format_pod_rejects_invalid_spec() {
        let valid = VideoSpec::new(1280, 720, 60, 1, VideoPixelFormat::Bgrx).unwrap();
        let bytes = build_fixed_format_pod(&valid).expect("pod should build");
        assert!(Pod::from_bytes(&bytes).is_some());
        let invalid = VideoSpec {
            width: 0,
            height: 720,
            framerate_num: 60,
            framerate_den: 1,
            format: VideoPixelFormat::Bgrx,
        };
        assert!(build_fixed_format_pod(&invalid).is_err());
    }
}
