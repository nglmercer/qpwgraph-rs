//! Stable, deliberately small ABI shared by WASM effect authors and hosts.
//!
//! WASM modules are compiled for `wasm32-unknown-unknown`. The realtime call
//! has no WASI imports: module discovery, metadata, instantiation, and memory
//! growth happen before the PipeWire callback starts.

use super::{EffectDescriptor, EffectError, EffectIoCapabilities};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
#[cfg(feature = "wasm")]
use std::path::{Path, PathBuf};

pub const ABI_VERSION: u32 = 1;
pub const EXPORT_METADATA: &str = "effect_metadata";
pub const EXPORT_INIT: &str = "effect_init";
pub const EXPORT_PROCESS: &str = "effect_process";
pub const EXPORT_SET_PARAMETER: &str = "effect_set_parameter";
pub const EXPORT_RESET: &str = "effect_reset";
pub const EXPORT_MEMORY: &str = "memory";

pub fn required_exports() -> BTreeSet<&'static str> {
    [
        EXPORT_METADATA,
        EXPORT_INIT,
        EXPORT_PROCESS,
        EXPORT_SET_PARAMETER,
        EXPORT_RESET,
    ]
    .into_iter()
    .collect()
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WasmManifest {
    pub abi_version: u32,
    pub descriptor: EffectDescriptor,
    /// IO constraints are part of the manifest so a host can negotiate a
    /// module without instantiating it on the realtime thread.  Older
    /// manifests default to the original stereo contract.
    #[serde(default = "default_io_capabilities")]
    pub io: EffectIoCapabilities,
    /// Fixed guest-memory offset reserved for host audio/parameter buffers.
    /// The host validates capacity during preparation and never grows memory
    /// from EffectProcessor::process.
    #[serde(default)]
    pub audio_buffer_offset: u32,
}

fn default_io_capabilities() -> EffectIoCapabilities {
    EffectIoCapabilities::stereo()
}

impl WasmManifest {
    pub fn from_json(bytes: &[u8]) -> Result<Self, EffectError> {
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|error| EffectError::InvalidWasmManifest(error.to_string()))?;
        if manifest.abi_version != ABI_VERSION {
            return Err(EffectError::InvalidWasmManifest(format!(
                "unsupported ABI version {}",
                manifest.abi_version
            )));
        }
        if manifest.descriptor.id.trim().is_empty() {
            return Err(EffectError::InvalidWasmManifest(
                "effect id cannot be empty".into(),
            ));
        }
        manifest.io.validate()?;
        Ok(manifest)
    }
}

pub fn validate_exports<'a>(exports: impl IntoIterator<Item = &'a str>) -> Result<(), EffectError> {
    let exports: BTreeSet<&str> = exports.into_iter().collect();
    for required in required_exports() {
        if !exports.contains(required) {
            return Err(EffectError::MissingWasmExport(required.into()));
        }
    }
    Ok(())
}

/// Guest-side ABI shape. A guest SDK can implement these exports without
/// importing WASI. Pointer arguments refer to the module's linear memory.
pub const ABI_DOCUMENTATION: &str =
    "effect_metadata() -> (ptr,len), effect_init(rate,channels,max_frames) -> i32, effect_process(ptr,frames,channels) -> i32, effect_set_parameter(ptr,len,value) -> i32, effect_reset(); the host provides an exported memory and never grows it from process";

#[cfg(feature = "wasm")]
use crate::{
    apply_parameters, global_effect_resources, AudioSpec, EffectCancellation, EffectPrepareRequest,
    EffectProcessor, EffectProvider, PreparedEffect, ResourceId, ResourceSource,
};

/// Provider for the deliberately constrained qpwgraph WASM ABI.
///
/// A module is identified by its path for discovery, while its compiled
/// `wasmi::Module` is cached by [`global_effect_resources`]. The provider does
/// not expose WASI or arbitrary host imports; a module can only use its own
/// linear memory and the five ABI exports.
#[cfg(feature = "wasm")]
pub struct WasmEffectProvider {
    path: PathBuf,
    requested_id: String,
    descriptor: EffectDescriptor,
    io: EffectIoCapabilities,
}

#[cfg(feature = "wasm")]
impl WasmEffectProvider {
    pub fn new(path: impl Into<PathBuf>, requested_id: impl Into<String>) -> Self {
        let path = path.into();
        let requested_id = requested_id.into();
        let (descriptor, io) = sidecar_metadata(&path, &requested_id);
        Self {
            path,
            requested_id,
            descriptor,
            io,
        }
    }

    /// Discover a module and its optional adjacent JSON manifest. The module
    /// itself is still compiled lazily by `prepare_instance` so browsing a
    /// plugin directory remains cheap.
    pub fn discover(path: impl Into<PathBuf>) -> Result<Self, EffectError> {
        let path = path.into();
        let sidecar = sidecar_path(&path);
        let bytes = std::fs::read(&sidecar).map_err(|error| {
            EffectError::InvalidWasmManifest(format!(
                "could not read {}: {error}",
                sidecar.display()
            ))
        })?;
        let manifest = WasmManifest::from_json(&bytes)?;
        Ok(Self {
            path,
            requested_id: manifest.descriptor.id.clone(),
            descriptor: manifest.descriptor,
            io: manifest.io,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load_component(
        &self,
        cancellation: &EffectCancellation,
    ) -> Result<std::sync::Arc<WasmModuleComponent>, EffectError> {
        let id = ResourceId::new(format!("wasm:module:{}", self.path.display()));
        let source = ResourceSource::file(&self.path, None::<String>);
        let loaded = global_effect_resources()
            .load(id, source, cancellation, |bytes| {
                let engine = wasmi::Engine::default();
                let module = wasmi::Module::new(&engine, bytes)
                    .map_err(|error| format!("WASM validation/compilation failed: {error}"))?;
                validate_module_exports(&module)?;
                Ok(WasmModuleComponent { engine, module })
            })
            .map_err(|error| EffectError::InvalidWasmModule(error.to_string()))?;
        Ok(loaded.value)
    }
}

#[cfg(feature = "wasm")]
impl EffectProvider for WasmEffectProvider {
    fn descriptor(&self) -> &EffectDescriptor {
        &self.descriptor
    }

    fn create(&self) -> Box<dyn EffectProcessor> {
        Box::new(WasmProcessor::unprepared(self.descriptor.clone()))
    }

    fn io_capabilities(&self) -> EffectIoCapabilities {
        self.io
    }

    fn prepare_instance(
        &self,
        request: EffectPrepareRequest,
    ) -> Result<PreparedEffect, EffectError> {
        if request.cancellation.is_cancelled() {
            return Err(EffectError::PreparationCancelled);
        }
        let started = std::time::Instant::now();
        let component = self.load_component(&request.cancellation)?;
        let mut processor = WasmProcessor::instantiate(&component, request.spec)?;
        if processor.descriptor.id != request.effect_id
            || processor.descriptor.id != self.requested_id
        {
            return Err(EffectError::InvalidWasmManifest(format!(
                "module descriptor id '{}' does not match requested effect '{}'",
                processor.descriptor.id, request.effect_id
            )));
        }
        apply_parameters(&mut processor, &request.parameters)?;
        if request.cancellation.is_cancelled() {
            return Err(EffectError::PreparationCancelled);
        }
        Ok(PreparedEffect {
            descriptor: processor.descriptor.clone(),
            spec: request.spec,
            processor: Box::new(processor),
            preparation_duration_ms: started.elapsed().as_millis() as u64,
        })
    }
}

#[cfg(feature = "wasm")]
#[derive(Debug)]
struct WasmModuleComponent {
    engine: wasmi::Engine,
    module: wasmi::Module,
}

#[cfg(feature = "wasm")]
fn sidecar_path(path: &Path) -> PathBuf {
    path.with_extension("json")
}

#[cfg(feature = "wasm")]
fn sidecar_metadata(path: &Path, requested_id: &str) -> (EffectDescriptor, EffectIoCapabilities) {
    let default_descriptor = EffectDescriptor {
        id: requested_id.to_owned(),
        name: path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("WASM effect")
            .to_owned(),
        vendor: "WASM".into(),
        version: "unknown".into(),
        parameters: Vec::new(),
    };
    let Ok(bytes) = std::fs::read(sidecar_path(path)) else {
        return (default_descriptor, EffectIoCapabilities::stereo());
    };
    match WasmManifest::from_json(&bytes) {
        Ok(manifest) => (manifest.descriptor, manifest.io),
        Err(_) => (default_descriptor, EffectIoCapabilities::stereo()),
    }
}

#[cfg(feature = "wasm")]
fn validate_module_exports(module: &wasmi::Module) -> Result<(), String> {
    let names = module.exports().map(|export| export.name());
    validate_exports(names).map_err(|error| error.to_string())
}

#[cfg(feature = "wasm")]
struct WasmRuntime {
    store: wasmi::Store<()>,
    memory: wasmi::Memory,
    process: wasmi::TypedFunc<(i32, i32, i32), i32>,
    set_parameter: wasmi::TypedFunc<(i32, i32, f32), i32>,
    reset: wasmi::TypedFunc<(), ()>,
    audio_offset: usize,
    parameter_offset: usize,
    audio_bytes: Vec<u8>,
    parameter_bytes: Vec<u8>,
}

#[cfg(feature = "wasm")]
struct WasmProcessor {
    descriptor: EffectDescriptor,
    spec: Option<AudioSpec>,
    runtime: Option<WasmRuntime>,
}

#[cfg(feature = "wasm")]
impl WasmProcessor {
    fn unprepared(descriptor: EffectDescriptor) -> Self {
        Self {
            descriptor,
            spec: None,
            runtime: None,
        }
    }

    fn instantiate(component: &WasmModuleComponent, spec: AudioSpec) -> Result<Self, EffectError> {
        spec.validate()?;
        let names = component.module.exports().map(|export| export.name());
        validate_exports(names)
            .map_err(|error| EffectError::InvalidWasmModule(error.to_string()))?;
        let linker = wasmi::Linker::new(&component.engine);
        let mut store = wasmi::Store::new(&component.engine, ());
        let instance = linker
            .instantiate_and_start(&mut store, &component.module)
            .map_err(|error| {
                EffectError::InvalidWasmModule(format!("instantiation failed: {error}"))
            })?;
        let memory = instance
            .get_memory(&store, EXPORT_MEMORY)
            .ok_or_else(|| EffectError::MissingWasmExport(EXPORT_MEMORY.into()))?;
        let metadata = instance
            .get_typed_func::<(), (i32, i32)>(&store, EXPORT_METADATA)
            .map_err(|error| {
                EffectError::InvalidWasmModule(format!("invalid metadata export: {error}"))
            })?;
        let (metadata_ptr, metadata_len) = metadata.call(&mut store, ()).map_err(|error| {
            EffectError::InvalidWasmModule(format!("metadata call failed: {error}"))
        })?;
        if metadata_ptr < 0 || metadata_len < 0 {
            return Err(EffectError::InvalidWasmManifest(
                "metadata returned a negative pointer or length".into(),
            ));
        }
        let mut metadata_bytes = vec![0_u8; metadata_len as usize];
        memory
            .read(&store, metadata_ptr as usize, &mut metadata_bytes)
            .map_err(|error| {
                EffectError::InvalidWasmManifest(format!("metadata read failed: {error}"))
            })?;
        let manifest = WasmManifest::from_json(&metadata_bytes)?;
        if spec.channels < manifest.io.min_channels || spec.channels > manifest.io.max_channels {
            return Err(EffectError::UnsupportedChannelCount {
                channels: spec.channels,
                min_channels: manifest.io.min_channels,
                max_channels: manifest.io.max_channels,
            });
        }
        let audio_samples = (spec.max_frames as usize)
            .checked_mul(spec.channels as usize)
            .ok_or(EffectError::InternalBufferOverflow)?;
        let audio_bytes = audio_samples
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or(EffectError::InternalBufferOverflow)?;
        let audio_offset = manifest.audio_buffer_offset as usize;
        let parameter_offset = audio_offset
            .checked_add(audio_bytes)
            .ok_or(EffectError::InternalBufferOverflow)?;
        let parameter_capacity = 256_usize;
        let required_memory = parameter_offset
            .checked_add(parameter_capacity)
            .ok_or(EffectError::InternalBufferOverflow)?;
        if memory.data_size(&store) < required_memory {
            return Err(EffectError::InvalidWasmModule(format!(
                "exported memory is {} bytes, but {} bytes are required for the prepared audio buffer",
                memory.data_size(&store), required_memory
            )));
        }
        let init = instance
            .get_typed_func::<(i32, i32, i32), i32>(&store, EXPORT_INIT)
            .map_err(|error| {
                EffectError::InvalidWasmModule(format!("invalid init export: {error}"))
            })?;
        let process = instance
            .get_typed_func::<(i32, i32, i32), i32>(&store, EXPORT_PROCESS)
            .map_err(|error| {
                EffectError::InvalidWasmModule(format!("invalid process export: {error}"))
            })?;
        let set_parameter = instance
            .get_typed_func::<(i32, i32, f32), i32>(&store, EXPORT_SET_PARAMETER)
            .map_err(|error| {
                EffectError::InvalidWasmModule(format!("invalid parameter export: {error}"))
            })?;
        let reset = instance
            .get_typed_func::<(), ()>(&store, EXPORT_RESET)
            .map_err(|error| {
                EffectError::InvalidWasmModule(format!("invalid reset export: {error}"))
            })?;
        let initialized = init
            .call(
                &mut store,
                (
                    spec.sample_rate as i32,
                    spec.channels as i32,
                    spec.max_frames as i32,
                ),
            )
            .map_err(|error| {
                EffectError::InvalidWasmModule(format!("effect_init trapped: {error}"))
            })?;
        if initialized != 0 {
            return Err(EffectError::InvalidWasmModule(format!(
                "effect_init returned error code {initialized}"
            )));
        }
        Ok(Self {
            descriptor: manifest.descriptor,
            spec: Some(spec),
            runtime: Some(WasmRuntime {
                store,
                memory,
                process,
                set_parameter,
                reset,
                audio_offset,
                parameter_offset,
                audio_bytes: vec![0; audio_bytes],
                parameter_bytes: vec![0; parameter_capacity],
            }),
        })
    }
}

#[cfg(feature = "wasm")]
impl EffectProcessor for WasmProcessor {
    fn descriptor(&self) -> &EffectDescriptor {
        &self.descriptor
    }

    fn prepare(&mut self, _spec: AudioSpec) -> Result<(), EffectError> {
        if self.runtime.is_some() {
            return Err(EffectError::InvalidWasmModule(
                "WASM processor was already prepared".into(),
            ));
        }
        Err(EffectError::InvalidWasmModule(
            "WASM processors must be prepared through EffectProvider".into(),
        ))
    }

    fn process(&mut self, buffer: &mut [f32], frames: u32) -> Result<(), EffectError> {
        let spec = self.spec.ok_or(EffectError::NotPrepared)?;
        if frames > spec.max_frames {
            return Err(EffectError::FrameLimitExceeded {
                frames,
                max_frames: spec.max_frames,
            });
        }
        let samples = (frames as usize)
            .checked_mul(spec.channels as usize)
            .ok_or(EffectError::InternalBufferOverflow)?;
        if buffer.len() != samples {
            return Err(EffectError::InvalidBufferLength {
                actual: buffer.len(),
                expected: samples,
            });
        }
        let runtime = self.runtime.as_mut().ok_or(EffectError::NotPrepared)?;
        let bytes = samples
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or(EffectError::InternalBufferOverflow)?;
        for (sample, encoded) in buffer
            .iter()
            .zip(runtime.audio_bytes[..bytes].chunks_exact_mut(4))
        {
            encoded.copy_from_slice(&sample.to_le_bytes());
        }
        runtime
            .memory
            .write(
                &mut runtime.store,
                runtime.audio_offset,
                &runtime.audio_bytes[..bytes],
            )
            .map_err(|error| {
                EffectError::InvalidWasmModule(format!("audio input write failed: {error}"))
            })?;
        let status = runtime
            .process
            .call(
                &mut runtime.store,
                (
                    runtime.audio_offset as i32,
                    frames as i32,
                    spec.channels as i32,
                ),
            )
            .map_err(|error| {
                EffectError::InvalidWasmModule(format!("effect_process trapped: {error}"))
            })?;
        if status != 0 {
            return Err(EffectError::InvalidWasmModule(format!(
                "effect_process returned error code {status}"
            )));
        }
        runtime
            .memory
            .read(
                &runtime.store,
                runtime.audio_offset,
                &mut runtime.audio_bytes[..bytes],
            )
            .map_err(|error| {
                EffectError::InvalidWasmModule(format!("audio output read failed: {error}"))
            })?;
        for (sample, encoded) in buffer
            .iter_mut()
            .zip(runtime.audio_bytes[..bytes].chunks_exact(4))
        {
            let value = f32::from_bits(u32::from_le_bytes([
                encoded[0], encoded[1], encoded[2], encoded[3],
            ]));
            if !value.is_finite() {
                return Err(EffectError::InvalidWasmModule(
                    "effect_process returned a non-finite sample".into(),
                ));
            }
            *sample = value;
        }
        Ok(())
    }

    fn set_parameter(&mut self, id: &str, value: f32) -> Result<(), EffectError> {
        let parameter = self
            .descriptor
            .parameters
            .iter()
            .find(|parameter| parameter.id == id)
            .ok_or_else(|| EffectError::UnsupportedParameter(id.into()))?;
        if !value.is_finite() || value < parameter.minimum || value > parameter.maximum {
            return Err(EffectError::InvalidParameter {
                id: id.into(),
                value,
            });
        }
        let runtime = self.runtime.as_mut().ok_or(EffectError::NotPrepared)?;
        if id.len() > runtime.parameter_bytes.len() {
            return Err(EffectError::InvalidWasmModule(
                "parameter identifier exceeds the prepared guest buffer".into(),
            ));
        }
        runtime.parameter_bytes[..id.len()].copy_from_slice(id.as_bytes());
        runtime
            .memory
            .write(
                &mut runtime.store,
                runtime.parameter_offset,
                &runtime.parameter_bytes[..id.len()],
            )
            .map_err(|error| {
                EffectError::InvalidWasmModule(format!("parameter write failed: {error}"))
            })?;
        let status = runtime
            .set_parameter
            .call(
                &mut runtime.store,
                (runtime.parameter_offset as i32, id.len() as i32, value),
            )
            .map_err(|error| {
                EffectError::InvalidWasmModule(format!("parameter call trapped: {error}"))
            })?;
        if status != 0 {
            return Err(EffectError::InvalidParameter {
                id: id.into(),
                value,
            });
        }
        Ok(())
    }

    fn reset(&mut self) {
        if let Some(runtime) = &mut self.runtime {
            let _ = runtime.reset.call(&mut runtime.store, ());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_exports_are_validated() {
        let exports = required_exports();
        validate_exports(exports.iter().copied()).unwrap();
        let missing = [EXPORT_INIT, EXPORT_PROCESS, EXPORT_RESET];
        assert!(validate_exports(missing).is_err());
    }

    #[test]
    fn manifest_rejects_unknown_abi() {
        let descriptor = EffectDescriptor {
            id: "test.effect".into(),
            name: "Test".into(),
            vendor: "test".into(),
            version: "1".into(),
            parameters: Vec::new(),
        };
        let json = serde_json::to_vec(&WasmManifest {
            abi_version: ABI_VERSION + 1,
            descriptor,
            io: EffectIoCapabilities::stereo(),
            audio_buffer_offset: 0,
        })
        .unwrap();
        assert!(WasmManifest::from_json(&json).is_err());
    }
}

#[cfg(all(test, feature = "wasm"))]
mod runtime_tests {
    use super::*;
    use crate::{
        AudioSpec, EffectCancellation, EffectComponentManager, EffectHost, EffectPreparationEvent,
        EffectPrepareRequest, EffectProvider,
    };
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_module() -> Vec<u8> {
        let metadata = serde_json::json!({
            "abi_version": ABI_VERSION,
            "descriptor": {
                "id": "test.wasm",
                "name": "Test WASM",
                "vendor": "qpwgraph-rs tests",
                "version": "1",
                "parameters": []
            },
            "io": {
                "min_channels": 1,
                "max_channels": 1,
                "independent_channels": true,
                "preferred_channels": 1
            },
            "audio_buffer_offset": 4096
        })
        .to_string();
        let wat = format!(
            r#"(module
                (memory (export "memory") 1)
                (data (i32.const 0) {:?})
                (func (export "effect_metadata") (result i32 i32)
                    i32.const 0
                    i32.const {}
                )
                (func (export "effect_init") (param i32 i32 i32) (result i32)
                    i32.const 0
                )
                (func (export "effect_process") (param $ptr i32) (param $frames i32) (param $channels i32) (result i32)
                    (local $end i32)
                    local.get $ptr
                    local.get $frames
                    local.get $channels
                    i32.mul
                    i32.const 4
                    i32.mul
                    i32.add
                    local.set $end
                    (block $done
                        (loop $again
                            local.get $ptr
                            local.get $end
                            i32.ge_u
                            br_if $done
                            local.get $ptr
                            local.get $ptr
                            f32.load
                            f32.const 0.5
                            f32.mul
                            f32.store
                            local.get $ptr
                            i32.const 4
                            i32.add
                            local.set $ptr
                            br $again
                        )
                    )
                    i32.const 0
                )
                (func (export "effect_set_parameter") (param i32 i32 f32) (result i32)
                    i32.const 0
                )
                (func (export "effect_reset"))
            )"#,
            metadata,
            metadata.len()
        );
        wat::parse_str(wat).expect("test WASM module should compile")
    }

    fn unique_module_path() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        std::env::temp_dir().join(format!(
            "qpwgraph-test-{}-{}.wasm",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn provider_compiles_prepares_and_processes_a_module() {
        let path = unique_module_path();
        fs::write(&path, test_module()).unwrap();
        let provider = WasmEffectProvider::new(&path, "test.wasm");
        let prepared = provider
            .prepare_instance(EffectPrepareRequest {
                effect_id: "test.wasm".into(),
                module_path: Some(path.display().to_string()),
                spec: AudioSpec {
                    sample_rate: 48_000,
                    channels: 1,
                    max_frames: 32,
                },
                parameters: BTreeMap::new(),
                cancellation: EffectCancellation::default(),
            })
            .unwrap();
        let mut processor = prepared.processor;
        let mut audio = vec![1.0, -0.5, 0.25, 0.0];
        processor.process(&mut audio, 4).unwrap();
        assert_eq!(audio, vec![0.5, -0.25, 0.125, 0.0]);
        processor.reset();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn invalid_module_fails_during_preparation() {
        let path = unique_module_path();
        fs::write(&path, b"not wasm").unwrap();
        let provider = WasmEffectProvider::new(&path, "test.wasm");
        let error = provider
            .prepare_instance(EffectPrepareRequest {
                effect_id: "test.wasm".into(),
                module_path: Some(path.display().to_string()),
                spec: AudioSpec {
                    sample_rate: 48_000,
                    channels: 1,
                    max_frames: 32,
                },
                parameters: BTreeMap::new(),
                cancellation: EffectCancellation::default(),
            })
            .unwrap_err();
        assert!(error.to_string().contains("WASM"));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn module_request_uses_the_async_provider_lifecycle() {
        let path = unique_module_path();
        fs::write(&path, test_module()).unwrap();
        let mut manager = EffectComponentManager::new(1, 1);
        let host = EffectHost::default();
        let ticket = manager
            .begin_prepare(
                &host,
                EffectPrepareRequest {
                    effect_id: "test.wasm".into(),
                    module_path: Some(path.display().to_string()),
                    spec: AudioSpec {
                        sample_rate: 48_000,
                        channels: 1,
                        max_frames: 32,
                    },
                    parameters: BTreeMap::new(),
                    cancellation: EffectCancellation::default(),
                },
            )
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut ready = false;
        while std::time::Instant::now() < deadline {
            for event in manager.poll_events() {
                if let EffectPreparationEvent::Ready { ticket: actual, .. } = event {
                    ready = actual == ticket;
                }
            }
            if ready {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(ready, "WASM preparation did not produce a Ready event");
        fs::remove_file(path).unwrap();
    }
}
