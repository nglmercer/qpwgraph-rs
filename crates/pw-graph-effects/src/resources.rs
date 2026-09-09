//! Shared, bounded, non-realtime effect resources.
//!
//! A resource is keyed independently from an effect instance so multiple
//! preparations can share one immutable model/module load. Acquisition is
//! intentionally a blocking API: callers must use it from the effect loader
//! or another control-plane worker, never from EffectProcessor::process.

use crate::EffectCancellation;
use sha2::{Digest, Sha256};
use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

/// Stable cache identity. Include the source path/hash in IDs when the same
/// logical resource can be supplied by more than one source.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ResourceId(String);

impl ResourceId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ResourceId {
    fn from(id: &str) -> Self {
        Self::new(id)
    }
}

impl From<String> for ResourceId {
    fn from(id: String) -> Self {
        Self::new(id)
    }
}

/// Where one resource's bytes come from. Providers should keep source
/// metadata in the key when they allow an environment override or multiple
/// files to represent the same effect resource.
#[derive(Clone, Debug)]
pub enum ResourceSource {
    Embedded {
        name: String,
        bytes: &'static [u8],
        expected_sha256: Option<String>,
    },
    File {
        path: PathBuf,
        expected_sha256: Option<String>,
    },
}

impl ResourceSource {
    pub fn embedded(
        name: impl Into<String>,
        bytes: &'static [u8],
        expected_sha256: Option<impl Into<String>>,
    ) -> Self {
        Self::Embedded {
            name: name.into(),
            bytes,
            expected_sha256: expected_sha256.map(Into::into),
        }
    }

    pub fn file(path: impl Into<PathBuf>, expected_sha256: Option<impl Into<String>>) -> Self {
        Self::File {
            path: path.into(),
            expected_sha256: expected_sha256.map(Into::into),
        }
    }

    fn metadata(&self) -> (String, String, Option<String>, bool, Option<String>) {
        match self {
            Self::Embedded {
                name,
                expected_sha256,
                ..
            } => (
                "embedded".into(),
                name.clone(),
                None,
                true,
                expected_sha256.clone(),
            ),
            Self::File {
                path,
                expected_sha256,
            } => (
                "file".into(),
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("effect resource")
                    .into(),
                Some(path.display().to_string()),
                false,
                expected_sha256.clone(),
            ),
        }
    }
}

/// Provenance exposed to provider diagnostics after a resource load.
#[derive(Clone, Debug, Default)]
pub struct ResourceProvenance {
    pub source: String,
    pub name: String,
    pub path: Option<String>,
    pub embedded: bool,
    pub compressed_bytes: u64,
    pub decompressed_bytes: Option<u64>,
    pub checksum: String,
    pub load_started: bool,
    pub load_completed: bool,
    pub load_duration_ms: u64,
    pub parse_started: bool,
    pub parse_completed: bool,
}

/// A resource acquisition/validation failure. The provenance is retained so
/// a provider can report the exact source and checksum even when parsing or
/// reading failed.
#[derive(Clone, Debug)]
pub struct ResourceError {
    pub message: String,
    pub provenance: Box<ResourceProvenance>,
}

impl ResourceError {
    fn new(message: impl Into<String>, provenance: ResourceProvenance) -> Self {
        Self {
            message: message.into(),
            provenance: Box::new(provenance),
        }
    }

    fn cancelled(provenance: ResourceProvenance) -> Self {
        Self::new("resource acquisition was cancelled", provenance)
    }

    fn is_cancelled(&self) -> bool {
        self.message == "resource acquisition was cancelled"
    }
}

impl fmt::Display for ResourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ResourceError {}

/// A successful resource plus its immutable provenance snapshot.
#[derive(Clone, Debug)]
pub struct LoadedResource<T> {
    pub value: Arc<T>,
    pub provenance: ResourceProvenance,
}

enum ResourceState {
    Loading,
    Ready {
        value: Arc<dyn Any + Send + Sync>,
        provenance: ResourceProvenance,
    },
    Failed(ResourceError),
}

struct ResourceEntry {
    state: Mutex<ResourceState>,
    changed: Condvar,
}

impl ResourceEntry {
    fn loading() -> Self {
        Self {
            state: Mutex::new(ResourceState::Loading),
            changed: Condvar::new(),
        }
    }
}

/// Process-wide or host-owned cache for heavyweight immutable effect
/// resources. Concurrent acquisition of the same ResourceId performs exactly
/// one load/parse; other control-plane callers wait for that result.
#[derive(Clone, Default)]
pub struct EffectResourceManager {
    entries: Arc<Mutex<BTreeMap<ResourceId, Arc<ResourceEntry>>>>,
}

/// The process-wide cache used by built-in and dynamically discovered
/// providers.  Keeping this accessor here, instead of in Hush, makes model
/// and module resources share the same deduplication boundary.
pub fn global_effect_resources() -> &'static EffectResourceManager {
    static RESOURCES: OnceLock<EffectResourceManager> = OnceLock::new();
    RESOURCES.get_or_init(EffectResourceManager::new)
}

impl EffectResourceManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load, validate, parse, and cache a typed resource.
    pub fn load<T, F>(
        &self,
        id: impl Into<ResourceId>,
        source: ResourceSource,
        cancellation: &EffectCancellation,
        parse: F,
    ) -> Result<LoadedResource<T>, ResourceError>
    where
        T: Any + Send + Sync + 'static,
        F: FnOnce(&[u8]) -> Result<T, String>,
    {
        let id = id.into();
        let (entry, owner) = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(entry) = entries.get(&id) {
                (Arc::clone(entry), false)
            } else {
                let entry = Arc::new(ResourceEntry::loading());
                entries.insert(id.clone(), Arc::clone(&entry));
                (entry, true)
            }
        };

        if owner {
            let result = load_owned(&source, cancellation, parse);
            let cancelled = result
                .as_ref()
                .err()
                .is_some_and(ResourceError::is_cancelled);
            {
                let mut state = entry
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *state = match &result {
                    Ok(loaded) => ResourceState::Ready {
                        value: loaded.value.clone() as Arc<dyn Any + Send + Sync>,
                        provenance: loaded.provenance.clone(),
                    },
                    Err(error) => ResourceState::Failed(error.clone()),
                };
                entry.changed.notify_all();
            }
            if cancelled {
                // Cancellation is a request-local outcome, not a permanent
                // broken-resource state. Let a later request retry it.
                self.entries
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id);
            }
            return result;
        }

        loop {
            let state = entry
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match &*state {
                ResourceState::Loading => {
                    if cancellation.is_cancelled() {
                        return Err(ResourceError::cancelled(ResourceProvenance::default()));
                    }
                    let _ = entry
                        .changed
                        .wait_timeout(state, std::time::Duration::from_millis(10))
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
                ResourceState::Ready { value, provenance } => {
                    let value = Arc::clone(value).downcast::<T>().map_err(|_| {
                        ResourceError::new("resource type mismatch", provenance.clone())
                    })?;
                    return Ok(LoadedResource {
                        value,
                        provenance: provenance.clone(),
                    });
                }
                ResourceState::Failed(error) => return Err(error.clone()),
            }
        }
    }
}

fn load_owned<T, F>(
    source: &ResourceSource,
    cancellation: &EffectCancellation,
    parse: F,
) -> Result<LoadedResource<T>, ResourceError>
where
    T: Any + Send + Sync + 'static,
    F: FnOnce(&[u8]) -> Result<T, String>,
{
    let started = Instant::now();
    let (source_name, name, path, embedded, expected_sha256) = source.metadata();
    let mut provenance = ResourceProvenance {
        source: source_name,
        name,
        path,
        embedded,
        load_started: true,
        ..ResourceProvenance::default()
    };
    if cancellation.is_cancelled() {
        return Err(ResourceError::cancelled(provenance));
    }

    let bytes = match source {
        ResourceSource::Embedded { bytes, .. } => bytes.to_vec(),
        ResourceSource::File { path, .. } => std::fs::read(path).map_err(|error| {
            provenance.load_duration_ms = elapsed_ms(started);
            ResourceError::new(
                format!("could not read effect resource {}: {error}", path.display()),
                provenance.clone(),
            )
        })?,
    };
    provenance.compressed_bytes = bytes.len() as u64;
    provenance.decompressed_bytes = gzip_uncompressed_size(&bytes);
    provenance.checksum = format!("{:x}", Sha256::digest(&bytes));
    provenance.load_completed = true;
    provenance.load_duration_ms = elapsed_ms(started);
    if let Some(expected) = expected_sha256 {
        if !provenance.checksum.eq_ignore_ascii_case(&expected) {
            return Err(ResourceError::new(
                format!(
                    "resource checksum mismatch for {} (expected {}, got {})",
                    provenance.name, expected, provenance.checksum
                ),
                provenance,
            ));
        }
    }
    if cancellation.is_cancelled() {
        return Err(ResourceError::cancelled(provenance));
    }
    provenance.parse_started = true;
    let value = parse(&bytes).map_err(|error| {
        ResourceError::new(
            format!("could not parse resource {}: {error}", provenance.name),
            provenance.clone(),
        )
    })?;
    provenance.parse_completed = true;
    Ok(LoadedResource {
        value: Arc::new(value),
        provenance,
    })
}

fn gzip_uncompressed_size(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < 18 || bytes.get(..2) != Some(&[0x1f, 0x8b]) {
        return None;
    }
    Some(u64::from(u32::from_le_bytes(
        bytes[bytes.len() - 4..].try_into().ok()?,
    )))
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    #[test]
    fn concurrent_duplicate_acquisition_loads_once() {
        let manager = EffectResourceManager::new();
        let loads = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..3 {
            let manager = manager.clone();
            let loads = loads.clone();
            workers.push(thread::spawn(move || {
                let token = EffectCancellation::default();
                let loaded = manager
                    .load(
                        ResourceId::new("test:shared"),
                        ResourceSource::embedded("test", b"resource", None::<String>),
                        &token,
                        |bytes| {
                            loads.fetch_add(1, Ordering::Relaxed);
                            thread::sleep(std::time::Duration::from_millis(10));
                            Ok(bytes.to_vec())
                        },
                    )
                    .unwrap();
                assert_eq!(&*loaded.value, b"resource");
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(loads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn bad_checksum_is_cached_as_a_visible_error() {
        let manager = EffectResourceManager::new();
        let token = EffectCancellation::default();
        let error = manager
            .load::<Vec<u8>, _>(
                "test:checksum",
                ResourceSource::embedded("test", b"resource", Some("00")),
                &token,
                |bytes| Ok(bytes.to_vec()),
            )
            .unwrap_err();
        assert!(error.message.contains("checksum mismatch"));
        assert_eq!(error.provenance.checksum.len(), 64);
    }
}
