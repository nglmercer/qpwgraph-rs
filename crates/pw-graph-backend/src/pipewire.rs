//! Native PipeWire graph driver implemented with the official Rust bindings.
//!
//! PipeWire objects are deliberately kept on a dedicated `ThreadLoop`. The
//! public driver remains synchronous for the rest of the application, while
//! every registry, link, and stream operation is protected by the loop lock.

use super::*;
use ::pipewire as pw;
use pw::proxy::ProxyT;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::param::ParamType;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::pod::{Pod, Value};
use pw::spa::utils::Direction as SpaDirection;
use pw_graph_core::NodeIdentity;
use pw_graph_effects::{
    AudioSpec, EffectComponentManager, EffectHost, EffectPreparationEvent, EffectPrepareRequest,
    EffectTicket, PreparedEffect,
};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Cursor;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod effect_lifecycle;
mod effect_topology;
mod effects;
mod filter_runtime;
mod links;
mod metering;
mod properties;
mod readback;
mod recorder;
mod registry;
#[cfg(all(target_os = "linux", feature = "relay"))]
mod relay;
#[cfg(all(target_os = "linux", feature = "relay"))]
mod relay_driver;

use crate::router::{
    replace_recording_preserving_source, RecorderWriterState, DEFAULT_RECORDER_CAPACITY_MS,
};
use effect_topology::active_channel_mask;
use effects::NativeEffect;
use metering::{process_meter_buffer, MeterCallbackState, MeterHandle, MeterReadingState};
use recorder::RecorderHandle;
use registry::{
    classify_port_type, install_default_metadata_listener, install_registry_listener,
    MetadataBindings, NodeRecord, RegistryState,
};

const NODE_NAME: &str = "node.name";
const NODE_DESCRIPTION: &str = "node.description";
const OBJECT_PATH: &str = "object.path";
const MEDIA_CLASS: &str = "media.class";
const MEDIA_NAME: &str = "media.name";
const MEDIA_TYPE: &str = "media.type";
const FORMAT_DSP: &str = "format.dsp";
const NODE_ID: &str = "node.id";
const OBJECT_SERIAL: &str = "object.serial";
const CLIENT_ID: &str = "client.id";
const CLIENT_NAME: &str = "client.name";
const CLIENT_API: &str = "client.api";
const APPLICATION_ID: &str = "application.id";
const APPLICATION_NAME: &str = "application.name";
const APPLICATION_ICON_NAME: &str = "application.icon-name";
const APPLICATION_PROCESS_BINARY: &str = "application.process.binary";
const DEVICE_ID: &str = "device.id";
const DEVICE_ICON_NAME: &str = "device.icon-name";
const MEDIA_ICON_NAME: &str = "media.icon-name";
const PORT_NAME: &str = "port.name";
const AUDIO_CHANNEL: &str = "audio.channel";
const PORT_DIRECTION: &str = "port.direction";
const LINK_OUTPUT_PORT: &str = "link.output.port";
const LINK_INPUT_PORT: &str = "link.input.port";
const NODE_INTERFACE_VERSION: u32 = 3;

struct PendingEffect {
    request: EffectCreateRequest,
}

struct NativeRecorder {
    request: RecorderCreateRequest,
    instance: RecorderInstance,
    /// A restart can lose the native PipeWire stream after the old one has
    /// been destroyed. Keep the stable recorder record even in that degraded
    /// state so callers never observe a spurious "unknown recorder" ID.
    handle: Option<RecorderHandle>,
}

/// Property keys for the client-owned helper nodes (`pw_filter`s and meter
/// streams). Shared by the effect, relay, and metering runtimes so a property
/// name only needs to be spelled once.
const PROP_NODE_VIRTUAL: &str = "node.virtual";
const PROP_NODE_AUTOCONNECT: &str = "node.autoconnect";
const PROP_NODE_GROUP: &str = "node.group";
const PROP_MEDIA_CATEGORY: &str = "media.category";
const PROP_MEDIA_ROLE: &str = "media.role";
const PROP_FORMAT_DSP_VALUE: &str = "32 bit float mono audio";

/// Media classes and roles shared by the virtual nodes the backend creates.
const MEDIA_CLASS_AUDIO_FILTER: &str = "Audio/Filter";
const MEDIA_ROLE_DSP: &str = "DSP";
const MEDIA_TYPE_AUDIO: &str = "Audio";
const MEDIA_CATEGORY_FILTER: &str = "Filter";

/// Node name given to our own metering streams. They are helper objects, so
/// they are filtered back out of the graph the UI renders.
const METER_NODE_PREFIX: &str = "qpwgraph-rs meter";
const RECORDER_NODE_PREFIX: &str = "qpwgraph-rs recorder";

/// How long a metering stream outlives the last request for it. Without a
/// grace period, minimizing and immediately restoring the window would tear
/// down and rebuild every visible stream.
const METER_LINGER: Duration = Duration::from_secs(5);

/// Boost ceiling applied by `set_node_volume_locked`; the fader matches it.
pub(super) const PIPEWIRE_MAX_VOLUME: f32 = 1.5;

fn graph_id(native_id: u64) -> u64 {
    encode_backend_id(BackendNamespace::PipeWire, native_id)
}

fn native_id(graph_id: u64) -> u32 {
    decode_backend_local_id(graph_id) as u32
}

fn native_node_id(node_id: NodeId) -> u32 {
    native_id(node_id.0)
}

fn native_port_id(port_id: PortId) -> u32 {
    native_id(port_id.0)
}

fn native_link_id(link_id: LinkId) -> u32 {
    native_id(link_id.0)
}

pub struct PipewireDriver {
    thread_loop: pw::thread_loop::ThreadLoop,
    context: Option<pw::context::Context>,
    core: Option<pw::core::Core>,
    registry: Option<pw::registry::Registry>,
    registry_listener: Option<pw::registry::Listener>,
    /// Listener for WirePlumber's `default` metadata object plus the bound
    /// proxy/listener pairs it creates when the metadata global appears.
    metadata_listener: Option<pw::registry::Listener>,
    metadata_bindings: MetadataBindings,
    state: Arc<Mutex<RegistryState>>,
    registry_dirty: Arc<AtomicBool>,
    meters: BTreeMap<NodeId, MeterHandle>,
    /// Client-owned input streams and their asynchronous WAV writers.
    recorders: BTreeMap<RecorderId, NativeRecorder>,
    meter_policy: MeterPolicy,
    /// Nodes the UI asked to measure, with the time of the last request so a
    /// stream can linger briefly instead of dying the moment a tooltip closes.
    meter_requests: BTreeMap<NodeId, Instant>,
    /// Zero point for the millisecond timestamps meters publish atomically.
    epoch: Instant,
    graph: Graph,
    positions: BTreeMap<NodeId, [f32; 2]>,
    audio_controls: BTreeMap<NodeId, NodeAudioState>,
    effect_host: EffectHost,
    /// Live `pw_filter` owners.  Keeping them in the driver makes their
    /// lifecycle match the PipeWire thread loop and lets graph snapshots map
    /// transient global IDs back to stable effect instance IDs.
    effects: BTreeMap<String, NativeEffect>,
    /// Heavyweight effect preparation is deliberately separate from
    /// PipeWire publication.  The loader owns no graph state; completed
    /// processors are activated on the driver/control thread.
    effect_loader: EffectComponentManager,
    pending_effects: BTreeMap<EffectTicket, PendingEffect>,
    /// Manual disconnects are kept as stable endpoint pairs. WirePlumber may
    /// recreate an application's link when it resumes; the next synchronized
    /// snapshot removes only those links the user explicitly deleted.
    blocked_connections: Vec<(PortKey, PortKey)>,
    /// Relay engine plus the two virtual devices. Created on first relay use
    /// and kept until the driver drops so reconnects stay cheap.
    #[cfg(all(target_os = "linux", feature = "relay"))]
    relay: Option<relay::RelayRuntimeSet>,
    /// Keep generic endpoint selections even before the relay runtime is
    /// created. The UI applies persisted preferences during startup, while
    /// discovery or a first connection may create the runtime later.
    #[cfg(all(target_os = "linux", feature = "relay"))]
    relay_send_source: RelaySendSource,
    #[cfg(all(target_os = "linux", feature = "relay"))]
    relay_receive_sink: RelayReceiveSink,
}

impl PipewireDriver {
    pub fn new() -> BackendResult<Self> {
        pw::init();

        let thread_loop = unsafe { pw::thread_loop::ThreadLoop::new(Some("qpwgraph-rs"), None) }
            .map_err(|error| native_error("PipeWire thread loop creation", error))?;
        let context = pw::context::Context::new(&thread_loop)
            .map_err(|error| native_error("PipeWire context creation", error))?;
        let core = context
            .connect(None)
            .map_err(|error| native_error("PipeWire core connection", error))?;
        let registry = core
            .get_registry()
            .map_err(|error| native_error("PipeWire registry creation", error))?;
        let state = Arc::new(Mutex::new(RegistryState::default()));
        let registry_dirty = Arc::new(AtomicBool::new(true));

        let registry_listener = install_registry_listener(&registry, &state, &registry_dirty);
        let (metadata_listener, metadata_bindings) =
            install_default_metadata_listener(&registry, &state, &registry_dirty);

        thread_loop.start();

        let driver = Self {
            thread_loop,
            context: Some(context),
            core: Some(core),
            registry: Some(registry),
            registry_listener: Some(registry_listener),
            metadata_listener: Some(metadata_listener),
            metadata_bindings,
            state,
            registry_dirty,
            meters: BTreeMap::new(),
            recorders: BTreeMap::new(),
            meter_policy: MeterPolicy::default(),
            meter_requests: BTreeMap::new(),
            epoch: Instant::now(),
            graph: Graph::default(),
            positions: BTreeMap::new(),
            audio_controls: BTreeMap::new(),
            effect_host: EffectHost::new(),
            effects: BTreeMap::new(),
            effect_loader: EffectComponentManager::new(2, 16),
            pending_effects: BTreeMap::new(),
            blocked_connections: Vec::new(),
            #[cfg(all(target_os = "linux", feature = "relay"))]
            relay: None,
            #[cfg(all(target_os = "linux", feature = "relay"))]
            relay_send_source: RelaySendSource::DefaultInput,
            #[cfg(all(target_os = "linux", feature = "relay"))]
            relay_receive_sink: RelayReceiveSink::DefaultOutput,
        };

        let loop_for_initial_sync = driver.thread_loop.clone();
        let _guard = loop_for_initial_sync.lock();
        driver.roundtrip_locked()?;
        Ok(driver)
    }

    fn core(&self) -> BackendResult<&pw::core::Core> {
        self.core
            .as_ref()
            .ok_or_else(|| BackendError::Native("PipeWire core is closed".into()))
    }

    fn registry(&self) -> BackendResult<&pw::registry::Registry> {
        self.registry
            .as_ref()
            .ok_or_else(|| BackendError::Native("PipeWire registry is closed".into()))
    }

    /// Wait for all registry events queued before the sync request.
    /// The caller must hold the thread-loop lock.
    fn roundtrip_locked(&self) -> BackendResult<()> {
        let core = self.core()?.clone();
        let pending = core
            .sync(0)
            .map_err(|error| native_error("PipeWire registry synchronization", error))?;
        let done = Rc::new(Cell::new(false));
        let failure = Rc::new(RefCell::new(None::<String>));
        let done_for_callback = done.clone();
        let loop_for_done_callback = self.thread_loop.clone();
        let loop_for_error_callback = self.thread_loop.clone();
        let failure_for_callback = failure.clone();
        let listener = core
            .add_listener_local()
            .done(move |id, sequence| {
                if id == pw::core::PW_ID_CORE && sequence == pending {
                    done_for_callback.set(true);
                    loop_for_done_callback.signal(false);
                }
            })
            .error(move |_id, _sequence, result, message| {
                *failure_for_callback.borrow_mut() = Some(format!("{message} ({result})"));
                loop_for_error_callback.signal(false);
            })
            .register();

        while !done.get() && failure.borrow().is_none() {
            self.thread_loop.wait();
        }
        drop(listener);

        if let Some(error) = failure.borrow_mut().take() {
            return Err(BackendError::Native(format!(
                "PipeWire registry synchronization failed: {error}"
            )));
        }
        if !done.get() {
            return Err(BackendError::Native(
                "PipeWire registry synchronization ended unexpectedly".into(),
            ));
        }
        Ok(())
    }

    /// Run an operation with the thread loop locked, releasing the guard
    /// before returning so nested lock acquisition cannot deadlock.
    fn with_loop<T>(&mut self, op: impl FnOnce(&mut Self) -> BackendResult<T>) -> BackendResult<T> {
        let loop_for_op = self.thread_loop.clone();
        let _guard = loop_for_op.lock();
        op(self)
    }

    /// Synchronize with the daemon and rebuild the graph snapshot. The caller
    /// must hold the thread-loop lock.
    fn sync(&mut self) -> BackendResult<()> {
        self.roundtrip_locked()?;
        self.rebuild_graph_locked()
    }

    /// Run a bounded number of round-trips until `ready` reports the expected
    /// globals are visible. A single synchronization normally observes objects
    /// published by another client, but a bounded second pass covers the
    /// cross-client publication race without ever waiting in a loop. The
    /// caller must hold the thread-loop lock.
    fn wait_for_publication(
        &mut self,
        mut ready: impl FnMut(&mut Self) -> bool,
    ) -> BackendResult<()> {
        for _ in 0..2 {
            self.sync()?;
            if ready(self) {
                return Ok(());
            }
        }
        Ok(())
    }

    fn retain_failed_recorder(
        &mut self,
        id: RecorderId,
        mut instance: RecorderInstance,
        request: RecorderCreateRequest,
        message: impl Into<String>,
    ) {
        instance.status.state = RecorderState::Error;
        instance.status.writer_state = RecorderWriterState::Error;
        instance.status.error = Some(message.into());
        self.recorders.insert(
            id,
            NativeRecorder {
                request,
                instance,
                handle: None,
            },
        );
    }

    fn abort_recorder_restart(
        &mut self,
        id: RecorderId,
        instance: &RecorderInstance,
        request: &RecorderCreateRequest,
        error: BackendError,
    ) -> BackendError {
        self.recorders.remove(&id);
        let _ = self.roundtrip_locked();
        let _ = self.rebuild_graph_locked();
        self.retain_failed_recorder(id, instance.clone(), request.clone(), error.to_string());
        error
    }

    /// Recreate a finished recorder stream and keep its graph destination
    /// usable for another take. PipeWire stream writers are deliberately
    /// single-use: once the WAV worker has finalized, its sink cannot be
    /// swapped from outside the realtime callback. Replacing the stream on
    /// the control side gives the next take a fresh bounded sink while the
    /// stable Rust recorder ID, name, position, and incoming graph edges are
    /// retained.
    ///
    /// The caller must hold the ThreadLoop lock.
    fn restart_recorder_locked(
        &mut self,
        id: RecorderId,
        final_path: Option<std::path::PathBuf>,
        remove_old_path: bool,
    ) -> BackendResult<Option<String>> {
        let old = self
            .recorders
            .remove(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        let old_instance = old.instance.clone();
        let old_node_id = old.instance.node_id;
        let old_input = old.instance.input_port;
        let incoming: Vec<PortKey> = self
            .graph
            .links
            .values()
            .filter(|link| link.input_port == old_input)
            .filter_map(|link| self.graph.port_key(link.output_port))
            .collect();
        let name = old
            .handle
            .as_ref()
            .map(|handle| handle.name.clone())
            .unwrap_or_else(|| format!("{RECORDER_NODE_PREFIX} {id}"));
        let request = old.request.clone();
        let position = self
            .positions
            .get(&old_node_id)
            .copied()
            .unwrap_or(request.position);
        let old_path = old
            .handle
            .as_ref()
            .map(|handle| handle.writer.temporary_path().to_owned())
            .or_else(|| old.instance.status.temporary_path.clone());
        drop(old);

        // Let PipeWire observe destruction before publishing another stream
        // with the same node.name. This also removes links touching the old
        // global port from the next graph snapshot.
        if let Err(error) = self.roundtrip_locked() {
            return Err(self.abort_recorder_restart(id, &old_instance, &request, error));
        }
        if let Err(error) = self.rebuild_graph_locked() {
            return Err(self.abort_recorder_restart(id, &old_instance, &request, error));
        }

        let core = match self.core() {
            Ok(core) => core.clone(),
            Err(error) => {
                return Err(self.abort_recorder_restart(id, &old_instance, &request, error));
            }
        };
        let capacity = request.capacity_frames.max(
            (usize::try_from(request.format.sample_rate)
                .unwrap_or(48_000)
                .saturating_mul(DEFAULT_RECORDER_CAPACITY_MS as usize)
                / 1_000)
                .max(1),
        );
        let handle = match RecorderHandle::create(
            &core,
            name.clone(),
            request.format,
            request.recording_dir.as_deref(),
            capacity,
        ) {
            Ok(handle) => handle,
            Err(error) => {
                return Err(self.abort_recorder_restart(id, &old_instance, &request, error));
            }
        };
        let temporary_path = handle.writer.temporary_path().to_owned();
        self.recorders.insert(
            id,
            NativeRecorder {
                request: request.clone(),
                instance: RecorderInstance {
                    id,
                    node_id: NodeId(0),
                    input_port: PortId(0),
                    status: RecorderStatus {
                        id,
                        state: RecorderState::Idle,
                        writer_state: RecorderWriterState::Starting,
                        sample_rate: request.format.sample_rate,
                        channels: request.format.channels,
                        temporary_path: Some(temporary_path),
                        final_path,
                        queue_capacity: capacity,
                        ..RecorderStatus::default()
                    },
                },
                handle: Some(handle),
            },
        );

        let publication = self.wait_for_publication(|driver| {
            let Some(node) = driver.graph.nodes.values().find(|node| node.name == name) else {
                return false;
            };
            driver.graph.ports.values().any(|port| {
                port.node_id == node.id
                    && port.direction.is_sink()
                    && port.port_type == PortType::Audio
            })
        });
        if let Err(error) = publication {
            return Err(self.abort_recorder_restart(id, &old_instance, &request, error));
        }
        let (node_id, input_port) = {
            let Some(node) = self.graph.nodes.values().find(|node| node.name == name) else {
                return Err(self.abort_recorder_restart(
                    id,
                    &old_instance,
                    &request,
                    BackendError::native("PipeWire did not publish the restarted recorder node"),
                ));
            };
            let Some(input_port) = node.ports.iter().copied().find(|port_id| {
                self.graph.port(*port_id).is_some_and(|port| {
                    port.direction.is_sink() && port.port_type == PortType::Audio
                })
            }) else {
                return Err(self.abort_recorder_restart(
                    id,
                    &old_instance,
                    &request,
                    BackendError::native(
                        "PipeWire did not publish the restarted recorder input port",
                    ),
                ));
            };
            (node.id, input_port)
        };

        {
            let recorder = self
                .recorders
                .get_mut(&id)
                .expect("restarted recorder was inserted before publication");
            recorder.instance.node_id = node_id;
            recorder.instance.input_port = input_port;
        }
        self.positions.remove(&old_node_id);
        self.positions.insert(node_id, position);
        if let Some(node) = self.graph.nodes.get_mut(&node_id) {
            node.position = position;
        }

        // Stream destruction removes the native links. Recreate them from
        // stable source keys where the source is still present; a vanished
        // source is intentionally skipped and will not produce a phantom
        // connection in the graph.
        let mut warnings = Vec::new();
        for output_key in incoming {
            let Some(output) = self.graph.resolve_port_key(&output_key) else {
                warnings.push("a source disappeared before it could be reconnected".into());
                continue;
            };
            if let Err(error) = self.connect_locked(output, input_port) {
                warnings.push(format!("a source could not be reconnected: {error}"));
            }
        }
        if remove_old_path {
            if let Some(old_path) = old_path.filter(|path| path.exists()) {
                if let Err(error) = fs::remove_file(old_path) {
                    warnings.push(format!("temporary recording cleanup failed: {error}"));
                }
            }
        }
        Ok((!warnings.is_empty()).then(|| warnings.join("; ")))
    }

    /// Reattach backend-owned effect instances to the global IDs PipeWire is
    /// currently using. A `pw_filter` has a stable Rust-side instance ID, but
    /// its node and port globals are assigned asynchronously and may change
    /// while a client reconnects. The registry remains the source of truth for
    /// graph IDs, while the filter's unique friendly node name is the fallback
    /// identity until `pw_filter_get_node_id` is available.
    ///
    /// The caller must hold the ThreadLoop lock because `runtime_node_id`
    /// touches the raw `pw_filter` object.
    fn reconcile_effects_locked(&mut self) {
        let state = self.state.lock().unwrap().clone();
        let resolutions: Vec<(String, NodeId, PortId, PortId)> = self
            .effects
            .iter()
            .filter_map(|(instance_id, effect)| {
                let raw_node = effect.runtime_node_id().filter(|node_id| {
                    state
                        .nodes
                        .get(&native_node_id(*node_id))
                        .is_some_and(|record| record.name == effect.node_name())
                });
                let node_id = raw_node.or_else(|| {
                    state
                        .nodes
                        .iter()
                        .find(|(_, record)| record.name == effect.node_name())
                        .map(|(id, _)| NodeId(graph_id(*id as u64)))
                })?;
                let input_port = state
                    .ports
                    .iter()
                    .find(|(_, port)| {
                        port.node_id == native_node_id(node_id)
                            && port.direction.is_sink()
                            && (port.name == "input_FL" || port.name == "input_MONO")
                    })
                    .map(|(id, _)| PortId(graph_id(*id as u64)))?;
                let output_port = state
                    .ports
                    .iter()
                    .find(|(_, port)| {
                        port.node_id == native_node_id(node_id)
                            && port.direction.is_source()
                            && (port.name == "output_FL" || port.name == "output_MONO")
                    })
                    .map(|(id, _)| PortId(graph_id(*id as u64)))?;
                Some((instance_id.clone(), node_id, input_port, output_port))
            })
            .collect();

        for (instance_id, node_id, input_port, output_port) in resolutions {
            let Some(effect) = self.effects.get_mut(&instance_id) else {
                continue;
            };
            let old_node_id = effect.instance.node_id;
            let position = self
                .positions
                .get(&old_node_id)
                .copied()
                .unwrap_or_else(|| effect.position());
            effect.set_identity(node_id, input_port, output_port);
            effect.set_position(position);
            if old_node_id != node_id {
                self.positions.remove(&old_node_id);
            }
            self.positions.insert(node_id, position);
        }
    }

    fn build_graph_from_state(&mut self, state: RegistryState) -> BackendResult<Graph> {
        let mut graph = Graph::default();
        let mut node_media_classes = HashMap::new();
        let clients = &state.clients;
        let devices = &state.devices;
        let effect_nodes: HashMap<NodeId, String> = self
            .effects
            .values()
            .filter(|effect| effect.resolved())
            .map(|effect| {
                (
                    effect.instance.node_id,
                    effect.instance.config.instance_id.clone(),
                )
            })
            .collect();

        for (id, record) in state.nodes.iter() {
            let node_id = NodeId(graph_id(*id as u64));
            if record.name.starts_with(METER_NODE_PREFIX) {
                continue;
            }
            node_media_classes.insert(node_id, record.media_class.to_ascii_lowercase());
            let effect_instance_id = effect_nodes.get(&node_id);
            let recorder = self.recorders.values().find(|recorder| {
                recorder
                    .handle
                    .as_ref()
                    .is_some_and(|handle| handle.name == record.name)
            });
            let client = record
                .client_id
                .and_then(|client_id| clients.get(&client_id));
            let icon_name = record
                .icon_name
                .clone()
                .or_else(|| {
                    record.device_id.and_then(|device_id| {
                        devices
                            .get(&device_id)
                            .and_then(|device| device.icon_name.clone())
                    })
                })
                .or_else(|| client.and_then(|client| client.icon_name.clone()))
                // Some clients publish the icon only as process metadata.
                // It uses the same XDG naming convention in practice and is
                // a useful fallback for applications such as Firefox.
                .or_else(|| record.process_binary.clone())
                .or_else(|| client.and_then(|client| client.process_binary.clone()))
                .or_else(|| {
                    record
                        .application_id
                        .as_deref()
                        .and_then(|application_id| application_id.rsplit(['.', '/']).next())
                        .map(str::to_owned)
                });
            let mut node = Node::new(
                node_id,
                &record.name,
                if recorder.is_some() {
                    NodeType::Recorder
                } else if effect_instance_id.is_some() {
                    NodeType::Effect
                } else {
                    NodeType::PipeWire
                },
            );
            let identity = NodeIdentity {
                application_id: record
                    .application_id
                    .clone()
                    .or_else(|| client.and_then(|client| client.application_id.clone())),
                application_name: record
                    .application_name
                    .clone()
                    .or_else(|| client.and_then(|client| client.application_name.clone())),
                process_binary: record
                    .process_binary
                    .clone()
                    .or_else(|| client.and_then(|client| client.process_binary.clone())),
                node_name: record.name.clone(),
                description: record.description.clone(),
                media_role: record.media_role.clone(),
                media_name: record.media_name.clone(),
                client_name: record
                    .client_name
                    .clone()
                    .or_else(|| client.and_then(|client| client.client_name.clone())),
                client_api: record
                    .client_api
                    .clone()
                    .or_else(|| client.and_then(|client| client.client_api.clone())),
                client_id: record.client_id,
                object_path: record.object_path.clone(),
                object_serial: record.serial,
                effect_instance_id: effect_instance_id.cloned(),
            };
            node = node.with_identity(identity);
            if let Some(icon_name) = icon_name {
                node = node.with_icon_name(icon_name);
            }
            if let Some(serial) = record.serial {
                node = node.with_serial(serial);
            }
            if let Some(instance_id) = effect_instance_id {
                node = node.with_effect_instance(instance_id.clone());
            }
            node.position = self.positions.get(&node_id).copied().unwrap_or([0.0, 0.0]);
            graph.add_node(node)?;
        }

        for (id, record) in state.ports {
            let node_id = NodeId(graph_id(record.node_id as u64));
            if graph.node(node_id).is_none() {
                continue;
            }
            let port_type = classify_port_type(
                &record.media_type,
                node_media_classes.get(&node_id).map(String::as_str),
            );
            let port = Port::new(
                PortId(graph_id(id as u64)),
                node_id,
                record.name,
                record.direction,
                port_type,
            );
            let port = match record.channel {
                Some(channel) => port.with_channel(channel),
                None => port,
            };
            graph.add_port(port)?;
        }

        for (id, record) in state.links {
            let _ = graph.insert_existing_link(Link {
                id: LinkId(graph_id(id as u64)),
                output_port: PortId(graph_id(record.output_port as u64)),
                input_port: PortId(graph_id(record.input_port as u64)),
            });
        }

        // Compute defaults after links are in the graph. Otherwise the first
        // refresh has no topology for the layered layout to follow and every
        // node falls back to its port-role column until the next arrange.
        let default_positions = graph.default_node_positions();
        for (node_id, node) in &mut graph.nodes {
            if let Some(position) = self.positions.get(node_id).copied() {
                node.position = position;
            } else if let Some(position) = default_positions.get(node_id).copied() {
                node.position = position;
                self.positions.insert(*node_id, position);
            }
        }

        Ok(graph)
    }

    fn rebuild_graph_locked(&mut self) -> BackendResult<()> {
        // A session manager can race us by recreating a deleted link while a
        // stream resumes. Bound the cleanup passes so a broken policy cannot
        // make a refresh loop forever.
        for pass in 0..3 {
            self.reconcile_effects_locked();
            let state = self.state.lock().unwrap().clone();
            self.graph = self.build_graph_from_state(state)?;
            let suppressed: Vec<LinkId> = self
                .graph
                .links
                .values()
                .filter(|link| self.connection_is_blocked(link))
                .map(|link| link.id)
                .collect();
            if suppressed.is_empty() || pass == 2 {
                // Volume and mute are read back here so a change made in
                // pavucontrol or with a media key reaches the cards.
                self.refresh_effect_channel_masks_locked();
                self.read_node_controls_locked();
                // The node proxy's Info callback carries the complete
                // application/device property dictionary. Rebuild once more
                // so metadata discovered during the readback is visible in
                // this graph snapshot rather than waiting for the next poll.
                let state = self.state.lock().unwrap().clone();
                self.graph = self.build_graph_from_state(state)?;
                self.ensure_meters_locked();
                return Ok(());
            }
            for link_id in suppressed {
                self.registry()?
                    .destroy_global(native_link_id(link_id))
                    .into_result()
                    .map_err(|error| native_error("PipeWire suppressed link destruction", error))?;
            }
            self.roundtrip_locked()?;
        }
        Ok(())
    }

    /// Publish graph topology to each live effect through its atomic
    /// realtime control.  This is deliberately derived from stable registry
    /// links, not from the presence of a DSP buffer in the most recent
    /// callback.  An Auto effect may expose two physical ports while only the
    /// linked input channels are active, so the mask also controls expensive
    /// per-channel work such as Hush inference.
    fn refresh_effect_channel_masks_locked(&self) {
        let updates: Vec<(String, u16)> = self
            .effects
            .iter()
            .map(|(instance_id, effect)| {
                let mask = if effect.resolved() {
                    active_channel_mask(
                        &self.graph,
                        effect.instance.node_id,
                        effect.physical_channels(),
                    )
                } else {
                    0
                };
                (instance_id.clone(), mask)
            })
            .collect();

        for (instance_id, mask) in updates {
            if let Some(effect) = self.effects.get(&instance_id) {
                effect.set_active_channel_mask(mask);
            }
        }
    }

    fn port_keys_equal(left: &PortKey, right: &PortKey) -> bool {
        left.node_name == right.node_name
            && left.node_type == right.node_type
            && left.port_name == right.port_name
            && (left.channel.is_none() || right.channel.is_none() || left.channel == right.channel)
            && left.direction == right.direction
            && left.port_type == right.port_type
    }

    fn is_blocked_pair(&self, output: &PortKey, input: &PortKey) -> bool {
        self.blocked_connections
            .iter()
            .any(|(blocked_output, blocked_input)| {
                Self::port_keys_equal(blocked_output, output)
                    && Self::port_keys_equal(blocked_input, input)
            })
    }

    fn connection_is_blocked(&self, link: &Link) -> bool {
        let Some(output) = self.graph.port_key(link.output_port) else {
            return false;
        };
        let Some(input) = self.graph.port_key(link.input_port) else {
            return false;
        };
        self.is_blocked_pair(&output, &input)
    }

    fn allow_blocked_connection(&mut self, output: &PortKey, input: &PortKey) {
        self.blocked_connections
            .retain(|(blocked_output, blocked_input)| {
                !(Self::port_keys_equal(blocked_output, output)
                    && Self::port_keys_equal(blocked_input, input))
            });
    }

    fn block_connection(&mut self, link: &Link) {
        let (Some(output), Some(input)) = (
            self.graph.port_key(link.output_port),
            self.graph.port_key(link.input_port),
        ) else {
            return;
        };
        if !self.is_blocked_pair(&output, &input) {
            self.blocked_connections.push((output, input));
        }
    }

    /// Nodes that can be measured.
    ///
    /// Audio source ports are read directly; playback sinks are read through
    /// their monitor, which `create_meter_locked` already arranges with
    /// `stream.capture.sink`. The rule itself lives in [`crate::api`] so it can
    /// be unit-tested without a PipeWire daemon.
    /// Whether one node can be measured, without enumerating every node.
    /// `node_capabilities` runs once per node per UI sync, so building the
    /// whole set there made each sync quadratic.
    fn is_node_measurable(&self, node_id: NodeId) -> bool {
        let Some(node) = self.graph.nodes.get(&node_id) else {
            return false;
        };
        let mut has_source = false;
        let mut has_sink = false;
        for port_id in &node.ports {
            let Some(port) = self.graph.port(*port_id) else {
                continue;
            };
            if port.port_type != PortType::Audio {
                continue;
            }
            has_source |= port.direction.is_source();
            has_sink |= port.direction.is_sink();
        }
        let state = self.state.lock().unwrap();
        let media_class = state
            .nodes
            .get(&native_node_id(node_id))
            .map(|record| record.media_class.as_str())
            .unwrap_or_default();
        is_measurable_audio_node(media_class, has_source, has_sink)
    }

    fn measurable_nodes(&self) -> BTreeSet<NodeId> {
        let state = self.state.lock().unwrap().clone();
        self.graph
            .nodes
            .values()
            .filter(|node| {
                let mut has_source = false;
                let mut has_sink = false;
                for port_id in &node.ports {
                    let Some(port) = self.graph.port(*port_id) else {
                        continue;
                    };
                    if port.port_type != PortType::Audio {
                        continue;
                    }
                    has_source |= port.direction.is_source();
                    has_sink |= port.direction.is_sink();
                }
                let media_class = state
                    .nodes
                    .get(&native_node_id(node.id))
                    .map(|record| record.media_class.as_str())
                    .unwrap_or_default();
                is_measurable_audio_node(media_class, has_source, has_sink)
            })
            .map(|node| node.id)
            .collect()
    }

    /// Nodes that should currently own a metering stream.
    ///
    /// Under [`MeterPolicy::OnDemand`] this is driven purely by what the UI
    /// asked for, so a minimized window releases streams after the linger
    /// period and stops nudging the daemon's audio devices.
    fn wanted_meter_nodes(&self) -> BTreeSet<NodeId> {
        let requested: BTreeSet<NodeId> = self.meter_requests.keys().copied().collect();
        nodes_to_meter(self.meter_policy, &self.measurable_nodes(), &requested)
    }

    /// Number of live metering streams. Tests use this to prove that a plain
    /// launch attaches nothing to the user's audio graph.
    #[cfg(test)]
    pub(crate) fn active_meter_count(&self) -> usize {
        self.meters.len()
    }

    fn elapsed_ms(&self) -> u64 {
        metering::elapsed_ms_since(self.epoch)
    }

    /// Drop request entries that have outlived [`METER_LINGER`].
    fn expire_meter_requests(&mut self) {
        let now = Instant::now();
        self.meter_requests
            .retain(|_, requested_at| now.saturating_duration_since(*requested_at) < METER_LINGER);
    }

    fn ensure_meters_locked(&mut self) {
        self.expire_meter_requests();
        let wanted = self.wanted_meter_nodes();
        self.meters.retain(|node_id, _| wanted.contains(node_id));

        let missing: Vec<(NodeId, NodeRecord)> = {
            let state = self.state.lock().unwrap().clone();
            wanted
                .into_iter()
                .filter(|node_id| !self.meters.contains_key(node_id))
                .filter_map(|node_id| {
                    state
                        .nodes
                        .get(&native_node_id(node_id))
                        .map(|record| (node_id, record.clone()))
                })
                .collect()
        };
        for (node_id, record) in missing {
            if let Ok(handle) = self.create_meter_locked(node_id, &record) {
                self.meters.insert(node_id, handle);
            }
        }
    }

    fn create_meter_locked(
        &self,
        node_id: NodeId,
        record: &NodeRecord,
    ) -> BackendResult<MeterHandle> {
        let core = self.core()?.clone();
        // Node names are not unique; the daemon-assigned serial is. Falling back
        // to the name only matters for objects that predate `object.serial`.
        let target = record
            .serial
            .map(|serial| serial.to_string())
            .unwrap_or_else(|| record.name.clone());
        let stream_name = format!("{METER_NODE_PREFIX} {}", node_id.0);
        let description = format!("Level meter: {}", record.name);
        let mut properties = pw::properties::properties! {
            NODE_NAME => stream_name.as_str(),
            NODE_DESCRIPTION => description.as_str(),
            MEDIA_TYPE => MEDIA_TYPE_AUDIO,
            "media.category" => "Capture",
            "media.role" => "DSP",
            "media.class" => "Stream/Input/Audio",
            // Tells the session manager this client only observes. Monitoring
            // streams are excluded from routing decisions such as switching the
            // default device or counting active streams on a node.
            "stream.monitor" => "true",
            // Passive links never make our stream a driver and never keep the
            // target awake, so devices can still suspend while we are attached.
            "node.passive" => "true",
            // Never let the session manager move us to another node; without
            // this a meter can silently follow the default device instead.
            "node.dont-reconnect" => "true",
            "stream.dont-remix" => "true",
            "target.object" => target.as_str(),
        };
        // A capture stream aimed at a sink must be told to read that sink's
        // monitor ports. Otherwise the session manager treats it as an ordinary
        // recording client and routes it to the default *source* -- which would
        // open the user's microphone instead of metering the sink.
        if record.media_class.to_ascii_lowercase().contains("sink") {
            properties.insert("stream.capture.sink", "true");
        }
        let stream = pw::stream::Stream::new(&core, &stream_name, properties)
            .map_err(|error| native_error("PipeWire audio meter stream creation", error))?;
        let shared = Arc::new(MeterReadingState::default());
        let listener = stream
            .add_local_listener_with_user_data(MeterCallbackState {
                shared: shared.clone(),
                epoch: self.epoch,
            })
            .state_changed(|_, data, _old, new| {
                data.shared.connected.store(
                    matches!(new, pw::stream::StreamState::Streaming),
                    Ordering::Relaxed,
                );
            })
            .param_changed(|_stream, data, id, param| {
                if id != ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else {
                    return;
                };
                let mut format = AudioInfoRaw::new();
                if format.parse(param).is_ok() {
                    data.shared
                        .format
                        .store(format.format().as_raw(), Ordering::Relaxed);
                }
            })
            .process(process_meter_buffer)
            .register()
            .map_err(|error| native_error("PipeWire audio meter listener", error))?;

        let pod_bytes = audio_format_pod()?;
        let pod = Pod::from_bytes(&pod_bytes).ok_or_else(|| {
            BackendError::Native("could not serialize PipeWire audio meter format".into())
        })?;
        let mut params = [pod];
        stream
            .connect(
                SpaDirection::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS
                    | pw::stream::StreamFlags::DONT_RECONNECT,
                &mut params,
            )
            .map_err(|error| native_error("PipeWire audio meter stream connection", error))?;

        Ok(MeterHandle {
            _stream: stream,
            _listener: listener,
            shared,
        })
    }

    fn create_effect_node_prepared_locked(
        &mut self,
        request: EffectNodeRequest,
        prepared: PreparedEffect,
    ) -> BackendResult<EffectInstance> {
        if self.effects.contains_key(&request.instance_id) {
            return Err(BackendError::effect_already_exists(&request.instance_id));
        }
        let instance_id = request.instance_id.clone();
        let effect = NativeEffect::activate(&self.thread_loop, request, prepared)?;
        self.finish_create_effect_node_locked(instance_id, effect)
    }

    fn finish_create_effect_node_locked(
        &mut self,
        instance_id: String,
        effect: NativeEffect,
    ) -> BackendResult<EffectInstance> {
        self.effects.insert(instance_id.clone(), effect);

        let result = (|| {
            // `pw_filter_new_simple` owns a small client connection of its
            // own. One round-trip from this driver's core normally observes
            // its globals, but a bounded second synchronization covers the
            // cross-client publication race without ever waiting in a loop.
            self.wait_for_publication(|driver| {
                let Some(effect) = driver.effects.get(&instance_id) else {
                    return false;
                };
                effect.resolved()
                    && driver.graph.node(effect.instance.node_id).is_some()
                    && driver.graph.port(effect.instance.input_port).is_some()
                    && driver.graph.port(effect.instance.output_port).is_some()
            })?;
            let effect = self.effects.get(&instance_id).ok_or_else(|| {
                BackendError::native("new PipeWire effect disappeared during creation")
            })?;
            if effect.resolved() {
                return Ok(effect.snapshot());
            }
            Err(BackendError::native(
                "PipeWire did not publish the effect node and both DSP ports",
            ))
        })();

        if result.is_err() {
            // Dropping the raw filter removes its links/ports. Best-effort
            // synchronization prevents a failed creation from lingering as an
            // unclassified node in the next UI frame.
            if let Some(effect) = self.effects.remove(&instance_id) {
                self.positions.remove(&effect.instance.node_id);
                drop(effect);
            }
            let _ = self.roundtrip_locked();
            let _ = self.rebuild_graph_locked();
        }
        result
    }

    fn effect_link_endpoints_locked(
        &self,
        source: &PortKey,
        destination: &PortKey,
    ) -> BackendResult<(PortId, PortId, Link)> {
        let (output, input, link) = self.effect_link_endpoints(source, destination)?;
        let output_port = self
            .graph
            .port(output)
            .ok_or(GraphError::MissingPort(output))?;
        let input_port = self
            .graph
            .port(input)
            .ok_or(GraphError::MissingPort(input))?;
        if output_port.port_type != PortType::Audio || input_port.port_type != PortType::Audio {
            return Err(BackendError::native(
                "PipeWire effects can only be inserted into audio links",
            ));
        }
        Ok((output, input, link))
    }

    /// Remove only the filter node. PipeWire removes all links touching a
    /// destroyed filter, so this is the rollback primitive for a failed insert
    /// as well as the standalone-node removal path.
    fn destroy_effect_node_locked(&mut self, instance_id: &str) -> BackendResult<EffectInstance> {
        let effect = self
            .effects
            .remove(instance_id)
            .ok_or_else(|| BackendError::unknown_effect_instance(instance_id))?;
        let snapshot = effect.snapshot();
        self.positions.remove(&snapshot.node_id);
        drop(effect);
        self.roundtrip_locked()?;
        self.rebuild_graph_locked()?;
        Ok(snapshot)
    }

    /// Restore a direct connection after an inserted filter has gone away.
    /// A user may already have recreated it manually, in which case keeping
    /// that link is the successful, idempotent result.
    fn restore_direct_connection_locked(
        &mut self,
        output: PortId,
        input: PortId,
    ) -> BackendResult<()> {
        if self
            .graph
            .links
            .values()
            .any(|link| link.output_port == output && link.input_port == input)
        {
            if let (Some(output_key), Some(input_key)) =
                (self.graph.port_key(output), self.graph.port_key(input))
            {
                self.allow_blocked_connection(&output_key, &input_key);
            }
            return Ok(());
        }
        self.connect_locked(output, input)?;
        Ok(())
    }

    fn insert_effect_prepared_locked(
        &mut self,
        request: EffectInsertRequest,
        prepared: PreparedEffect,
    ) -> BackendResult<EffectInstance> {
        let source = request.source.clone();
        let destination = request.destination.clone();
        let EffectInsertRequest {
            instance_id,
            effect_id,
            module_path,
            enabled,
            parameters,
            channel_policy,
            position,
            ..
        } = request;
        self.effect_link_endpoints_locked(&source, &destination)?;
        let instance_request = EffectNodeRequest {
            instance_id: instance_id.clone(),
            effect_id,
            module_path,
            enabled,
            parameters,
            channel_policy,
            position,
        };
        let instance = self.create_effect_node_prepared_locked(instance_request, prepared)?;
        self.commit_insert_effect_locked(source, destination, instance_id, instance)
    }

    fn commit_insert_effect_locked(
        &mut self,
        source: PortKey,
        destination: PortKey,
        instance_id: String,
        instance: EffectInstance,
    ) -> BackendResult<EffectInstance> {
        let result = (|| {
            let (output, input, direct_link) =
                self.effect_link_endpoints_locked(&source, &destination)?;
            self.disconnect_locked(direct_link.id)?;
            self.connect_locked(output, instance.input_port)?;
            self.connect_locked(instance.output_port, input)?;
            let effect = self.effects.get_mut(&instance_id).ok_or_else(|| {
                BackendError::native("effect disappeared while committing insertion")
            })?;
            effect.instance.source = Some(source.clone());
            effect.instance.destination = Some(destination.clone());
            Ok(effect.snapshot())
        })();

        if let Err(error) = result {
            // The direct link is restored after filter destruction. This also
            // cleans up either half of a partially connected insertion.
            let cleanup = self.destroy_effect_node_locked(&instance_id);
            let restore = (|| {
                let (output, input) = self.effect_restore_endpoints(&source, &destination)?;
                self.restore_direct_connection_locked(output, input)
            })();

            // Both rollback directions are independent. Preserve every
            // failure so a caller can distinguish a cleanly rolled-back
            // operation from a graph that still contains a partial effect.
            let mut rollback_errors = Vec::new();
            if let Err(restore_error) = restore {
                rollback_errors.push(format!(
                    "failed to restore the original link: {restore_error}"
                ));
            }
            if let Err(cleanup_error) = cleanup {
                rollback_errors.push(format!(
                    "failed to clean up the effect node: {cleanup_error}"
                ));
            }
            if rollback_errors.is_empty() {
                return Err(error);
            }
            return Err(BackendError::Native(format!(
                "{error}; additionally, {}",
                rollback_errors.join("; ")
            )));
        }
        result
    }

    fn begin_create_effect_async(
        &mut self,
        request: EffectCreateRequest,
    ) -> BackendResult<EffectTicket> {
        if request.instance_id.trim().is_empty() {
            return Err(BackendError::native("effect instance id cannot be empty"));
        }
        if request.effect_id.trim().is_empty() {
            return Err(BackendError::native("effect id cannot be empty"));
        }
        if self.effects.contains_key(&request.instance_id)
            || self
                .pending_effects
                .values()
                .any(|pending| pending.request.instance_id == request.instance_id)
        {
            return Err(BackendError::effect_already_exists(&request.instance_id));
        }

        let topology_channels = match &request.target {
            EffectTarget::Standalone { .. } => None,
            EffectTarget::Insert {
                source,
                destination,
                ..
            } => self.with_loop(|driver| {
                driver.sync()?;
                driver
                    .effect_link_endpoints_locked(source, destination)
                    .map(|_| driver.inferred_audio_channels(source))
            })?,
        };
        let channels = self
            .effect_host
            .physical_channels_for_request(
                &request.effect_id,
                request.module_path.as_deref(),
                request.channel_policy,
                topology_channels,
            )
            .map_err(BackendError::native)?;
        let ticket = self
            .effect_loader
            .begin_prepare(
                &self.effect_host,
                EffectPrepareRequest {
                    effect_id: request.effect_id.clone(),
                    module_path: request.module_path.clone(),
                    spec: AudioSpec {
                        sample_rate: effects::PREPARED_SAMPLE_RATE,
                        channels,
                        max_frames: effects::MAX_DSP_FRAMES,
                    },
                    parameters: request.parameters.clone(),
                    cancellation: pw_graph_effects::EffectCancellation::default(),
                },
            )
            .map_err(BackendError::native)?;
        self.pending_effects
            .insert(ticket, PendingEffect { request });
        Ok(ticket)
    }

    fn activate_pending_effect(
        &mut self,
        ticket: EffectTicket,
        pending: PendingEffect,
        prepared: PreparedEffect,
    ) -> BackendResult<EffectInstance> {
        match pending.request.target {
            EffectTarget::Standalone { position } => {
                let request = EffectNodeRequest {
                    instance_id: pending.request.instance_id,
                    effect_id: pending.request.effect_id,
                    module_path: pending.request.module_path,
                    enabled: pending.request.enabled,
                    parameters: pending.request.parameters,
                    channel_policy: pending.request.channel_policy,
                    position,
                };
                self.with_loop(|driver| {
                    driver.sync()?;
                    driver.create_effect_node_prepared_locked(request, prepared)
                })
            }
            EffectTarget::Insert {
                source,
                destination,
                position,
            } => {
                let request = EffectInsertRequest {
                    instance_id: pending.request.instance_id,
                    effect_id: pending.request.effect_id,
                    module_path: pending.request.module_path,
                    source,
                    destination,
                    enabled: pending.request.enabled,
                    parameters: pending.request.parameters,
                    channel_policy: pending.request.channel_policy,
                    position,
                };
                self.with_loop(|driver| {
                    driver.sync()?;
                    driver.insert_effect_prepared_locked(request, prepared)
                })
            }
        }
        .map_err(|error| {
            BackendError::Native(format!(
                "effect ticket {} could not be activated: {error}",
                ticket.0
            ))
        })
    }

    fn poll_effect_lifecycle(&mut self) -> BackendResult<Vec<EffectEvent>> {
        let preparation_events = self.effect_loader.poll_events();
        let mut events = Vec::with_capacity(preparation_events.len());
        for event in preparation_events {
            match event {
                EffectPreparationEvent::Loading { ticket, stage } => {
                    events.push(EffectEvent::Loading { ticket, stage });
                }
                EffectPreparationEvent::Ready { ticket, prepared } => {
                    let Some(pending) = self.pending_effects.remove(&ticket) else {
                        // A cancelled/removed request may finish racing with
                        // the control poll. Its prepared processor is dropped
                        // here and can never mutate the graph.
                        continue;
                    };
                    match self.activate_pending_effect(ticket, pending, prepared) {
                        Ok(instance) => events.push(EffectEvent::Ready {
                            ticket,
                            instance: Box::new(instance),
                        }),
                        Err(error) => events.push(EffectEvent::Failed {
                            ticket,
                            error: error.to_string(),
                        }),
                    }
                }
                EffectPreparationEvent::Failed { ticket, error } => {
                    self.pending_effects.remove(&ticket);
                    events.push(EffectEvent::Failed {
                        ticket,
                        error: error.to_string(),
                    });
                }
                EffectPreparationEvent::Cancelled { ticket } => {
                    self.pending_effects.remove(&ticket);
                    events.push(EffectEvent::Cancelled { ticket });
                }
            }
        }
        Ok(events)
    }

    /// Infer the selected source's connected audio width for generic effect
    /// negotiation.  The effect provider decides whether that width is
    /// supported; the backend does not branch on a concrete effect ID.
    fn inferred_audio_channels(&self, source: &PortKey) -> Option<u16> {
        if source.port_type != PortType::Audio
            || source.node_type != NodeType::PipeWire && source.node_type != NodeType::Effect
        {
            return None;
        }
        let source_id = self.graph.resolve_port_key(source)?;
        let source_port = self.graph.port(source_id)?;
        if source_port.channel.as_deref().is_some_and(|channel| {
            channel.eq_ignore_ascii_case("mono")
                || channel.eq_ignore_ascii_case("fc")
                || channel.eq_ignore_ascii_case("center")
        }) {
            return Some(1);
        }
        let node = self.graph.node(source_port.node_id)?;
        let audio_outputs = node
            .ports
            .iter()
            .filter_map(|id| self.graph.port(*id))
            .filter(|port| port.direction == Direction::Source && port.port_type == PortType::Audio)
            .count();
        Some(if audio_outputs <= 1 { 1 } else { 2 })
    }

    fn remove_effect_locked(&mut self, instance_id: &str) -> BackendResult<()> {
        let instance = self
            .effects
            .get(instance_id)
            .ok_or_else(|| BackendError::unknown_effect_instance(instance_id))?
            .snapshot();
        let endpoints = match (&instance.source, &instance.destination) {
            (Some(source), Some(destination)) => {
                // Refuse to destroy an inserted effect if the persisted
                // endpoints have already vanished; otherwise its original
                // routing could not honestly be restored.
                Some(self.effect_restore_endpoints(source, destination)?)
            }
            (None, None) => None,
            _ => return Err(BackendError::effect_routing_incomplete()),
        };

        self.destroy_effect_node_locked(instance_id)?;
        if let Some((output, input)) = endpoints {
            if let Err(error) = self.restore_direct_connection_locked(output, input) {
                return Err(BackendError::Native(format!(
                    "effect {instance_id} was removed but the original direct connection could not be restored: {error}"
                )));
            }
        }
        Ok(())
    }
}

impl Drop for PipewireDriver {
    fn drop(&mut self) {
        let guard = self.thread_loop.lock();
        self.meters.clear();
        self.recorders.clear();
        // Each raw `pw_filter` owns callbacks on this loop, so destroy them
        // before releasing the registry/core that created their globals.
        #[cfg(all(target_os = "linux", feature = "relay"))]
        self.relay.take();
        self.effects.clear();
        self.registry_listener.take();
        self.metadata_listener.take();
        self.metadata_bindings.borrow_mut().clear();
        self.registry.take();
        self.core.take();
        self.context.take();
        drop(guard);
        self.thread_loop.stop();
    }
}

impl GraphDriver for PipewireDriver {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            topology: true,
            connect: true,
            disconnect: true,
            volume: true,
            mute: true,
            meters: true,
            effects: true,
            relay: cfg!(feature = "relay"),
            recorders: true,
        }
    }

    fn refresh(&mut self) -> BackendResult<Vec<Node>> {
        self.with_loop(|driver| {
            driver.sync()?;
            #[cfg(all(target_os = "linux", feature = "relay"))]
            if let Some(mode) = driver.relay.as_ref().and_then(|set| set.local_router.mode) {
                // Default-device metadata and endpoint topology share the
                // registry dirty bit. Reconcile here so a WirePlumber
                // default change moves only the qpwgraph-owned automatic
                // route; ordinary user links remain untouched.
                driver.ensure_relay_local_route_locked(mode)?;
            }
            driver.registry_dirty.store(false, Ordering::Relaxed);
            Ok(driver.graph.nodes.values().cloned().collect())
        })
    }

    fn connect(&mut self, src: PortId, dst: PortId) -> BackendResult<Link> {
        self.with_loop(|driver| {
            driver.sync()?;
            driver.connect_locked(src, dst)
        })
    }

    fn disconnect(&mut self, link: LinkId) -> BackendResult<Link> {
        self.with_loop(|driver| {
            driver.sync()?;
            driver.disconnect_locked(link)
        })
    }

    fn allow_connection(&mut self, output: &PortKey, input: &PortKey) {
        self.allow_blocked_connection(output, input);
    }

    fn suppress_connection(&mut self, output: &PortKey, input: &PortKey) {
        if !self.is_blocked_pair(output, input) {
            self.blocked_connections
                .push((output.clone(), input.clone()));
        }
    }

    fn set_node_position(&mut self, node: NodeId, position: [f32; 2]) -> BackendResult<()> {
        if !self.graph.nodes.contains_key(&node) {
            return Err(GraphError::MissingNode(node).into());
        }
        self.positions.insert(node, position);
        if let Some(effect) = self
            .effects
            .values_mut()
            .find(|effect| effect.instance.node_id == node)
        {
            effect.set_position(position);
        }
        self.graph
            .nodes
            .get_mut(&node)
            .expect("node checked above")
            .position = position;
        Ok(())
    }

    fn set_node_mute(&mut self, node: NodeId, muted: bool) -> BackendResult<()> {
        if !self.graph.nodes.contains_key(&node) {
            return Err(GraphError::MissingNode(node).into());
        }
        // One round-trip per control write, inside `set_node_props_locked`.
        // A second one here made every fader/mute tick wait for PipeWire
        // twice, which is what made dragged sliders feel sticky.
        self.with_loop(|driver| driver.set_node_mute_locked(node, muted))
    }

    fn set_node_volume(&mut self, node: NodeId, volume: f32) -> BackendResult<()> {
        if !self.graph.nodes.contains_key(&node) {
            return Err(GraphError::MissingNode(node).into());
        }
        // See `set_node_mute`: the write itself round-trips, so a second
        // pre-write round-trip only doubled slider latency.
        self.with_loop(|driver| driver.set_node_volume_locked(node, volume))
    }

    /// Audio state for one node.
    ///
    /// Both controls are writable, and both are read back from the node''s
    /// `Props` during each graph rebuild. A node that has never answered stays
    /// `None`, so the UI shows "not read" rather than inventing a level.
    fn node_audio_state(&self, node: NodeId) -> BackendResult<NodeAudioState> {
        let record = self
            .graph
            .nodes
            .get(&node)
            .ok_or(GraphError::MissingNode(node))?;
        if matches!(record.node_type, NodeType::Effect | NodeType::Recorder) {
            return Ok(NodeAudioState::UNSUPPORTED);
        }
        // The `relay` field only exists with the `relay` feature; without it
        // `is_relay_device_node` always returns false.
        #[cfg(all(target_os = "linux", feature = "relay"))]
        if is_relay_device_node(&record.name) {
            // Relay nodes expose application-local gain (0.0..2.0) instead of
            // PipeWire Props. This mirrors the node-audio slider/mute that
            // Firefox/WEBRTC nodes have, but without touching the system sink
            // volume.
            if let Some(relay) = self.relay.as_ref() {
                let gain = relay.playback_shared.gain();
                let muted = relay.playback_shared.muted();
                return Ok(NodeAudioState {
                    volume: Some(gain),
                    muted: Some(muted),
                    volume_readable: true,
                    volume_writable: true,
                    mute_readable: true,
                    mute_writable: true,
                });
            }
            return Ok(NodeAudioState::UNSUPPORTED);
        }
        #[cfg(not(all(target_os = "linux", feature = "relay")))]
        if is_relay_device_node(&record.name) {
            return Ok(NodeAudioState::UNSUPPORTED);
        }
        let known = self.audio_controls.get(&node).copied().unwrap_or_default();
        Ok(NodeAudioState {
            volume: known.volume,
            muted: known.muted,
            volume_readable: known.volume_readable,
            volume_writable: true,
            mute_readable: known.mute_readable,
            mute_writable: true,
        })
    }

    /// PipeWire accepts gain above unity, which `set_node_volume_locked`
    /// clamps at 1.5, so the fader is allowed the same boost range.
    fn node_capabilities(&self, node: NodeId) -> NodeCapabilities {
        let Ok(state) = self.node_audio_state(node) else {
            return NodeCapabilities::NONE;
        };
        let mut capabilities = state.control_capabilities();
        // Relay nodes use linear 0..2.0 (200%) gain, not the PipeWire cubic curve.
        if self
            .graph
            .node(node)
            .is_some_and(|n| is_relay_device_node(&n.name))
        {
            capabilities.volume_max = 2.0;
            capabilities.meter_peak = true;
            capabilities.meter_rms = true;
            return capabilities;
        }
        if self.is_node_measurable(node) {
            capabilities.volume_max = PIPEWIRE_MAX_VOLUME;
            capabilities.meter_peak = true;
            capabilities.meter_rms = true;
        }
        capabilities
    }

    fn graph(&self) -> &Graph {
        &self.graph
    }

    fn graph_dirty(&self) -> bool {
        self.registry_dirty.load(Ordering::Relaxed)
    }

    /// The registry listener fires for every global added or removed, so the
    /// dirty flag covers every topology change.
    fn reports_graph_changes(&self) -> bool {
        true
    }

    fn is_node_type(&self, node_type: NodeType) -> bool {
        matches!(
            node_type,
            NodeType::PipeWire | NodeType::Effect | NodeType::Recorder
        )
    }

    fn is_port_type(&self, port_type: PortType) -> bool {
        matches!(
            port_type,
            PortType::Audio | PortType::Video | PortType::MidiJack | PortType::Unknown
        )
    }

    fn audio_meters(&mut self) -> BackendResult<Vec<AudioMeter>> {
        // Readings are published through atomics by the realtime data thread and
        // `self.meters` is only ever mutated from this thread, so no lock is
        // needed. Taking the thread-loop lock here would stall the loop on every
        // UI frame, and a registry round-trip would cost a full core sync for
        // data the sync does not affect.
        let now_ms = self.elapsed_ms();
        Ok(self
            .meters
            .iter()
            .filter_map(|(node_id, meter)| {
                if !meter.shared.connected.load(Ordering::Relaxed) {
                    return None;
                }
                let (rms, peak, age_ms) = meter.shared.levels(now_ms)?;
                // A helper stream currently aggregates the target node's
                // buffer, so it cannot honestly report independent levels for
                // each port. Keep the optional port association in the public
                // API for backends that can provide it and use node fallback
                // here until PipeWire per-port capture is implemented.
                Some(AudioMeter {
                    node_id: *node_id,
                    port_id: None,
                    rms: rms.clamp(0.0, 1.0),
                    peak: peak.clamp(0.0, 1.0),
                    age_ms,
                    available: true,
                })
            })
            .collect())
    }

    fn set_meter_policy(&mut self, policy: MeterPolicy) -> BackendResult<()> {
        if self.meter_policy == policy {
            return Ok(());
        }
        self.meter_policy = policy;
        if policy != MeterPolicy::OnDemand {
            self.meter_requests.clear();
        }
        self.with_loop(|driver| {
            driver.ensure_meters_locked();
            Ok(())
        })
    }

    fn request_meters(&mut self, nodes: &BTreeSet<NodeId>) -> BackendResult<()> {
        if self.meter_policy != MeterPolicy::OnDemand {
            return Ok(());
        }
        if nodes.is_empty() {
            // An empty visible set is an explicit lifecycle event (for
            // example, the application was minimized or hidden), rather
            // than a request that should linger until the normal timeout.
            // Release helper streams immediately so a UI cannot keep an
            // audio device awake after it disappears.
            self.meter_requests.clear();
            return self.with_loop(|driver| {
                driver.ensure_meters_locked();
                Ok(())
            });
        }
        let now = Instant::now();
        for node_id in nodes {
            self.meter_requests.insert(*node_id, now);
        }
        self.expire_meter_requests();
        // The UI repeats this every frame. Only take the loop lock when the set
        // of live streams actually has to change.
        let wanted = self.wanted_meter_nodes();
        if wanted.iter().eq(self.meters.keys()) {
            return Ok(());
        }
        self.with_loop(|driver| {
            driver.ensure_meters_locked();
            Ok(())
        })
    }

    fn reset_audio_config(&mut self) -> BackendResult<()> {
        self.meter_requests.clear();
        self.with_loop(|driver| {
            driver.meters.clear();
            Ok(())
        })
    }
}

/// Whether a node is one of the relay's own `pw_filter` devices.
///
/// Those filters publish no `Props`, so their volume and mute can neither be
/// read nor written. Reporting them as unsupported keeps the card from
/// offering a fader that does nothing and a mute button stuck on "unknown".
impl RecorderDriver for PipewireDriver {
    fn supports_recorders(&self) -> bool {
        true
    }

    fn create_recorder(
        &mut self,
        request: RecorderCreateRequest,
    ) -> BackendResult<RecorderInstance> {
        let id = self
            .recorders
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        let name = format!("{RECORDER_NODE_PREFIX} {id}");
        let format = request.format;
        let directory = request.recording_dir.clone();
        let capacity = request.capacity_frames.max(
            (usize::try_from(format.sample_rate)
                .unwrap_or(48_000)
                .saturating_mul(DEFAULT_RECORDER_CAPACITY_MS as usize)
                / 1_000)
                .max(1),
        );
        self.with_loop(|driver| {
            let core = driver.core()?.clone();
            let handle = RecorderHandle::create(
                &core,
                name.clone(),
                format,
                directory.as_deref(),
                capacity,
            )?;
            driver.recorders.insert(
                id,
                NativeRecorder {
                    request: request.clone(),
                    instance: RecorderInstance {
                        id,
                        node_id: NodeId(0),
                        input_port: PortId(0),
                        status: RecorderStatus {
                            id,
                            state: RecorderState::Idle,
                            writer_state: RecorderWriterState::Starting,
                            sample_rate: format.sample_rate,
                            channels: format.channels,
                            temporary_path: Some(handle.writer.temporary_path().to_owned()),
                            queue_capacity: capacity,
                            ..RecorderStatus::default()
                        },
                    },
                    handle: Some(handle),
                },
            );
            let published = driver.wait_for_publication(|driver| {
                let Some(node) = driver.graph.nodes.values().find(|node| node.name == name) else {
                    return false;
                };
                driver.graph.ports.values().any(|port| {
                    port.node_id == node.id
                        && port.direction.is_sink()
                        && port.port_type == PortType::Audio
                })
            });
            if let Err(error) = published {
                driver.recorders.remove(&id);
                return Err(error);
            }
            let Some(node) = driver.graph.nodes.values().find(|node| node.name == name) else {
                driver.recorders.remove(&id);
                return Err(BackendError::native(
                    "PipeWire did not publish the recorder node",
                ));
            };
            let Some(input_port) = node.ports.iter().copied().find(|port_id| {
                driver.graph.port(*port_id).is_some_and(|port| {
                    port.direction.is_sink() && port.port_type == PortType::Audio
                })
            }) else {
                driver.recorders.remove(&id);
                return Err(BackendError::native(
                    "PipeWire did not publish the recorder input port",
                ));
            };
            let recorder = driver
                .recorders
                .get_mut(&id)
                .expect("recorder inserted before publication");
            recorder.instance.node_id = node.id;
            recorder.instance.input_port = input_port;
            Ok(recorder.instance.clone())
        })
    }

    fn remove_recorder(&mut self, id: RecorderId) -> BackendResult<()> {
        let state = self
            .recorders
            .get(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        if state.instance.status.state == RecorderState::Recording {
            return Err(BackendError::native(
                "stop or discard the recording before removing its node",
            ));
        }
        if matches!(
            state.instance.status.state,
            RecorderState::Unsaved | RecorderState::Error
        ) && state.instance.status.temporary_path.is_some()
        {
            return Err(BackendError::native(
                "save or explicitly discard the pending recording before removing its node",
            ));
        }
        self.with_loop(|driver| {
            let recorder = driver
                .recorders
                .remove(&id)
                .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
            let path = recorder
                .handle
                .as_ref()
                .map(|handle| handle.writer.temporary_path().to_owned())
                .or_else(|| recorder.instance.status.temporary_path.clone());
            drop(recorder);
            if let Some(path) = path {
                if path.exists() {
                    std::fs::remove_file(path)
                        .map_err(|error| BackendError::native(error.to_string()))?;
                }
            }
            driver.sync()?;
            Ok(())
        })
    }

    fn start_recording(&mut self, id: RecorderId) -> BackendResult<()> {
        let (state, writer_state, final_path) = self
            .recorders
            .get(&id)
            .map(|recorder| {
                (
                    recorder.instance.status.state,
                    recorder
                        .handle
                        .as_ref()
                        .map(|handle| handle.status().writer_state)
                        .unwrap_or(recorder.instance.status.writer_state),
                    recorder.instance.status.final_path.clone(),
                )
            })
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        if state == RecorderState::Recording {
            return Err(BackendError::native("recorder is already recording"));
        }
        if state != RecorderState::Idle {
            return Err(BackendError::native(
                "save or discard the pending recording before starting again",
            ));
        }
        if writer_state != crate::router::RecorderWriterState::Starting
            || self
                .recorders
                .get(&id)
                .and_then(|recorder| recorder.handle.as_ref())
                .is_none()
        {
            self.with_loop(|driver| {
                let warning = driver.restart_recorder_locked(id, final_path, true)?;
                let recorder = driver
                    .recorders
                    .get_mut(&id)
                    .expect("restarted recorder was inserted before resuming");
                let Some(handle) = recorder.handle.as_mut() else {
                    return Err(BackendError::native(
                        "PipeWire recorder stream is unavailable after restart",
                    ));
                };
                if let Err(error) = handle.resume() {
                    recorder.instance.status.state = RecorderState::Error;
                    recorder.instance.status.error = Some(error.to_string());
                    return Err(error);
                }
                recorder.instance.status.state = RecorderState::Recording;
                recorder.instance.status.writer_state = RecorderWriterState::Recording;
                recorder.instance.status.final_path = None;
                recorder.instance.status.error = warning;
                Ok(())
            })
        } else {
            let recorder = self
                .recorders
                .get_mut(&id)
                .expect("recorder was checked above");
            let Some(handle) = recorder.handle.as_mut() else {
                return Err(BackendError::native(
                    "PipeWire recorder stream is unavailable",
                ));
            };
            if let Err(error) = handle.resume() {
                recorder.instance.status.state = RecorderState::Error;
                recorder.instance.status.error = Some(error.to_string());
                return Err(error);
            }
            recorder.instance.status.state = RecorderState::Recording;
            recorder.instance.status.writer_state = RecorderWriterState::Recording;
            recorder.instance.status.final_path = None;
            recorder.instance.status.error = None;
            Ok(())
        }
    }

    fn request_stop_recording(&mut self, id: RecorderId) -> BackendResult<()> {
        let recorder = self
            .recorders
            .get_mut(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        if recorder.instance.status.state != RecorderState::Recording {
            return Err(BackendError::native("recorder is not recording"));
        }
        let Some(handle) = recorder.handle.as_mut() else {
            return Err(BackendError::native(
                "PipeWire recorder stream is unavailable",
            ));
        };
        handle.request_stop()?;
        // The writer drains asynchronously. Keep the public state as
        // Recording until poll_recording observes the finalized result, so
        // the UI cannot offer Save while the `.part` header is still open.
        Ok(())
    }

    fn stop_recording(&mut self, id: RecorderId) -> BackendResult<RecorderResult> {
        let recorder = self
            .recorders
            .get_mut(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        if recorder.instance.status.state != RecorderState::Recording {
            return Err(BackendError::native("recorder is not recording"));
        }
        let Some(handle) = recorder.handle.as_mut() else {
            return Err(BackendError::native(
                "PipeWire recorder stream is unavailable",
            ));
        };
        let result = match handle.stop() {
            Ok(result) => result,
            Err(error) => {
                recorder.instance.status.state = RecorderState::Error;
                recorder.instance.status.error = Some(error.to_string());
                return Err(error);
            }
        };
        recorder.instance.status.state = RecorderState::Unsaved;
        recorder.instance.status.writer_state = RecorderWriterState::Finished;
        recorder.instance.status.elapsed_frames = result.frames_written;
        recorder.instance.status.frames_written = result.frames_written;
        recorder.instance.status.dropped_frames = result.dropped_frames;
        recorder.instance.status.file_bytes = result.file_bytes;
        recorder.instance.status.temporary_path = Some(result.temporary_path.clone());
        Ok(RecorderResult {
            id,
            temporary_path: result.temporary_path,
            elapsed_frames: result.frames_written,
            sample_rate: recorder.request.format.sample_rate,
            channels: recorder.request.format.channels,
            frames_written: result.frames_written,
            dropped_frames: result.dropped_frames,
            file_bytes: result.file_bytes,
        })
    }

    fn save_recording(
        &mut self,
        id: RecorderId,
        destination: &std::path::Path,
    ) -> BackendResult<std::path::PathBuf> {
        let recorder = self
            .recorders
            .get(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        if recorder.instance.status.state == RecorderState::Recording {
            return Err(BackendError::native("stop the recorder before saving it"));
        }
        if recorder.instance.status.state != RecorderState::Unsaved {
            return Err(BackendError::native(
                "recorder has no finished recording to save",
            ));
        }
        let source = recorder
            .instance
            .status
            .temporary_path
            .clone()
            .ok_or_else(|| BackendError::native("recorder has no pending recording"))?;
        if let Err(error) = replace_recording_preserving_source(&source, destination) {
            let message = error.to_string();
            if let Some(recorder) = self.recorders.get_mut(&id) {
                recorder.instance.status.error = Some(message.clone());
            }
            return Err(BackendError::native(message));
        }
        let destination = destination.to_owned();
        let restart = self.with_loop(|driver| {
            driver.restart_recorder_locked(id, Some(destination.clone()), false)
        });
        let mut warnings = Vec::new();
        let reset_succeeded = match restart {
            Ok(warning) => {
                if let Some(warning) = warning {
                    warnings.push(warning);
                }
                true
            }
            Err(error) => {
                warnings.push(format!("recorder reset failed: {error}"));
                false
            }
        };
        if let Err(error) = fs::remove_file(&source) {
            if error.kind() != std::io::ErrorKind::NotFound {
                warnings.push(format!("temporary recording cleanup failed: {error}"));
            }
        }
        let recorder = self
            .recorders
            .get_mut(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        recorder.instance.status.state = RecorderState::Idle;
        recorder.instance.status.final_path = Some(destination.clone());
        recorder.instance.status.temporary_path = None;
        if reset_succeeded {
            recorder.instance.status.writer_state = RecorderWriterState::Starting;
        }
        recorder.instance.status.error = warning_text(warnings);
        Ok(destination)
    }

    fn recorder_status(&self, id: RecorderId) -> BackendResult<RecorderStatus> {
        let recorder = self
            .recorders
            .get(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        let mut status = recorder.instance.status.clone();
        if let Some(handle) = recorder.handle.as_ref() {
            let diagnostics = handle.status();
            status.elapsed_frames = diagnostics.frames_written;
            status.frames_written = diagnostics.frames_written;
            status.dropped_frames = diagnostics.dropped_frames;
            status.queue_depth = diagnostics.queue_depth;
            status.queue_capacity = diagnostics.queue_capacity;
            status.file_bytes = diagnostics.file_bytes;
            status.error = diagnostics.last_error.or(status.error);
            status.writer_state = diagnostics.writer_state;
            if diagnostics.writer_state == crate::router::RecorderWriterState::Error {
                status.state = RecorderState::Error;
            }
            if let Some(error) = handle.failure_message() {
                status.state = RecorderState::Error;
                status.error = Some(error.into());
            }
        }
        Ok(status)
    }

    fn recorder_instance(&self, id: RecorderId) -> BackendResult<RecorderInstance> {
        self.recorders
            .get(&id)
            .map(|recorder| recorder.instance.clone())
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))
    }

    fn poll_recording(&mut self, id: RecorderId) -> BackendResult<Option<RecorderResult>> {
        let recorder = self
            .recorders
            .get_mut(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        if recorder.instance.status.state != RecorderState::Recording {
            return Ok(None);
        }
        let Some(handle) = recorder.handle.as_mut() else {
            let error = BackendError::native("PipeWire recorder stream is unavailable");
            recorder.instance.status.state = RecorderState::Error;
            recorder.instance.status.error = Some(error.to_string());
            return Err(error);
        };
        let polled = handle.poll();
        let Some(result) = (match polled {
            Ok(result) => result,
            Err(error) => {
                let message = error.to_string();
                recorder.instance.status.state = RecorderState::Error;
                recorder.instance.status.error = Some(message.clone());
                return Err(BackendError::native(message));
            }
        }) else {
            return Ok(None);
        };
        recorder.instance.status.state = RecorderState::Unsaved;
        recorder.instance.status.writer_state = RecorderWriterState::Finished;
        recorder.instance.status.elapsed_frames = result.frames_written;
        recorder.instance.status.frames_written = result.frames_written;
        recorder.instance.status.dropped_frames = result.dropped_frames;
        recorder.instance.status.file_bytes = result.file_bytes;
        recorder.instance.status.temporary_path = Some(result.temporary_path.clone());
        recorder.instance.status.error = None;
        Ok(Some(RecorderResult {
            id,
            temporary_path: result.temporary_path,
            elapsed_frames: result.frames_written,
            sample_rate: recorder.request.format.sample_rate,
            channels: recorder.request.format.channels,
            frames_written: result.frames_written,
            dropped_frames: result.dropped_frames,
            file_bytes: result.file_bytes,
        }))
    }

    fn discard_recording(&mut self, id: RecorderId) -> BackendResult<()> {
        let state = self
            .recorders
            .get(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        if state.instance.status.state == RecorderState::Recording {
            return Err(BackendError::native(
                "stop the recorder before discarding it",
            ));
        }
        self.with_loop(|driver| {
            let warning = driver.restart_recorder_locked(id, None, true)?;
            let recorder = driver
                .recorders
                .get_mut(&id)
                .expect("restarted recorder was inserted before discard");
            recorder.instance.status.state = RecorderState::Idle;
            recorder.instance.status.writer_state = RecorderWriterState::Starting;
            recorder.instance.status.temporary_path = None;
            recorder.instance.status.final_path = None;
            recorder.instance.status.error = warning;
            Ok(())
        })?;
        Ok(())
    }
}

fn is_relay_device_node(name: &str) -> bool {
    #[cfg(all(target_os = "linux", feature = "relay"))]
    {
        matches!(name, relay::RELAY_SOURCE_NAME | relay::RELAY_SINK_NAME)
    }
    #[cfg(not(all(target_os = "linux", feature = "relay")))]
    {
        let _ = name;
        false
    }
}

fn warning_text(warnings: Vec<String>) -> Option<String> {
    (!warnings.is_empty()).then(|| warnings.join("; "))
}

fn native_error(operation: &str, error: impl std::fmt::Display) -> BackendError {
    BackendError::Native(format!("{operation} failed: {error}"))
}

fn audio_format_pod() -> BackendResult<Vec<u8>> {
    // Rate and channel count are deliberately left unset: a zeroed field is
    // omitted from the pod, so the daemon negotiates the node's own values
    // instead of asking it to resample or reconfigure for the meter.
    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::F32LE);
    let object = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map(|success| success.0.into_inner())
        .map_err(|error| native_error("PipeWire audio format serialization", error))
}

/// PipeWire's conventional UI volume curve is cubic: a displayed 50% is sent
/// as 0.5³, which corresponds to roughly −18 dB. Sending the UI percentage
/// directly made the control much louder than its displayed value implied.
/// The driver''s own wrapper so call sites do not repeat the boost ceiling.
fn ui_volume_to_spa_volume(volume: f32) -> f32 {
    crate::api::ui_volume_to_spa_volume(volume, PIPEWIRE_MAX_VOLUME)
}

#[cfg(test)]
mod tests {
    use super::{classify_port_type, ui_volume_to_spa_volume, PipewireDriver};
    use crate::{EffectCreateRequest, EffectDriver, EffectEvent, GraphDriver};
    use pw_graph_core::{Direction, NodeType, PortType};
    use std::collections::BTreeMap;
    use std::thread;
    use std::time::{Duration, Instant};

    fn wait_for_effect(
        driver: &mut PipewireDriver,
        request: EffectCreateRequest,
    ) -> crate::BackendResult<crate::EffectInstance> {
        let ticket = driver.begin_create_effect(request)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            for event in driver.poll_effect_events()? {
                match event {
                    EffectEvent::Ready {
                        ticket: ready_ticket,
                        instance,
                    } if ready_ticket == ticket => return Ok(*instance),
                    EffectEvent::Failed {
                        ticket: failed_ticket,
                        error,
                    } if failed_ticket == ticket => {
                        return Err(crate::BackendError::native(error));
                    }
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        Err(crate::BackendError::native(
            "timed out waiting for effect preparation",
        ))
    }

    #[test]
    fn classifies_media_types_without_case_sensitive_metadata() {
        assert_eq!(classify_port_type("Audio", None), PortType::Audio);
        assert_eq!(classify_port_type("video/raw", None), PortType::Video);
        assert_eq!(
            classify_port_type("", Some("Midi/Source")),
            PortType::MidiJack
        );
    }

    #[test]
    fn prefers_explicit_port_media_metadata() {
        assert_eq!(
            classify_port_type("audio/raw", Some("Video/Source")),
            PortType::Audio
        );
        assert_eq!(
            classify_port_type("midi/raw", Some("Audio/Source")),
            PortType::MidiJack
        );
        assert_eq!(
            classify_port_type("", Some("Stream/Output")),
            PortType::Unknown
        );
    }

    #[test]
    fn converts_ui_volume_to_pipewire_cubic_scale() {
        assert!((ui_volume_to_spa_volume(1.0) - 1.0).abs() < f32::EPSILON);
        assert!((ui_volume_to_spa_volume(0.5) - 0.125).abs() < f32::EPSILON);
        assert!((ui_volume_to_spa_volume(1.5) - 3.375).abs() < f32::EPSILON);
    }

    #[test]
    fn channel_policy_negotiation_is_shared_by_all_creation_paths() {
        let host = pw_graph_effects::EffectHost::new();
        let id = pw_graph_effects::HUSH_NOISE_SUPPRESSOR_ID;
        assert_eq!(
            host.negotiate_channels(id, pw_graph_effects::ChannelPolicy::Auto, None)
                .unwrap(),
            1
        );
        assert_eq!(
            host.negotiate_channels(id, pw_graph_effects::ChannelPolicy::Auto, Some(1))
                .unwrap(),
            1
        );
        assert_eq!(
            host.negotiate_channels(id, pw_graph_effects::ChannelPolicy::Auto, Some(2))
                .unwrap(),
            2
        );
        // An explicit saved/API layout wins over topology inference.
        assert_eq!(
            host.negotiate_channels(id, pw_graph_effects::ChannelPolicy::Fixed(1), Some(2))
                .unwrap(),
            1
        );
        assert_eq!(
            host.negotiate_channels(id, pw_graph_effects::ChannelPolicy::Fixed(2), Some(1))
                .unwrap(),
            2
        );
    }

    #[test]
    fn hush_layout_rejects_invalid_channel_counts_instead_of_defaulting() {
        let host = pw_graph_effects::EffectHost::new();
        for requested in [0, 3, u16::MAX] {
            let error = host
                .negotiate_channels(
                    pw_graph_effects::HUSH_NOISE_SUPPRESSOR_ID,
                    pw_graph_effects::ChannelPolicy::Fixed(requested),
                    None,
                )
                .unwrap_err();
            assert!(error.to_string().contains("does not support"));
        }
    }

    /// Opt-in because it creates a real PipeWire node in the user's session.
    /// The node has no links, so it cannot alter a live audio route; the test
    /// exists to exercise the raw `pw_filter` lifetime and registry mapping on
    /// a machine with PipeWire available.
    #[test]
    fn native_backend_creates_and_removes_a_standalone_effect_when_enabled() {
        if std::env::var_os("PW_GRAPH_TEST_EFFECTS").is_none() {
            return;
        }
        let mut driver = PipewireDriver::new().expect("PipeWire daemon should be available");
        driver
            .refresh()
            .expect("PipeWire registry snapshot should succeed");
        let instance = wait_for_effect(
            &mut driver,
            EffectCreateRequest {
                instance_id: "qpwgraph-rs-test-effect".into(),
                effect_id: pw_graph_effects::NOISE_SUPPRESSOR_ID.into(),
                module_path: None,
                enabled: true,
                parameters: BTreeMap::new(),
                channel_policy: pw_graph_effects::ChannelPolicy::Auto,
                target: crate::EffectTarget::Standalone {
                    position: [12.0, 34.0],
                },
            },
        )
        .expect("the raw PipeWire filter should publish a node and ports");
        let node = driver
            .graph()
            .node(instance.node_id)
            .expect("effect node should be present in the rebuilt graph");
        assert_eq!(node.node_type, NodeType::Effect);
        assert_eq!(
            node.effect_instance_id.as_deref(),
            Some("qpwgraph-rs-test-effect")
        );
        assert!(driver.graph().port(instance.input_port).is_some());
        assert!(driver.graph().port(instance.output_port).is_some());
        let effect_ports: BTreeMap<_, _> = node
            .ports
            .iter()
            .filter_map(|port_id| driver.graph().port(*port_id))
            .map(|port| {
                (
                    port.name.as_str(),
                    (port.direction, port.channel.as_deref()),
                )
            })
            .collect();
        assert_eq!(
            effect_ports,
            BTreeMap::from([
                ("input_FL", (Direction::Sink, Some("FL"))),
                ("input_FR", (Direction::Sink, Some("FR"))),
                ("output_FL", (Direction::Source, Some("FL"))),
                ("output_FR", (Direction::Source, Some("FR"))),
            ])
        );

        driver
            .set_effect_enabled("qpwgraph-rs-test-effect", false)
            .expect("bypass should update the callback-safe state");
        driver
            .remove_effect("qpwgraph-rs-test-effect")
            .expect("destroying the raw filter should remove its node");
        assert!(driver.effect_instances().is_empty());
        assert!(driver.graph().node(instance.node_id).is_none());
    }

    /// Opt-in Hush-specific lifecycle coverage. This keeps the model-backed
    /// worker on the same raw PipeWire node path used by the legacy effect.
    #[test]
    fn native_backend_creates_and_removes_a_standalone_hush_effect_when_enabled() {
        if std::env::var_os("PW_GRAPH_TEST_EFFECTS").is_none() {
            return;
        }
        let mut driver = PipewireDriver::new().expect("PipeWire daemon should be available");
        driver
            .refresh()
            .expect("PipeWire registry snapshot should succeed");
        let instance_id = "qpwgraph-rs-test-hush-effect";
        let instance = wait_for_effect(
            &mut driver,
            EffectCreateRequest {
                instance_id: instance_id.into(),
                effect_id: pw_graph_effects::HUSH_NOISE_SUPPRESSOR_ID.into(),
                module_path: None,
                enabled: true,
                parameters: BTreeMap::new(),
                channel_policy: pw_graph_effects::ChannelPolicy::Auto,
                target: crate::EffectTarget::Standalone {
                    position: [56.0, 78.0],
                },
            },
        )
        .expect("the Hush PipeWire filter should publish a node and ports");
        assert_eq!(
            instance.config.channel_policy,
            pw_graph_effects::ChannelPolicy::Auto
        );
        let node = driver
            .graph()
            .node(instance.node_id)
            .expect("Hush node should be present in the rebuilt graph");
        let port_names: Vec<_> = node
            .ports
            .iter()
            .filter_map(|port_id| driver.graph().port(*port_id))
            .map(|port| port.name.as_str())
            .collect();
        assert_eq!(
            port_names,
            ["input_FL", "output_FL", "input_FR", "output_FR"]
        );
        assert_eq!(
            driver
                .graph()
                .node(instance.node_id)
                .and_then(|node| node.effect_instance_id.as_deref()),
            Some(instance_id)
        );
        driver
            .remove_effect(instance_id)
            .expect("destroying the Hush filter should remove its worker and node");
        assert!(driver.effect_instances().is_empty());
        assert!(driver.graph().node(instance.node_id).is_none());
    }
}
