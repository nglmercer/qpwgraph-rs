//! The public driver.
//!
//! It owns no COM interface. It sends owned commands to the worker thread
//! and reads back owned snapshots, which is what keeps every COM pointer on
//! the single thread that initialized the apartment.

use super::*;
#[cfg(feature = "relay")]
use crate::api;
use crate::api::EffectTicket;

#[path = "driver_application_routes.rs"]
mod application_routes;
#[cfg(feature = "relay")]
#[path = "driver_relay.rs"]
mod relay_driver;
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

/// A route whose effect processors are being prepared off the UI/control
/// stack. No graph or router mutation occurs until every processor is ready.
#[derive(Debug)]
struct PendingApplicationRoute {
    activation: ApplicationRouteActivation,
    source: PortId,
    destination: PortId,
    prepared: BTreeMap<usize, pw_graph_effects::PreparedEffect>,
}

/// Identifies one private route processor in the shared preparation pool.
#[derive(Debug)]
struct PendingApplicationEffect {
    rule_index: usize,
    effect_index: usize,
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
    /// Pending private route preparation is kept separate from public effect
    /// tickets. A route is not visible in the graph until this map is empty
    /// and the complete chain can be activated transactionally.
    pending_application_routes: BTreeMap<usize, PendingApplicationRoute>,
    pending_application_effects: BTreeMap<EffectTicket, PendingApplicationEffect>,
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
    /// The sole boundary for the optional, undocumented per-application
    /// endpoint policy. It is disabled until a build-matched ABI is verified.
    pub(super) app_route_policy: VerifiedAudioPolicyConfig,
    /// Runtime-only ownership records for private per-application endpoint
    /// changes. The role is part of the key because AudioPolicyConfig stores
    /// one persisted endpoint per flow/role; neither the key nor the PID is
    /// written to configuration.
    pub(super) application_route_leases: BTreeMap<(usize, AudioRole), AutomaticAppRouteLease>,
    pub(super) application_route_policy_generation: u64,
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
            pending_application_routes: BTreeMap::new(),
            pending_application_effects: BTreeMap::new(),
            application_route_activations: BTreeMap::new(),
            process_captures: snapshot.process_captures,
            virtual_endpoint_identities: snapshot.virtual_endpoint_identities,
            virtual_driver_health: snapshot.virtual_driver_health,
            app_route_policy: VerifiedAudioPolicyConfig::disabled(),
            application_route_leases: BTreeMap::new(),
            application_route_policy_generation: 0,
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
        self.app_route_policy.support()
    }

    /// Return the privacy-safe diagnostics for automatic application routing.
    /// This never exposes COM pointers, process paths, or endpoint property
    /// blobs.
    pub fn app_route_policy_diagnostics(&self) -> AudioPolicyDiagnostics {
        self.app_route_policy.diagnostics()
    }

    /// Apply the persisted opt-in switch to the isolated policy boundary.
    /// Disabling the switch first stops qpwgraph-owned rerender links, then
    /// gives any live, still-owned lease a chance to restore through the old
    /// policy object. A failed restore is retained for the next live
    /// reconciliation rather than being silently forgotten.
    pub fn set_experimental_app_routing(&mut self, enabled: bool) {
        if !enabled {
            let _ = self.clear_application_route_links();
            self.restore_automatic_application_route_policies(true);
        }
        self.app_route_policy = VerifiedAudioPolicyConfig::new(enabled);
    }

    /// Return the capability split for a live Windows application session.
    /// `None` means that the node is not an application session or has no
    /// stable live process identity to which capture could be attached.
    pub fn process_audio_capabilities(&self, node: NodeId) -> Option<ProcessAudioCapabilities> {
        self.process_audio_capabilities.get(&node).copied()
    }
}

impl Drop for WindowsAudioDriver {
    fn drop(&mut self) {
        // A qpwgraph restart must not abandon a live private policy lease.
        // Restore only while the currently observed endpoint still matches
        // qpwgraph's applied value; user overrides remain untouched. This is
        // best effort because Drop cannot report a native-policy failure.
        if !self.application_route_leases.is_empty() {
            let _ = self.clear_application_route_links();
            self.restore_automatic_application_route_policies(true);
        }
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
        let mut instances = self.effects.instances();
        for instance in &mut instances {
            if let Some(diagnostics) = self
                .routing
                .as_ref()
                .and_then(|routing| routing.effect_diagnostics(instance.input_port))
            {
                instance.diagnostics = Some(diagnostics.summary_text());
                instance.error = diagnostics.failure_reason();
                instance.health = if diagnostics.is_bypassed() {
                    pw_graph_effects::EffectHealth::Bypassed
                } else {
                    pw_graph_effects::EffectHealth::from(diagnostics.health())
                };
                instance.lifecycle = if instance.health == pw_graph_effects::EffectHealth::Failed {
                    pw_graph_effects::EffectLifecycle::Degraded
                } else {
                    pw_graph_effects::EffectLifecycle::Active
                };
            }
        }
        instances
    }

    fn effect_diagnostics(&self, instance_id: &str) -> BackendResult<Option<String>> {
        let input_port = self
            .effects
            .get(instance_id)
            .ok_or_else(|| BackendError::unknown_effect_instance(instance_id))?
            .input_port;
        Ok(self
            .routing
            .as_ref()
            .and_then(|routing| routing.effect_diagnostics(input_port))
            .map(|diagnostics| diagnostics.status_text()))
    }

    fn supports_effect_nodes(&self) -> bool {
        true
    }

    fn begin_create_effect(
        &mut self,
        request: crate::api::EffectCreateRequest,
    ) -> BackendResult<crate::api::EffectTicket> {
        self.effects.begin_create_effect(request)
    }

    fn poll_effect_events(&mut self) -> BackendResult<Vec<crate::api::EffectEvent>> {
        let mut events = Vec::new();
        for event in self.effects.poll_preparation_events() {
            let private_route_ticket = match &event {
                pw_graph_effects::EffectPreparationEvent::Loading { ticket, .. }
                | pw_graph_effects::EffectPreparationEvent::Ready { ticket, .. }
                | pw_graph_effects::EffectPreparationEvent::Failed { ticket, .. }
                | pw_graph_effects::EffectPreparationEvent::Cancelled { ticket } => {
                    self.pending_application_effects.contains_key(ticket)
                }
            };
            if private_route_ticket {
                match event {
                    pw_graph_effects::EffectPreparationEvent::Loading { .. } => {}
                    pw_graph_effects::EffectPreparationEvent::Ready { ticket, prepared } => {
                        self.handle_application_effect_ready(ticket, prepared)?;
                    }
                    pw_graph_effects::EffectPreparationEvent::Failed { ticket, error } => {
                        self.handle_application_effect_failure(ticket, error.to_string());
                    }
                    pw_graph_effects::EffectPreparationEvent::Cancelled { ticket } => {
                        self.handle_application_effect_failure(
                            ticket,
                            "effect preparation was cancelled".into(),
                        );
                    }
                }
                continue;
            }
            match event {
                pw_graph_effects::EffectPreparationEvent::Loading { ticket, stage } => {
                    events.push(crate::api::EffectEvent::Loading { ticket, stage });
                }
                pw_graph_effects::EffectPreparationEvent::Ready { ticket, prepared } => {
                    let Some(request) = self.effects.take_pending(ticket) else {
                        continue;
                    };
                    let result = match request.target {
                        crate::api::EffectTarget::Standalone { position } => self
                            .create_effect_prepared(
                                crate::api::EffectNodeRequest {
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
                        crate::api::EffectTarget::Insert {
                            source,
                            destination,
                            position,
                        } => self.insert_effect_prepared_into_link(
                            crate::api::EffectInsertRequest {
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
                        Ok(instance) => events.push(crate::api::EffectEvent::Ready {
                            ticket,
                            instance: Box::new(instance),
                        }),
                        Err(error) => events.push(crate::api::EffectEvent::Failed {
                            ticket,
                            error: error.to_string(),
                        }),
                    }
                }
                pw_graph_effects::EffectPreparationEvent::Failed { ticket, error } => {
                    self.effects.take_pending(ticket);
                    events.push(crate::api::EffectEvent::Failed {
                        ticket,
                        error: error.to_string(),
                    });
                }
                pw_graph_effects::EffectPreparationEvent::Cancelled { ticket } => {
                    self.effects.take_pending(ticket);
                    events.push(crate::api::EffectEvent::Cancelled { ticket });
                }
            }
        }
        Ok(events)
    }

    fn cancel_effect(&mut self, ticket: crate::api::EffectTicket) -> BackendResult<()> {
        if self.effects.cancel_effect(ticket) {
            Ok(())
        } else {
            Err(BackendError::native(format!(
                "unknown or completed effect ticket {}",
                ticket.0
            )))
        }
    }

    fn create_effect_node(
        &mut self,
        request: crate::api::EffectNodeRequest,
    ) -> BackendResult<crate::api::EffectInstance> {
        let _ = request;
        Err(BackendError::unsupported(
            "Windows effects must be created asynchronously with begin_create_effect",
        ))
    }

    fn insert_effect(
        &mut self,
        request: crate::api::EffectInsertRequest,
    ) -> BackendResult<crate::api::EffectInstance> {
        let _ = request;
        Err(BackendError::unsupported(
            "Windows effects must be inserted asynchronously with begin_create_effect",
        ))
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
