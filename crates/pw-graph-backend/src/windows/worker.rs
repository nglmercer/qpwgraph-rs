//! The Core Audio worker thread.
//!
//! Everything here runs on one thread with its own COM apartment: endpoint
//! and session enumeration, meters, and the graph it rebuilds from them.
//! The driver never reaches in; it sends a [`WorkerCommand`] and receives a
//! [`WorkerSnapshot`].

use super::*;

const MAX_PROCESS_METER_CAPTURES: usize = 32;

fn bound_process_meter_targets(
    targets: impl IntoIterator<Item = ProcessMeterTarget>,
) -> Vec<ProcessMeterTarget> {
    targets
        .into_iter()
        .take(MAX_PROCESS_METER_CAPTURES)
        .collect()
}

fn process_meter_levels(
    process_meter: Option<&ProcessMeterReading>,
    native_peak: Option<f32>,
) -> Option<(f32, f32, u32)> {
    let process_meter = process_meter.filter(|meter| meter.available);
    let peak = process_meter.map(|meter| meter.peak).or(native_peak)?;
    Some((
        process_meter.map_or(0.0, |meter| meter.rms),
        peak,
        process_meter.map_or(0, |meter| meter.age_ms),
    ))
}

/// One refresh answer. Audio state is not carried here -- it lives in the
/// shared map, which the refresh fills and the callbacks keep current.
pub(super) struct WorkerSnapshot {
    pub(super) graph: Graph,
    /// Nodes that can report a level: endpoints, and sessions whose control
    /// answered the meter query.
    pub(super) meterable: BTreeSet<NodeId>,
    /// Ports the router can open a real WASAPI stream for.
    ///
    /// Session ports are present only for render sessions already attached to
    /// QPWGraph Virtual Output. That documented user action proves the dry
    /// path is isolated; ordinary sessions remain observed-only.
    pub(super) endpoint_ports: BTreeMap<PortId, EndpointPort>,
    pub(super) endpoint_selectors: BTreeMap<String, WindowsEndpointSelector>,
    /// Per-application capabilities. Capture-only capabilities are separate
    /// from the mutable route proof in `endpoint_ports`.
    pub(super) process_audio_capabilities: BTreeMap<NodeId, ProcessAudioCapabilities>,
    /// Stable render-session candidates used by the persisted application
    /// route reconciler. The live PID is runtime-only and never persisted.
    pub(super) application_route_candidates: Vec<ApplicationRouteCandidate>,
    /// Runtime source ports for isolated application sessions. These are
    /// looked up by selector/PID during route restoration and never stored in
    /// configuration.
    pub(super) application_route_ports: BTreeMap<(String, u32), PortId>,
    /// Safe, bounded diagnostics for process-loopback workers owned by this
    /// COM worker. The actual PCM never leaves the worker through a snapshot.
    pub(super) process_captures: Vec<ProcessCaptureStatus>,
    /// Endpoint roles that were proved by the driver service/property
    /// contract. Friendly names are intentionally absent from this proof.
    pub(super) virtual_endpoint_identities: Vec<QpwVirtualEndpointIdentity>,
    pub(super) virtual_driver_health: VirtualAudioDriverHealth,
    /// Render endpoints as `(device id, display name)`, for relay selection.
    #[cfg(feature = "relay")]
    pub(super) playback_endpoints: Vec<(String, String)>,
    /// Capture endpoints as `(device id, display name)`, for relay selection.
    #[cfg(feature = "relay")]
    pub(super) capture_endpoints: Vec<(String, String)>,
    /// Live render sessions that can be captured read-only by process
    /// loopback as `(stable process selector, display name, current PID)`.
    #[cfg(feature = "relay")]
    pub(super) application_sources: Vec<(String, String, u32)>,
    /// Current defaults and a monotonically changing notification generation.
    /// The relay uses the generation even when both requested ids are `None`,
    /// because `None` means "follow default", not "nothing changed".
    #[cfg(feature = "relay")]
    pub(super) default_input: Option<String>,
    #[cfg(feature = "relay")]
    pub(super) default_output: Option<String>,
    #[cfg(feature = "relay")]
    pub(super) default_generation: u64,
}

/// What a port means to the router: which Core Audio device, and which way
/// audio crosses it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct EndpointPort {
    pub(super) device_id: String,
    pub(super) role: EndpointPortRole,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum EndpointPortRole {
    /// A recording device. Opens as a capture source.
    Capture,
    /// A playback device's monitor. Opens as a loopback source, so whatever
    /// that device is playing can be routed on.
    Monitor,
    /// A playback device. Opens as a render sink.
    Render,
    /// An application already isolated on QPWGraph Virtual Output.
    /// The selector is carried alongside the PID so route activation can
    /// re-check identity immediately before opening process loopback.
    Process { pid: u32, selector: String },
}

#[derive(Debug)]
pub(super) enum WorkerCommand {
    Refresh(Sender<BackendResult<WorkerSnapshot>>),
    RefreshIfNeeded(Sender<BackendResult<WorkerSnapshot>>),
    SetVolume(NodeId, f32, Sender<BackendResult<()>>),
    SetMute(NodeId, bool, Sender<BackendResult<()>>),
    SetMeterPolicy(MeterPolicy, Sender<BackendResult<()>>),
    RequestMeters(BTreeSet<NodeId>, Sender<BackendResult<()>>),
    ReconcileProcessCaptures(
        Vec<ProcessCaptureRequest>,
        Sender<BackendResult<Vec<ProcessCaptureStatus>>>,
    ),
    #[cfg(feature = "relay")]
    SetExternalRelayCapture(
        Option<ProcessCaptureKey>,
        Sender<BackendResult<Vec<ProcessCaptureStatus>>>,
    ),
    AudioMeters(Sender<BackendResult<(Vec<AudioMeter>, Vec<ProcessCaptureStatus>)>>),
    ResetAudio(Sender<BackendResult<()>>),
    Shutdown,
}

pub(super) fn worker_thread(
    command_rx: Receiver<WorkerCommand>,
    ready_tx: Sender<BackendResult<WorkerSnapshot>>,
    dirty: Arc<AtomicBool>,
    topology_dirty: Arc<AtomicBool>,
    session_dirty_endpoints: Arc<Mutex<BTreeSet<String>>>,
    audio_states: AudioStateMap,
) {
    let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
    if initialized.is_err() {
        let _ = ready_tx.send(Err(BackendError::Native(format!(
            "could not initialize Windows COM: {initialized:?}"
        ))));
        return;
    }

    let worker = CoreAudioWorker::new(
        Arc::clone(&dirty),
        Arc::clone(&topology_dirty),
        Arc::clone(&session_dirty_endpoints),
        audio_states,
    );
    let mut worker = match worker {
        Ok(worker) => worker,
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            unsafe { Com::CoUninitialize() };
            return;
        }
    };

    match worker.refresh_graph() {
        Ok(snapshot) => {
            let _ = ready_tx.send(Ok(snapshot));
        }
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            unsafe { Com::CoUninitialize() };
            return;
        }
    }

    while let Ok(command) = command_rx.recv() {
        match command {
            WorkerCommand::Refresh(sender) => {
                let _ = sender.send(worker.refresh_graph());
            }
            WorkerCommand::RefreshIfNeeded(sender) => {
                let _ = sender.send(worker.refresh_if_needed());
            }
            WorkerCommand::SetVolume(node, volume, sender) => {
                let _ = sender.send(worker.set_volume(node, volume));
            }
            WorkerCommand::SetMute(node, muted, sender) => {
                let _ = sender.send(worker.set_mute(node, muted));
            }
            WorkerCommand::SetMeterPolicy(policy, sender) => {
                worker.meter_policy = policy;
                let _ = sender.send(Ok(()));
            }
            WorkerCommand::RequestMeters(nodes, sender) => {
                worker.requested_meters = nodes;
                let _ = sender.send(Ok(()));
            }
            WorkerCommand::ReconcileProcessCaptures(requests, sender) => {
                worker
                    .process_captures
                    .reconcile_routes(requests, crate::router::AudioFormat::new(48_000, 2));
                let _ = sender.send(Ok(worker.process_captures.statuses()));
            }
            #[cfg(feature = "relay")]
            WorkerCommand::SetExternalRelayCapture(key, sender) => {
                worker.process_captures.set_external_relay(key);
                let _ = sender.send(Ok(worker.process_captures.statuses()));
            }
            WorkerCommand::AudioMeters(sender) => {
                let result = worker
                    .audio_meters()
                    .map(|meters| (meters, worker.process_captures.statuses()));
                let _ = sender.send(result);
            }
            WorkerCommand::ResetAudio(sender) => {
                worker.requested_meters.clear();
                let _ = sender.send(Ok(()));
            }
            WorkerCommand::Shutdown => break,
        }
    }

    drop(worker);
    unsafe { Com::CoUninitialize() };
}

#[derive(Clone)]
pub(super) struct EndpointRecord {
    pub(super) id: String,
    pub(super) flow: Audio::EDataFlow,
    pub(super) device: Audio::IMMDevice,
    pub(super) node_id: NodeId,
    pub(super) port_id: PortId,
    /// A playback endpoint's monitor port. Capture endpoints have none:
    /// there is nothing to read back from a microphone that its own port does
    /// not already carry.
    pub(super) monitor_port_id: Option<PortId>,
    pub(super) selector: WindowsEndpointSelector,
    /// Present only after the endpoint provider has proved both qpwgraph
    /// service ownership and the semantic role property.
    pub(super) virtual_identity: Option<QpwVirtualEndpointIdentity>,
}

pub(super) struct SessionRecord {
    pub(super) endpoint_id: String,
    pub(super) session_id: String,
    pub(super) flow: Audio::EDataFlow,
    pub(super) node_id: NodeId,
    pub(super) port_id: PortId,
    pub(super) process_id: u32,
    /// Stable identity resolved while this live session was enumerated. The
    /// PID is still useful for the immediate activation, but never stands in
    /// for this selector when a process restarts.
    pub(super) process_identity: Option<ProcessIdentity>,
    /// The session's own peak meter, kept so a level can be read without
    /// re-enumerating the endpoint every frame.
    ///
    /// `IAudioMeterInformation` is documented as an endpoint facility, but a
    /// session control implements it too, and that is the only per-application
    /// level Windows offers short of process loopback capture -- which needs
    /// build 20348. Verified against a played tone: a 0.4 amplitude sine reads
    /// back as 0.39999998 on the owning session and 0.0 on every other.
    pub(super) meter: Option<IAudioMeterInformation>,
}

pub(super) struct CoreAudioWorker {
    pub(super) enumerator: Audio::IMMDeviceEnumerator,
    pub(super) endpoint_notification: Audio::IMMNotificationClient,
    pub(super) dirty: Arc<AtomicBool>,
    pub(super) topology_dirty: Arc<AtomicBool>,
    pub(super) session_dirty_endpoints: Arc<Mutex<BTreeSet<String>>>,
    pub(super) graph: Graph,
    pub(super) session_notifications: Vec<(
        String,
        Audio::IAudioSessionManager2,
        Audio::IAudioSessionNotification,
    )>,
    pub(super) session_events: Vec<(
        String,
        Audio::IAudioSessionControl,
        Audio::IAudioSessionEvents,
    )>,
    pub(super) endpoints: Vec<EndpointRecord>,
    pub(super) sessions: Vec<SessionRecord>,
    pub(super) process_captures: ProcessCaptureManager,
    pub(super) meter_policy: MeterPolicy,
    pub(super) requested_meters: BTreeSet<NodeId>,
    /// Shared with the public driver and every change callback.
    pub(super) audio_states: AudioStateMap,
    /// Endpoint volume callbacks, kept registered for the endpoint's lifetime.
    pub(super) endpoint_volume_events: Vec<(
        IAudioEndpointVolume,
        Audio::Endpoints::IAudioEndpointVolumeCallback,
    )>,
    /// Incremented by `OnDefaultDeviceChanged`; read into each public
    /// snapshot so the driver can restart only default-following relay paths.
    #[cfg(feature = "relay")]
    pub(super) default_generation: Arc<AtomicU64>,
}

impl CoreAudioWorker {
    pub(super) fn new(
        dirty: Arc<AtomicBool>,
        topology_dirty: Arc<AtomicBool>,
        session_dirty_endpoints: Arc<Mutex<BTreeSet<String>>>,
        audio_states: AudioStateMap,
    ) -> BackendResult<Self> {
        let enumerator: Audio::IMMDeviceEnumerator =
            unsafe { Com::CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| native_error("create MMDeviceEnumerator", error))?;
        #[cfg(feature = "relay")]
        let default_generation = Arc::new(AtomicU64::new(0));
        let endpoint_notification: Audio::IMMNotificationClient = EndpointNotificationClient {
            dirty: Arc::clone(&dirty),
            topology_dirty: Arc::clone(&topology_dirty),
            #[cfg(feature = "relay")]
            default_generation: Arc::clone(&default_generation),
        }
        .into();
        unsafe {
            enumerator
                .RegisterEndpointNotificationCallback(&endpoint_notification)
                .map_err(|error| native_error("register endpoint notifications", error))?;
        }
        Ok(Self {
            enumerator,
            endpoint_notification,
            dirty,
            topology_dirty,
            session_dirty_endpoints,
            graph: Graph::default(),
            session_notifications: Vec::new(),
            session_events: Vec::new(),
            audio_states,
            endpoint_volume_events: Vec::new(),
            #[cfg(feature = "relay")]
            default_generation,
            endpoints: Vec::new(),
            sessions: Vec::new(),
            process_captures: ProcessCaptureManager::new(),
            meter_policy: MeterPolicy::OnDemand,
            requested_meters: BTreeSet::new(),
        })
    }

    pub(super) fn refresh_graph(&mut self) -> BackendResult<WorkerSnapshot> {
        // Consume the reason for this rebuild before touching COM. A callback
        // that races the refresh sets the flag again and will be observed by
        // the next command; clearing it at the end would lose that event.
        self.dirty.store(false, Ordering::Release);
        self.topology_dirty.store(false, Ordering::Release);
        take_session_dirty_endpoints(&self.session_dirty_endpoints);
        self.clear_session_callbacks();
        let mut endpoint_specs = Vec::new();
        for flow in [Audio::eRender, Audio::eCapture] {
            endpoint_specs.extend(self.enumerate_endpoints(flow)?);
        }
        endpoint_specs.sort_by(|left, right| left.0.cmp(&right.0));

        let mut graph = Graph::default();
        let mut endpoints = Vec::with_capacity(endpoint_specs.len());
        let mut sessions = Vec::new();

        for (endpoint_id, flow, device) in endpoint_specs {
            let node_id = NodeId(graph_id(endpoint_node_local_id(&endpoint_id)));
            let port_id = PortId(graph_id(endpoint_port_local_id(&endpoint_id)));
            let direction = endpoint_direction(flow);
            let name = endpoint_name(&device).unwrap_or_else(|| {
                format!(
                    "Windows {} endpoint",
                    if flow == Audio::eRender {
                        "playback"
                    } else {
                        "capture"
                    }
                )
            });
            graph.add_node(
                Node::new(node_id, name, NodeType::WindowsAudioEndpoint)
                    .with_serial(stable_local_id(&format!("endpoint:{endpoint_id}"))),
            )?;
            graph.add_port(Port::new(
                port_id,
                node_id,
                "audio",
                direction,
                PortType::Audio,
            ))?;
            // A playback endpoint also gets a monitor, the way a PipeWire sink
            // does: it is what makes "send what these speakers are playing
            // somewhere else" a link the user can draw. WASAPI loopback is the
            // Windows mechanism behind it.
            let monitor_port_id = (flow == Audio::eRender)
                .then(|| PortId(graph_id(endpoint_monitor_port_local_id(&endpoint_id))));
            if let Some(monitor_port_id) = monitor_port_id {
                graph.add_port(Port::new(
                    monitor_port_id,
                    node_id,
                    "monitor",
                    Direction::Source,
                    PortType::Audio,
                ))?;
            }

            let data_flow = if flow == Audio::eRender {
                AudioFlow::Render
            } else {
                AudioFlow::Capture
            };
            let selector = WindowsEndpointSelector::from_device(&device, data_flow).unwrap_or(
                WindowsEndpointSelector {
                    stable_id: None,
                    current_mmdevice_id: None,
                    friendly_name: None,
                    data_flow,
                },
            );
            let virtual_identity = qpwgraph_virtual_endpoint_identity(&device, &endpoint_id)
                .filter(|identity| qpwgraph_endpoint_role_matches_flow(flow, identity.role));
            let endpoint = EndpointRecord {
                id: endpoint_id,
                flow,
                device,
                node_id,
                port_id,
                monitor_port_id,
                selector,
                virtual_identity,
            };
            sessions.extend(self.add_sessions(&endpoint, &mut graph)?);
            endpoints.push(endpoint);
        }

        for (node_id, position) in graph.default_node_positions() {
            if let Some(node) = graph.nodes.get_mut(&node_id) {
                node.position = position;
            }
        }
        self.endpoints = endpoints;
        self.sessions = sessions;
        self.graph = graph;
        let states = self.read_audio_states();
        if let Ok(mut shared) = self.audio_states.lock() {
            *shared = states;
        }
        self.register_endpoint_volume_callbacks();
        Ok(self.snapshot())
    }

    /// Apply a session-only notification without enumerating the endpoint
    /// collection again. The callback never crosses the COM apartment with a
    /// borrowed `IAudioSessionControl`; it only records the owning endpoint,
    /// and this worker performs all enumeration and registration here.
    pub(super) fn refresh_if_needed(&mut self) -> BackendResult<WorkerSnapshot> {
        if self.topology_dirty.load(Ordering::Acquire) {
            return self.refresh_graph();
        }
        let dirty_endpoints = take_session_dirty_endpoints(&self.session_dirty_endpoints);
        if dirty_endpoints.is_empty() {
            // This is the periodic safety reconciliation for an event-driven
            // backend. It is intentionally the only path that re-enumerates
            // every endpoint when nothing more precise was signalled.
            return self.refresh_graph();
        }
        self.refresh_dirty_sessions(&dirty_endpoints)
    }

    pub(super) fn refresh_dirty_sessions(
        &mut self,
        dirty_endpoints: &BTreeSet<String>,
    ) -> BackendResult<WorkerSnapshot> {
        // If an endpoint notification raced the session callback, the set can
        // mention an endpoint that has already disappeared. A full refresh is
        // the safe way to discover that topology change.
        if dirty_endpoints
            .iter()
            .any(|id| !self.endpoints.iter().any(|endpoint| &endpoint.id == id))
        {
            self.topology_dirty.store(true, Ordering::Release);
            return self.refresh_graph();
        }

        self.dirty.store(false, Ordering::Release);
        let old_session_nodes: Vec<_> = self
            .sessions
            .iter()
            .filter(|session| dirty_endpoints.contains(&session.endpoint_id))
            .map(|session| session.node_id)
            .collect();
        self.clear_session_callbacks_for(dirty_endpoints);
        self.sessions
            .retain(|session| !dirty_endpoints.contains(&session.endpoint_id));

        // Remove only the old session subgraphs. Endpoint nodes and ports stay
        // intact, so links belonging to unrelated endpoints and their COM
        // registrations remain untouched.
        let old_ports: BTreeSet<_> = old_session_nodes
            .iter()
            .filter_map(|node_id| self.graph.node(*node_id))
            .flat_map(|node| node.ports.iter().copied())
            .collect();
        let old_links: Vec<_> = self
            .graph
            .links
            .values()
            .filter(|link| {
                old_ports.contains(&link.output_port) || old_ports.contains(&link.input_port)
            })
            .map(|link| link.id)
            .collect();
        for link_id in old_links {
            let _ = self.graph.remove_link(link_id);
        }
        for node_id in &old_session_nodes {
            if let Some(node) = self.graph.nodes.remove(node_id) {
                for port_id in node.ports {
                    self.graph.ports.remove(&port_id);
                }
            }
        }

        let endpoints: Vec<_> = self
            .endpoints
            .iter()
            .filter(|endpoint| dirty_endpoints.contains(&endpoint.id))
            .cloned()
            .collect();
        let mut graph = std::mem::take(&mut self.graph);
        let sessions_result = (|| {
            for endpoint in endpoints {
                let sessions = self.add_sessions(&endpoint, &mut graph)?;
                self.sessions.extend(sessions);
            }
            Ok::<(), BackendError>(())
        })();
        self.graph = graph;
        sessions_result?;

        // New session controls may have different readability from the old
        // ones; update only the affected endpoint and leave every other node's
        // cache untouched.
        if let Ok(mut shared) = self.audio_states.lock() {
            for node_id in &old_session_nodes {
                shared.remove(node_id);
            }
            for endpoint in &self.endpoints {
                if dirty_endpoints.contains(&endpoint.id) {
                    shared.insert(endpoint.node_id, self.endpoint_audio_state(endpoint));
                }
            }
            for session in &self.sessions {
                if dirty_endpoints.contains(&session.endpoint_id) {
                    shared.insert(session.node_id, self.session_audio_state(session));
                }
            }
        }

        for (node_id, position) in self.graph.default_node_positions() {
            if let Some(node) = self.graph.nodes.get_mut(&node_id) {
                node.position = position;
            }
        }
        Ok(self.snapshot())
    }

    /// Which graph ports the router can open a real stream for.
    ///
    /// Endpoint ports always appear. A render-session port appears only when
    /// Windows reports that the application is already attached to
    /// QPWGraph Virtual Output. That proves its dry path is isolated, so a
    /// process-loopback source cannot accidentally create dry + processed
    /// duplicate audio. Capture-only consumers use `process_audio_capabilities`
    /// and never become graph edges.
    pub(super) fn endpoint_ports(&self) -> BTreeMap<PortId, EndpointPort> {
        let mut ports = BTreeMap::new();
        for endpoint in &self.endpoints {
            let role = if endpoint.flow == Audio::eRender {
                EndpointPortRole::Render
            } else {
                EndpointPortRole::Capture
            };
            ports.insert(
                endpoint.port_id,
                EndpointPort {
                    device_id: endpoint.id.clone(),
                    role,
                },
            );
            if let Some(monitor_port_id) = endpoint.monitor_port_id {
                ports.insert(
                    monitor_port_id,
                    EndpointPort {
                        device_id: endpoint.id.clone(),
                        role: EndpointPortRole::Monitor,
                    },
                );
            }
        }
        for session in &self.sessions {
            if session.flow != Audio::eRender || session.process_id == 0 {
                continue;
            }
            let virtualized = self.session_is_virtualized(session);
            if virtualized {
                let Some(selector) = session
                    .process_identity
                    .as_ref()
                    .and_then(ProcessIdentity::selector_key)
                else {
                    // A mutable process route is not safe without a stable
                    // identity to compare at activation time.
                    continue;
                };
                ports.insert(
                    session.port_id,
                    EndpointPort {
                        device_id: session.endpoint_id.clone(),
                        role: EndpointPortRole::Process {
                            pid: session.process_id,
                            selector,
                        },
                    },
                );
            }
        }
        ports
    }

    /// Process-loopback capture is valid for any live render session. Only
    /// the mutable route/effects bits depend on the session already being on
    /// QPWGraph Virtual Output.
    pub(super) fn process_audio_capabilities(&self) -> BTreeMap<NodeId, ProcessAudioCapabilities> {
        self.sessions
            .iter()
            .filter(|session| session.flow == Audio::eRender && session.process_id != 0)
            .map(|session| {
                let mut capabilities = if self.session_is_virtualized(session) {
                    ProcessAudioCapabilities::isolated()
                } else {
                    ProcessAudioCapabilities::capture_only()
                };
                capabilities.meter_peak = session.meter.is_some() || capabilities.capture_readonly;
                (session.node_id, capabilities)
            })
            .collect()
    }

    fn session_is_virtualized(&self, session: &SessionRecord) -> bool {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.id == session.endpoint_id)
            .and_then(|endpoint| endpoint.virtual_identity.as_ref())
            .is_some_and(|identity| identity.role == QpwVirtualEndpointRole::AppRender)
    }

    #[cfg(feature = "relay")]
    fn application_sources(&self) -> Vec<(String, String, u32)> {
        let mut sources = BTreeMap::new();
        let mut ambiguous = BTreeSet::new();
        for session in &self.sessions {
            if session.flow != Audio::eRender || session.process_id == 0 {
                continue;
            }
            let Some(identity) = session.process_identity.as_ref() else {
                continue;
            };
            let Some(selector) = identity.selector_key() else {
                continue;
            };
            let name = self
                .graph
                .nodes
                .get(&session.node_id)
                .map(|node| node.name.trim().to_owned())
                .filter(|name| !name.is_empty())
                .or(identity.display_name.clone())
                .or(identity.executable_name.clone())
                .unwrap_or_else(|| format!("Application ({})", session.process_id));
            match sources.entry(selector.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert((name, session.process_id));
                }
                std::collections::btree_map::Entry::Occupied(entry)
                    if entry.get().1 != session.process_id =>
                {
                    ambiguous.insert(selector);
                }
                std::collections::btree_map::Entry::Occupied(_) => {}
            }
        }
        sources.retain(|selector, _| !ambiguous.contains(selector));
        sources
            .into_iter()
            .map(|(selector, (name, pid))| (selector, name, pid))
            .collect()
    }

    pub(super) fn snapshot(&self) -> WorkerSnapshot {
        WorkerSnapshot {
            graph: self.graph.clone(),
            meterable: self.meterable_nodes(),
            endpoint_ports: self.endpoint_ports(),
            endpoint_selectors: self
                .endpoints
                .iter()
                .map(|endpoint| (endpoint.id.clone(), endpoint.selector.clone()))
                .collect(),
            virtual_endpoint_identities: self
                .endpoints
                .iter()
                .filter_map(|endpoint| endpoint.virtual_identity.clone())
                .collect(),
            virtual_driver_health: VirtualAudioDriverHealth::from_verified_identities(
                self.endpoints
                    .iter()
                    .filter_map(|endpoint| endpoint.virtual_identity.clone()),
            ),
            #[cfg(feature = "relay")]
            playback_endpoints: self
                .endpoints
                .iter()
                .filter(|endpoint| endpoint.flow == Audio::eRender)
                .map(|endpoint| {
                    let name = self
                        .graph
                        .nodes
                        .get(&endpoint.node_id)
                        .map(|node| node.name.clone())
                        .unwrap_or_else(|| endpoint.id.clone());
                    (endpoint.id.clone(), name)
                })
                .collect(),
            #[cfg(feature = "relay")]
            capture_endpoints: self
                .endpoints
                .iter()
                .filter(|endpoint| endpoint.flow == Audio::eCapture)
                .map(|endpoint| {
                    let name = self
                        .graph
                        .nodes
                        .get(&endpoint.node_id)
                        .map(|node| node.name.clone())
                        .unwrap_or_else(|| endpoint.id.clone());
                    (endpoint.id.clone(), name)
                })
                .collect(),
            #[cfg(feature = "relay")]
            application_sources: self.application_sources(),
            process_audio_capabilities: self.process_audio_capabilities(),
            application_route_candidates: self.application_route_candidates(),
            application_route_ports: self.application_route_ports(),
            process_captures: self.process_captures.statuses(),
            #[cfg(feature = "relay")]
            default_input: self.default_endpoint_id(Audio::eCapture),
            #[cfg(feature = "relay")]
            default_output: self.default_endpoint_id(Audio::eRender),
            #[cfg(feature = "relay")]
            default_generation: self.default_generation.load(Ordering::Acquire),
        }
    }

    pub(super) fn application_route_candidates(&self) -> Vec<ApplicationRouteCandidate> {
        let mut candidates = BTreeMap::new();
        for session in &self.sessions {
            if session.flow != Audio::eRender || session.process_id == 0 {
                continue;
            }
            let Some(identity) = session.process_identity.as_ref() else {
                continue;
            };
            let selector = identity.application_selector();
            let Some(key) = selector.runtime_key() else {
                continue;
            };
            let isolated = self.session_is_virtualized(session);
            match candidates.entry((key.to_owned(), session.process_id)) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(ApplicationRouteCandidate {
                        selector,
                        pid: session.process_id,
                        isolated,
                    });
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    // Process loopback captures the process, not one Core
                    // Audio session. If any session for this PID still
                    // reaches a physical endpoint, local rerendering would
                    // create a dry + processed duplicate.
                    entry.get_mut().isolated &= isolated;
                }
            }
        }
        candidates.into_values().collect()
    }

    pub(super) fn application_route_ports(&self) -> BTreeMap<(String, u32), PortId> {
        self.sessions
            .iter()
            .filter(|session| {
                session.flow == Audio::eRender
                    && session.process_id != 0
                    && self.session_is_virtualized(session)
            })
            .filter_map(|session| {
                Some((
                    session.process_identity.as_ref()?.selector_key()?,
                    session.process_id,
                    session.port_id,
                ))
            })
            .map(|(selector, pid, port)| ((selector, pid), port))
            .collect()
    }

    #[cfg(feature = "relay")]
    fn default_endpoint_id(&self, flow: Audio::EDataFlow) -> Option<String> {
        let device = unsafe {
            self.enumerator
                .GetDefaultAudioEndpoint(flow, Audio::eConsole)
        }
        .ok()?;
        unsafe { device.GetId() }.ok().map(take_pwstr)
    }

    /// Subscribe to endpoint volume/mute changes so the hardware keys and the
    /// system mixer are reflected without polling or a topology rebuild.
    pub(super) fn register_endpoint_volume_callbacks(&mut self) {
        self.clear_endpoint_volume_callbacks();
        for endpoint in &self.endpoints {
            let Ok(control) = (unsafe {
                endpoint
                    .device
                    .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
            }) else {
                continue;
            };
            let callback: Audio::Endpoints::IAudioEndpointVolumeCallback = EndpointVolumeCallback {
                node_id: endpoint.node_id,
                states: Arc::clone(&self.audio_states),
            }
            .into();
            if unsafe { control.RegisterControlChangeNotify(&callback) }.is_ok() {
                self.endpoint_volume_events.push((control, callback));
            }
        }
    }

    pub(super) fn clear_endpoint_volume_callbacks(&mut self) {
        for (control, callback) in self.endpoint_volume_events.drain(..) {
            let _ = unsafe { control.UnregisterControlChangeNotify(&callback) };
        }
    }

    /// Read volume and mute for every endpoint and session Core Audio knows
    /// about. A node whose control cannot be activated right now is reported as
    /// unreadable rather than dropped, so the card still renders and simply
    /// shows no value.
    pub(super) fn read_audio_states(&self) -> BTreeMap<NodeId, NodeAudioState> {
        let mut states = BTreeMap::new();
        for endpoint in &self.endpoints {
            states.insert(endpoint.node_id, self.endpoint_audio_state(endpoint));
        }
        for session in &self.sessions {
            states.insert(session.node_id, self.session_audio_state(session));
        }
        states
    }

    pub(super) fn endpoint_audio_state(&self, endpoint: &EndpointRecord) -> NodeAudioState {
        let Ok(control) = (unsafe {
            endpoint
                .device
                .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
        }) else {
            return NodeAudioState::UNSUPPORTED;
        };
        // Endpoint volume is a 0..=1 scalar; Windows has no boost range here,
        // which is why volume never exceeds unity on this backend.
        let volume = unsafe { control.GetMasterVolumeLevelScalar() }.ok();
        let muted = unsafe { control.GetMute() }
            .ok()
            .map(|muted| muted.as_bool());
        NodeAudioState {
            volume,
            muted,
            volume_readable: volume.is_some(),
            volume_writable: true,
            mute_readable: muted.is_some(),
            mute_writable: true,
        }
    }

    pub(super) fn session_audio_state(&self, session: &SessionRecord) -> NodeAudioState {
        let Some(endpoint) = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == session.endpoint_id && endpoint.flow == session.flow)
        else {
            return NodeAudioState::UNSUPPORTED;
        };
        let Ok(control) = self.find_session_control(endpoint, &session.session_id) else {
            return NodeAudioState::UNSUPPORTED;
        };
        let Ok(volume_control) = control.cast::<Audio::ISimpleAudioVolume>() else {
            return NodeAudioState::UNSUPPORTED;
        };
        let volume = unsafe { volume_control.GetMasterVolume() }.ok();
        let muted = unsafe { volume_control.GetMute() }
            .ok()
            .map(|muted| muted.as_bool());
        NodeAudioState {
            volume,
            muted,
            volume_readable: volume.is_some(),
            volume_writable: true,
            mute_readable: muted.is_some(),
            mute_writable: true,
        }
    }

    pub(super) fn enumerate_endpoints(
        &self,
        flow: Audio::EDataFlow,
    ) -> BackendResult<Vec<(String, Audio::EDataFlow, Audio::IMMDevice)>> {
        let collection = unsafe {
            self.enumerator
                .EnumAudioEndpoints(flow, Audio::DEVICE_STATE_ACTIVE)
        }
        .map_err(|error| native_error("enumerate audio endpoints", error))?;
        let count = unsafe { collection.GetCount() }
            .map_err(|error| native_error("read audio endpoint count", error))?;
        let mut result = Vec::with_capacity(count as usize);
        for index in 0..count {
            let device = unsafe { collection.Item(index) }
                .map_err(|error| native_error("read audio endpoint", error))?;
            let id = unsafe { device.GetId() }
                .map(take_pwstr)
                .map_err(|error| native_error("read audio endpoint ID", error))?;
            result.push((id, flow, device));
        }
        Ok(result)
    }

    pub(super) fn add_sessions(
        &mut self,
        endpoint: &EndpointRecord,
        graph: &mut Graph,
    ) -> BackendResult<Vec<SessionRecord>> {
        let manager: Audio::IAudioSessionManager2 =
            match unsafe { endpoint.device.Activate(CLSCTX_ALL, None) } {
                Ok(manager) => manager,
                Err(_) => return Ok(Vec::new()),
            };

        let notification: Audio::IAudioSessionNotification = SessionNotificationClient {
            dirty: Arc::clone(&self.dirty),
            endpoint_id: endpoint.id.clone(),
            session_dirty_endpoints: Arc::clone(&self.session_dirty_endpoints),
        }
        .into();
        if unsafe { manager.RegisterSessionNotification(&notification) }.is_ok() {
            self.session_notifications
                .push((endpoint.id.clone(), manager.clone(), notification));
        }

        let enumerator = match unsafe { manager.GetSessionEnumerator() } {
            Ok(enumerator) => enumerator,
            Err(_) => return Ok(Vec::new()),
        };
        let count = unsafe { enumerator.GetCount() }.unwrap_or(0);
        let mut result = Vec::new();
        for index in 0..count {
            let control = match unsafe { enumerator.GetSession(index) } {
                Ok(control) => control,
                Err(_) => continue,
            };
            let control2: Audio::IAudioSessionControl2 = match control.cast() {
                Ok(control) => control,
                Err(_) => continue,
            };
            let state = match unsafe { control.GetState() } {
                Ok(state) => state,
                Err(_) => continue,
            };
            if state != Audio::AudioSessionStateActive {
                continue;
            }
            let session_id = match unsafe { control2.GetSessionInstanceIdentifier() } {
                Ok(value) => take_pwstr(value),
                Err(_) => continue,
            };
            let process_id = unsafe { control2.GetProcessId() }.unwrap_or(0);
            let process_identity = (process_id != 0)
                .then(|| ProcessIdentity::from_pid(process_id).ok())
                .flatten();
            let display_name = unsafe { control2.GetDisplayName() }
                .map(take_pwstr)
                .unwrap_or_default();
            let name = if display_name.trim().is_empty() {
                process_name(process_id).unwrap_or_else(|| format!("Audio session ({process_id})"))
            } else {
                display_name
            };
            let node_id = NodeId(graph_id(session_node_local_id(&endpoint.id, &session_id)));
            let port_id = PortId(graph_id(session_port_local_id(&endpoint.id, &session_id)));
            graph.add_node(
                Node::new(node_id, name, NodeType::WindowsAudioSession).with_serial(
                    stable_local_id(&format!("session:{}:{session_id}", endpoint.id)),
                ),
            )?;
            let session_direction = session_direction(endpoint.flow);
            graph.add_port(Port::new(
                port_id,
                node_id,
                "audio",
                session_direction,
                PortType::Audio,
            ))?;
            let (output, input) = session_link_ports(endpoint.flow, port_id, endpoint.port_id);
            let link_id = LinkId(graph_id(session_link_local_id(&endpoint.id, &session_id)));
            graph.insert_existing_link(Link {
                id: link_id,
                output_port: output,
                input_port: input,
            })?;

            // Query the meter before the control is handed to the notification
            // registration, which consumes it.
            let meter = control.cast::<IAudioMeterInformation>().ok();
            let events: Audio::IAudioSessionEvents = SessionEventsClient {
                dirty: Arc::clone(&self.dirty),
                endpoint_id: endpoint.id.clone(),
                session_dirty_endpoints: Arc::clone(&self.session_dirty_endpoints),
                node_id,
                states: Arc::clone(&self.audio_states),
            }
            .into();
            if unsafe { control.RegisterAudioSessionNotification(&events) }.is_ok() {
                self.session_events
                    .push((endpoint.id.clone(), control, events));
            }
            result.push(SessionRecord {
                endpoint_id: endpoint.id.clone(),
                session_id,
                flow: endpoint.flow,
                node_id,
                port_id,
                process_id,
                process_identity,
                meter,
            });
        }
        Ok(result)
    }

    pub(super) fn clear_session_callbacks(&mut self) {
        for (_, control, events) in self.session_events.drain(..) {
            let _ = unsafe { control.UnregisterAudioSessionNotification(&events) };
        }
        for (_, manager, notification) in self.session_notifications.drain(..) {
            let _ = unsafe { manager.UnregisterSessionNotification(&notification) };
        }
    }

    pub(super) fn clear_session_callbacks_for(&mut self, endpoint_ids: &BTreeSet<String>) {
        self.session_events
            .retain(|(endpoint_id, control, events)| {
                if endpoint_ids.contains(endpoint_id) {
                    let _ = unsafe { control.UnregisterAudioSessionNotification(events) };
                    false
                } else {
                    true
                }
            });
        self.session_notifications
            .retain(|(endpoint_id, manager, notification)| {
                if endpoint_ids.contains(endpoint_id) {
                    let _ = unsafe { manager.UnregisterSessionNotification(notification) };
                    false
                } else {
                    true
                }
            });
    }

    pub(super) fn set_volume(&self, node: NodeId, volume: f32) -> BackendResult<()> {
        let volume = volume.clamp(0.0, 1.0);
        if let Some(endpoint) = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.node_id == node)
        {
            let control: IAudioEndpointVolume =
                unsafe { endpoint.device.Activate(CLSCTX_ALL, None) }
                    .map_err(|error| native_error("activate endpoint volume", error))?;
            return unsafe { control.SetMasterVolumeLevelScalar(volume, std::ptr::null()) }
                .map_err(|error| native_error("set endpoint volume", error));
        }
        let session = self
            .sessions
            .iter()
            .find(|session| session.node_id == node)
            .ok_or_else(|| BackendError::Unsupported("Windows audio node is unavailable".into()))?;
        let endpoint = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == session.endpoint_id && endpoint.flow == session.flow)
            .ok_or_else(|| {
                BackendError::Unsupported("Windows audio endpoint is unavailable".into())
            })?;
        let control = self.find_session_control(endpoint, &session.session_id)?;
        let volume_control: Audio::ISimpleAudioVolume = control
            .cast()
            .map_err(|error| native_error("activate session volume", error))?;
        unsafe { volume_control.SetMasterVolume(volume, std::ptr::null()) }
            .map_err(|error| native_error("set session volume", error))
    }

    pub(super) fn set_mute(&self, node: NodeId, muted: bool) -> BackendResult<()> {
        if let Some(endpoint) = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.node_id == node)
        {
            let control: IAudioEndpointVolume =
                unsafe { endpoint.device.Activate(CLSCTX_ALL, None) }
                    .map_err(|error| native_error("activate endpoint volume", error))?;
            return unsafe { control.SetMute(muted, std::ptr::null()) }
                .map_err(|error| native_error("set endpoint mute", error));
        }
        let session = self
            .sessions
            .iter()
            .find(|session| session.node_id == node)
            .ok_or_else(|| BackendError::Unsupported("Windows audio node is unavailable".into()))?;
        let endpoint = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.id == session.endpoint_id && endpoint.flow == session.flow)
            .ok_or_else(|| {
                BackendError::Unsupported("Windows audio endpoint is unavailable".into())
            })?;
        let control = self.find_session_control(endpoint, &session.session_id)?;
        let volume_control: Audio::ISimpleAudioVolume = control
            .cast()
            .map_err(|error| native_error("activate session volume", error))?;
        unsafe { volume_control.SetMute(muted, std::ptr::null()) }
            .map_err(|error| native_error("set session mute", error))
    }

    pub(super) fn find_session_control(
        &self,
        endpoint: &EndpointRecord,
        expected_id: &str,
    ) -> BackendResult<Audio::IAudioSessionControl> {
        let manager: Audio::IAudioSessionManager2 =
            unsafe { endpoint.device.Activate(CLSCTX_ALL, None) }
                .map_err(|error| native_error("activate session manager", error))?;
        let sessions = unsafe { manager.GetSessionEnumerator() }
            .map_err(|error| native_error("enumerate audio sessions", error))?;
        let count = unsafe { sessions.GetCount() }
            .map_err(|error| native_error("read audio session count", error))?;
        for index in 0..count {
            let control = unsafe { sessions.GetSession(index) }
                .map_err(|error| native_error("read audio session", error))?;
            let control2: Audio::IAudioSessionControl2 = control
                .cast()
                .map_err(|error| native_error("read audio session identity", error))?;
            let session_id = unsafe { control2.GetSessionInstanceIdentifier() }
                .map(take_pwstr)
                .map_err(|error| native_error("read audio session identity", error))?;
            if session_id == expected_id {
                return Ok(control);
            }
        }
        Err(BackendError::Unsupported(
            "Windows audio session is no longer available".into(),
        ))
    }

    /// Whether a node should currently own a meter, under the active policy.
    pub(super) fn meter_wanted(&self, node: NodeId) -> bool {
        match self.meter_policy {
            MeterPolicy::Disabled => false,
            MeterPolicy::Always => true,
            MeterPolicy::OnDemand => self.requested_meters.contains(&node),
        }
    }

    /// Nodes that can report a level: every endpoint, plus every session whose
    /// control answered the meter query.
    pub(super) fn meterable_nodes(&self) -> BTreeSet<NodeId> {
        self.endpoints
            .iter()
            .map(|endpoint| endpoint.node_id)
            .chain(
                self.sessions
                    .iter()
                    .filter(|session| {
                        session.meter.is_some()
                            || (session.flow == Audio::eRender
                                && session.process_id != 0
                                && session.process_identity.is_some())
                    })
                    .map(|session| session.node_id),
            )
            .collect()
    }

    pub(super) fn audio_meters(&mut self) -> BackendResult<Vec<AudioMeter>> {
        if self.meter_policy == MeterPolicy::Disabled {
            self.process_captures.reconcile_meters(
                std::iter::empty(),
                crate::router::AudioFormat::new(48_000, 2),
            );
            return Ok(Vec::new());
        }
        let mut result = Vec::new();
        let format = crate::router::AudioFormat::new(48_000, 2);
        let targets = bound_process_meter_targets(
            self.sessions
                .iter()
                .filter(|session| {
                    session.flow == Audio::eRender
                        && session.process_id != 0
                        && session.process_identity.is_some()
                        && self.meter_wanted(session.node_id)
                })
                .filter_map(|session| {
                    Some(ProcessMeterTarget {
                        node_id: session.node_id,
                        selector: session.process_identity.as_ref()?.selector_key()?,
                        pid: session.process_id,
                        mode: ProcessLoopbackMode::IncludeProcessTree,
                    })
                }),
        );
        self.process_captures.reconcile_meters(targets, format);
        // Per-application levels, straight off each session's own meter.
        for session in &self.sessions {
            if !self.meter_wanted(session.node_id) {
                continue;
            }
            let process_meter = self.process_captures.meter(session.node_id);
            let native_peak = session
                .meter
                .as_ref()
                .and_then(|meter| unsafe { meter.GetPeakValue() }.ok())
                .map(|peak| peak.clamp(0.0, 1.0));
            let Some((rms, peak, age_ms)) =
                process_meter_levels(process_meter.as_ref(), native_peak)
            else {
                continue;
            };
            result.push(AudioMeter {
                node_id: session.node_id,
                port_id: None,
                // The process-loopback path reports true RMS. If it is
                // unavailable, keep the native peak fallback honest and do
                // not present that peak as an RMS value.
                rms,
                peak,
                age_ms,
                available: true,
            });
        }
        for endpoint in &self.endpoints {
            if !self.meter_wanted(endpoint.node_id) {
                continue;
            }
            let meter: IAudioMeterInformation =
                match unsafe { endpoint.device.Activate(CLSCTX_ALL, None) } {
                    Ok(meter) => meter,
                    Err(_) => continue,
                };
            let peak = match unsafe { meter.GetPeakValue() } {
                Ok(peak) => peak.clamp(0.0, 1.0),
                Err(_) => continue,
            };
            // Core Audio's endpoint meter exposes peak level, not RMS. The
            // legacy application contract still has an f32 RMS field, so it
            // is left at zero rather than presenting peak as fabricated RMS.
            result.push(AudioMeter {
                node_id: endpoint.node_id,
                port_id: Some(endpoint.port_id),
                rms: 0.0,
                peak,
                age_ms: 0,
                available: true,
            });
        }
        Ok(result)
    }
}

impl Drop for CoreAudioWorker {
    fn drop(&mut self) {
        self.clear_session_callbacks();
        let _ = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.endpoint_notification)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(node: usize) -> ProcessMeterTarget {
        ProcessMeterTarget {
            node_id: NodeId(node as u64),
            selector: format!("sha256:{node:064x}"),
            pid: node as u32 + 1,
            mode: ProcessLoopbackMode::IncludeProcessTree,
        }
    }

    #[test]
    fn always_policy_process_targets_are_bounded() {
        let targets = bound_process_meter_targets((0..MAX_PROCESS_METER_CAPTURES + 8).map(target));
        assert_eq!(targets.len(), MAX_PROCESS_METER_CAPTURES);
        assert_eq!(
            targets.first().map(|target| target.node_id),
            Some(NodeId(0))
        );
        assert_eq!(
            targets.last().map(|target| target.node_id),
            Some(NodeId((MAX_PROCESS_METER_CAPTURES - 1) as u64))
        );
    }

    #[test]
    fn native_peak_survives_unavailable_process_loopback() {
        let process_meter = ProcessMeterReading {
            rms: 0.0,
            peak: 0.0,
            age_ms: u32::MAX,
            available: false,
            state: ProcessCaptureState::Unavailable {
                reason: "activation failed".into(),
            },
        };
        assert_eq!(
            process_meter_levels(Some(&process_meter), Some(0.42)),
            Some((0.0, 0.42, 0))
        );
        assert_eq!(process_meter_levels(Some(&process_meter), None), None);
    }

    #[test]
    fn process_rms_and_peak_replace_native_peak_when_available() {
        let process_meter = ProcessMeterReading {
            rms: 0.25,
            peak: 0.5,
            age_ms: 17,
            available: true,
            state: ProcessCaptureState::Active,
        };
        assert_eq!(
            process_meter_levels(Some(&process_meter), Some(0.9)),
            Some((0.25, 0.5, 17))
        );
    }
}
