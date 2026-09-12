//! Deterministic in-memory backend used by demo mode and tests.

#[cfg(feature = "relay")]
use super::api::RelayDriver;
use super::api::{
    BackendCapabilities, BackendError, BackendResult, EffectCreateRequest, EffectDriver,
    EffectEvent, EffectInsertRequest, EffectInstance, EffectNodeRequest, EffectTarget, GraphDriver,
    NodeAudioControl, NodeAudioState, NodeCapabilities, RecorderCreateRequest, RecorderDriver,
    RecorderId, RecorderInstance, RecorderResult, RecorderState, RecorderStatus,
};
use pw_graph_core::{
    Direction, Graph, GraphError, Link, LinkId, Node, NodeId, NodeType, Port, PortId, PortKey,
    PortType,
};
use pw_graph_effects::{
    AudioSpec, EffectComponentManager, EffectHost, EffectInstanceConfig, EffectPreparationEvent,
    EffectPrepareRequest, EffectProcessor, EffectTicket, PreparedEffect,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::router::{
    save_recording as save_recording_file, RecorderSink, RecorderWriter,
    DEFAULT_RECORDER_CAPACITY_MS,
};

/// Boost headroom, matching PipeWire so demo mode behaves like the real thing.
const DEMO_MAX_VOLUME: f32 = 1.5;

/// A deterministic demo backend that behaves like a PipeWire registry from
/// the perspective of the application. It is useful for `--demo`, examples,
/// and integration tests where a live PipeWire session is not available.
#[derive(Default)]
pub struct DemoDriver {
    graph: Graph,
    next_link_id: u64,
    audio_controls: BTreeMap<NodeId, NodeAudioControl>,
    observed_links: BTreeSet<LinkId>,
    effects: BTreeMap<String, EffectInstance>,
    effect_host: EffectHost,
    effect_processors: BTreeMap<String, Box<dyn EffectProcessor>>,
    effect_loader: EffectComponentManager,
    pending_effects: BTreeMap<EffectTicket, EffectCreateRequest>,
    next_effect_id: u64,
    recorders: BTreeMap<RecorderId, DemoRecorder>,
    next_recorder_id: RecorderId,
    /// Suppression state used by backends that remember an explicit manual
    /// disconnect. Keeping it in the demo driver makes command rollback tests
    /// able to verify that unrelated pairs are not accidentally unsuppressed.
    suppressed_connections: Vec<(PortKey, PortKey)>,
    /// Operations forced to fail. Boxed and absent by default so the common
    /// driver stays small; see [`DemoDriver::fail_connect_of`].
    forced_failures: Option<Box<ForcedFailures>>,
}

struct DemoRecorder {
    instance: RecorderInstance,
    request: RecorderCreateRequest,
    sink: Option<RecorderSink>,
    writer: Option<RecorderWriter>,
    result: Option<RecorderResult>,
}

/// Operations a test wants the driver to refuse.
///
/// Real backends reject individual connects and disconnects for reasons a
/// test cannot reproduce — a device disappearing mid-operation, a session
/// manager refusing a route. Without a hook like this, the rollback paths
/// written for exactly those cases could never be exercised.
#[derive(Default)]
struct ForcedFailures {
    connects: BTreeSet<(PortId, PortId)>,
    disconnects: BTreeSet<LinkId>,
    /// Disconnects refused by endpoint rather than by link id, so a test can
    /// arm a failure for a link a command has not created yet.
    disconnect_pairs: BTreeSet<(PortId, PortId)>,
    /// Position writes refused for an exact node/coordinate pair. The bit
    /// representation keeps the test hook deterministic without imposing an
    /// ordering on `f32`.
    positions: Vec<(NodeId, [u32; 2])>,
}

impl DemoDriver {
    pub fn new(graph: Graph) -> Self {
        let next_link_id = graph.links.keys().map(|id| id.0).max().unwrap_or(0) + 1;
        Self {
            graph,
            next_link_id,
            audio_controls: BTreeMap::new(),
            observed_links: BTreeSet::new(),
            effects: BTreeMap::new(),
            effect_host: EffectHost::new(),
            effect_processors: BTreeMap::new(),
            effect_loader: EffectComponentManager::new(2, 16),
            pending_effects: BTreeMap::new(),
            next_effect_id: 1000,
            recorders: BTreeMap::new(),
            next_recorder_id: 1,
            suppressed_connections: Vec::new(),
            forced_failures: None,
        }
    }

    /// Replace the deterministic graph used by integration tests. A real
    /// backend gets this event from its registry; tests use it to model a
    /// destroy/recreate cycle without exposing the driver's private state.
    pub fn replace_graph(&mut self, graph: Graph) {
        self.recorders.clear();
        self.next_link_id = graph.links.keys().map(|id| id.0).max().unwrap_or(0) + 1;
        self.graph = graph;
        self.observed_links.clear();
    }

    pub fn demo() -> Self {
        let mut graph = Graph::default();
        let nodes = [
            (1, "Audio Capture", [80.0, 100.0]),
            (2, "Audio Playback", [520.0, 100.0]),
            (3, "MIDI Controller", [80.0, 360.0]),
            (4, "MIDI Monitor", [520.0, 360.0]),
        ];
        for (id, name, position) in nodes {
            let mut node = Node::new(NodeId(id), name, NodeType::PipeWire);
            node.position = position;
            graph.add_node(node).expect("demo node ids are unique");
        }
        add_demo_port(
            &mut graph,
            1,
            1,
            "capture_FL",
            Direction::Source,
            PortType::Audio,
        );
        add_demo_port(
            &mut graph,
            2,
            1,
            "capture_FR",
            Direction::Source,
            PortType::Audio,
        );
        add_demo_port(
            &mut graph,
            3,
            2,
            "playback_FL",
            Direction::Sink,
            PortType::Audio,
        );
        add_demo_port(
            &mut graph,
            4,
            2,
            "playback_FR",
            Direction::Sink,
            PortType::Audio,
        );
        add_demo_port(
            &mut graph,
            5,
            3,
            "midi_out",
            Direction::Source,
            PortType::MidiJack,
        );
        add_demo_port(
            &mut graph,
            6,
            4,
            "midi_in",
            Direction::Sink,
            PortType::MidiJack,
        );
        Self::new(graph)
    }

    pub fn into_graph(self) -> Graph {
        self.graph
    }

    /// Mark a graph relationship as observed rather than user-mutable.
    ///
    /// Demo mode normally models a mutable PipeWire graph. This hook also
    /// lets deterministic tests model backends that expose informational
    /// links, such as Windows audio-session relationships.
    pub fn mark_link_observed(&mut self, link: LinkId) {
        self.observed_links.insert(link);
    }

    /// Make every future `connect` of this port pair fail, so a caller's
    /// rollback path can be tested.
    pub fn fail_connect_of(&mut self, src: PortId, dst: PortId) {
        self.forced_failures
            .get_or_insert_with(Default::default)
            .connects
            .insert((src, dst));
    }

    /// Make every future `disconnect` of this link fail.
    pub fn fail_disconnect_of(&mut self, link: LinkId) {
        self.forced_failures
            .get_or_insert_with(Default::default)
            .disconnects
            .insert(link);
    }

    /// Make every future `disconnect` of this port pair fail.
    ///
    /// The id-keyed variant can only name a link that already exists, which
    /// is no use for testing the rollback of a command that creates its own
    /// links: the id is not known until the command has already run past the
    /// point the test wants to break.
    pub fn fail_disconnect_of_pair(&mut self, src: PortId, dst: PortId) {
        self.forced_failures
            .get_or_insert_with(Default::default)
            .disconnect_pairs
            .insert((src, dst));
    }

    /// Stop refusing this port pair's connects, so a test can let a rollback
    /// succeed after the operation it was rolling back has failed.
    pub fn allow_connect_of(&mut self, src: PortId, dst: PortId) {
        if let Some(forced) = self.forced_failures.as_mut() {
            forced.connects.remove(&(src, dst));
        }
    }

    /// Seed the backend's connection-suppression state for a rollback test.
    pub fn mark_connection_suppressed(&mut self, output: PortKey, input: PortKey) {
        if !self
            .suppressed_connections
            .contains(&(output.clone(), input.clone()))
        {
            self.suppressed_connections.push((output, input));
        }
    }

    /// Inspect whether a stable pair remains suppressed.
    pub fn is_connection_suppressed(&self, output: &PortKey, input: &PortKey) -> bool {
        self.suppressed_connections
            .contains(&(output.clone(), input.clone()))
    }

    /// Make a future node-position write fail for an exact coordinate. This
    /// is used to exercise transactional layout rollback, including a failed
    /// restoration of an earlier node.
    pub fn fail_position_at(&mut self, node: NodeId, position: [f32; 2]) {
        let forced = self.forced_failures.get_or_insert_with(Default::default);
        let key = (node, [position[0].to_bits(), position[1].to_bits()]);
        if !forced.positions.contains(&key) {
            forced.positions.push(key);
        }
    }

    fn allocate_link_id(&mut self) -> LinkId {
        let id = LinkId(self.next_link_id);
        self.next_link_id += 1;
        id
    }

    fn default_recording_directory() -> PathBuf {
        std::env::temp_dir().join("qpwgraph-rs-recordings")
    }

    fn create_effect_node_internal(
        &mut self,
        request: EffectNodeRequest,
    ) -> BackendResult<EffectInstance> {
        self.create_effect_node_with_topology_internal(request, None)
    }

    fn create_effect_node_with_topology_internal(
        &mut self,
        request: EffectNodeRequest,
        topology_channels: Option<u16>,
    ) -> BackendResult<EffectInstance> {
        if self.effects.contains_key(&request.instance_id) {
            return Err(BackendError::effect_already_exists(&request.instance_id));
        }

        // Prepare and validate before touching the graph. This keeps failed
        // module/parameter requests from leaving an empty node behind.
        let mut processor = self
            .effect_host
            .create(&request.effect_id)
            .map_err(BackendError::effect_create_failed)?;
        let channels = self
            .effect_host
            .negotiate_channels(
                &request.effect_id,
                request.channel_policy,
                topology_channels,
            )
            .map_err(BackendError::native)?;
        let spec = AudioSpec {
            sample_rate: 48_000,
            channels,
            max_frames: 1024,
        };
        processor.prepare(spec).map_err(BackendError::native)?;
        pw_graph_effects::apply_parameters(&mut *processor, &request.parameters)
            .map_err(BackendError::native)?;
        // Keep the compatibility path's initial state identical to the
        // asynchronous/live backends. Stateful processors such as Hush need
        // their aligned host bypass set before they are handed to the router;
        // otherwise a restored disabled instance can emit wet audio until its
        // first explicit toggle.
        if !request.enabled {
            processor.set_host_bypass(true);
        }
        self.create_effect_node_prepared_internal(
            request,
            PreparedEffect {
                descriptor: processor.descriptor().clone(),
                spec,
                processor,
                preparation_duration_ms: 0,
            },
        )
    }

    fn create_effect_node_prepared_internal(
        &mut self,
        request: EffectNodeRequest,
        prepared: PreparedEffect,
    ) -> BackendResult<EffectInstance> {
        if self.effects.contains_key(&request.instance_id) {
            return Err(BackendError::effect_already_exists(&request.instance_id));
        }
        if prepared.descriptor.id != request.effect_id {
            return Err(BackendError::native(format!(
                "prepared effect {} does not match requested effect {}",
                prepared.descriptor.id, request.effect_id
            )));
        }
        let processor = prepared.processor;

        // PortKey identifies a saved/manual connection by node name and
        // port name. Use the stable instance id in the visible node name so
        // several copies of the same effect never collapse into one routing
        // target when a patchbay or undo command is restored.
        let node_name = format!("{} ({})", processor.descriptor().name, request.instance_id);

        let node_id = NodeId(self.next_effect_id);
        self.next_effect_id += 1;
        let input_port = PortId(self.next_effect_id);
        self.next_effect_id += 1;
        let output_port = PortId(self.next_effect_id);
        self.next_effect_id += 1;
        let mut node = Node::new(node_id, node_name, NodeType::Effect)
            .with_effect_instance(request.instance_id.clone());
        node.position = request.position;
        self.graph.add_node(node)?;
        if let Err(error) = self.graph.add_port(Port::new(
            input_port,
            node_id,
            "input",
            Direction::Sink,
            PortType::Audio,
        )) {
            self.graph.nodes.remove(&node_id);
            return Err(error.into());
        }
        if let Err(error) = self.graph.add_port(Port::new(
            output_port,
            node_id,
            "output",
            Direction::Source,
            PortType::Audio,
        )) {
            self.graph.ports.remove(&input_port);
            self.graph.nodes.remove(&node_id);
            return Err(error.into());
        }

        let instance = EffectInstance {
            config: EffectInstanceConfig {
                instance_id: request.instance_id.clone(),
                effect_id: request.effect_id,
                module_path: request.module_path,
                enabled: request.enabled,
                parameters: request.parameters,
                channel_policy: request.channel_policy,
            },
            node_id,
            input_port,
            output_port,
            source: None,
            destination: None,
            error: None,
            diagnostics: None,
            lifecycle: pw_graph_effects::EffectLifecycle::Active,
            health: pw_graph_effects::EffectHealth::Healthy,
        };
        self.effects
            .insert(instance.config.instance_id.clone(), instance.clone());
        self.effect_processors
            .insert(instance.config.instance_id.clone(), processor);
        Ok(instance)
    }

    fn begin_create_effect_internal(
        &mut self,
        request: EffectCreateRequest,
    ) -> BackendResult<EffectTicket> {
        if request.instance_id.trim().is_empty() || request.effect_id.trim().is_empty() {
            return Err(BackendError::native(
                "effect instance and effect IDs cannot be empty",
            ));
        }
        if self.effects.contains_key(&request.instance_id)
            || self
                .pending_effects
                .values()
                .any(|pending| pending.instance_id == request.instance_id)
        {
            return Err(BackendError::effect_already_exists(&request.instance_id));
        }
        if let EffectTarget::Insert {
            source,
            destination,
            ..
        } = &request.target
        {
            self.effect_link_endpoints(source, destination)?;
        }
        // The deterministic graph exposes one logical audio pair rather than
        // PipeWire's physical port-capacity envelope. Use negotiated width in
        // this compatibility backend; the real PipeWire backend uses
        // `physical_channels_for_request` and separately publishes its active
        // mask. This keeps a standalone Auto Hush instance mono in demo/tests
        // without pretending the demo graph has an unrepresented second port.
        let topology_channels = match &request.target {
            EffectTarget::Standalone { .. } => None,
            EffectTarget::Insert {
                source,
                destination,
                ..
            } => {
                self.effect_link_endpoints(source, destination)?;
                self.inferred_audio_channels(source)
            }
        };
        let channels = self
            .effect_host
            .negotiate_channels(
                &request.effect_id,
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
                        sample_rate: 48_000,
                        channels,
                        max_frames: 1024,
                    },
                    parameters: request.parameters.clone(),
                    cancellation: pw_graph_effects::EffectCancellation::default(),
                },
            )
            .map_err(BackendError::native)?;
        self.pending_effects.insert(ticket, request);
        Ok(ticket)
    }

    fn inferred_audio_channels(&self, source: &PortKey) -> Option<u16> {
        if source.port_type != PortType::Audio || source.direction != Direction::Source {
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
        Some(audio_outputs.clamp(1, u16::MAX as usize) as u16)
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
                    let Some(request) = self.pending_effects.remove(&ticket) else {
                        continue;
                    };
                    let result = match request.target {
                        EffectTarget::Standalone { position } => self
                            .create_effect_node_prepared_internal(
                                EffectNodeRequest {
                                    instance_id: request.instance_id,
                                    effect_id: request.effect_id,
                                    module_path: request.module_path,
                                    enabled: request.enabled,
                                    parameters: request.parameters,
                                    channel_policy: request.channel_policy,
                                    position,
                                },
                                prepared,
                            ),
                        EffectTarget::Insert {
                            source,
                            destination,
                            position,
                        } => self.insert_effect_prepared_internal(
                            EffectInsertRequest {
                                instance_id: request.instance_id,
                                effect_id: request.effect_id,
                                module_path: request.module_path,
                                source,
                                destination,
                                enabled: request.enabled,
                                parameters: request.parameters,
                                channel_policy: request.channel_policy,
                                position,
                            },
                            prepared,
                        ),
                    };
                    match result {
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

    fn insert_effect_prepared_internal(
        &mut self,
        request: EffectInsertRequest,
        prepared: PreparedEffect,
    ) -> BackendResult<EffectInstance> {
        let source = request.source.clone();
        let destination = request.destination.clone();
        let (output, input, original) = self.effect_link_endpoints(&source, &destination)?;
        let instance = self.create_effect_node_prepared_internal(request.into(), prepared)?;

        if let Err(error) = self.graph.remove_link(original.id) {
            let mut rollback_errors = Vec::new();
            if let Err(cleanup) = self.remove_effect_node_internal(&instance) {
                rollback_errors.push(cleanup);
            }
            return Err(Self::rollback_error(error.into(), rollback_errors));
        }
        let first = self.allocate_link_id();
        let second = self.allocate_link_id();
        if let Err(error) = self.graph.add_link(first, output, instance.input_port) {
            let mut rollback_errors = Vec::new();
            if let Err(cleanup) = self.remove_effect_node_internal(&instance) {
                rollback_errors.push(cleanup);
            }
            if let Err(restore) = self.graph.add_link(original.id, output, input) {
                rollback_errors.push(restore.into());
            }
            return Err(Self::rollback_error(error.into(), rollback_errors));
        }
        if let Err(error) = self.graph.add_link(second, instance.output_port, input) {
            let mut rollback_errors = Vec::new();
            if let Err(cleanup) = self.remove_effect_node_internal(&instance) {
                rollback_errors.push(cleanup);
            }
            if let Err(restore) = self.graph.add_link(original.id, output, input) {
                rollback_errors.push(restore.into());
            }
            return Err(Self::rollback_error(error.into(), rollback_errors));
        }
        let mut instance = instance;
        instance.source = Some(source);
        instance.destination = Some(destination);
        self.effects
            .insert(instance.config.instance_id.clone(), instance.clone());
        Ok(instance)
    }

    /// Links attached to either of an effect node's DSP ports. Both removal
    /// paths (standalone node and inserted-effect rollback) collect these.
    fn links_touching(&self, instance: &EffectInstance) -> Vec<LinkId> {
        self.graph
            .links
            .values()
            .filter(|link| {
                link.output_port == instance.output_port || link.input_port == instance.input_port
            })
            .map(|link| link.id)
            .collect()
    }

    /// Remove an effect node and all links touching it without restoring an
    /// original connection. Used for standalone-node removal and insertion
    /// rollback after a graph mutation fails. Every link-removal error is
    /// retained so a failed cleanup is never silently reported as complete.
    fn remove_effect_node_internal(&mut self, instance: &EffectInstance) -> BackendResult<()> {
        let mut errors = Vec::new();
        for link in self.links_touching(instance) {
            if let Err(error) = self.graph.remove_link(link) {
                errors.push(error.into());
            }
        }
        self.graph.ports.remove(&instance.input_port);
        self.graph.ports.remove(&instance.output_port);
        self.graph.nodes.remove(&instance.node_id);
        self.effects.remove(&instance.config.instance_id);
        self.effect_processors.remove(&instance.config.instance_id);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(Self::rollback_error(
                BackendError::native("effect-node cleanup failed"),
                errors,
            ))
        }
    }

    /// Preserve the primary failure while making incomplete graph cleanup
    /// explicit to callers. The demo backend is used by the command and
    /// patchbay tests, so it must model the same partial-rollback semantics
    /// as a real backend rather than hiding GraphError values.
    fn rollback_error(cause: BackendError, errors: Vec<BackendError>) -> BackendError {
        if errors.is_empty() {
            cause
        } else {
            let details = errors
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; ");
            BackendError::native(format!("{cause}; rollback incomplete: {details}"))
        }
    }
}

/// Compatibility name retained for callers of the original backend API.
pub type InMemoryDriver = DemoDriver;

fn add_demo_port(
    graph: &mut Graph,
    id: u64,
    node_id: u64,
    name: &str,
    direction: Direction,
    port_type: PortType,
) {
    graph
        .add_port(Port::new(
            PortId(id),
            NodeId(node_id),
            name,
            direction,
            port_type,
        ))
        .expect("demo port ids are unique");
}

impl GraphDriver for DemoDriver {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            topology: true,
            connect: true,
            disconnect: true,
            volume: true,
            mute: true,
            meters: true,
            effects: true,
            relay: false,
            recorders: true,
        }
    }

    fn refresh(&mut self) -> BackendResult<Vec<Node>> {
        Ok(self.graph.nodes.values().cloned().collect())
    }

    fn connect(&mut self, src: PortId, dst: PortId) -> BackendResult<Link> {
        if self
            .forced_failures
            .as_ref()
            .is_some_and(|forced| forced.connects.contains(&(src, dst)))
        {
            return Err(BackendError::native("connect refused by the test driver"));
        }
        let link_id = self.allocate_link_id();
        let link = self.graph.add_link(link_id, src, dst)?;
        Ok(link)
    }

    fn disconnect(&mut self, link: LinkId) -> BackendResult<Link> {
        let endpoints = self
            .graph
            .links
            .get(&link)
            .map(|link| (link.output_port, link.input_port));
        if self.forced_failures.as_ref().is_some_and(|forced| {
            forced.disconnects.contains(&link)
                || endpoints.is_some_and(|pair| forced.disconnect_pairs.contains(&pair))
        }) {
            return Err(BackendError::native(
                "disconnect refused by the test driver",
            ));
        }
        Ok(self.graph.remove_link(link)?)
    }

    fn is_link_mutable(&self, link: LinkId) -> bool {
        self.graph.link(link).is_some() && !self.observed_links.contains(&link)
    }

    fn set_node_position(&mut self, node: NodeId, position: [f32; 2]) -> BackendResult<()> {
        if self.forced_failures.as_ref().is_some_and(|forced| {
            forced
                .positions
                .contains(&(node, [position[0].to_bits(), position[1].to_bits()]))
        }) {
            return Err(BackendError::native(
                "node position refused by the test driver",
            ));
        }
        self.graph
            .nodes
            .get_mut(&node)
            .ok_or(GraphError::MissingNode(node))?
            .position = position;
        Ok(())
    }

    fn set_node_mute(&mut self, node: NodeId, muted: bool) -> BackendResult<()> {
        if !self.graph.nodes.contains_key(&node) {
            return Err(GraphError::MissingNode(node).into());
        }
        self.audio_controls.entry(node).or_default().muted = muted;
        Ok(())
    }

    fn set_node_volume(&mut self, node: NodeId, volume: f32) -> BackendResult<()> {
        if !self.graph.nodes.contains_key(&node) {
            return Err(GraphError::MissingNode(node).into());
        }
        self.audio_controls.entry(node).or_default().volume = volume.clamp(0.0, DEMO_MAX_VOLUME);
        Ok(())
    }

    /// The demo driver owns its controls outright, so this is a real read of
    /// backend state rather than a placeholder. Effect nodes carry no audio
    /// controls, exactly as a DSP node would not on a native backend.
    fn node_audio_state(&self, node: NodeId) -> BackendResult<NodeAudioState> {
        let record = self
            .graph
            .nodes
            .get(&node)
            .ok_or(GraphError::MissingNode(node))?;
        if matches!(record.node_type, NodeType::Effect | NodeType::Recorder) {
            return Ok(NodeAudioState::UNSUPPORTED);
        }
        let control = self.audio_controls.get(&node).copied().unwrap_or_default();
        Ok(NodeAudioState::readable(control.volume, control.muted))
    }

    fn node_capabilities(&self, node: NodeId) -> NodeCapabilities {
        let Ok(state) = self.node_audio_state(node) else {
            return NodeCapabilities::NONE;
        };
        let mut capabilities = state.control_capabilities();
        // Effect nodes are pass-through DSP: nothing to meter and nothing to
        // control, so they must not be given a meter either.
        if state.is_supported() {
            capabilities.volume_max = DEMO_MAX_VOLUME;
            capabilities.meter_peak = true;
            capabilities.meter_rms = true;
        }
        capabilities
    }

    fn graph(&self) -> &Graph {
        &self.graph
    }

    fn is_node_type(&self, node_type: NodeType) -> bool {
        matches!(
            node_type,
            NodeType::PipeWire | NodeType::Effect | NodeType::Recorder
        )
    }

    fn allow_connection(&mut self, output: &PortKey, input: &PortKey) {
        self.suppressed_connections
            .retain(|pair| pair != &(output.clone(), input.clone()));
    }

    fn suppress_connection(&mut self, output: &PortKey, input: &PortKey) {
        if !self
            .suppressed_connections
            .contains(&(output.clone(), input.clone()))
        {
            self.suppressed_connections
                .push((output.clone(), input.clone()));
        }
    }
}

impl RecorderDriver for DemoDriver {
    fn supports_recorders(&self) -> bool {
        true
    }

    fn create_recorder(
        &mut self,
        request: RecorderCreateRequest,
    ) -> BackendResult<RecorderInstance> {
        let id = self.next_recorder_id;
        self.next_recorder_id = self.next_recorder_id.saturating_add(1);
        let mut node_id = NodeId(self.next_effect_id);
        while self.graph.nodes.contains_key(&node_id) {
            self.next_effect_id = self.next_effect_id.saturating_add(1);
            node_id = NodeId(self.next_effect_id);
        }
        self.next_effect_id = self.next_effect_id.saturating_add(1);
        let input_port = PortId(self.next_effect_id);
        self.next_effect_id = self.next_effect_id.saturating_add(1);
        let name = if request.name.trim().is_empty() {
            format!("Recorder {id}")
        } else {
            request.name.clone()
        };
        let mut node = Node::new(node_id, name, NodeType::Recorder);
        node.position = request.position;
        self.graph.add_node(node)?;
        if let Err(error) = self.graph.add_port(Port::new(
            input_port,
            node_id,
            "record",
            Direction::Sink,
            PortType::Audio,
        )) {
            self.graph.nodes.remove(&node_id);
            return Err(error.into());
        }
        let status = RecorderStatus {
            id,
            state: RecorderState::Idle,
            writer_state: crate::router::RecorderWriterState::Starting,
            sample_rate: request.format.sample_rate,
            channels: request.format.channels,
            ..RecorderStatus::default()
        };
        let instance = RecorderInstance {
            id,
            node_id,
            input_port,
            status,
        };
        self.recorders.insert(
            id,
            DemoRecorder {
                instance: instance.clone(),
                request,
                sink: None,
                writer: None,
                result: None,
            },
        );
        Ok(instance)
    }

    fn remove_recorder(&mut self, id: RecorderId) -> BackendResult<()> {
        let recorder = self
            .recorders
            .get(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        if recorder.instance.status.state == RecorderState::Recording {
            return Err(BackendError::native(
                "stop or discard the recording before removing its node",
            ));
        }
        if recorder.instance.status.temporary_path.is_some()
            && matches!(
                recorder.instance.status.state,
                RecorderState::Unsaved | RecorderState::Error
            )
        {
            return Err(BackendError::native(
                "save or explicitly discard the pending recording before removing its node",
            ));
        }
        let recorder = self
            .recorders
            .remove(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        self.graph
            .links
            .retain(|_, link| link.input_port != recorder.instance.input_port);
        self.graph.ports.remove(&recorder.instance.input_port);
        self.graph.nodes.remove(&recorder.instance.node_id);
        Ok(())
    }

    fn start_recording(&mut self, id: RecorderId) -> BackendResult<()> {
        let directory = self
            .recorders
            .get(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?
            .request
            .recording_dir
            .clone()
            .unwrap_or_else(Self::default_recording_directory);
        let recorder = self
            .recorders
            .get_mut(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        if recorder.instance.status.state == RecorderState::Recording {
            return Err(BackendError::native("recorder is already recording"));
        }
        if recorder.instance.status.state != RecorderState::Idle {
            return Err(BackendError::native(
                "save or discard the pending recording before starting again",
            ));
        }
        let capacity = recorder.request.capacity_frames.max(
            (usize::try_from(recorder.request.format.sample_rate)
                .unwrap_or(48_000)
                .saturating_mul(DEFAULT_RECORDER_CAPACITY_MS as usize)
                / 1_000)
                .max(1),
        );
        let (sink, writer) =
            RecorderWriter::start_pending(directory, recorder.request.format, capacity)
                .map_err(|error| BackendError::native(error.to_string()))?;
        let path = writer.temporary_path().to_owned();
        recorder.sink = Some(sink);
        recorder.writer = Some(writer);
        recorder.result = None;
        recorder.instance.status = RecorderStatus {
            id,
            state: RecorderState::Recording,
            writer_state: crate::router::RecorderWriterState::Recording,
            sample_rate: recorder.request.format.sample_rate,
            channels: recorder.request.format.channels,
            temporary_path: Some(path),
            queue_capacity: capacity,
            ..RecorderStatus::default()
        };
        Ok(())
    }

    fn request_stop_recording(&mut self, id: RecorderId) -> BackendResult<()> {
        let recorder = self
            .recorders
            .get_mut(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        if recorder.instance.status.state != RecorderState::Recording {
            return Err(BackendError::native("recorder is not recording"));
        }
        recorder
            .writer
            .as_mut()
            .ok_or_else(|| BackendError::native("recorder is not recording"))?
            .request_stop()
            .map_err(|error| BackendError::native(error.to_string()))
    }

    fn stop_recording(&mut self, id: RecorderId) -> BackendResult<RecorderResult> {
        let recorder = self
            .recorders
            .get_mut(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        let writer = recorder
            .writer
            .as_mut()
            .ok_or_else(|| BackendError::native("recorder is not recording"))?;
        let result = writer
            .finish()
            .map_err(|error| BackendError::native(error.to_string()))?;
        let result = RecorderResult {
            id,
            temporary_path: result.temporary_path,
            elapsed_frames: result.frames_written,
            sample_rate: recorder.request.format.sample_rate,
            channels: recorder.request.format.channels,
            frames_written: result.frames_written,
            dropped_frames: result.dropped_frames,
            file_bytes: result.file_bytes,
        };
        recorder.instance.status.state = RecorderState::Unsaved;
        recorder.instance.status.writer_state = crate::router::RecorderWriterState::Finished;
        recorder.instance.status.elapsed_frames = result.elapsed_frames;
        recorder.instance.status.frames_written = result.frames_written;
        recorder.instance.status.dropped_frames = result.dropped_frames;
        recorder.instance.status.file_bytes = result.file_bytes;
        recorder.instance.status.temporary_path = Some(result.temporary_path.clone());
        recorder.result = Some(result.clone());
        Ok(result)
    }

    fn save_recording(
        &mut self,
        id: RecorderId,
        destination: &std::path::Path,
    ) -> BackendResult<PathBuf> {
        let recorder = self
            .recorders
            .get_mut(&id)
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
        if let Some(writer) = recorder.writer.as_mut() {
            if writer.status().writer_state != crate::router::RecorderWriterState::Finished {
                let _ = writer
                    .finish()
                    .map_err(|error| BackendError::native(error.to_string()))?;
            }
        }
        save_recording_file(&source, destination)
            .map_err(|error| BackendError::native(error.to_string()))?;
        let destination = destination.to_owned();
        recorder.sink = None;
        recorder.writer = None;
        recorder.result = None;
        recorder.instance.status.state = RecorderState::Idle;
        recorder.instance.status.writer_state = crate::router::RecorderWriterState::Starting;
        recorder.instance.status.final_path = Some(destination.clone());
        recorder.instance.status.temporary_path = None;
        Ok(destination)
    }

    fn recorder_status(&self, id: RecorderId) -> BackendResult<RecorderStatus> {
        let recorder = self
            .recorders
            .get(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        let mut status = recorder.instance.status.clone();
        if let Some(writer) = recorder.writer.as_ref() {
            let diagnostics = writer.status();
            status.elapsed_frames = diagnostics.frames_written;
            status.frames_written = diagnostics.frames_written;
            status.dropped_frames = diagnostics.dropped_frames;
            status.queue_depth = diagnostics.queue_depth;
            status.queue_capacity = diagnostics.queue_capacity;
            status.file_bytes = diagnostics.file_bytes;
            status.error = diagnostics.last_error;
            status.writer_state = diagnostics.writer_state;
            if diagnostics.writer_state == crate::router::RecorderWriterState::Error {
                status.state = RecorderState::Error;
            }
        }
        Ok(status)
    }

    fn poll_recording(&mut self, id: RecorderId) -> BackendResult<Option<RecorderResult>> {
        let recorder = self
            .recorders
            .get_mut(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        let Some(writer) = recorder.writer.as_mut() else {
            return Ok(None);
        };
        let polled = writer.poll();
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
        let result = RecorderResult {
            id,
            temporary_path: result.temporary_path,
            elapsed_frames: result.frames_written,
            sample_rate: recorder.request.format.sample_rate,
            channels: recorder.request.format.channels,
            frames_written: result.frames_written,
            dropped_frames: result.dropped_frames,
            file_bytes: result.file_bytes,
        };
        recorder.instance.status.state = RecorderState::Unsaved;
        recorder.instance.status.writer_state = crate::router::RecorderWriterState::Finished;
        recorder.instance.status.elapsed_frames = result.elapsed_frames;
        recorder.instance.status.frames_written = result.frames_written;
        recorder.instance.status.dropped_frames = result.dropped_frames;
        recorder.instance.status.file_bytes = result.file_bytes;
        recorder.instance.status.temporary_path = Some(result.temporary_path.clone());
        recorder.result = Some(result.clone());
        Ok(Some(result))
    }

    fn discard_recording(&mut self, id: RecorderId) -> BackendResult<()> {
        let recorder = self
            .recorders
            .get_mut(&id)
            .ok_or_else(|| BackendError::native(format!("unknown recorder {id}")))?;
        if recorder.instance.status.state == RecorderState::Recording {
            return Err(BackendError::native(
                "stop the recorder before discarding it",
            ));
        }
        if let Some(path) = recorder.instance.status.temporary_path.take() {
            if path.exists() {
                std::fs::remove_file(path)
                    .map_err(|error| BackendError::native(error.to_string()))?;
            }
        }
        recorder.sink = None;
        recorder.writer = None;
        recorder.result = None;
        recorder.instance.status = RecorderStatus {
            id,
            state: RecorderState::Idle,
            writer_state: crate::router::RecorderWriterState::Starting,
            sample_rate: recorder.request.format.sample_rate,
            channels: recorder.request.format.channels,
            ..RecorderStatus::default()
        };
        Ok(())
    }
}

#[cfg(feature = "relay")]
impl RelayDriver for DemoDriver {}

impl EffectDriver for DemoDriver {
    fn effect_descriptors(&self) -> Vec<pw_graph_effects::EffectDescriptor> {
        self.effect_host.descriptors()
    }

    fn effect_instances(&self) -> Vec<EffectInstance> {
        self.effects.values().cloned().collect()
    }

    fn supports_effect_nodes(&self) -> bool {
        true
    }

    fn begin_create_effect(&mut self, request: EffectCreateRequest) -> BackendResult<EffectTicket> {
        self.begin_create_effect_internal(request)
    }

    fn poll_effect_events(&mut self) -> BackendResult<Vec<EffectEvent>> {
        self.poll_effect_lifecycle()
    }

    fn cancel_effect(&mut self, ticket: EffectTicket) -> BackendResult<()> {
        if self.effect_loader.cancel(ticket) {
            Ok(())
        } else {
            Err(BackendError::native(format!(
                "unknown or completed effect ticket {}",
                ticket.0
            )))
        }
    }

    fn create_effect_node(&mut self, request: EffectNodeRequest) -> BackendResult<EffectInstance> {
        self.create_effect_node_internal(request)
    }

    fn insert_effect(&mut self, request: EffectInsertRequest) -> BackendResult<EffectInstance> {
        let source = request.source.clone();
        let destination = request.destination.clone();
        let (output, input, original) = self.effect_link_endpoints(&source, &destination)?;

        let request_for_node: EffectNodeRequest = request.into();
        let topology_channels = self.inferred_audio_channels(&source);
        let mut instance =
            self.create_effect_node_with_topology_internal(request_for_node, topology_channels)?;

        // Commit the link rewrite only after the free node has been fully
        // created. Every failure below removes that node and restores the
        // original direct connection.
        if let Err(error) = self.graph.remove_link(original.id) {
            // The effect node was already inserted, so a failed removal of
            // the original route must not leave that new node behind while
            // the original graph is still connected. Cleanup is best effort,
            // but every cleanup failure is part of the returned error.
            let mut rollback_errors = Vec::new();
            if let Err(cleanup) = self.remove_effect_node_internal(&instance) {
                rollback_errors.push(cleanup);
            }
            return Err(Self::rollback_error(error.into(), rollback_errors));
        }
        let first = self.allocate_link_id();
        let second = self.allocate_link_id();
        if let Err(error) = self.graph.add_link(first, output, instance.input_port) {
            let mut rollback_errors = Vec::new();
            if let Err(cleanup) = self.remove_effect_node_internal(&instance) {
                rollback_errors.push(cleanup);
            }
            if let Err(restore) = self.graph.add_link(original.id, output, input) {
                rollback_errors.push(restore.into());
            }
            return Err(Self::rollback_error(error.into(), rollback_errors));
        }
        if let Err(error) = self.graph.add_link(second, instance.output_port, input) {
            let mut rollback_errors = Vec::new();
            // Removing the effect node also removes every route created for
            // it, including `first`, and reports every failure.
            if let Err(cleanup) = self.remove_effect_node_internal(&instance) {
                rollback_errors.push(cleanup);
            }
            if let Err(restore) = self.graph.add_link(original.id, output, input) {
                rollback_errors.push(restore.into());
            }
            return Err(Self::rollback_error(error.into(), rollback_errors));
        }
        instance.source = Some(source);
        instance.destination = Some(destination);
        self.effects
            .insert(instance.config.instance_id.clone(), instance.clone());
        Ok(instance)
    }

    fn set_effect_enabled(&mut self, instance_id: &str, enabled: bool) -> BackendResult<()> {
        let instance = self
            .effects
            .get_mut(instance_id)
            .ok_or_else(|| BackendError::unknown_effect_instance(instance_id))?;
        if instance.config.enabled != enabled {
            if let Some(processor) = self.effect_processors.get_mut(instance_id) {
                if !processor.set_host_bypass(!enabled) {
                    processor.reset();
                }
            }
        }
        instance.config.enabled = enabled;
        Ok(())
    }

    fn set_effect_parameter(
        &mut self,
        instance_id: &str,
        parameter: &str,
        value: f32,
    ) -> BackendResult<()> {
        let processor = self
            .effect_processors
            .get_mut(instance_id)
            .ok_or_else(|| BackendError::unknown_effect_instance(instance_id))?;
        processor
            .set_parameter(parameter, value)
            .map_err(BackendError::native)?;
        let instance = self
            .effects
            .get_mut(instance_id)
            .ok_or_else(|| BackendError::unknown_effect_instance(instance_id))?;
        instance.config.parameters.insert(parameter.into(), value);
        Ok(())
    }

    fn remove_effect(&mut self, instance_id: &str) -> BackendResult<()> {
        let instance = self
            .effects
            .get(instance_id)
            .cloned()
            .ok_or_else(|| BackendError::unknown_effect_instance(instance_id))?;

        let restored_endpoints = match (&instance.source, &instance.destination) {
            (Some(source), Some(destination)) => {
                Some(self.effect_restore_endpoints(source, destination)?)
            }
            (None, None) => None,
            _ => return Err(BackendError::effect_routing_incomplete()),
        };
        let links = self.links_touching(&instance);
        let mut removed_links = Vec::with_capacity(links.len());
        for link in links {
            match self.graph.remove_link(link) {
                Ok(removed) => removed_links.push(removed),
                Err(error) => {
                    let mut rollback_errors = Vec::new();
                    for removed in removed_links.into_iter().rev() {
                        if let Err(restore) = self.graph.insert_existing_link(removed) {
                            rollback_errors.push(restore.into());
                        }
                    }
                    return Err(Self::rollback_error(error.into(), rollback_errors));
                }
            }
        }
        if let Some((source, destination)) = restored_endpoints {
            let link_id = self.allocate_link_id();
            if let Err(error) = self.graph.add_link(link_id, source, destination) {
                let mut rollback_errors = Vec::new();
                for link in removed_links {
                    if let Err(restore) = self.graph.insert_existing_link(link) {
                        rollback_errors.push(restore.into());
                    }
                }
                return Err(Self::rollback_error(error.into(), rollback_errors));
            }
        }
        self.remove_effect_node_internal(&instance)
    }
}
