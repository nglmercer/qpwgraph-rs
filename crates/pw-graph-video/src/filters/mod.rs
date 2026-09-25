//! Deterministic CPU video filters.
//!
//! No GPU dependencies. Every filter is a pure function of its input frame
//! plus construction parameters, which makes them unit-testable without a
//! compositor or PipeWire daemon.

mod crop;
mod flip;
mod grayscale;
mod passthrough;
mod scale;

pub use crop::{Crop, CropRect};
pub use flip::{HorizontalFlip, VerticalFlip};
pub use grayscale::Grayscale;
pub use passthrough::Passthrough;
pub use scale::{Scale, ScaleSize};

use crate::format::VideoError;
use crate::processor::VideoProcessor;
use serde::{Deserialize, Serialize};

pub const FILTER_PASSTHROUGH: &str = "passthrough";
pub const FILTER_GRAYSCALE: &str = "grayscale";
pub const FILTER_HFLIP: &str = "hflip";
pub const FILTER_VFLIP: &str = "vflip";
pub const FILTER_CROP: &str = "crop";
pub const FILTER_SCALE: &str = "scale";

pub const ALL_FILTERS: [&str; 6] = [
    FILTER_PASSTHROUGH,
    FILTER_GRAYSCALE,
    FILTER_HFLIP,
    FILTER_VFLIP,
    FILTER_CROP,
    FILTER_SCALE,
];

/// Construction parameters for geometry-changing filters.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct FilterParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<CropRect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<ScaleSize>,
}

/// Create a filter by id. Unknown ids and invalid parameters are errors, not
/// panics, so UI input can never crash the graph.
pub fn create_filter(
    id: &str,
    params: &FilterParams,
) -> Result<Box<dyn VideoProcessor>, VideoError> {
    match id {
        FILTER_PASSTHROUGH => Ok(Box::new(Passthrough)),
        FILTER_GRAYSCALE => Ok(Box::new(Grayscale)),
        FILTER_HFLIP => Ok(Box::new(HorizontalFlip)),
        FILTER_VFLIP => Ok(Box::new(VerticalFlip)),
        FILTER_CROP => {
            let rect = params.crop.ok_or_else(|| {
                VideoError::Unsupported("crop filter needs a crop rectangle".into())
            })?;
            Ok(Box::new(Crop::new(rect)?))
        }
        FILTER_SCALE => {
            let size = params.scale.ok_or_else(|| {
                VideoError::Unsupported("scale filter needs a target size".into())
            })?;
            Ok(Box::new(Scale::new(size)?))
        }
        _ => Err(VideoError::Unsupported(format!(
            "unknown video filter {id}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_filter_is_an_error() {
        assert!(create_filter("bloom", &FilterParams::default()).is_err());
        assert!(create_filter(FILTER_CROP, &FilterParams::default()).is_err());
    }

    #[test]
    fn all_filters_construct_with_valid_params() {
        let params = FilterParams {
            crop: Some(CropRect {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
            }),
            scale: Some(ScaleSize {
                width: 32,
                height: 32,
            }),
        };
        for id in ALL_FILTERS {
            create_filter(id, &params).expect("filter should construct");
        }
    }
}
