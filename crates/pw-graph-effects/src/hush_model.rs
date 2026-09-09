//! Hush model provenance and shared resource loading.
//!
//! Model bytes and parsed state are owned by the control-plane resource
//! layer. The realtime Hush processor only receives a ready `Arc<HushModel>`
//! through its worker setup.

use crate::{global_effect_resources, EffectCancellation, ResourceId, ResourceSource};
use nnnoiseless::HushModel;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

pub(crate) const HUSH_MODEL_SHA256: &str =
    "45632ccaa82b71bb743d6caa7c78e983fe2f2790a3af7f6ec48e6ed7ba085df6";
const EMBEDDED_HUSH_MODEL: &[u8] =
    include_bytes!("../resources/hush/advanced_dfnet16k_model_best_onnx.tar.gz");
const EMBEDDED_HUSH_MODEL_NAME: &str = "advanced_dfnet16k_model_best_onnx.tar.gz";

/// Immutable metadata for one Hush model load attempt. The parsed model is
/// retained separately; this structure is only the diagnostic provenance.
#[derive(Clone, Debug, Default)]
pub(crate) struct HushModelLoadInfo {
    pub(crate) source: String,
    pub(crate) name: String,
    pub(crate) path: Option<String>,
    pub(crate) embedded: bool,
    pub(crate) compressed_bytes: u64,
    pub(crate) decompressed_bytes: Option<u64>,
    pub(crate) checksum: String,
    pub(crate) load_started: bool,
    pub(crate) load_completed: bool,
    pub(crate) load_duration_ms: u64,
    pub(crate) parse_started: bool,
    pub(crate) parse_completed: bool,
    pub(crate) error: Option<String>,
}

pub(crate) fn shared_hush_model() -> Result<Arc<HushModel>, String> {
    shared_hush_model_load().model.clone()
}

pub(crate) fn hush_model_load_info() -> HushModelLoadInfo {
    shared_hush_model_load().info.clone()
}

pub(crate) struct SharedHushModelLoad {
    pub(crate) model: Result<Arc<HushModel>, String>,
    pub(crate) info: HushModelLoadInfo,
}

fn shared_hush_model_load() -> &'static SharedHushModelLoad {
    static MODEL: OnceLock<SharedHushModelLoad> = OnceLock::new();
    MODEL.get_or_init(load_hush_model)
}

fn load_hush_model() -> SharedHushModelLoad {
    let override_path = non_empty_env_path("QPWGRAPH_HUSH_MODEL")
        .map(|path| ("QPWGRAPH_HUSH_MODEL", path))
        .or_else(|| non_empty_env_path("HUSH_MODEL").map(|path| ("HUSH_MODEL", path)));
    load_hush_model_with_override(override_path)
}

pub(crate) fn load_hush_model_with_override(
    override_path: Option<(&str, PathBuf)>,
) -> SharedHushModelLoad {
    let (source, name, path, embedded, resource_id, resource) = match override_path.as_ref() {
        Some((variable, path)) => (
            format!("environment override ({variable})"),
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("external Hush model")
                .to_owned(),
            Some(path.display().to_string()),
            false,
            ResourceId::new(format!("hush:model:{variable}:{}", path.display())),
            ResourceSource::file(path.clone(), None::<String>),
        ),
        None => (
            "embedded".to_owned(),
            EMBEDDED_HUSH_MODEL_NAME.to_owned(),
            None,
            true,
            ResourceId::new(format!("hush:model:embedded:{HUSH_MODEL_SHA256}")),
            ResourceSource::embedded(
                EMBEDDED_HUSH_MODEL_NAME,
                EMBEDDED_HUSH_MODEL,
                Some(HUSH_MODEL_SHA256),
            ),
        ),
    };
    let cancellation = EffectCancellation::default();
    eprintln!(
        "INFO hush: loading model source={} name={}{}",
        source,
        name,
        path.as_deref()
            .map(|path| format!(" path={path}"))
            .unwrap_or_default()
    );

    let loaded = global_effect_resources().load(resource_id, resource, &cancellation, |bytes| {
        if embedded {
            HushModel::from_static_bytes(EMBEDDED_HUSH_MODEL).map_err(|error| error.to_string())
        } else {
            HushModel::from_bytes(bytes).map_err(|error| error.to_string())
        }
    });
    let provenance = loaded
        .as_ref()
        .map(|loaded| loaded.provenance.clone())
        .unwrap_or_else(|error| (*error.provenance).clone());
    let mut info = HushModelLoadInfo {
        source,
        name,
        path,
        embedded,
        compressed_bytes: provenance.compressed_bytes,
        decompressed_bytes: provenance.decompressed_bytes,
        checksum: provenance.checksum.clone(),
        load_started: provenance.load_started,
        load_completed: provenance.load_completed,
        load_duration_ms: provenance.load_duration_ms,
        parse_started: provenance.parse_started,
        parse_completed: provenance.parse_completed,
        ..HushModelLoadInfo::default()
    };
    match loaded {
        Ok(loaded) => {
            let model = Ok(loaded.value);
            eprintln!(
                "INFO hush: model parsed source={} checksum={} compressed_bytes={} decompressed_bytes={} load_ms={}",
                info.source,
                info.checksum,
                info.compressed_bytes,
                info.decompressed_bytes
                    .map_or_else(|| "unknown".to_owned(), |bytes| bytes.to_string()),
                info.load_duration_ms
            );
            SharedHushModelLoad { model, info }
        }
        Err(error) => {
            info.error = Some(error.message.clone());
            eprintln!("ERROR hush: {}", error.message);
            SharedHushModelLoad {
                model: Err(error.message),
                info,
            }
        }
    }
}

fn non_empty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
