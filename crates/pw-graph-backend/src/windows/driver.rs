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

/// Read the kernel-reported Windows version for diagnostics.  `GetVersionExW`
/// is compatibility-shimmed for some application manifests; `RtlGetVersion`
/// reports the host build that matters when reproducing Core Audio behavior.
fn windows_os_build() -> String {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..OSVERSIONINFOW::default()
    };
    let status = unsafe { RtlGetVersion(&mut version) };
    if status.0 >= 0 {
        format!(
            "{}.{}.{}",
            version.dwMajorVersion, version.dwMinorVersion, version.dwBuildNumber
        )
    } else {
        format!("unavailable (NTSTATUS 0x{:08x})", status.0 as u32)
    }
}

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
    pub(super) endpoint_selectors: BTreeMap<String, WindowsEndpointSelector>,
    /// Per-session process capabilities. Read-only capture and relay are
    /// available for ordinary render sessions; mutable routing/effects stay
    /// limited to sessions proven isolated on the qpwgraph virtual output.
    pub(super) process_audio_capabilities: BTreeMap<NodeId, ProcessAudioCapabilities>,
    /// Live candidates and persisted-rule decisions are kept separate from
    /// the graph so a process restart can be reconciled without inventing a
    /// graph edge or persisting a PID.
    pub(super) application_route_candidates: Vec<ApplicationRouteCandidate>,
    pub(super) application_route_ports: BTreeMap<(String, u32), PortId>,
    pub(super) application_routes: ApplicationRouteReconciler,
    /// Links restored from persisted application rules, keyed by rule index.
    /// A route with effects owns a short chain of links rather than one direct
    /// edge; all of them are removed/rebuilt as the live selector changes.
    pub(super) application_route_links: BTreeMap<usize, Vec<LinkId>>,
    /// Private effect instances created for persisted application routes.
    pub(super) application_route_effects: BTreeMap<usize, Vec<String>>,
    /// Last successfully installed activation. Keeping the accepted plan
    /// lets refreshes preserve realtime processors instead of tearing them
    /// down and recreating them when nothing actually changed.
    pub(super) application_route_activations: BTreeMap<usize, ApplicationRouteActivation>,
    pub(super) process_captures: Vec<ProcessCaptureStatus>,
    /// Provider-verified virtual endpoint roles for diagnostics and future
    /// endpoint-specific worker restart decisions.
    pub(super) virtual_endpoint_identities: Vec<QpwVirtualEndpointIdentity>,
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
    /// Live render applications available as read-only process-loopback relay
    /// sources, as `(stable selector, display name, current PID)`. This list
    /// is intentionally independent of virtual-output isolation; isolation is
    /// checked only when a mutable local route is requested.
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
            endpoint_selectors: snapshot.endpoint_selectors,
            process_audio_capabilities: snapshot.process_audio_capabilities,
            application_route_candidates: snapshot.application_route_candidates,
            application_route_ports: snapshot.application_route_ports,
            application_routes: ApplicationRouteReconciler::default(),
            application_route_links: BTreeMap::new(),
            application_route_effects: BTreeMap::new(),
            application_route_activations: BTreeMap::new(),
            process_captures: snapshot.process_captures,
            virtual_endpoint_identities: snapshot.virtual_endpoint_identities,
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
        // The persisted/UI selector may be a PKEY stable id. Resolve it to
        // the current MMDevice id only at the WASAPI boundary; the relay
        // object keeps the durable selector for restart comparisons.
        let resolved_send_source = self.resolve_relay_send_source(&send_source);
        let resolved_receive_sink = self.resolve_relay_receive_sink(&receive_sink);
        let application = if mode == api::RelayMode::Emitter {
            match &send_source {
                api::RelaySendSource::Application(selector) => Some(
                    self.relay_application_source(selector)
                        .ok_or_else(|| {
                            BackendError::unsupported(
                                "selected application is not currently available for process-loopback capture",
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
                                .is_some_and(|key| {
                                    key.eq_ignore_ascii_case(&application.selector_key)
                                });
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
        let selection = crate::windows_relay::RelayWorkerSelection {
            mode,
            send_source,
            receive_sink,
            application,
            default_generation,
            resolved_send_source,
            resolved_receive_sink,
        };
        let restart = self.relay.as_ref().is_some_and(|devices| {
            devices.endpoints() != &wanted
                || devices.needs_restart(
                    selection.mode,
                    &selection.send_source,
                    &selection.receive_sink,
                    selection.application.as_ref(),
                    selection.default_generation,
                )
        });
        if let Some(devices) = self.relay.as_mut() {
            if restart {
                // Keep the authenticated engine/session table alive while
                // replacing only the WASAPI worker.
                devices.restart_endpoint(selection)?;
            }
            devices.handle().update_config(config);
        } else {
            self.relay = Some(crate::windows_relay::WindowsRelayDevices::start_mode(
                config, wanted, selection,
            )?);
        }
        self.sync_relay_capture_manager()?;
        Ok(self.relay.as_ref().expect("relay was just created"))
    }

    #[cfg(feature = "relay")]
    fn sync_relay_capture_manager(&mut self) -> BackendResult<()> {
        let key = self
            .relay
            .as_ref()
            .and_then(|devices| devices.process_capture_key().cloned());
        let (sender, receiver) = mpsc::channel();
        self.command_tx
            .send(WorkerCommand::SetExternalRelayCapture(key, sender))
            .map_err(|_| BackendError::Native("Windows audio worker is unavailable".into()))?;
        self.process_captures = Self::response(receiver)?;
        if !self.application_routes.rules().is_empty() {
            self.reconcile_application_route_snapshot();
        }
        Ok(())
    }

    #[cfg(feature = "relay")]
    fn resolve_relay_send_source(&self, source: &api::RelaySendSource) -> api::RelaySendSource {
        match source {
            api::RelaySendSource::InputDevice(id) => api::RelaySendSource::InputDevice(
                self.resolve_relay_endpoint_id(id, AudioFlow::Capture),
            ),
            api::RelaySendSource::OutputMonitor(id) => api::RelaySendSource::OutputMonitor(
                self.resolve_relay_endpoint_id(id, AudioFlow::Render),
            ),
            other => other.clone(),
        }
    }

    #[cfg(feature = "relay")]
    fn resolve_relay_receive_sink(&self, sink: &api::RelayReceiveSink) -> api::RelayReceiveSink {
        match sink {
            api::RelayReceiveSink::OutputDevice(id) => api::RelayReceiveSink::OutputDevice(
                self.resolve_relay_endpoint_id(id, AudioFlow::Render),
            ),
            other => other.clone(),
        }
    }

    #[cfg(feature = "relay")]
    fn resolve_relay_endpoint_id(&self, selector: &str, flow: AudioFlow) -> String {
        self.endpoint_selectors
            .iter()
            .find(|(_, endpoint)| {
                endpoint.data_flow == flow
                    && (endpoint
                        .stable_id
                        .as_deref()
                        .is_some_and(|stable_id| stable_id == selector)
                        || endpoint.current_mmdevice_id.as_deref() == Some(selector))
            })
            .map(|(current_id, _)| current_id.clone())
            .unwrap_or_else(|| selector.to_owned())
    }

    #[cfg(feature = "relay")]
    fn relay_endpoint_selector_token(&self, current_id: &str, flow: AudioFlow) -> String {
        self.endpoint_selectors
            .get(current_id)
            .filter(|endpoint| endpoint.data_flow == flow)
            .and_then(|endpoint| {
                endpoint
                    .stable_id
                    .clone()
                    .or_else(|| endpoint.current_mmdevice_id.clone())
            })
            .unwrap_or_else(|| current_id.to_owned())
    }

    #[cfg(feature = "relay")]
    fn relay_application_source(
        &self,
        selector: &str,
    ) -> Option<crate::windows_relay::RelayApplicationSource> {
        self.relay_application_sources
            .iter()
            .find(|(key, _, _)| key.eq_ignore_ascii_case(selector))
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

    /// Return the capability split for a live Windows application session.
    /// `None` means that the node is not an application session or has no
    /// stable live process identity to which capture could be attached.
    pub fn process_audio_capabilities(&self, node: NodeId) -> Option<ProcessAudioCapabilities> {
        self.process_audio_capabilities.get(&node).copied()
    }

    /// Reconcile persisted application routes against the latest live Windows
    /// snapshot. This method only emits transactional plans; the caller must
    /// apply an `Active` plan through the graph/router and report any apply
    /// failure as a degraded route. A PID is accepted only as part of the
    /// current candidate snapshot and is never written to configuration.
    pub fn reconcile_application_routes(
        &mut self,
        routes: Vec<pw_graph_config::WindowsApplicationRoute>,
    ) -> BackendResult<Vec<ApplicationRoutePlan>> {
        self.application_routes.set_rules(routes);
        // Refreshing after installing the rules lets the same refresh pass
        // resolve the current PID, request its capture lease, and publish the
        // final state instead of returning an artificial intermediate plan.
        self.refresh_snapshot(true)?;
        Ok(self.application_routes.plans().cloned().collect())
    }

    fn clear_application_route_links(&mut self) -> BackendResult<()> {
        let mut rules = BTreeSet::new();
        rules.extend(self.application_route_links.keys().copied());
        rules.extend(self.application_route_effects.keys().copied());
        rules.extend(self.application_route_activations.keys().copied());
        for rule in rules {
            self.remove_application_route(rule)?;
        }
        Ok(())
    }

    fn remove_application_route(&mut self, rule_index: usize) -> BackendResult<()> {
        let links = self
            .application_route_links
            .remove(&rule_index)
            .unwrap_or_default();
        let mut first_error = None;
        for link_id in links {
            if let Some(routing) = self.routing.as_mut() {
                if routing.owns(link_id) {
                    match routing.disconnect(link_id) {
                        Ok(removed) => {
                            let _ = self.graph.remove_link(removed.id);
                        }
                        Err(error) => {
                            first_error.get_or_insert(error);
                        }
                    }
                } else {
                    let _ = self.graph.remove_link(link_id);
                }
            } else {
                let _ = self.graph.remove_link(link_id);
            }
        }

        let effect_ids = self
            .application_route_effects
            .remove(&rule_index)
            .unwrap_or_default();
        for effect_id in effect_ids.into_iter().rev() {
            if let Err(error) = self.destroy_effect(&effect_id) {
                first_error.get_or_insert(error);
            }
        }
        self.application_route_activations.remove(&rule_index);
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }

    /// Apply the safe, direct portion of an active persisted route. The
    /// reconciler performs identity/isolation/capture checks; this is the
    /// transactional graph/router boundary that turns an accepted plan into
    /// an actual process-loopback -> endpoint route.
    ///
    /// Effect instances and their links are created as one route transaction.
    /// A route must never be reported active while silently bypassing a saved
    /// processor or while leaving only half of its graph chain installed.
    fn apply_application_route_plans(&mut self) -> BackendResult<()> {
        let mut desired: BTreeMap<usize, (ApplicationRouteActivation, PortId, PortId)> =
            BTreeMap::new();
        let mut unapplied = Vec::new();
        for plan in self.application_routes.plans() {
            let Some(activation) = plan.activation.as_ref() else {
                continue;
            };
            let Some(selector) = activation.selector.runtime_key() else {
                unapplied.push((
                    plan.rule_index,
                    "active route has no runtime application identity".into(),
                ));
                continue;
            };
            let Some(source) = self
                .application_route_ports
                .get(&(selector, activation.pid))
                .copied()
            else {
                unapplied.push((
                    plan.rule_index,
                    "isolated application source port is no longer present".into(),
                ));
                continue;
            };
            let Some(destination_id) = activation.destination.current_mmdevice_id.as_deref() else {
                unapplied.push((
                    plan.rule_index,
                    "resolved destination has no current MMDevice id".into(),
                ));
                continue;
            };
            let Some(destination) = self
                .endpoint_ports
                .iter()
                .find(|(_, endpoint)| {
                    endpoint.role == EndpointPortRole::Render
                        && endpoint.device_id == destination_id
                })
                .map(|(port, _)| *port)
            else {
                unapplied.push((
                    plan.rule_index,
                    "resolved destination render port is no longer present".into(),
                ));
                continue;
            };
            desired.insert(plan.rule_index, (activation.clone(), source, destination));
        }
        for (rule, reason) in unapplied {
            self.application_routes.mark_degraded(rule, reason);
        }

        let existing: BTreeSet<_> = self
            .application_route_activations
            .keys()
            .chain(self.application_route_links.keys())
            .chain(self.application_route_effects.keys())
            .copied()
            .collect();
        for rule in existing {
            let keep = desired
                .get(&rule)
                .is_some_and(|(activation, source, destination)| {
                    self.application_route_activations.get(&rule) == Some(activation)
                        && self
                            .application_route_effects
                            .get(&rule)
                            .and_then(|effect_ids| {
                                self.route_link_ids(*source, *destination, effect_ids)
                            })
                            .is_some_and(|link_ids| {
                                self.application_route_links.get(&rule) == Some(&link_ids)
                                    && link_ids.iter().all(|link| {
                                        self.routing
                                            .as_ref()
                                            .is_some_and(|routing| routing.owns(*link))
                                    })
                            })
                });
            if !keep {
                self.remove_application_route(rule)?;
            }
        }

        for plan in self.application_routes.plans().cloned().collect::<Vec<_>>() {
            let Some((activation, source, destination)) = desired.get(&plan.rule_index).cloned()
            else {
                continue;
            };
            if self.application_route_activations.get(&plan.rule_index) == Some(&activation) {
                continue;
            }
            if self.routing.is_none() {
                self.routing = Some(WindowsRouting::start()?);
            }
            match self.install_application_route(
                plan.rule_index,
                activation.clone(),
                source,
                destination,
            ) {
                Ok((links, effects)) => {
                    self.application_route_links.insert(plan.rule_index, links);
                    self.application_route_effects
                        .insert(plan.rule_index, effects);
                    self.application_route_activations
                        .insert(plan.rule_index, activation);
                }
                Err(error) => {
                    let reason = format!("could not activate restored route: {error}");
                    if activation.effect_instances.is_empty() {
                        self.application_routes
                            .mark_degraded(plan.rule_index, reason);
                    } else {
                        self.application_routes
                            .mark_effect_restore_failed(plan.rule_index, reason);
                    }
                }
            }
        }
        Ok(())
    }

    fn route_effect_id(rule_index: usize, position: usize, instance_id: &str) -> String {
        format!("windows-application-route:{rule_index}:{position}:{instance_id}")
    }

    fn route_link_ids(
        &self,
        source: PortId,
        destination: PortId,
        effect_ids: &[String],
    ) -> Option<Vec<LinkId>> {
        let mut output = source;
        let mut links = Vec::with_capacity(effect_ids.len() + 1);
        for effect_id in effect_ids {
            let effect = self.effects.get(effect_id)?;
            links.push(managed_link(output, effect.input_port).id);
            output = effect.output_port;
        }
        links.push(managed_link(output, destination).id);
        Some(links)
    }

    fn route_effect_position(
        &self,
        source: PortId,
        destination: PortId,
        index: usize,
        count: usize,
    ) -> [f32; 2] {
        let source_position = self
            .graph
            .port(source)
            .and_then(|port| self.graph.node(port.node_id))
            .map(|node| node.position)
            .unwrap_or([0.0, 0.0]);
        let destination_position = self
            .graph
            .port(destination)
            .and_then(|port| self.graph.node(port.node_id))
            .map(|node| node.position)
            .unwrap_or([320.0, 0.0]);
        let fraction = (index + 1) as f32 / (count + 1) as f32;
        [
            source_position[0] + (destination_position[0] - source_position[0]) * fraction,
            source_position[1] + (destination_position[1] - source_position[1]) * fraction,
        ]
    }

    fn install_application_route(
        &mut self,
        rule_index: usize,
        activation: ApplicationRouteActivation,
        source: PortId,
        destination: PortId,
    ) -> BackendResult<(Vec<LinkId>, Vec<String>)> {
        if activation.effect_instances.len() > 16 {
            return Err(BackendError::unsupported(
                "application effect chain exceeds the Windows route limit of 16 processors",
            ));
        }
        let mut effect_ids = Vec::with_capacity(activation.effect_instances.len());
        for (index, config) in activation.effect_instances.iter().enumerate() {
            let mut route_config = config.clone();
            route_config.instance_id =
                Self::route_effect_id(rule_index, index, &config.instance_id);
            let position = self.route_effect_position(
                source,
                destination,
                index,
                activation.effect_instances.len(),
            );
            match self.create_application_effect(route_config, position) {
                Ok(_) => effect_ids.push(Self::route_effect_id(
                    rule_index,
                    index,
                    &config.instance_id,
                )),
                Err(error) => {
                    for effect_id in effect_ids.into_iter().rev() {
                        let _ = self.destroy_effect(&effect_id);
                    }
                    return Err(error);
                }
            }
        }

        let Some(link_ids) = self.route_link_ids(source, destination, &effect_ids) else {
            for effect_id in effect_ids.into_iter().rev() {
                let _ = self.destroy_effect(&effect_id);
            }
            return Err(BackendError::native(
                "restored application effect disappeared",
            ));
        };
        let links: Vec<_> = {
            let mut output = source;
            let mut links = Vec::with_capacity(effect_ids.len() + 1);
            for effect_id in &effect_ids {
                let effect = self
                    .effects
                    .get(effect_id)
                    .expect("effect was just created");
                links.push(managed_link(output, effect.input_port));
                output = effect.output_port;
            }
            links.push(managed_link(output, destination));
            links
        };
        let mut connected = Vec::new();
        for link in &links {
            let result = self
                .routing
                .as_mut()
                .expect("routing was started before installing a route")
                .connect(link.clone(), &self.endpoint_ports);
            if let Err(error) = result {
                for link_id in connected.into_iter().rev() {
                    if let Some(routing) = self.routing.as_mut() {
                        let _ = routing.disconnect(link_id);
                    }
                    let _ = self.graph.remove_link(link_id);
                }
                for effect_id in effect_ids.into_iter().rev() {
                    let _ = self.destroy_effect(&effect_id);
                }
                return Err(error);
            }
            connected.push(link.id);
        }

        let gain_result = self
            .routing
            .as_mut()
            .expect("routing is still present")
            .set_source_gain(source, activation.gain.clamp(0.0, 1.5));
        if let Err(error) = gain_result {
            for link_id in connected.iter().rev().copied() {
                if let Some(routing) = self.routing.as_mut() {
                    let _ = routing.disconnect(link_id);
                }
                let _ = self.graph.remove_link(link_id);
            }
            for effect_id in effect_ids.into_iter().rev() {
                let _ = self.destroy_effect(&effect_id);
            }
            return Err(error);
        }

        let mut graph_links = Vec::new();
        for link in &links {
            if let Err(error) = self
                .graph
                .add_link(link.id, link.output_port, link.input_port)
            {
                for link_id in graph_links.into_iter().rev() {
                    let _ = self.graph.remove_link(link_id);
                }
                for link_id in connected.into_iter().rev() {
                    if let Some(routing) = self.routing.as_mut() {
                        let _ = routing.disconnect(link_id);
                    }
                }
                for effect_id in effect_ids.into_iter().rev() {
                    let _ = self.destroy_effect(&effect_id);
                }
                return Err(error.into());
            }
            graph_links.push(link.id);
        }
        Ok((link_ids, effect_ids))
    }

    fn reconcile_application_route_snapshot(&mut self) {
        let mut captures = BTreeMap::new();
        for capture in &self.process_captures {
            let readiness = match &capture.state {
                ProcessCaptureState::Active => ProcessCaptureReadiness::Ready,
                ProcessCaptureState::Unavailable { reason }
                | ProcessCaptureState::Lost { reason } => {
                    ProcessCaptureReadiness::Failed(reason.clone())
                }
            };
            captures.insert((capture.key.selector.clone(), capture.key.pid), readiness);
        }
        let environment = ApplicationRouteEnvironment {
            // This is deliberately a capability boundary, not a version
            // guess. Unsupported activation is reported by the capture
            // readiness state and never becomes an active route.
            os_supported: true,
            virtual_driver_ready: matches!(
                self.virtual_driver_health,
                VirtualAudioDriverHealth::Ready { .. }
            ),
            effects_available: true,
            applications: self.application_route_candidates.clone(),
            endpoints: self.endpoint_selectors.values().cloned().collect(),
            captures,
        };
        self.application_routes
            .migrate_destination_selectors(&environment.endpoints);
        self.application_routes.reconcile(&environment);
    }

    fn sync_application_route_captures(&mut self) -> BackendResult<()> {
        let requests = self
            .application_routes
            .capture_requests()
            .into_iter()
            .filter_map(|(_, selector, pid)| {
                Some(ProcessCaptureRequest {
                    selector: selector.runtime_key()?,
                    pid,
                    mode: ProcessLoopbackMode::IncludeProcessTree,
                })
            })
            .collect();
        let (sender, receiver) = mpsc::channel();
        self.command_tx
            .send(WorkerCommand::ReconcileProcessCaptures(requests, sender))
            .map_err(|_| BackendError::Native("Windows audio worker is unavailable".into()))?;
        self.process_captures = Self::response(receiver)?;
        self.reconcile_application_route_snapshot();
        Ok(())
    }

    /// Route candidates that still need a process-loopback activation before
    /// their saved local route can be considered. The caller can feed these
    /// requests to the worker-owned capture manager and invoke reconciliation
    /// again after the manager reports `Active`.
    pub fn application_route_capture_requests(
        &self,
    ) -> Vec<(usize, pw_graph_config::WindowsApplicationSelector, u32)> {
        self.application_routes.capture_requests()
    }

    /// Return the reconciler's current rules after any legacy endpoint
    /// selector migration. The caller may persist this copy; live PIDs and
    /// native endpoint objects are never part of it.
    pub fn application_route_rules(&self) -> Vec<pw_graph_config::WindowsApplicationRoute> {
        self.application_routes.rules().to_vec()
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
        let _ = writeln!(report, "os=Windows build={}", windows_os_build());
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
        let _ = writeln!(
            report,
            "process_loopback_captures={}",
            self.process_captures.len()
        );
        let _ = writeln!(
            report,
            "process_loopback_support=operational_activation_probe_per_session"
        );
        for capture in &self.process_captures {
            let _ = writeln!(
                report,
                "process_capture selector={:?} pid={} generation={} mode={:?} consumers={} state={:?} error={:?}",
                capture.key.selector,
                capture.key.pid,
                capture.key.generation,
                capture.key.mode,
                capture.consumers.len(),
                capture.state,
                capture.last_error,
            );
        }
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
        for (mmdevice_id, selector) in &self.endpoint_selectors {
            let _ = writeln!(
                report,
                "endpoint mmdevice_id_hash={:016x} stable_id={:?} friendly_name={:?} flow={:?}",
                stable_local_id(mmdevice_id),
                selector.stable_id,
                selector.friendly_name,
                selector.data_flow,
            );
        }
        for identity in &self.virtual_endpoint_identities {
            let _ = writeln!(
                report,
                "virtual_endpoint role={:?} mmdevice_id_hash={:016x} stable_id={:?} driver_version={:?}",
                identity.role,
                stable_local_id(&identity.mmdevice_id),
                identity.stable_endpoint_id,
                identity.driver_version,
            );
        }
        for plan in self.application_routes.plans() {
            let app_key = plan
                .application
                .as_ref()
                .and_then(|application| application.selector.runtime_key())
                .unwrap_or_else(|| "<none>".into());
            let pid = plan
                .application
                .as_ref()
                .map_or(0, |application| application.pid);
            let destination = plan
                .destination
                .as_ref()
                .map(|endpoint| endpoint.stable_id.as_deref().unwrap_or("<no-stable-id>"))
                .unwrap_or("<none>");
            let _ = writeln!(
                report,
                "application_route rule={} state={:?} selector={:?} pid={} destination_stable_id={:?} reason={:?}",
                plan.rule_index, plan.state, app_key, pid, destination, plan.reason,
            );
        }
        #[cfg(feature = "relay")]
        if let Some(relay) = &self.relay {
            let config = relay.handle().config();
            let resolved_hash = relay.resolved_endpoint().map(stable_local_id);
            let _ = writeln!(
                report,
                "relay mode={:?} endpoint_active={} source={:?} sink={:?} resolved_id_hash={:?}",
                config.mode,
                relay.endpoint_active(),
                self.relay_send_source,
                self.relay_receive_sink,
                resolved_hash,
            );
        }
        #[cfg(feature = "relay")]
        for (selector, name, pid) in &self.relay_application_sources {
            let _ = writeln!(
                report,
                "relay_application selector={selector:?} name={name:?} pid={pid} active=capture-only",
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
        self.endpoint_selectors = snapshot.endpoint_selectors;
        self.process_audio_capabilities = snapshot.process_audio_capabilities;
        self.application_route_candidates = snapshot.application_route_candidates;
        self.application_route_ports = snapshot.application_route_ports;
        self.process_captures = snapshot.process_captures;
        self.virtual_endpoint_identities = snapshot.virtual_endpoint_identities;
        self.virtual_driver_health = snapshot.virtual_driver_health;
        if !self.application_routes.rules().is_empty() {
            self.reconcile_application_route_snapshot();
            self.sync_application_route_captures()?;
        } else {
            // Rules may have been removed after a previous refresh. Release
            // route-owned process captures as well as the graph links.
            if self.process_captures.iter().any(|capture| {
                capture
                    .consumers
                    .contains(&ProcessCaptureConsumer::OwnedRoute)
            }) {
                self.sync_application_route_captures()?;
            }
            self.clear_application_route_links()?;
        }
        // Core Audio has never heard of an effect, so the rebuilt graph has
        // no effect nodes in it. Draw them again before the links, or the
        // links that pass through them would have nowhere to land.
        for instance in self.effects.all_instances() {
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
        self.apply_application_route_plans()?;
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
                        .any(|(key, _, _)| key.eq_ignore_ascii_case(selector))
                );
                if selected_missing {
                    if let Some(devices) = self.relay.as_mut() {
                        devices.deactivate_application(
                            "Windows relay application source disappeared; process-loopback worker stopped",
                        );
                    }
                } else {
                    self.reconcile_relay_worker()?;
                }
            }
            self.sync_relay_capture_manager()?;
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
                    && self
                        .process_audio_capabilities
                        .get(&node)
                        .is_some_and(|capabilities| capabilities.mutable_route))
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
        if let Some(process) = self.process_audio_capabilities.get(&node) {
            capabilities.meter_peak |= process.meter_peak;
            capabilities.meter_rms = process.meter_rms;
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
        let (mut meters, process_captures) = Self::response(receiver)?;
        self.process_captures = process_captures;
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
                    id: format!(
                        "input:{}",
                        self.relay_endpoint_selector_token(id, AudioFlow::Capture)
                    ),
                    name: name.clone(),
                    description: "WASAPI eCapture input device; selection uses the stable endpoint identity when available".into(),
                }),
        );
        sources.extend(
            self.relay_application_sources
                .iter()
                .map(|(selector, name, _pid)| api::RelayEndpointInfo {
                    id: format!("application:{selector}"),
                    name: name.clone(),
                    description: "Capture-only process-loopback source; the app keeps its normal local output".into(),
                }),
        );
        sources.push(api::RelayEndpointInfo {
            id: "default-output-monitor".into(),
            name: "Default output monitor".into(),
            description: "Current Windows eRender loopback monitor".into(),
        });
        sources.extend(self.relay_endpoint_choices.iter().map(|(id, name)| {
            api::RelayEndpointInfo {
                id: format!(
                    "monitor:{}",
                    self.relay_endpoint_selector_token(id, AudioFlow::Render)
                ),
                name: format!("{name} monitor"),
                description: "WASAPI eRender loopback monitor; selection uses the stable endpoint identity when available".into(),
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
                    id: format!(
                        "output:{}",
                        self.relay_endpoint_selector_token(id, AudioFlow::Render)
                    ),
                    name: name.clone(),
                    description: "WASAPI eRender output device; selection uses the stable endpoint identity when available".into(),
                }),
        );
        // The receive choice is a provider-owned endpoint, not a friendly
        // name. A third-party device may use the same label, and advertising
        // it here would create a UI option that can only fail later at the
        // WASAPI open boundary.
        let relay_render_present = self
            .virtual_endpoint_identities
            .iter()
            .any(|identity| identity.role == QpwVirtualEndpointRole::RelayRender);
        let relay_capture_present = self
            .virtual_endpoint_identities
            .iter()
            .any(|identity| identity.role == QpwVirtualEndpointRole::RelayCapture);
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
