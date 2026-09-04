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
use pw_graph_effects::{EffectDescriptor, EffectHost};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Cursor;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod effects;
mod filter_runtime;
mod links;
mod metering;
mod properties;
mod readback;
mod registry;
#[cfg(all(target_os = "linux", feature = "relay"))]
mod relay;

use effects::NativeEffect;
use metering::{process_meter_buffer, MeterCallbackState, MeterHandle, MeterReadingState};
use registry::{classify_port_type, install_registry_listener, NodeRecord, RegistryState};

const NODE_NAME: &str = "node.name";
const NODE_DESCRIPTION: &str = "node.description";
const MEDIA_CLASS: &str = "media.class";
const MEDIA_TYPE: &str = "media.type";
const FORMAT_DSP: &str = "format.dsp";
const NODE_ID: &str = "node.id";
const OBJECT_SERIAL: &str = "object.serial";
const PORT_NAME: &str = "port.name";
const AUDIO_CHANNEL: &str = "audio.channel";
const PORT_DIRECTION: &str = "port.direction";
const LINK_OUTPUT_PORT: &str = "link.output.port";
const LINK_INPUT_PORT: &str = "link.input.port";
const NODE_INTERFACE_VERSION: u32 = 3;

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
    state: Arc<Mutex<RegistryState>>,
    registry_dirty: Arc<AtomicBool>,
    meters: BTreeMap<NodeId, MeterHandle>,
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
    /// Manual disconnects are kept as stable endpoint pairs. WirePlumber may
    /// recreate an application's link when it resumes; the next synchronized
    /// snapshot removes only those links the user explicitly deleted.
    blocked_connections: Vec<(PortKey, PortKey)>,
    /// Relay engine plus the two virtual devices. Created on first relay use
    /// and kept until the driver drops so reconnects stay cheap.
    #[cfg(all(target_os = "linux", feature = "relay"))]
    relay: Option<relay::RelayRuntimeSet>,
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

        thread_loop.start();

        let driver = Self {
            thread_loop,
            context: Some(context),
            core: Some(core),
            registry: Some(registry),
            registry_listener: Some(registry_listener),
            state,
            registry_dirty,
            meters: BTreeMap::new(),
            meter_policy: MeterPolicy::default(),
            meter_requests: BTreeMap::new(),
            epoch: Instant::now(),
            graph: Graph::default(),
            positions: BTreeMap::new(),
            audio_controls: BTreeMap::new(),
            effect_host: EffectHost::new(),
            effects: BTreeMap::new(),
            blocked_connections: Vec::new(),
            #[cfg(all(target_os = "linux", feature = "relay"))]
            relay: None,
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
                            && port.name == "input_FL"
                    })
                    .map(|(id, _)| PortId(graph_id(*id as u64)))?;
                let output_port = state
                    .ports
                    .iter()
                    .find(|(_, port)| {
                        port.node_id == native_node_id(node_id)
                            && port.direction.is_source()
                            && port.name == "output_FL"
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
            let mut node = Node::new(
                node_id,
                &record.name,
                if effect_instance_id.is_some() {
                    NodeType::Effect
                } else {
                    NodeType::PipeWire
                },
            );
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

        let default_positions = graph.default_node_positions();
        for (node_id, node) in &mut graph.nodes {
            if let Some(position) = self.positions.get(node_id).copied() {
                node.position = position;
            } else if let Some(position) = default_positions.get(node_id).copied() {
                node.position = position;
                self.positions.insert(*node_id, position);
            }
        }

        for (id, record) in state.links {
            let _ = graph.insert_existing_link(Link {
                id: LinkId(graph_id(id as u64)),
                output_port: PortId(graph_id(record.output_port as u64)),
                input_port: PortId(graph_id(record.input_port as u64)),
            });
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
                self.read_node_controls_locked();
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

    /// Create a live `pw_filter` and wait until the registry has published its
    /// node and both ports. Callers hold the ThreadLoop lock for the entire
    /// transaction, which makes callback-data destruction safe on rollback.
    fn create_effect_node_locked(
        &mut self,
        request: EffectNodeRequest,
    ) -> BackendResult<EffectInstance> {
        if self.effects.contains_key(&request.instance_id) {
            return Err(BackendError::effect_already_exists(&request.instance_id));
        }
        let instance_id = request.instance_id.clone();
        let effect = NativeEffect::create(&self.effect_host, &self.thread_loop, request)?;
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

    fn insert_effect_locked(
        &mut self,
        request: EffectInsertRequest,
    ) -> BackendResult<EffectInstance> {
        let source = request.source.clone();
        let destination = request.destination.clone();
        // Verify the selected link before publishing a new node. It can still
        // disappear while the effect initializes, so we resolve it once more
        // immediately before disconnecting it below.
        self.effect_link_endpoints_locked(&source, &destination)?;
        let instance_id = request.instance_id.clone();
        let instance = self.create_effect_node_locked(request.into())?;

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
        // Each raw `pw_filter` owns callbacks on this loop, so destroy them
        // before releasing the registry/core that created their globals.
        #[cfg(all(target_os = "linux", feature = "relay"))]
        self.relay.take();
        self.effects.clear();
        self.registry_listener.take();
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
        }
    }

    fn refresh(&mut self) -> BackendResult<Vec<Node>> {
        self.with_loop(|driver| {
            driver.sync()?;
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
        self.with_loop(|driver| {
            driver.roundtrip_locked()?;
            driver.set_node_mute_locked(node, muted)
        })
    }

    fn set_node_volume(&mut self, node: NodeId, volume: f32) -> BackendResult<()> {
        if !self.graph.nodes.contains_key(&node) {
            return Err(GraphError::MissingNode(node).into());
        }
        self.with_loop(|driver| {
            driver.roundtrip_locked()?;
            driver.set_node_volume_locked(node, volume)
        })
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
        if record.node_type == NodeType::Effect {
            return Ok(NodeAudioState::UNSUPPORTED);
        }
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
        if self.measurable_nodes().contains(&node) {
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

impl EffectDriver for PipewireDriver {
    fn effect_descriptors(&self) -> Vec<EffectDescriptor> {
        self.effect_host.descriptors()
    }

    fn effect_instances(&self) -> Vec<EffectInstance> {
        self.effects.values().map(NativeEffect::snapshot).collect()
    }

    fn supports_effect_nodes(&self) -> bool {
        true
    }

    fn create_effect_node(&mut self, request: EffectNodeRequest) -> BackendResult<EffectInstance> {
        self.with_loop(|driver| {
            driver.sync()?;
            driver.create_effect_node_locked(request)
        })
    }

    fn insert_effect(&mut self, request: EffectInsertRequest) -> BackendResult<EffectInstance> {
        self.with_loop(|driver| {
            driver.sync()?;
            driver.insert_effect_locked(request)
        })
    }

    fn set_effect_enabled(&mut self, instance_id: &str, enabled: bool) -> BackendResult<()> {
        let effect = self
            .effects
            .get_mut(instance_id)
            .ok_or_else(|| BackendError::unknown_effect_instance(instance_id))?;
        effect.set_enabled(enabled);
        Ok(())
    }

    fn set_effect_parameter(
        &mut self,
        instance_id: &str,
        parameter: &str,
        value: f32,
    ) -> BackendResult<()> {
        let effect = self
            .effects
            .get_mut(instance_id)
            .ok_or_else(|| BackendError::unknown_effect_instance(instance_id))?;
        effect.set_parameter(parameter, value)
    }

    fn remove_effect(&mut self, instance_id: &str) -> BackendResult<()> {
        self.with_loop(|driver| {
            driver.sync()?;
            driver.remove_effect_locked(instance_id)
        })
    }
}

#[cfg(all(target_os = "linux", feature = "relay"))]
impl RelayDriver for PipewireDriver {
    fn relay_available(&self) -> bool {
        true
    }

    fn relay_status(&self) -> RelayEngineStatus {
        self.relay
            .as_ref()
            .map(|set| set.handle().status())
            .unwrap_or_default()
    }

    fn relay_devices_active(&self) -> bool {
        self.relay.is_some()
    }

    fn relay_start_host(&mut self, request: RelayHostRequest) -> BackendResult<u16> {
        self.with_loop(|driver| {
            let set = driver.ensure_relay_devices_locked(&request.device_name)?;
            let config = pw_graph_relay::EngineConfig {
                device_id: request.device_id,
                device_name: request.device_name,
                pin: request.pin,
                port: request.port,
                codec: request.codec,
                frame_ms: request.frame_ms,
                transport: request.transport,
                trusted_peers: request.trusted_peers,
                trust_new_peers: request.trust_new_peers,
                direction: request.direction,
                direction_generation: request.direction_generation,
                ..Default::default()
            };
            set.handle().update_config(config);
            set.handle()
                .host_start()
                .map_err(|error| BackendError::native(format!("relay host start failed: {error}")))
        })
    }

    fn relay_stop_host(&mut self) -> BackendResult<()> {
        if let Some(set) = self.relay.as_mut() {
            set.handle().host_stop().map_err(|error| {
                BackendError::native(format!("relay host stop failed: {error}"))
            })?;
        }
        Ok(())
    }

    fn relay_connect(
        &mut self,
        target: std::net::SocketAddr,
        pin: &str,
        direction: RelayDirection,
        direction_generation: u64,
    ) -> BackendResult<RelaySessionId> {
        self.with_loop(|driver| {
            let device_name = driver
                .relay
                .as_ref()
                .map(|set| set.handle().config().device_name)
                .unwrap_or_else(|| "qpwgraph-rs".into());
            let set = driver.ensure_relay_devices_locked(&device_name)?;
            let mut config = set.handle().config();
            config.direction = direction;
            config.direction_generation = direction_generation;
            let roles = super::api::desktop_relay_client_roles(direction);
            config.client_roles = roles;
            set.handle().update_config(config);
            Ok(set
                .handle()
                .connect(target, pin, roles))
        })
    }

    fn relay_connect_trusted(
        &mut self,
        target: std::net::SocketAddr,
        peer_id: &str,
        secret: [u8; 32],
        direction: RelayDirection,
        direction_generation: u64,
    ) -> BackendResult<RelaySessionId> {
        self.with_loop(|driver| {
            let device_name = driver
                .relay
                .as_ref()
                .map(|set| set.handle().config().device_name)
                .unwrap_or_else(|| "qpwgraph-rs".into());
            let set = driver.ensure_relay_devices_locked(&device_name)?;
            let mut config = set.handle().config();
            config.direction = direction;
            config.direction_generation = direction_generation;
            let roles = super::api::desktop_relay_client_roles(direction);
            config.client_roles = roles;
            set.handle().update_config(config);
            Ok(set.handle().connect_trusted(
                target,
                peer_id,
                secret,
                roles,
            ))
        })
    }

    fn relay_configure_identity(
        &mut self,
        device_id: String,
        trusted_peers: Vec<super::api::RelayTrustedPeer>,
        transport: RelayTransportPreference,
    ) -> BackendResult<()> {
        self.with_loop(|driver| {
            let device_name = driver
                .relay
                .as_ref()
                .map(|set| set.handle().config().device_name)
                .unwrap_or_else(|| "qpwgraph-rs".into());
            let set = driver.ensure_relay_devices_locked(&device_name)?;
            let mut config = set.handle().config();
            config.device_id = device_id;
            config.trusted_peers = trusted_peers;
            config.transport = transport;
            config.trust_new_peers = true;
            set.handle().update_config(config);
            Ok(())
        })
    }

    fn relay_offer_direction(
        &mut self,
        session: RelaySessionId,
        direction: RelayDirection,
        generation: u64,
    ) -> BackendResult<()> {
        let Some(set) = self.relay.as_ref() else {
            return Err(BackendError::native("no relay session exists"));
        };
        set.handle()
            .offer_direction(session, direction, generation)
            .map_err(|error| BackendError::native(format!("relay direction offer failed: {error}")))
    }

    fn relay_disconnect(&mut self, session: RelaySessionId) -> BackendResult<()> {
        let Some(set) = self.relay.as_mut() else {
            return Err(BackendError::native(
                "no relay session exists to disconnect",
            ));
        };
        set.handle()
            .disconnect(session)
            .map_err(|error| BackendError::native(format!("relay disconnect failed: {error}")))
    }

    fn relay_trusted_enrollment_secret(
        &self,
        transaction_id: u64,
    ) -> BackendResult<Option<[u8; 32]>> {
        Ok(self
            .relay
            .as_ref()
            .and_then(|set| set.handle().trusted_enrollment_secret(transaction_id)))
    }

    fn relay_accept_trusted_enrollment(&mut self, transaction_id: u64) -> BackendResult<()> {
        let Some(set) = self.relay.as_ref() else {
            return Err(BackendError::native("no relay host is running"));
        };
        set.handle()
            .accept_trusted_enrollment(transaction_id)
            .map_err(|error| {
                BackendError::native(format!("trusted enrollment commit failed: {error}"))
            })
    }

    fn relay_reject_trusted_enrollment(
        &mut self,
        transaction_id: u64,
        reason: &str,
    ) -> BackendResult<()> {
        let Some(set) = self.relay.as_ref() else {
            return Err(BackendError::native("no relay host is running"));
        };
        set.handle()
            .reject_trusted_enrollment(transaction_id, reason)
            .map_err(|error| {
                BackendError::native(format!("trusted enrollment rejection failed: {error}"))
            })
    }

    fn relay_remove_trusted_peer(&mut self, peer_id: &str) -> BackendResult<()> {
        let Some(set) = self.relay.as_ref() else {
            return Err(BackendError::native("no relay engine is running"));
        };
        set.handle()
            .remove_trusted_peer(peer_id)
            .map_err(|error| BackendError::native(format!("trusted peer removal failed: {error}")))
    }

    fn relay_events(&mut self) -> Vec<RelayEvent> {
        let events = self
            .relay
            .as_mut()
            .map(|set| set.handle().events())
            .unwrap_or_default();
        // Session readiness is the authoritative signal that audio workers
        // are ready. When it arrives, the graph must be refreshed and the
        // relay route reconciled/verified before the UI reports Connected.
        // This makes the first connect work without requiring disconnect/reconnect.
        if events
            .iter()
            .any(|e| matches!(e, RelayEvent::SessionEstablished { .. }))
        {
            let _ = self.with_loop(|driver| {
                driver.rebuild_graph_locked()?;
                driver.ensure_relay_playback_route_locked()?;
                Ok(())
            });
        }
        events
    }

    fn relay_discovery_start(&mut self) -> BackendResult<()> {
        self.with_loop(|driver| {
            let set = driver.ensure_relay_devices_locked("qpwgraph-rs")?;
            set.handle()
                .discovery_start()
                .map_err(|error| BackendError::native(format!("relay discovery failed: {error}")))
        })
    }

    fn relay_discovery_stop(&mut self) {
        if let Some(set) = self.relay.as_ref() {
            set.handle().discovery_stop();
        }
    }

    fn relay_discovery_usb_link_lost(&mut self) {
        if let Some(set) = self.relay.as_ref() {
            set.handle().discovery_usb_link_lost();
        }
    }

    fn relay_usb_link_present(&self) -> bool {
        pw_graph_relay::netlink::local_links()
            .iter()
            .any(|link| link.kind == pw_graph_relay::LinkKind::Usb)
    }

    fn relay_peers(&self) -> Vec<RelayPeerInfo> {
        self.relay
            .as_ref()
            .map(|set| set.handle().discovered_peers())
            .unwrap_or_default()
    }

    fn relay_local_links(&self) -> Vec<pw_graph_relay::LocalLink> {
        pw_graph_relay::netlink::display_links()
    }

    fn relay_playback_status(&self) -> crate::RelayPlaybackStatus {
        if let Some(set) = &self.relay {
            let meters = set.playback_shared.snapshot();
            let router = &set.router;
            crate::RelayPlaybackStatus {
                state: match &router.state {
                    relay::RelayPlaybackState::Disabled => crate::RelayPlaybackState::Disabled,
                    relay::RelayPlaybackState::WaitingForSink => {
                        crate::RelayPlaybackState::WaitingForSink
                    }
                    relay::RelayPlaybackState::Connected => crate::RelayPlaybackState::Connected,
                    relay::RelayPlaybackState::Error(m) => {
                        crate::RelayPlaybackState::Error(m.clone())
                    }
                },
                sink_name: router.current_sink_name.clone(),
                gain: set.playback_shared.gain(),
                muted: set.playback_shared.muted(),
                enabled: set.playback_shared.enabled(),
                meters: crate::RelayMeterSnapshot {
                    input_rms: meters.input_rms,
                    input_peak: meters.input_peak,
                    output_rms: meters.output_rms,
                    output_peak: meters.output_peak,
                    input_dbfs: meters.input_dbfs,
                    output_dbfs: meters.output_dbfs,
                    peak_dbfs: meters.peak_dbfs,
                },
            }
        } else {
            crate::RelayPlaybackStatus::default()
        }
    }

    fn relay_set_playback_enabled(&mut self, enabled: bool) -> BackendResult<()> {
        if let Some(set) = self.relay.as_mut() {
            set.playback_shared.set_enabled(enabled);
            set.router.set_enabled(enabled);
            // Log major state transitions only, not per frame
            if enabled {
                eprintln!("Relay playback starting");
            } else {
                eprintln!("Relay playback disabled");
            }
        }
        // Propagate routing failures so the UI does not show a healthy state
        // while the PipeWire link could not be created.
        self.with_loop(|driver| driver.ensure_relay_playback_route_locked())?;
        Ok(())
    }

    fn relay_set_playback_gain(&mut self, gain: f32) -> BackendResult<()> {
        if let Some(set) = self.relay.as_mut() {
            let g = gain.clamp(0.0, 2.0);
            set.playback_shared.set_gain(g);
            eprintln!(
                "Relay playback gain: {}% ({:.1} dB)",
                (g * 100.0) as u32,
                if g > 0.0 {
                    20.0 * g.log10()
                } else {
                    f32::NEG_INFINITY
                }
            );
        }
        Ok(())
    }

    fn relay_set_playback_mute(&mut self, muted: bool) -> BackendResult<()> {
        if let Some(set) = self.relay.as_mut() {
            set.playback_shared.set_muted(muted);
            eprintln!("Relay playback mute: {}", muted);
        }
        Ok(())
    }

    fn relay_set_playback_sink(&mut self, sink: Option<String>) -> BackendResult<()> {
        if let Some(set) = self.relay.as_mut() {
            // Persist stable identifier: node.name
            let serial = sink.as_ref().and_then(|name| {
                self.graph
                    .nodes
                    .values()
                    .find(|n| &n.name == name)
                    .and_then(|n| n.serial)
            });
            set.router.set_preferred_sink(sink.clone(), serial);
            if let Some(name) = &sink {
                eprintln!("Relay playback sink selected: {name}");
            } else {
                eprintln!("Relay playback sink selected: Default");
            }
        }
        self.with_loop(|driver| driver.ensure_relay_playback_route_locked())?;
        Ok(())
    }

    fn relay_playback_sinks(&self) -> Vec<crate::RelaySinkInfo> {
        self.graph
            .nodes
            .values()
            .filter(|n| {
                n.name != relay::RELAY_SOURCE_NAME
                    && n.name != relay::RELAY_SINK_NAME
                    && n.ports.iter().any(|pid| {
                        self.graph.port(*pid).is_some_and(|p| {
                            p.direction.is_sink() && p.port_type == PortType::Audio
                        })
                    })
            })
            .map(|n| crate::RelaySinkInfo {
                name: n.name.clone(),
                description: n.name.clone(),
                serial: n.serial,
            })
            .collect()
    }

    fn relay_ensure_playback_route(&mut self) -> BackendResult<crate::RelayPlaybackState> {
        self.with_loop(|driver| {
            driver.ensure_relay_playback_route_locked()?;
            let status = driver.relay_playback_status();
            Ok(status.state)
        })
    }
}

#[cfg(all(target_os = "linux", feature = "relay"))]
impl PipewireDriver {
    /// Create the relay engine and virtual devices on first use. The caller
    /// must hold the ThreadLoop lock.
    fn ensure_relay_devices_locked(
        &mut self,
        device_name: &str,
    ) -> BackendResult<&mut relay::RelayRuntimeSet> {
        if self.relay.is_none() {
            let set = relay::RelayRuntimeSet::create(&self.thread_loop, device_name)?;
            self.relay = Some(set);
            // `pw_filter_new_simple` owns a small client connection of its
            // own, so the new virtual devices are published across clients.
            // Mirror the effect-creation synchronization: one round-trip
            // normally observes the globals, and a bounded second pass
            // covers the publication race without ever waiting in a loop.
            self.wait_for_publication(|driver| driver.relay_devices_visible_locked())?;
        }
        Ok(self
            .relay
            .as_mut()
            .expect("relay set was just created above"))
    }

    /// Whether both relay virtual devices are present in the current graph.
    fn relay_devices_visible_locked(&self) -> bool {
        self.graph
            .nodes
            .values()
            .any(|node| node.name == relay::RELAY_SOURCE_NAME)
            && self
                .graph
                .nodes
                .values()
                .any(|node| node.name == relay::RELAY_SINK_NAME)
    }

    /// Ensure relay playback routing: Relay Microphone -> selected/default sink.
    /// Idempotent and realtime-safe (graph mutation outside callback).
    fn ensure_relay_playback_route_locked(&mut self) -> BackendResult<()> {
        if self.relay.is_none() {
            return Ok(());
        }
        // Refresh graph view before deciding
        self.rebuild_graph_locked()?;
        let registry = self.state.lock().unwrap().clone();

        // Need to avoid holding relay borrow across self.connect_locked
        let (enabled, shared_enabled) = {
            let r = self.relay.as_ref().unwrap();
            (r.router.enabled, r.playback_shared.enabled())
        };

        // Validate existing links (need mutable)
        {
            let relay = self.relay.as_mut().unwrap();
            relay.router.validate_links(&self.graph);
        }

        if !enabled || !shared_enabled {
            let to_remove: Vec<LinkId> = {
                let relay = self.relay.as_ref().unwrap();
                relay
                    .router
                    .link_ids
                    .iter()
                    .filter_map(|id| self.graph.link(LinkId(*id)).map(|l| l.id))
                    .collect()
            };
            for link_id in to_remove {
                let _ = self.disconnect_locked(link_id);
                eprintln!("Relay playback link removed (disabled)");
            }
            let relay = self.relay.as_mut().unwrap();
            relay.router.link_ids.clear();
            relay.router.state = relay::RelayPlaybackState::Disabled;
            relay.router.current_sink_name = None;
            return Ok(());
        }

        // Discover desired links
        let desired_opt = {
            let relay = self.relay.as_ref().unwrap();
            relay.router.desired_links(&self.graph, &registry.nodes)
        };
        let Some(((src_fl, src_fr), (sink_fl, sink_fr), sink_name, sink_serial)) = desired_opt
        else {
            // Distinguish between "no sink yet" and "sink exists but FL/FR
            // is ambiguous" — the latter must be Error, not WaitingForSink,
            // so the UI does not silently create a swapped L/R route.
            let has_source =
                relay::RelayPlaybackRouter::find_relay_source_ports(&self.graph).is_some();
            let has_sink = {
                let relay = self.relay.as_ref().unwrap();
                relay
                    .router
                    .find_target_sink(&self.graph, &registry.nodes)
                    .is_some()
            };
            let relay = self.relay.as_mut().unwrap();
            if has_source && has_sink {
                let msg = "Relay playback route failed: stereo channel mapping is ambiguous (missing FL/FR channel metadata)".to_string();
                eprintln!("{msg}");
                relay.router.state = relay::RelayPlaybackState::Error(msg);
                relay.router.current_sink_name = None;
                relay.router.current_sink_serial = None;
            } else {
                if relay.router.state != relay::RelayPlaybackState::WaitingForSink {
                    eprintln!("Relay playback sink selected: waiting for output device");
                }
                relay.router.state = relay::RelayPlaybackState::WaitingForSink;
                relay.router.current_sink_name = None;
                relay.router.current_sink_serial = None;
            }
            return Ok(());
        };

        // Check if we already have correct links
        let existing_pairs: std::collections::BTreeSet<(PortId, PortId)> = self
            .graph
            .links
            .values()
            .map(|l| (l.output_port, l.input_port))
            .collect();

        let desired = vec![(src_fl, sink_fl), (src_fr, sink_fr)];
        let mut new_link_ids = Vec::new();
        let mut created = false;
        let mut error_msg: Option<String> = None;
        for (out, inp) in &desired {
            if existing_pairs.contains(&(*out, *inp)) {
                if let Some(link) = self
                    .graph
                    .links
                    .values()
                    .find(|l| l.output_port == *out && l.input_port == *inp)
                {
                    new_link_ids.push(link.id.0);
                }
                continue;
            }
            match self.connect_locked(*out, *inp) {
                Ok(link) => {
                    new_link_ids.push(link.id.0);
                    created = true;
                    eprintln!("Relay playback link created: {} -> {}", out.0, inp.0);
                }
                Err(e) => {
                    error_msg = Some(format!("Unable to create PipeWire link: {e}"));
                    eprintln!("Unable to create PipeWire link: {e}");
                    break;
                }
            }
        }
        if let Some(msg) = error_msg {
            let relay = self.relay.as_mut().unwrap();
            relay.router.state = relay::RelayPlaybackState::Error(msg);
            return Ok(());
        }

        // Remove stale tracked links that are no longer desired (e.g., old sink disappeared)
        let stale: Vec<LinkId> = {
            let relay = self.relay.as_ref().unwrap();
            relay
                .router
                .link_ids
                .iter()
                .filter(|id| !new_link_ids.contains(*id))
                .filter_map(|id| self.graph.link(LinkId(*id)).map(|l| l.id))
                .collect()
        };
        for link_id in &stale {
            let _ = self.disconnect_locked(*link_id);
            eprintln!("Relay playback link removed (sink changed)");
        }

        // Transactional verification: refresh and verify both L/R links actually exist
        // via stable endpoint check, not just cached IDs.
        if created || !stale.is_empty() {
            self.rebuild_graph_locked()?;
        }
        let verified = desired.iter().all(|(out, inp)| {
            self.graph
                .links
                .values()
                .any(|l| l.output_port == *out && l.input_port == *inp)
        });
        if !verified {
            let relay = self.relay.as_mut().unwrap();
            let msg = if desired.len() == 2 {
                "Relay playback route failed: partial stereo route (only one channel linked)"
                    .to_string()
            } else {
                "Relay playback route failed: link verification failed".to_string()
            };
            eprintln!("{msg}");
            relay.router.state = relay::RelayPlaybackState::Error(msg);
            relay.router.link_ids = new_link_ids;
            // Keep current_sink as None to force retry on next reconciliation
            relay.router.current_sink_name = None;
            relay.router.current_sink_serial = None;
            return Ok(());
        }

        let (prev_state_is_connected, sink_name_clone) = {
            let relay = self.relay.as_ref().unwrap();
            (
                relay.router.state == relay::RelayPlaybackState::Connected,
                sink_name.clone(),
            )
        };
        {
            let relay = self.relay.as_mut().unwrap();
            relay.router.link_ids = new_link_ids;
            if !prev_state_is_connected || created {
                eprintln!("Relay playback connected to sink: {sink_name_clone}");
            }
            relay.router.state = relay::RelayPlaybackState::Connected;
            relay.router.current_sink_name = Some(sink_name);
            relay.router.current_sink_serial = sink_serial;
        }
        Ok(())
    }
}

/// Whether a node is one of the relay's own `pw_filter` devices.
///
/// Those filters publish no `Props`, so their volume and mute can neither be
/// read nor written. Reporting them as unsupported keeps the card from
/// offering a fader that does nothing and a mute button stuck on "unknown".
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
    use crate::{EffectDriver, EffectNodeRequest, GraphDriver};
    use pw_graph_core::{Direction, NodeType, PortType};
    use std::collections::BTreeMap;

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
        let instance = driver
            .create_effect_node(EffectNodeRequest {
                instance_id: "qpwgraph-rs-test-effect".into(),
                effect_id: pw_graph_effects::NOISE_SUPPRESSOR_ID.into(),
                module_path: None,
                enabled: true,
                parameters: BTreeMap::new(),
                position: [12.0, 34.0],
            })
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
}
