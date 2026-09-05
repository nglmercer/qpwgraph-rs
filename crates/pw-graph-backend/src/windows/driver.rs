//! The public driver.
//!
//! It owns no COM interface. It sends owned commands to the worker thread
//! and reads back owned snapshots, which is what keeps every COM pointer on
//! the single thread that initialized the apartment.

use super::*;
#[cfg(feature = "relay")]
use crate::api;

/// Highest volume a routed node accepts.
///
/// The same 1.5 PipeWire offers, so the fader has the same top of scale on
/// both platforms when qpwgraph owns the audio. An unrouted Windows endpoint
/// still reports unity, because that is all its own control can do.
pub(super) const ROUTED_VOLUME_MAX: f32 = 1.5;

pub(super) const WINDOWS_AUDIO_CAPABILITIES: BackendCapabilities = BackendCapabilities {
    topology: true,
    // True because qpwgraph carries these routes itself: a link between two
    // endpoint ports is a real route in `crate::router`, with WASAPI streams
    // at both ends. It is emphatically *not* true for application sessions --
    // Core Audio exposes no supported way to move one -- so `connect` refuses
    // them explicitly and `node_supports_routing` keeps the canvas from
    // offering a gesture there at all. A session already isolated on the
    // optional virtual sink is the documented exception and is captured via
    // process loopback.
    connect: true,
    disconnect: true,
    volume: true,
    mute: true,
    meters: true,
    effects: true,
    // Kept in step with `RelayDriver::relay_available` below: the WASAPI relay
    // endpoints exist whenever the feature is compiled in.
    relay: cfg!(feature = "relay"),
};

/// Audio state shared between the COM worker, the Core Audio change callbacks,
/// and the public driver.
///
/// Volume and mute arrive on notification threads with the new values already
/// in the payload, so they are written straight in. Nothing about the graph's
/// shape changes when a fader moves, which is why these events deliberately do
/// not mark the topology dirty: a volume change used to force a full endpoint
/// and session re-enumeration.
pub(super) type AudioStateMap = Arc<Mutex<BTreeMap<NodeId, NodeAudioState>>>;

#[cfg(feature = "relay")]
struct RelayConfigOptions {
    device_name: String,
    pin: String,
    port: u16,
    codec: api::RelayCodecKind,
    frame_ms: u16,
    transport: api::RelayTransportPreference,
    direction: api::RelayDirection,
    direction_generation: u64,
    mode: api::RelayMode,
    mode_generation: u64,
    device_id: String,
    trusted_peers: Vec<api::RelayTrustedPeer>,
    trust_new_peers: bool,
}

/// Public Windows audio driver. The COM worker owns all Core Audio objects;
/// this value only owns a graph snapshot, command channel, and lifecycle state.
#[derive(Debug)]
pub struct WindowsAudioDriver {
    pub(super) graph: Graph,
    /// Audio state as Core Audio last reported it, kept current by change
    /// callbacks. The backend owns these values; nothing upstream keeps a copy.
    pub(super) audio_states: AudioStateMap,
    /// Nodes Core Audio can meter: endpoints, and sessions that expose a meter.
    pub(super) meterable: BTreeSet<NodeId>,
    pub(super) positions: BTreeMap<NodeId, [f32; 2]>,
    pub(super) command_tx: Sender<WorkerCommand>,
    pub(super) dirty: Arc<AtomicBool>,
    pub(super) worker: Option<JoinHandle<()>>,
    /// Which ports name a device the router can open a stream for. Rebuilt
    /// with the graph, because an unplugged endpoint takes its ports with it.
    pub(super) endpoint_ports: BTreeMap<PortId, EndpointPort>,
    /// Health of the optional four-endpoint qpwgraph driver package.
    pub(super) virtual_driver_health: VirtualAudioDriverHealth,
    /// The routes qpwgraph owns, and the audio behind them.
    ///
    /// Started on the first connect rather than at construction: a session
    /// that never draws a link should not pay for an audio thread.
    pub(super) routing: Option<WindowsRouting>,
    /// Effect instances and the factory that builds them.
    pub(super) effects: WindowsEffects,
    /// Where each effect node sits, by instance id. Kept separately from
    /// `positions` because an effect outlives the node id a rebuild gave it.
    pub(super) effect_positions: BTreeMap<String, [f32; 2]>,
    /// Relay engine plus its WASAPI endpoints, created on first use.
    #[cfg(feature = "relay")]
    pub(super) relay: Option<crate::windows_relay::WindowsRelayDevices>,
    /// Which endpoints the relay should use next time it starts.
    #[cfg(feature = "relay")]
    pub(super) relay_endpoints: crate::windows_relay::RelayEndpoints,
    /// Playback endpoints the relay can be pointed at, refreshed with the graph.
    #[cfg(feature = "relay")]
    pub(super) relay_endpoint_choices: Vec<(String, String)>,
    /// Physical eCapture devices offered to an Emitter.
    #[cfg(feature = "relay")]
    pub(super) relay_input_choices: Vec<(String, String)>,
    /// Live applications already isolated on QPWGraph Virtual Output, as
    /// `(stable selector, display name, current PID)`.
    #[cfg(feature = "relay")]
    pub(super) relay_application_sources: Vec<(String, String, u32)>,
    /// Current local mode and source/sink selection. These are independent of
    /// the legacy `RelayEndpoints` pair retained for old callers.
    #[cfg(feature = "relay")]
    pub(super) relay_mode: api::RelayMode,
    #[cfg(feature = "relay")]
    pub(super) relay_mode_generation: u64,
    #[cfg(feature = "relay")]
    pub(super) relay_send_source: api::RelaySendSource,
    #[cfg(feature = "relay")]
    pub(super) relay_receive_sink: api::RelayReceiveSink,
    #[cfg(feature = "relay")]
    pub(super) relay_default_generation: u64,
    #[cfg(feature = "relay")]
    pub(super) relay_default_input: Option<String>,
    #[cfg(feature = "relay")]
    pub(super) relay_default_output: Option<String>,
}

impl WindowsAudioDriver {
    pub fn new() -> BackendResult<Self> {
        let (command_tx, command_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let dirty = Arc::new(AtomicBool::new(true));
        let topology_dirty = Arc::new(AtomicBool::new(true));
        let session_dirty_endpoints = Arc::new(Mutex::new(BTreeSet::new()));
        let worker_dirty = Arc::clone(&dirty);
        let worker_topology_dirty = Arc::clone(&topology_dirty);
        let worker_session_dirty_endpoints = Arc::clone(&session_dirty_endpoints);
        let audio_states: AudioStateMap = Arc::new(Mutex::new(BTreeMap::new()));
        let worker_states = Arc::clone(&audio_states);
        let worker = thread::Builder::new()
            .name("qpwgraph-windows-audio".into())
            .spawn(move || {
                worker_thread(
                    command_rx,
                    ready_tx,
                    worker_dirty,
                    worker_topology_dirty,
                    worker_session_dirty_endpoints,
                    worker_states,
                )
            })
            .map_err(|error| {
                BackendError::Native(format!("could not start audio worker: {error}"))
            })?;

        let snapshot = match ready_rx.recv() {
            Ok(Ok(snapshot)) => snapshot,
            Ok(Err(error)) => {
                let _ = worker.join();
                return Err(error);
            }
            Err(_) => {
                let _ = worker.join();
                return Err(BackendError::Native(
                    "Windows audio worker exited during startup".into(),
                ));
            }
        };

        Ok(Self {
            graph: snapshot.graph,
            audio_states,
            meterable: snapshot.meterable,
            positions: BTreeMap::new(),
            command_tx,
            dirty,
            worker: Some(worker),
            endpoint_ports: snapshot.endpoint_ports,
            virtual_driver_health: snapshot.virtual_driver_health,
            routing: None,
            effects: WindowsEffects::new(),
            effect_positions: BTreeMap::new(),
            #[cfg(feature = "relay")]
            relay: None,
            #[cfg(feature = "relay")]
            relay_endpoints: Default::default(),
            #[cfg(feature = "relay")]
            relay_endpoint_choices: snapshot.playback_endpoints,
            #[cfg(feature = "relay")]
            relay_input_choices: snapshot.capture_endpoints,
            #[cfg(feature = "relay")]
            relay_application_sources: snapshot.application_sources,
            #[cfg(feature = "relay")]
            relay_mode: api::RelayMode::Receiver,
            #[cfg(feature = "relay")]
            relay_mode_generation: 0,
            #[cfg(feature = "relay")]
            relay_send_source: api::RelaySendSource::DefaultInput,
            #[cfg(feature = "relay")]
            relay_receive_sink: api::RelayReceiveSink::DefaultOutput,
            #[cfg(feature = "relay")]
            relay_default_generation: snapshot.default_generation,
            #[cfg(feature = "relay")]
            relay_default_input: snapshot.default_input,
            #[cfg(feature = "relay")]
            relay_default_output: snapshot.default_output,
        })
    }

    /// Create the relay engine and its WASAPI endpoints on first use.
    ///
    /// A WASAPI client is bound to the device it was opened on, so changing
    /// the selected endpoints tears the devices down and starts them again.
    #[cfg(feature = "relay")]
    pub(super) fn ensure_relay(
        &mut self,
        config: pw_graph_relay::EngineConfig,
    ) -> BackendResult<&crate::windows_relay::WindowsRelayDevices> {
        let wanted = self.relay_endpoints.clone();
        let mode = self.relay_mode;
        let send_source = self.relay_send_source.clone();
        let receive_sink = self.relay_receive_sink.clone();
        let application = if mode == api::RelayMode::Emitter {
            match &send_source {
                api::RelaySendSource::Application(selector) => Some(
                    self.relay_application_source(selector)
                        .ok_or_else(|| {
                            BackendError::unsupported(
                                "selected application is not currently isolated on QPWGraph Virtual Output",
                            )
                        })
                        .and_then(|application| {
                            // The snapshot deliberately stores a runtime PID,
                            // but a PID can be reused between refreshes. Verify
                            // the stable identity again immediately before
                            // activation so a stale selector can never bind to
                            // an unrelated process.
                            let matches = ProcessIdentity::from_pid(application.pid)
                                .ok()
                                .and_then(|identity| identity.selector_key())
                                .is_some_and(|key| key == application.selector_key);
                            matches.then_some(application).ok_or_else(|| {
                                BackendError::unsupported(
                                    "selected application changed before process-loopback activation; refresh the Windows audio graph",
                                )
                            })
                        })?,
                ),
                _ => None,
            }
        } else {
            None
        };
        let default_generation = self.relay_default_generation;
        let restart = self.relay.as_ref().is_some_and(|devices| {
            devices.endpoints() != &wanted
                || devices.needs_restart(
                    mode,
                    &send_source,
                    &receive_sink,
                    application.as_ref(),
                    default_generation,
                )
        });
        if let Some(devices) = self.relay.as_mut() {
            if restart {
                // Keep the authenticated engine/session table alive while
                // replacing only the WASAPI worker.
                devices.restart_endpoint(
                    mode,
                    send_source,
                    receive_sink,
                    application,
                    default_generation,
                )?;
            }
            devices.handle().update_config(config);
        } else {
            self.relay = Some(crate::windows_relay::WindowsRelayDevices::start_mode(
                config,
                wanted,
                mode,
                send_source,
                receive_sink,
                application,
                default_generation,
            )?);
        }
        Ok(self.relay.as_ref().expect("relay was just created"))
    }

    #[cfg(feature = "relay")]
    fn relay_application_source(
        &self,
        selector: &str,
    ) -> Option<crate::windows_relay::RelayApplicationSource> {
        self.relay_application_sources
            .iter()
            .find(|(key, _, _)| key == selector)
            .map(
                |(selector_key, _, pid)| crate::windows_relay::RelayApplicationSource {
                    selector_key: selector_key.clone(),
                    pid: *pid,
                },
            )
    }

    #[cfg(feature = "relay")]
    fn reconcile_relay_worker(&mut self) -> BackendResult<()> {
        let Some(config) = self.relay.as_ref().map(|devices| devices.handle().config()) else {
            return Ok(());
        };
        let _ = self.ensure_relay(config)?;
        Ok(())
    }

    /// Choose which endpoints the relay taps and plays on.
    ///
    /// Ids are Core Audio device ids, the same ones the endpoint nodes are
    /// built from, so the UI can offer the cards it already draws. `None`
    /// tracks the default playback endpoint. Takes effect on the next relay
    /// start; if the relay is already running it is restarted.
    #[cfg(feature = "relay")]
    pub fn set_relay_endpoints(
        &mut self,
        endpoints: crate::windows_relay::RelayEndpoints,
    ) -> BackendResult<()> {
        if self.relay_endpoints == endpoints {
            return Ok(());
        }
        self.relay_endpoints = endpoints;
        // Preserve the old pair API while translating it into the canonical
        // Emitter/Receiver selectors. The running engine stays alive; only the
        // one affected WASAPI worker is replaced by `ensure_relay`.
        self.relay_send_source = self
            .relay_endpoints
            .capture
            .clone()
            .map(api::RelaySendSource::OutputMonitor)
            .unwrap_or(api::RelaySendSource::DefaultOutputMonitor);
        self.relay_receive_sink = self
            .relay_endpoints
            .playback
            .clone()
            .map(api::RelayReceiveSink::OutputDevice)
            .unwrap_or(api::RelayReceiveSink::DefaultOutput);
        self.reconcile_relay_worker()
    }

    /// Endpoints the relay is configured to use.
    #[cfg(feature = "relay")]
    pub fn relay_endpoints(&self) -> &crate::windows_relay::RelayEndpoints {
        &self.relay_endpoints
    }

    /// Playback endpoints the relay can be pointed at, as `(id, name)`.
    #[cfg(feature = "relay")]
    pub fn relay_endpoint_choices(&self) -> Vec<(String, String)> {
        self.relay_endpoint_choices.clone()
    }

    /// The relay's format, fixed by the WASAPI endpoints that carry it.
    #[cfg(feature = "relay")]
    #[allow(deprecated)]
    fn relay_config(options: RelayConfigOptions) -> pw_graph_relay::EngineConfig {
        pw_graph_relay::EngineConfig {
            device_id: options.device_id,
            device_name: options.device_name,
            device_kind: api::RelayDeviceKind::Other,
            pin: options.pin,
            port: options.port,
            codec: options.codec,
            frame_ms: options.frame_ms,
            sample_rate: crate::windows_relay::RELAY_SAMPLE_RATE,
            channels: crate::windows_relay::RELAY_CHANNELS,
            client_roles: options.mode.roles(),
            direction: options.direction,
            direction_generation: options.direction_generation,
            mode: options.mode,
            mode_generation: options.mode_generation,
            transport: options.transport,
            trusted_peers: options.trusted_peers,
            trust_new_peers: options.trust_new_peers,
            // The WASAPI relay endpoints run 48 kHz stereo, so that is this
            // machine's local geometry; sessions negotiating anything else are
            // converted rather than misinterpreted.
            local_sample_rate: crate::windows_relay::RELAY_SAMPLE_RATE,
            local_channels: crate::windows_relay::RELAY_CHANNELS,
            ..pw_graph_relay::EngineConfig::default()
        }
    }

    /// Check a pair of ports against the graph before any device is opened.
    ///
    /// `Graph::add_link` performs the same checks, but it runs after the
    /// audio is already flowing; failing here first means a rejected pair
    /// never starts a WASAPI stream it would immediately have to close.
    fn validate_route(&self, src: PortId, dst: PortId) -> BackendResult<()> {
        let output = self.graph.port(src).ok_or(GraphError::MissingPort(src))?;
        let input = self.graph.port(dst).ok_or(GraphError::MissingPort(dst))?;
        if !output.direction.is_source() {
            return Err(GraphError::NotSource(src).into());
        }
        if !input.direction.is_sink() {
            return Err(GraphError::NotSink(dst).into());
        }
        if output.port_type != input.port_type {
            return Err(GraphError::IncompatiblePorts(src, dst).into());
        }
        if self
            .graph
            .links
            .values()
            .any(|link| link.output_port == src && link.input_port == dst)
        {
            return Err(GraphError::DuplicateConnection(src, dst).into());
        }
        Ok(())
    }

    /// A node's source port, if qpwgraph is routing that device.
    ///
    /// A playback endpoint has two source-side identities -- its monitor is
    /// routable, its input is not -- so this searches the node's ports rather
    /// than assuming one.
    pub(super) fn routed_source_port(&self, node: NodeId) -> Option<PortId> {
        let routing = self.routing.as_ref()?;
        self.graph
            .nodes
            .get(&node)?
            .ports
            .iter()
            .copied()
            .find(|port| routing.carries_source(*port))
    }

    /// Whether qpwgraph owns the PCM leaving this node.
    fn carries_node(&self, node: NodeId) -> bool {
        self.routed_source_port(node).is_some()
    }

    /// Fold each route's software gain back into the volume Core Audio just
    /// reported, so a boosted node keeps reading as boosted.
    fn restore_routed_gain(&mut self) {
        let Some(routing) = self.routing.as_ref() else {
            return;
        };
        let boosted: Vec<(NodeId, f32)> = self
            .graph
            .nodes
            .values()
            .filter_map(|node| {
                let port = node
                    .ports
                    .iter()
                    .copied()
                    .find(|port| routing.carries_source(*port))?;
                let gain = routing.source_gain(port);
                (gain != 1.0).then_some((node.id, gain))
            })
            .collect();
        if boosted.is_empty() {
            return;
        }
        if let Ok(mut states) = self.audio_states.lock() {
            for (node, gain) in boosted {
                if let Some(state) = states.get_mut(&node) {
                    if let Some(volume) = state.volume {
                        state.volume = Some(volume * gain);
                    }
                }
            }
        }
    }

    /// Counters for every route this driver is carrying.
    ///
    /// Empty when nothing has been connected. Reading them never touches the
    /// audio path, so this is safe to poll: it is how "this link is drawn but
    /// carries nothing" becomes a visible fact rather than a silent one.
    pub fn route_metrics(&self) -> Vec<(LinkId, crate::router::RouteMetrics)> {
        self.routing
            .as_ref()
            .map(WindowsRouting::metrics)
            .unwrap_or_default()
    }

    /// Current optional-driver state for diagnostics and UI capability badges.
    pub fn virtual_audio_driver_health(&self) -> &VirtualAudioDriverHealth {
        &self.virtual_driver_health
    }

    /// Return the safe app-routing capability without attempting any
    /// undocumented Windows ABI calls.  The manual fallback remains
    /// actionable and can be shown even when the optional driver is absent.
    pub fn app_route_policy_support(&self) -> AppRoutePolicySupport {
        UnsupportedAppRoutePolicy.support()
    }

    /// Export a bounded, text-only Windows audio report for diagnostics.
    ///
    /// The report deliberately contains graph identities, capabilities, and
    /// route counters only. It never includes PCM samples, endpoint property
    /// blobs, pairing credentials, or other opaque native data, so it is safe
    /// to copy into a bug report.
    pub fn windows_audio_report(&self) -> String {
        use std::fmt::Write as _;

        let mut report = String::from("qpwgraph Windows audio report\n");
        let _ = writeln!(
            report,
            "virtual_driver={:?}\nnodes={} ports={} observed_links={} managed_links={}",
            self.virtual_driver_health,
            self.graph.nodes.len(),
            self.graph.ports.len(),
            self.graph
                .links
                .values()
                .filter(|link| !self.is_link_mutable(link.id))
                .count(),
            self.routing
                .as_ref()
                .map_or(0, |routing| routing.links().count()),
        );
        for node in self.graph.nodes.values() {
            let capabilities = self.node_capabilities(node.id);
            let _ = writeln!(
                report,
                "node id={} type={:?} name={:?} meter_peak={} meter_rms={} routable={}",
                node.id.0,
                node.node_type,
                node.name,
                capabilities.meter_peak,
                capabilities.meter_rms,
                self.node_supports_routing(node.id),
            );
        }
        #[cfg(feature = "relay")]
        for (selector, name, pid) in &self.relay_application_sources {
            let _ = writeln!(
                report,
                "relay_application selector={selector:?} name={name:?} pid={pid} active=virtualized",
            );
        }
        for (link, metrics) in self.route_metrics() {
            let Some(record) = self.graph.links.get(&link) else {
                continue;
            };
            let source = self
                .graph
                .ports
                .get(&record.output_port)
                .and_then(|port| self.graph.nodes.get(&port.node_id))
                .map(|node| node.name.as_str())
                .unwrap_or("<missing source>");
            let destination = self
                .graph
                .ports
                .get(&record.input_port)
                .and_then(|port| self.graph.nodes.get(&port.node_id))
                .map(|node| node.name.as_str())
                .unwrap_or("<missing destination>");
            let _ = writeln!(
                report,
                "route id={} source={:?} destination={:?} frames={} source_underruns={} sink_overruns={} discontinuities={} restarts={} fault={:?}",
                link.0,
                source,
                destination,
                metrics.frames_processed,
                metrics.source_underruns,
                metrics.sink_overruns,
                metrics.discontinuities,
                metrics.restarts,
                metrics.fault,
            );
        }
        report
    }

    pub(super) fn response<T>(receiver: Receiver<BackendResult<T>>) -> BackendResult<T> {
        receiver
            .recv()
            .map_err(|_| BackendError::Native("Windows audio worker stopped responding".into()))?
    }

    pub(super) fn refresh_snapshot(&mut self, only_if_needed: bool) -> BackendResult<()> {
        let (sender, receiver) = mpsc::channel();
        self.command_tx
            .send(if only_if_needed {
                WorkerCommand::RefreshIfNeeded(sender)
            } else {
                WorkerCommand::Refresh(sender)
            })
            .map_err(|_| BackendError::Native("Windows audio worker is unavailable".into()))?;
        let snapshot = Self::response(receiver)?;
        let mut graph = snapshot.graph;
        for (node_id, position) in &self.positions {
            if let Some(node) = graph.nodes.get_mut(node_id) {
                node.position = *position;
            }
        }
        self.endpoint_ports = snapshot.endpoint_ports;
        self.virtual_driver_health = snapshot.virtual_driver_health;
        // Core Audio has never heard of an effect, so the rebuilt graph has
        // no effect nodes in it. Draw them again before the links, or the
        // links that pass through them would have nowhere to land.
        for instance in self.effects.iter() {
            let name = self
                .effects
                .descriptors()
                .into_iter()
                .find(|descriptor| descriptor.id == instance.config.effect_id)
                .map(|descriptor| descriptor.name)
                .unwrap_or_else(|| instance.config.effect_id.clone());
            let position = self
                .effect_positions
                .get(&instance.config.instance_id)
                .copied()
                .unwrap_or_default();
            let _ = Self::draw_effect(&mut graph, instance, &name, position);
        }
        // The worker rebuilds the graph from what Core Audio reports, which
        // knows nothing about the routes qpwgraph is carrying. Drop the ones
        // whose devices have gone, then put the survivors back: a link the
        // user drew must not disappear because an unrelated endpoint changed.
        if let Some(routing) = self.routing.as_mut() {
            let live: BTreeSet<PortId> = graph.ports.keys().copied().collect();
            routing.reconcile(&live)?;
            routing.recover_lost(&self.endpoint_ports)?;
            for link in routing.links() {
                let _ = graph.insert_existing_link(link.clone());
            }
        }
        self.graph = graph;
        self.meterable = snapshot.meterable;
        // A refresh re-reads volumes from Core Audio, which knows only about
        // the part of the level it is holding. Multiply the route's software
        // gain back in, or a boosted node would appear to drop to unity every
        // time an unrelated device changed.
        self.restore_routed_gain();
        #[cfg(feature = "relay")]
        {
            let application_sources_changed =
                self.relay_application_sources != snapshot.application_sources;
            self.relay_endpoint_choices = snapshot.playback_endpoints;
            self.relay_input_choices = snapshot.capture_endpoints;
            self.relay_application_sources = snapshot.application_sources;
            self.relay_default_input = snapshot.default_input;
            self.relay_default_output = snapshot.default_output;
            let default_generation = snapshot.default_generation;
            let default_changed = self.relay_default_generation != default_generation;
            if default_changed {
                self.relay_default_generation = default_generation;
            }
            let endpoint_inactive = self
                .relay
                .as_ref()
                .is_some_and(|devices| !devices.endpoint_active());
            if default_changed || application_sources_changed || endpoint_inactive {
                // A process restart keeps the selector stable but changes its
                // live PID. Rebind the capture worker to the new process. If
                // the app has disappeared entirely, drop the worker rather
                // than silently attaching to an unrelated process.
                let selected_missing = matches!(
                    (self.relay_mode, &self.relay_send_source),
                    (
                        api::RelayMode::Emitter,
                        api::RelaySendSource::Application(selector)
                    ) if !self
                        .relay_application_sources
                        .iter()
                        .any(|(key, _, _)| key == selector)
                );
                if selected_missing {
                    if let Some(devices) = self.relay.as_mut() {
                        devices.deactivate_application(
                            "Windows relay application source disappeared or left QPWGraph Virtual Output",
                        );
                    }
                } else {
                    self.reconcile_relay_worker()?;
                }
            }
        }
        Ok(())
    }
}

impl Drop for WindowsAudioDriver {
    fn drop(&mut self) {
        let _ = self.command_tx.send(WorkerCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl GraphDriver for WindowsAudioDriver {
    fn capabilities(&self) -> BackendCapabilities {
        WINDOWS_AUDIO_CAPABILITIES
    }

    fn refresh(&mut self) -> BackendResult<Vec<Node>> {
        self.refresh_snapshot(false)?;
        Ok(self.graph.nodes.values().cloned().collect())
    }

    fn refresh_if_needed(&mut self) -> BackendResult<Vec<Node>> {
        self.refresh_snapshot(true)?;
        Ok(self.graph.nodes.values().cloned().collect())
    }

    /// Carry audio from one endpoint to another, for real.
    ///
    /// The graph is only touched after the audio is running, so a link never
    /// appears for a route that failed to start. Ports that name an
    /// application session are refused with an explanation rather than drawn:
    /// see [`super::routing`] for what Windows does and does not allow.
    fn connect(&mut self, src: PortId, dst: PortId) -> BackendResult<Link> {
        self.validate_route(src, dst)?;
        let link = managed_link(src, dst);
        if self.graph.links.contains_key(&link.id) {
            return Err(GraphError::DuplicateLink(link.id).into());
        }
        if self.routing.is_none() {
            self.routing = Some(WindowsRouting::start()?);
        }
        let routing = self.routing.as_mut().expect("routing was just started");
        routing.connect(link.clone(), &self.endpoint_ports)?;
        self.graph.add_link(link.id, src, dst)?;
        Ok(link)
    }

    fn disconnect(&mut self, link: LinkId) -> BackendResult<Link> {
        let Some(routing) = self.routing.as_mut() else {
            return Err(BackendError::unsupported(
                "that link is a relationship Windows reports, not a route qpwgraph carries",
            ));
        };
        let removed = routing.disconnect(link)?;
        // The audio has stopped; drop the drawing to match. A link left in
        // the graph after its route is gone is exactly the stale link the
        // parity contract forbids.
        let _ = self.graph.remove_link(link);
        Ok(removed)
    }

    /// Only the routes qpwgraph carries are mutable.
    ///
    /// An observed session-to-endpoint relationship stays visible, selectable,
    /// and clickable, but it is not something a user can rewire, and letting
    /// it into patchbay persistence would promise a restore that cannot
    /// happen.
    fn is_link_mutable(&self, link: LinkId) -> bool {
        self.routing
            .as_ref()
            .is_some_and(|routing| routing.owns(link))
    }

    /// Endpoints can be rewired. An application session becomes routable only
    /// after Windows reports it on QPWGraph Virtual Output, which proves the
    /// original audible path has been isolated and prevents duplicate audio.
    fn node_supports_routing(&self, node: NodeId) -> bool {
        self.graph.nodes.get(&node).is_some_and(|node_record| {
            node_record.node_type == NodeType::WindowsAudioEndpoint
                || (node_record.node_type == NodeType::WindowsAudioSession
                    && self.graph.ports.values().any(|port| {
                        port.node_id == node
                            && matches!(
                                self.endpoint_ports.get(&port.id).map(|port| port.role),
                                Some(EndpointPortRole::Process { .. })
                            )
                    }))
        })
    }

    fn set_node_position(&mut self, node: NodeId, position: [f32; 2]) -> BackendResult<()> {
        self.graph
            .nodes
            .get_mut(&node)
            .ok_or(GraphError::MissingNode(node))?
            .position = position;
        self.positions.insert(node, position);
        Ok(())
    }

    /// Core Audio state as of the last refresh. Reads are served from that
    /// snapshot rather than re-entering COM, so the UI can ask per node per
    /// frame without a round trip to the worker thread.
    fn node_audio_state(&self, node: NodeId) -> BackendResult<NodeAudioState> {
        if !self.graph.nodes.contains_key(&node) {
            return Err(GraphError::MissingNode(node).into());
        }
        Ok(self
            .audio_states
            .lock()
            .ok()
            .and_then(|states| states.get(&node).copied())
            .unwrap_or(NodeAudioState::UNSUPPORTED))
    }

    /// A node only reports meter capability when something actually answered
    /// the meter query for it, so no card is given a meter it cannot fill.
    fn node_capabilities(&self, node: NodeId) -> NodeCapabilities {
        let Ok(state) = self.node_audio_state(node) else {
            return NodeCapabilities::NONE;
        };
        let mut capabilities = state.control_capabilities();
        if self.meterable.contains(&node) {
            // `IAudioMeterInformation` is a *peak* meter, on endpoints and on
            // sessions alike. It has no RMS reading, and `audio_meters` reports
            // rms: 0.0 accordingly, so claiming RMS here would make the UI draw
            // a permanently silent RMS bar next to a working peak one.
            capabilities.meter_peak = true;
            capabilities.meter_rms = false;
        }
        if self.carries_node(node) {
            // Once the router owns the PCM there is a real RMS to show, and
            // software gain that the endpoint's own fader cannot reach. Both
            // last exactly as long as the route does, which is why they are
            // reported per node rather than as a backend-wide capability.
            capabilities.meter_peak = true;
            capabilities.meter_rms = true;
            if capabilities.volume_write {
                capabilities.volume_max = ROUTED_VOLUME_MAX;
            }
        }
        capabilities
    }

    fn set_node_mute(&mut self, node: NodeId, muted: bool) -> BackendResult<()> {
        let (sender, receiver) = mpsc::channel();
        self.command_tx
            .send(WorkerCommand::SetMute(node, muted, sender))
            .map_err(|_| BackendError::Native("Windows audio worker is unavailable".into()))?;
        Self::response(receiver)?;
        // Reflect the write straight away so the card does not flick back to
        // the previous value while waiting for the change callback.
        if let Ok(mut states) = self.audio_states.lock() {
            if let Some(state) = states.get_mut(&node) {
                state.muted = Some(muted);
                state.mute_readable = true;
            }
        }
        Ok(())
    }

    /// Set a node's volume, using software gain for anything past unity.
    ///
    /// A Windows endpoint's own control stops at unity. Where qpwgraph is
    /// carrying that device's audio it can make up the difference itself, so
    /// the endpoint takes `min(volume, 1.0)` and the route takes the rest.
    /// The two multiply, which is why the composition is exact rather than
    /// approximate — and why the boost disappears honestly if the route does.
    fn set_node_volume(&mut self, node: NodeId, volume: f32) -> BackendResult<()> {
        let ceiling = self.node_capabilities(node).volume_max.max(UNITY_VOLUME);
        let volume = volume.clamp(0.0, ceiling);
        let endpoint_volume = volume.min(UNITY_VOLUME);

        let (sender, receiver) = mpsc::channel();
        self.command_tx
            .send(WorkerCommand::SetVolume(node, endpoint_volume, sender))
            .map_err(|_| BackendError::Native("Windows audio worker is unavailable".into()))?;
        Self::response(receiver)?;

        if let Some(port) = self.routed_source_port(node) {
            let routing = self
                .routing
                .as_mut()
                .expect("the port came from the router");
            routing.set_source_gain(port, volume.max(UNITY_VOLUME))?;
        }

        // Record the composed value: what the user will actually hear, not
        // just the part Windows is holding.
        if let Ok(mut states) = self.audio_states.lock() {
            if let Some(state) = states.get_mut(&node) {
                state.volume = Some(volume);
                state.volume_readable = true;
            }
        }
        Ok(())
    }

    fn graph(&self) -> &Graph {
        &self.graph
    }

    fn graph_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    /// Device and session notification callbacks set the dirty flag for every
    /// topology change, so the application does not have to poll for them.
    fn reports_graph_changes(&self) -> bool {
        true
    }

    fn is_node_type(&self, node_type: NodeType) -> bool {
        matches!(
            node_type,
            NodeType::WindowsAudioEndpoint | NodeType::WindowsAudioSession
        )
    }

    fn is_port_type(&self, port_type: PortType) -> bool {
        matches!(port_type, PortType::Audio)
    }

    /// Core Audio's peak meters, with the router's readings laid over the top
    /// wherever qpwgraph owns the PCM.
    ///
    /// Where a device is routed, the router measured the very samples it
    /// carried, so it can report a real RMS as well as a peak; where it is
    /// not, `IAudioMeterInformation` is still the only source and is still
    /// peak-only. The reading is attached to the port it came out of, which
    /// is what lets a playback device's monitor meter separately from the
    /// device itself.
    fn audio_meters(&mut self) -> BackendResult<Vec<AudioMeter>> {
        let (sender, receiver) = mpsc::channel();
        self.command_tx
            .send(WorkerCommand::AudioMeters(sender))
            .map_err(|_| BackendError::Native("Windows audio worker is unavailable".into()))?;
        let mut meters = Self::response(receiver)?;
        let Some(routing) = self.routing.as_ref() else {
            return Ok(meters);
        };
        for (port, reading) in routing.port_meters() {
            let Some(node_id) = self.graph.port(port).map(|port| port.node_id) else {
                continue;
            };
            let routed = AudioMeter {
                node_id,
                port_id: Some(port),
                rms: reading.rms,
                peak: reading.peak,
                age_ms: reading.age_ms,
                available: true,
            };
            // Replace Core Audio's node-level peak for this node rather than
            // sitting beside it: two readings for one card is one too many,
            // and this is the better of the two.
            match meters.iter_mut().find(|meter| meter.node_id == node_id) {
                Some(existing) => *existing = routed,
                None => meters.push(routed),
            }
        }
        Ok(meters)
    }

    fn set_meter_policy(&mut self, policy: MeterPolicy) -> BackendResult<()> {
        let (sender, receiver) = mpsc::channel();
        self.command_tx
            .send(WorkerCommand::SetMeterPolicy(policy, sender))
            .map_err(|_| BackendError::Native("Windows audio worker is unavailable".into()))?;
        Self::response(receiver)
    }

    fn request_meters(&mut self, nodes: &BTreeSet<NodeId>) -> BackendResult<()> {
        let (sender, receiver) = mpsc::channel();
        self.command_tx
            .send(WorkerCommand::RequestMeters(nodes.clone(), sender))
            .map_err(|_| BackendError::Native("Windows audio worker is unavailable".into()))?;
        Self::response(receiver)
    }

    fn reset_audio_config(&mut self) -> BackendResult<()> {
        let (sender, receiver) = mpsc::channel();
        self.command_tx
            .send(WorkerCommand::ResetAudio(sender))
            .map_err(|_| BackendError::Native("Windows audio worker is unavailable".into()))?;
        Self::response(receiver)
    }
}

/// Effects on Windows.
///
/// Real, because the router owns the PCM: an effect is a node with a processor
/// between its two ports, and routing audio through it is an ordinary graph
/// operation. See [`super::effects`].
impl crate::api::EffectDriver for WindowsAudioDriver {
    fn effect_descriptors(&self) -> Vec<pw_graph_effects::EffectDescriptor> {
        self.effects.descriptors()
    }

    fn effect_instances(&self) -> Vec<crate::api::EffectInstance> {
        self.effects.instances()
    }

    fn supports_effect_nodes(&self) -> bool {
        true
    }

    fn create_effect_node(
        &mut self,
        request: crate::api::EffectNodeRequest,
    ) -> BackendResult<crate::api::EffectInstance> {
        self.create_effect(request)
    }

    fn insert_effect(
        &mut self,
        request: crate::api::EffectInsertRequest,
    ) -> BackendResult<crate::api::EffectInstance> {
        self.insert_effect_into_link(request)
    }

    fn set_effect_enabled(&mut self, instance_id: &str, enabled: bool) -> BackendResult<()> {
        self.set_effect_bypassed(instance_id, enabled)
    }

    fn set_effect_parameter(
        &mut self,
        instance_id: &str,
        parameter: &str,
        value: f32,
    ) -> BackendResult<()> {
        self.set_effect_value(instance_id, parameter, value)
    }

    fn remove_effect(&mut self, instance_id: &str) -> BackendResult<()> {
        self.destroy_effect(instance_id)
    }
}

/// Relay support on Windows.
///
/// The engine is the same one PipeWire uses; only the audio endpoints differ.
/// Direct mode supports physical input capture, playback-monitor loopback,
/// process loopback for an app already isolated on QPWGraph Virtual Output,
/// and render output. A separate optional driver is needed only for a
/// system-wide virtual capture endpoint.
#[cfg(feature = "relay")]
impl api::RelayDriver for WindowsAudioDriver {
    fn relay_available(&self) -> bool {
        true
    }

    fn relay_status(&self) -> api::RelayEngineStatus {
        self.relay
            .as_ref()
            .map(|devices| devices.handle().status())
            .unwrap_or_default()
    }

    fn relay_devices_active(&self) -> bool {
        self.relay
            .as_ref()
            .is_some_and(crate::windows_relay::WindowsRelayDevices::endpoint_active)
    }

    fn relay_start_host(&mut self, request: api::RelayHostRequest) -> BackendResult<u16> {
        if request.mode != api::RelayMode::Receiver {
            return Err(BackendError::unsupported(
                "a local relay host must run in Receiver mode",
            ));
        }
        self.relay_mode = request.mode;
        self.relay_mode_generation = request.mode_generation;
        let config = Self::relay_config(RelayConfigOptions {
            device_name: request.device_name,
            pin: request.pin,
            port: request.port,
            codec: request.codec,
            frame_ms: request.frame_ms,
            transport: request.transport,
            direction: request.direction,
            direction_generation: request.direction_generation,
            mode: request.mode,
            mode_generation: request.mode_generation,
            device_id: request.device_id,
            trusted_peers: request.trusted_peers,
            trust_new_peers: request.trust_new_peers,
        });
        let devices = self.ensure_relay(config)?;
        devices
            .handle()
            .host_start()
            .map_err(|error| BackendError::native(format!("relay host start failed: {error}")))
    }

    fn relay_stop_host(&mut self) -> BackendResult<()> {
        if let Some(devices) = self.relay.as_mut() {
            devices.handle().host_stop().map_err(|error| {
                BackendError::native(format!("relay host stop failed: {error}"))
            })?;
        }
        Ok(())
    }

    #[allow(deprecated)]
    fn relay_connect(
        &mut self,
        target: std::net::SocketAddr,
        pin: &str,
        direction: api::RelayDirection,
        direction_generation: u64,
    ) -> BackendResult<api::RelaySessionId> {
        self.relay_mode = match direction {
            api::RelayDirection::MobileToDesktop => api::RelayMode::Receiver,
            api::RelayDirection::DesktopToMobile => api::RelayMode::Emitter,
        };
        self.relay_mode_generation = direction_generation;
        if self.relay.is_none() {
            let config = Self::relay_config(RelayConfigOptions {
                device_name: "qpwgraph-rs".into(),
                pin: pin.to_owned(),
                port: 0,
                codec: api::RelayCodecKind::Opus,
                frame_ms: 10,
                transport: api::RelayTransportPreference::Auto,
                direction,
                direction_generation,
                mode: match direction {
                    api::RelayDirection::MobileToDesktop => api::RelayMode::Receiver,
                    api::RelayDirection::DesktopToMobile => api::RelayMode::Emitter,
                },
                mode_generation: direction_generation,
                device_id: pw_graph_relay::generate_device_id(),
                trusted_peers: Vec::new(),
                trust_new_peers: true,
            });
            self.ensure_relay(config)?;
        }
        let mut config = self
            .relay
            .as_ref()
            .expect("relay was just created")
            .handle()
            .config();
        config.direction = direction;
        config.direction_generation = direction_generation;
        config.mode = self.relay_mode;
        config.mode_generation = self.relay_mode_generation;
        let roles = api::desktop_relay_client_roles(direction);
        config.client_roles = roles;
        let _ = self.ensure_relay(config.clone())?;
        let devices = self.relay.as_ref().expect("relay was just created");
        Ok(devices.handle().connect(target, pin, roles))
    }

    #[allow(deprecated)]
    fn relay_connect_mode(
        &mut self,
        target: std::net::SocketAddr,
        pin: &str,
        mode: api::RelayMode,
        generation: u64,
    ) -> BackendResult<api::RelaySessionId> {
        let direction = match mode {
            api::RelayMode::Emitter => api::RelayDirection::DesktopToMobile,
            api::RelayMode::Receiver => api::RelayDirection::MobileToDesktop,
        };
        self.relay_mode = mode;
        self.relay_mode_generation = generation;
        if self.relay.is_none() {
            let config = Self::relay_config(RelayConfigOptions {
                device_name: "qpwgraph-rs".into(),
                pin: pin.to_owned(),
                port: 0,
                codec: api::RelayCodecKind::Opus,
                frame_ms: 10,
                transport: api::RelayTransportPreference::Auto,
                direction,
                direction_generation: generation,
                mode,
                mode_generation: generation,
                device_id: pw_graph_relay::generate_device_id(),
                trusted_peers: Vec::new(),
                trust_new_peers: true,
            });
            self.ensure_relay(config)?;
        }
        let mut config = self
            .relay
            .as_ref()
            .expect("relay was just created")
            .handle()
            .config();
        config.pin = pin.to_owned();
        config.direction = direction;
        config.direction_generation = generation;
        config.mode = mode;
        config.mode_generation = generation;
        config.client_roles = mode.roles();
        let _ = self.ensure_relay(config)?;
        let devices = self.relay.as_ref().expect("relay was just created");
        Ok(devices.handle().connect_mode(target, pin, mode))
    }

    #[allow(deprecated)]
    fn relay_connect_trusted(
        &mut self,
        target: std::net::SocketAddr,
        peer_id: &str,
        secret: [u8; 32],
        direction: api::RelayDirection,
        direction_generation: u64,
    ) -> BackendResult<api::RelaySessionId> {
        self.relay_mode = match direction {
            api::RelayDirection::MobileToDesktop => api::RelayMode::Receiver,
            api::RelayDirection::DesktopToMobile => api::RelayMode::Emitter,
        };
        self.relay_mode_generation = direction_generation;
        if self.relay.is_none() {
            let config = Self::relay_config(RelayConfigOptions {
                device_name: "qpwgraph-rs".into(),
                pin: String::new(),
                port: 0,
                codec: api::RelayCodecKind::Opus,
                frame_ms: 10,
                transport: api::RelayTransportPreference::Auto,
                direction,
                direction_generation,
                mode: match direction {
                    api::RelayDirection::MobileToDesktop => api::RelayMode::Receiver,
                    api::RelayDirection::DesktopToMobile => api::RelayMode::Emitter,
                },
                mode_generation: direction_generation,
                device_id: pw_graph_relay::generate_device_id(),
                trusted_peers: Vec::new(),
                trust_new_peers: false,
            });
            self.ensure_relay(config)?;
        }
        let mut config = self
            .relay
            .as_ref()
            .expect("relay was just created")
            .handle()
            .config();
        config.direction = direction;
        config.direction_generation = direction_generation;
        config.mode = self.relay_mode;
        config.mode_generation = self.relay_mode_generation;
        let roles = api::desktop_relay_client_roles(direction);
        config.client_roles = roles;
        let _ = self.ensure_relay(config.clone())?;
        let devices = self.relay.as_ref().expect("relay was just created");
        Ok(devices
            .handle()
            .connect_trusted(target, peer_id, secret, roles))
    }

    #[allow(deprecated)]
    fn relay_connect_trusted_mode(
        &mut self,
        target: std::net::SocketAddr,
        peer_id: &str,
        secret: [u8; 32],
        mode: api::RelayMode,
        generation: u64,
    ) -> BackendResult<api::RelaySessionId> {
        let direction = match mode {
            api::RelayMode::Emitter => api::RelayDirection::DesktopToMobile,
            api::RelayMode::Receiver => api::RelayDirection::MobileToDesktop,
        };
        self.relay_mode = mode;
        self.relay_mode_generation = generation;
        if self.relay.is_none() {
            let config = Self::relay_config(RelayConfigOptions {
                device_name: "qpwgraph-rs".into(),
                pin: String::new(),
                port: 0,
                codec: api::RelayCodecKind::Opus,
                frame_ms: 10,
                transport: api::RelayTransportPreference::Auto,
                direction,
                direction_generation: generation,
                mode,
                mode_generation: generation,
                device_id: pw_graph_relay::generate_device_id(),
                trusted_peers: Vec::new(),
                trust_new_peers: false,
            });
            self.ensure_relay(config)?;
        }
        let mut config = self
            .relay
            .as_ref()
            .expect("relay was just created")
            .handle()
            .config();
        config.direction = direction;
        config.direction_generation = generation;
        config.mode = mode;
        config.mode_generation = generation;
        config.client_roles = mode.roles();
        let _ = self.ensure_relay(config)?;
        let devices = self.relay.as_ref().expect("relay was just created");
        Ok(devices
            .handle()
            .connect_trusted_mode(target, peer_id, secret, mode))
    }

    fn relay_configure_identity(
        &mut self,
        device_id: String,
        trusted_peers: Vec<api::RelayTrustedPeer>,
        transport: api::RelayTransportPreference,
    ) -> BackendResult<()> {
        if let Some(devices) = self.relay.as_ref() {
            let mut config = devices.handle().config();
            config.device_id = device_id;
            config.trusted_peers = trusted_peers;
            config.transport = transport;
            devices.handle().update_config(config);
        } else {
            let config = Self::relay_config(RelayConfigOptions {
                device_name: "qpwgraph-rs".into(),
                pin: String::new(),
                port: 0,
                codec: api::RelayCodecKind::Opus,
                frame_ms: 10,
                transport,
                direction: api::RelayDirection::MobileToDesktop,
                direction_generation: 0,
                mode: self.relay_mode,
                mode_generation: self.relay_mode_generation,
                device_id,
                trusted_peers,
                trust_new_peers: true,
            });
            let _ = self.ensure_relay(config)?;
        }
        Ok(())
    }

    fn relay_disconnect(&mut self, session: api::RelaySessionId) -> BackendResult<()> {
        let Some(devices) = self.relay.as_mut() else {
            return Err(BackendError::native(
                "no relay session exists to disconnect",
            ));
        };
        devices
            .handle()
            .disconnect(session)
            .map_err(|error| BackendError::native(format!("relay disconnect failed: {error}")))
    }

    fn relay_offer_direction(
        &mut self,
        session: api::RelaySessionId,
        direction: api::RelayDirection,
        generation: u64,
    ) -> BackendResult<()> {
        let Some(devices) = self.relay.as_ref() else {
            return Err(BackendError::native("no relay session exists"));
        };
        devices
            .handle()
            .offer_direction(session, direction, generation)
            .map_err(|error| BackendError::native(format!("relay direction offer failed: {error}")))
    }

    fn relay_offer_flow(
        &mut self,
        session: api::RelaySessionId,
        flow: api::RelayFlow,
        generation: u64,
    ) -> BackendResult<()> {
        let Some(devices) = self.relay.as_ref() else {
            return Err(BackendError::native("no relay session exists"));
        };
        devices
            .handle()
            .offer_flow(session, flow, generation)
            .map_err(|error| BackendError::native(format!("relay flow offer failed: {error}")))
    }

    fn relay_offer_mode(
        &mut self,
        session: api::RelaySessionId,
        mode: api::RelayMode,
        generation: u64,
    ) -> BackendResult<()> {
        let Some(devices) = self.relay.as_ref() else {
            return Err(BackendError::native("no relay session exists"));
        };
        devices
            .handle()
            .offer_mode(session, mode, generation)
            .map_err(|error| BackendError::native(format!("relay mode offer failed: {error}")))
    }

    fn relay_trusted_enrollment_secret(
        &self,
        transaction_id: u64,
    ) -> BackendResult<Option<[u8; 32]>> {
        Ok(self
            .relay
            .as_ref()
            .and_then(|devices| devices.handle().trusted_enrollment_secret(transaction_id)))
    }

    fn relay_accept_trusted_enrollment(&mut self, transaction_id: u64) -> BackendResult<()> {
        let Some(devices) = self.relay.as_ref() else {
            return Err(BackendError::native("no relay host is running"));
        };
        devices
            .handle()
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
        let Some(devices) = self.relay.as_ref() else {
            return Err(BackendError::native("no relay host is running"));
        };
        devices
            .handle()
            .reject_trusted_enrollment(transaction_id, reason)
            .map_err(|error| {
                BackendError::native(format!("trusted enrollment rejection failed: {error}"))
            })
    }

    fn relay_remove_trusted_peer(&mut self, peer_id: &str) -> BackendResult<()> {
        let Some(devices) = self.relay.as_ref() else {
            return Err(BackendError::native("no relay engine is running"));
        };
        devices
            .handle()
            .remove_trusted_peer(peer_id)
            .map_err(|error| BackendError::native(format!("trusted peer removal failed: {error}")))
    }

    fn relay_events(&mut self) -> Vec<api::RelayEvent> {
        let mut events = self
            .relay
            .as_mut()
            .map(|devices| devices.handle().events())
            .unwrap_or_default();
        let resolved_modes: Vec<(api::RelayMode, u64)> = events
            .iter()
            .filter_map(|event| match event {
                api::RelayEvent::FlowResolved {
                    generation, mode, ..
                } => Some((*mode, *generation)),
                _ => None,
            })
            .collect();
        for (mode, generation) in resolved_modes {
            self.relay_mode = mode;
            self.relay_mode_generation = generation;
            let Some(config) = self.relay.as_ref().map(|devices| devices.handle().config()) else {
                continue;
            };
            let mut config = config;
            config.mode = mode;
            config.mode_generation = generation;
            config.client_roles = mode.roles();
            if let Err(error) = self.ensure_relay(config) {
                events.push(api::RelayEvent::Error {
                    message: format!("could not switch Windows relay endpoint: {error}"),
                });
            }
        }
        events
    }

    fn relay_discovery_start(&mut self) -> BackendResult<()> {
        if self.relay.is_none() {
            let config = Self::relay_config(RelayConfigOptions {
                device_name: "qpwgraph-rs".into(),
                pin: String::new(),
                port: 0,
                codec: api::RelayCodecKind::Opus,
                frame_ms: 10,
                transport: api::RelayTransportPreference::Auto,
                direction: api::RelayDirection::MobileToDesktop,
                direction_generation: 0,
                mode: self.relay_mode,
                mode_generation: self.relay_mode_generation,
                device_id: pw_graph_relay::generate_device_id(),
                trusted_peers: Vec::new(),
                trust_new_peers: true,
            });
            self.ensure_relay(config)?;
        }
        let devices = self.relay.as_ref().expect("relay was just created");
        devices
            .handle()
            .discovery_start()
            .map_err(|error| BackendError::native(format!("relay discovery failed: {error}")))
    }

    fn relay_discovery_stop(&mut self) {
        if let Some(devices) = self.relay.as_ref() {
            devices.handle().discovery_stop();
        }
    }

    fn relay_discovery_usb_link_lost(&mut self) {
        if let Some(devices) = self.relay.as_ref() {
            devices.handle().discovery_usb_link_lost();
        }
    }

    fn relay_usb_link_present(&self) -> bool {
        pw_graph_relay::netlink::local_links()
            .iter()
            .any(|link| link.kind == pw_graph_relay::LinkKind::Usb)
    }

    fn relay_peers(&self) -> Vec<api::RelayPeerInfo> {
        self.relay
            .as_ref()
            .map(|devices| devices.handle().discovered_peers())
            .unwrap_or_default()
    }

    fn relay_local_links(&self) -> Vec<api::RelayLocalLink> {
        pw_graph_relay::netlink::display_links()
    }

    fn relay_send_sources(&self) -> Vec<api::RelayEndpointInfo> {
        let mut sources = vec![api::RelayEndpointInfo {
            id: "default-input".into(),
            name: "Default input".into(),
            description: "Current Windows eCapture default device".into(),
        }];
        sources.extend(
            self.relay_input_choices
                .iter()
                .map(|(id, name)| api::RelayEndpointInfo {
                    id: format!("input:{id}"),
                    name: name.clone(),
                    description: "WASAPI eCapture input device".into(),
                }),
        );
        sources.extend(
            self.relay_application_sources
                .iter()
                .map(|(selector, name, _pid)| api::RelayEndpointInfo {
                    id: format!("application:{selector}"),
                    name: name.clone(),
                    description: "Process-loopback source for an app on QPWGraph Virtual Output"
                        .into(),
                }),
        );
        sources.push(api::RelayEndpointInfo {
            id: "default-output-monitor".into(),
            name: "Default output monitor".into(),
            description: "Current Windows eRender loopback monitor".into(),
        });
        sources.extend(self.relay_endpoint_choices.iter().map(|(id, name)| {
            api::RelayEndpointInfo {
                id: format!("monitor:{id}"),
                name: format!("{name} monitor"),
                description: "WASAPI eRender loopback monitor".into(),
            }
        }));
        sources
    }

    fn relay_receive_sinks(&self) -> Vec<api::RelayEndpointInfo> {
        let mut sinks = vec![api::RelayEndpointInfo {
            id: "default-output".into(),
            name: "Default output".into(),
            description: "Current Windows eRender default device".into(),
        }];
        sinks.extend(
            self.relay_endpoint_choices
                .iter()
                .map(|(id, name)| api::RelayEndpointInfo {
                    id: format!("output:{id}"),
                    name: name.clone(),
                    description: "WASAPI eRender output device".into(),
                }),
        );
        let relay_render_present = self.relay_endpoint_choices.iter().any(|(_, name)| {
            classify_virtual_endpoint(name) == Some(QpwVirtualEndpointRole::RelayRender)
        });
        let relay_capture_present = self.relay_input_choices.iter().any(|(_, name)| {
            classify_virtual_endpoint(name) == Some(QpwVirtualEndpointRole::RelayCapture)
        });
        if relay_render_present && relay_capture_present {
            sinks.push(api::RelayEndpointInfo {
                id: "virtual-microphone".into(),
                name: "QPWGraph Relay Microphone".into(),
                description: "Optional driver capture endpoint for third-party applications".into(),
            });
        }
        sinks
    }

    fn relay_set_send_source(&mut self, source: api::RelaySendSource) -> BackendResult<()> {
        self.relay_send_source = normalize_windows_send_source(source)?;
        self.reconcile_relay_worker()
    }

    fn relay_set_receive_sink(&mut self, sink: api::RelayReceiveSink) -> BackendResult<()> {
        self.relay_receive_sink = normalize_windows_receive_sink(sink)?;
        self.reconcile_relay_worker()
    }

    fn relay_ensure_local_route(
        &mut self,
        mode: api::RelayMode,
    ) -> BackendResult<api::RelayLocalRouteState> {
        self.relay_mode = mode;
        self.reconcile_relay_worker()?;
        let resolved = self
            .relay
            .as_ref()
            .and_then(|devices| devices.resolved_endpoint().map(str::to_owned));
        Ok(api::RelayLocalRouteState {
            mode: Some(mode),
            active: self
                .relay
                .as_ref()
                .is_some_and(crate::windows_relay::WindowsRelayDevices::endpoint_active),
            source_id: (mode == api::RelayMode::Emitter)
                .then(|| resolved.clone().unwrap_or_else(|| "default-input".into())),
            sink_id: (mode == api::RelayMode::Receiver)
                .then(|| resolved.unwrap_or_else(|| "default-output".into())),
            description: match mode {
                api::RelayMode::Emitter => "Windows direct emitter route".into(),
                api::RelayMode::Receiver => "Windows direct receiver route".into(),
            },
        })
    }
}

#[cfg(feature = "relay")]
fn normalize_windows_send_source(
    source: api::RelaySendSource,
) -> BackendResult<api::RelaySendSource> {
    match source {
        api::RelaySendSource::InputDevice(id) => Ok(api::RelaySendSource::InputDevice(
            id.strip_prefix("input:").unwrap_or(&id).to_owned(),
        )),
        api::RelaySendSource::OutputMonitor(id) => Ok(api::RelaySendSource::OutputMonitor(
            id.strip_prefix("monitor:").unwrap_or(&id).to_owned(),
        )),
        api::RelaySendSource::Application(id) => Ok(api::RelaySendSource::Application(
            id.strip_prefix("application:").unwrap_or(&id).to_owned(),
        )),
        api::RelaySendSource::ManualGraph => Err(BackendError::unsupported(
            "Windows direct relay cannot use a manual graph source",
        )),
        other => Ok(other),
    }
}

#[cfg(feature = "relay")]
fn normalize_windows_receive_sink(
    sink: api::RelayReceiveSink,
) -> BackendResult<api::RelayReceiveSink> {
    match sink {
        api::RelayReceiveSink::OutputDevice(id) => Ok(api::RelayReceiveSink::OutputDevice(
            id.strip_prefix("output:").unwrap_or(&id).to_owned(),
        )),
        api::RelayReceiveSink::VirtualMicrophone => Ok(api::RelayReceiveSink::VirtualMicrophone),
        api::RelayReceiveSink::ManualGraph => Err(BackendError::unsupported(
            "Windows direct relay cannot use a manual graph sink",
        )),
        other => Ok(other),
    }
}
