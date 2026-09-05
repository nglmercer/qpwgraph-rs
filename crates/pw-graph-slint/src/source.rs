//! The single backend boundary used by the Slint application.
//!
//! This module deliberately contains no PipeWire/ALSA routing policy. The
//! live backend is opened by `pw-graph-app-core::CompositeDriver`; this small
//! wrapper only selects the deterministic demo backend or delegates to that
//! shared application backend.

use crate::args::Args;
use pw_graph_app_core::{BackendAvailability, CompositeDriver};
#[cfg(all(feature = "relay", target_os = "windows"))]
use pw_graph_backend::RelayEndpoints;
use pw_graph_backend::{
    AudioMeter, BackendCapabilities, DemoDriver, EffectDriver, EffectInsertRequest, EffectInstance,
    EffectNodeRequest, GraphDriver, MeterPolicy,
};
#[cfg(feature = "relay")]
use pw_graph_backend::{
    RelayDirection, RelayDriver, RelayEndpointInfo, RelayEngineStatus, RelayEvent, RelayFlow,
    RelayHostRequest, RelayLocalLink, RelayLocalRouteState, RelayMode, RelayPeerInfo,
    RelayReceiveSink, RelaySendSource, RelaySessionId, RelayTrustedPeer,
};
use pw_graph_core::{Graph, Node, NodeId, PortKey, PortType};
use pw_graph_effects::EffectDescriptor;
use pw_graph_i18n::I18n;
use std::collections::BTreeSet;
use std::time::Instant;

enum BackendKind {
    // Both variants are boxed: they are large, similar in size, and this enum
    // is moved around as part of the application state.
    Demo(Box<DemoDriver>),
    Live(Box<CompositeDriver>),
}

pub(crate) struct ApplicationDriver {
    backend: BackendKind,
    backend_name: String,
    meter_policy: MeterPolicy,
    meter_epoch: Instant,
}

impl ApplicationDriver {
    pub(crate) fn new(args: &Args, meter_policy: MeterPolicy, i18n: &I18n) -> (Self, String) {
        if args.demo {
            let mut source = Self {
                backend: BackendKind::Demo(Box::new(DemoDriver::demo())),
                backend_name: "demo".to_owned(),
                meter_policy,
                meter_epoch: Instant::now(),
            };
            let _ = source.refresh();
            let _ = source.set_meter_policy(meter_policy);
            return (source, i18n.text("status.demo_ready"));
        }

        let (live, availability) = CompositeDriver::open(args.no_midi);
        let backend_name = backend_name(&availability);
        let mut source = Self {
            backend: BackendKind::Live(Box::new(live)),
            backend_name,
            meter_policy,
            meter_epoch: Instant::now(),
        };
        let mut status = if source.backend_name == "none" {
            i18n.text("status.backend_unavailable")
        } else {
            i18n.format(
                "status.live_ready",
                &[("backend", source.backend_name.clone())],
            )
        };
        if !availability.failures.is_empty() {
            status.push_str(" · ");
            status.push_str(
                &availability
                    .failures
                    .iter()
                    .map(|error| i18n.format("status.backend_failed", &[("error", error.clone())]))
                    .collect::<Vec<_>>()
                    .join(" · "),
            );
        }
        if let Err(error) = source.set_meter_policy(meter_policy) {
            status.push_str(" · ");
            status.push_str(&i18n.format(
                "status.meter_policy_failed",
                &[("error", error.to_string())],
            ));
        }
        if let Err(error) = source.refresh() {
            status.push_str(" · ");
            status.push_str(&i18n.format("status.refresh_failed", &[("error", error.to_string())]));
        }
        (source, status)
    }

    pub(crate) fn graph(&self) -> &Graph {
        GraphDriver::graph(self)
    }

    pub(crate) fn backend_name(&self) -> &str {
        &self.backend_name
    }

    /// Text-only Windows audio diagnostics for the support/clipboard path.
    /// Non-Windows and demo backends return an explicit, non-sensitive
    /// availability line rather than pretending a native report exists.
    #[cfg(target_os = "windows")]
    #[allow(dead_code)]
    pub(crate) fn windows_audio_report(&self) -> String {
        match &self.backend {
            BackendKind::Demo(_) => {
                "qpwgraph Windows audio backend unavailable (demo mode)\n".into()
            }
            BackendKind::Live(driver) => driver.windows_audio_report(),
        }
    }

    #[cfg(target_os = "windows")]
    #[allow(dead_code)]
    pub(crate) fn windows_app_route_policy_support(
        &self,
    ) -> pw_graph_backend::AppRoutePolicySupport {
        match &self.backend {
            BackendKind::Demo(_) => pw_graph_backend::AppRoutePolicySupport::ManualOnly {
                reason: "Windows app routing is unavailable in demo mode".into(),
            },
            BackendKind::Live(driver) => driver.windows_app_route_policy_support(),
        }
    }

    pub(crate) fn has_alsa(&self) -> bool {
        match &self.backend {
            BackendKind::Demo(_) => false,
            BackendKind::Live(driver) => driver.has_alsa(),
        }
    }

    pub(crate) fn is_demo(&self) -> bool {
        matches!(self.backend, BackendKind::Demo(_))
    }

    pub(crate) fn has_meter_backend(&self) -> bool {
        self.is_demo()
            || match &self.backend {
                BackendKind::Live(driver) => driver.has_pipewire() || driver.has_windows_audio(),
                BackendKind::Demo(_) => true,
            }
    }

    #[cfg(all(feature = "relay", target_os = "windows"))]
    #[allow(dead_code)]
    pub(crate) fn windows_relay_endpoint_choices(&self) -> Vec<(String, String)> {
        match &self.backend {
            BackendKind::Demo(_) => Vec::new(),
            BackendKind::Live(driver) => driver.windows_relay_endpoint_choices(),
        }
    }

    #[cfg(all(feature = "relay", target_os = "windows"))]
    #[allow(dead_code)]
    pub(crate) fn windows_relay_endpoints(&self) -> RelayEndpoints {
        match &self.backend {
            BackendKind::Demo(_) => RelayEndpoints::default(),
            BackendKind::Live(driver) => driver.windows_relay_endpoints(),
        }
    }

    #[cfg(all(feature = "relay", target_os = "windows"))]
    #[allow(dead_code)]
    pub(crate) fn set_windows_relay_endpoints(
        &mut self,
        endpoints: RelayEndpoints,
    ) -> Result<(), String> {
        match &mut self.backend {
            BackendKind::Demo(_) => Err("Windows relay is unavailable in demo mode".into()),
            BackendKind::Live(driver) => driver
                .set_windows_relay_endpoints(endpoints)
                .map_err(|error| error.to_string()),
        }
    }

    pub(crate) fn capabilities(&self) -> BackendCapabilities {
        GraphDriver::capabilities(self)
    }

    pub(crate) fn is_link_mutable(&self, link: pw_graph_core::LinkId) -> bool {
        self.delegated_is_link_mutable(link)
    }

    fn delegated_is_link_mutable(&self, link: pw_graph_core::LinkId) -> bool {
        match &self.backend {
            BackendKind::Demo(driver) => driver.is_link_mutable(link),
            BackendKind::Live(driver) => driver.is_link_mutable(link),
        }
    }

    pub(crate) fn meter_policy(&self) -> MeterPolicy {
        self.meter_policy
    }

    pub(crate) fn refresh(&mut self) -> Result<(), String> {
        GraphDriver::refresh(self)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Restore persisted Windows application routes and return their explicit
    /// state-machine plans for diagnostics. Other backends reject the request
    /// instead of pretending that a route was applied.
    #[cfg(target_os = "windows")]
    pub(crate) fn reconcile_windows_application_routes(
        &mut self,
        routes: Vec<pw_graph_config::WindowsApplicationRoute>,
    ) -> Result<Vec<pw_graph_backend::ApplicationRoutePlan>, String> {
        match &mut self.backend {
            BackendKind::Demo(_) => {
                Err("Windows application routes are unavailable in demo mode".into())
            }
            BackendKind::Live(driver) => driver
                .reconcile_windows_application_routes(routes)
                .map_err(|error| error.to_string()),
        }
    }

    #[cfg(target_os = "windows")]
    pub(crate) fn windows_application_route_rules(
        &self,
    ) -> Option<Vec<pw_graph_config::WindowsApplicationRoute>> {
        match &self.backend {
            BackendKind::Demo(_) => None,
            BackendKind::Live(driver) => driver.windows_application_route_rules(),
        }
    }

    pub(crate) fn refresh_if_needed(&mut self) -> Result<(), String> {
        GraphDriver::refresh_if_needed(self)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn graph_dirty(&self) -> bool {
        GraphDriver::graph_dirty(self)
    }

    pub(crate) fn reports_graph_changes(&self) -> bool {
        GraphDriver::reports_graph_changes(self)
    }

    pub(crate) fn set_meter_policy(&mut self, policy: MeterPolicy) -> Result<(), String> {
        self.meter_policy = policy;
        GraphDriver::set_meter_policy(self, policy).map_err(|error| error.to_string())
    }

    pub(crate) fn request_meters(&mut self, nodes: &BTreeSet<NodeId>) -> Result<(), String> {
        GraphDriver::request_meters(self, nodes).map_err(|error| error.to_string())
    }

    pub(crate) fn audio_meters(&mut self) -> Result<Vec<AudioMeter>, String> {
        GraphDriver::audio_meters(self).map_err(|error| error.to_string())
    }

    pub(crate) fn reset_meters(&mut self) {
        let _ = GraphDriver::reset_audio_config(self);
    }

    pub(crate) fn set_node_volume(&mut self, node: NodeId, volume: f32) -> Result<(), String> {
        GraphDriver::set_node_volume(self, node, volume).map_err(|error| error.to_string())
    }

    pub(crate) fn set_node_mute(&mut self, node: NodeId, muted: bool) -> Result<(), String> {
        GraphDriver::set_node_mute(self, node, muted).map_err(|error| error.to_string())
    }

    /// Audio state as the backend reports it. The UI renders this and keeps no
    /// copy, so a value the backend cannot read stays visibly unknown.
    pub(crate) fn node_audio_state(
        &self,
        node: NodeId,
    ) -> Result<pw_graph_backend::NodeAudioState, String> {
        GraphDriver::node_audio_state(self, node).map_err(|error| error.to_string())
    }

    pub(crate) fn node_capabilities(&self, node: NodeId) -> pw_graph_backend::NodeCapabilities {
        GraphDriver::node_capabilities(self, node)
    }

    /// Whether the backend that owns this node can rewire it.
    ///
    /// Backend-wide `connect` is a union across children, so on Windows it is
    /// true because MIDI can route even though Core Audio cannot. Asking per
    /// node is what lets the canvas offer a connect gesture only where one can
    /// actually succeed.
    pub(crate) fn node_connectable(&self, node: NodeId) -> bool {
        match &self.backend {
            BackendKind::Demo(driver) => driver.capabilities().connect,
            BackendKind::Live(driver) => driver.capabilities_for_node(node).connect,
        }
    }

    pub(crate) fn connect_by_key_if_missing(
        &mut self,
        output: &PortKey,
        input: &PortKey,
    ) -> Result<bool, String> {
        GraphDriver::connect_by_key_if_missing(self, output, input)
            .map(|link| link.is_some())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn effect_descriptors(&self) -> Vec<EffectDescriptor> {
        EffectDriver::effect_descriptors(self)
    }

    pub(crate) fn effect_instances(&self) -> Vec<EffectInstance> {
        EffectDriver::effect_instances(self)
    }

    pub(crate) fn supports_effect_nodes(&self) -> bool {
        EffectDriver::supports_effect_nodes(self)
    }

    pub(crate) fn create_effect_node(
        &mut self,
        request: EffectNodeRequest,
    ) -> Result<EffectInstance, String> {
        EffectDriver::create_effect_node(self, request).map_err(|error| error.to_string())
    }

    pub(crate) fn insert_effect(
        &mut self,
        request: EffectInsertRequest,
    ) -> Result<EffectInstance, String> {
        EffectDriver::insert_effect(self, request).map_err(|error| error.to_string())
    }

    pub(crate) fn set_effect_enabled(
        &mut self,
        instance_id: &str,
        enabled: bool,
    ) -> Result<(), String> {
        EffectDriver::set_effect_enabled(self, instance_id, enabled)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn set_effect_parameter(
        &mut self,
        instance_id: &str,
        parameter: &str,
        value: f32,
    ) -> Result<(), String> {
        EffectDriver::set_effect_parameter(self, instance_id, parameter, value)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn remove_effect(&mut self, instance_id: &str) -> Result<(), String> {
        EffectDriver::remove_effect(self, instance_id).map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_available(&self) -> bool {
        RelayDriver::relay_available(self)
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_status(&self) -> RelayEngineStatus {
        RelayDriver::relay_status(self)
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_start_host(&mut self, request: RelayHostRequest) -> Result<u16, String> {
        RelayDriver::relay_start_host(self, request).map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_stop_host(&mut self) -> Result<(), String> {
        RelayDriver::relay_stop_host(self).map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    #[allow(dead_code)]
    pub(crate) fn relay_connect(
        &mut self,
        target: std::net::SocketAddr,
        pin: &str,
        direction: RelayDirection,
        direction_generation: u64,
    ) -> Result<RelaySessionId, String> {
        RelayDriver::relay_connect(self, target, pin, direction, direction_generation)
            .map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_connect_mode(
        &mut self,
        target: std::net::SocketAddr,
        pin: &str,
        mode: RelayMode,
        generation: u64,
    ) -> Result<RelaySessionId, String> {
        RelayDriver::relay_connect_mode(self, target, pin, mode, generation)
            .map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    #[allow(dead_code)]
    pub(crate) fn relay_connect_trusted(
        &mut self,
        target: std::net::SocketAddr,
        peer_id: &str,
        secret: [u8; 32],
        direction: RelayDirection,
        direction_generation: u64,
    ) -> Result<RelaySessionId, String> {
        RelayDriver::relay_connect_trusted(
            self,
            target,
            peer_id,
            secret,
            direction,
            direction_generation,
        )
        .map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_connect_trusted_mode(
        &mut self,
        target: std::net::SocketAddr,
        peer_id: &str,
        secret: [u8; 32],
        mode: RelayMode,
        generation: u64,
    ) -> Result<RelaySessionId, String> {
        RelayDriver::relay_connect_trusted_mode(self, target, peer_id, secret, mode, generation)
            .map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    #[allow(dead_code)]
    pub(crate) fn relay_offer_direction(
        &mut self,
        session: RelaySessionId,
        direction: RelayDirection,
        generation: u64,
    ) -> Result<(), String> {
        RelayDriver::relay_offer_direction(self, session, direction, generation)
            .map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    #[allow(dead_code)]
    pub(crate) fn relay_offer_flow(
        &mut self,
        session: RelaySessionId,
        flow: RelayFlow,
        generation: u64,
    ) -> Result<(), String> {
        RelayDriver::relay_offer_flow(self, session, flow, generation)
            .map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_offer_mode(
        &mut self,
        session: RelaySessionId,
        mode: RelayMode,
        generation: u64,
    ) -> Result<(), String> {
        RelayDriver::relay_offer_mode(self, session, mode, generation)
            .map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_send_sources(&self) -> Vec<RelayEndpointInfo> {
        RelayDriver::relay_send_sources(self)
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_receive_sinks(&self) -> Vec<RelayEndpointInfo> {
        RelayDriver::relay_receive_sinks(self)
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_set_send_source(&mut self, source: RelaySendSource) -> Result<(), String> {
        RelayDriver::relay_set_send_source(self, source).map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_set_receive_sink(&mut self, sink: RelayReceiveSink) -> Result<(), String> {
        RelayDriver::relay_set_receive_sink(self, sink).map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    #[allow(dead_code)]
    pub(crate) fn relay_ensure_local_route(
        &mut self,
        mode: RelayMode,
    ) -> Result<RelayLocalRouteState, String> {
        RelayDriver::relay_ensure_local_route(self, mode).map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_configure_identity(
        &mut self,
        device_id: String,
        trusted_peers: Vec<RelayTrustedPeer>,
        transport: pw_graph_backend::RelayTransportPreference,
    ) -> Result<(), String> {
        RelayDriver::relay_configure_identity(self, device_id, trusted_peers, transport)
            .map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_disconnect(&mut self, session: RelaySessionId) -> Result<(), String> {
        RelayDriver::relay_disconnect(self, session).map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_trusted_enrollment_secret(
        &self,
        transaction_id: u64,
    ) -> Result<Option<[u8; 32]>, String> {
        RelayDriver::relay_trusted_enrollment_secret(self, transaction_id)
            .map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_accept_trusted_enrollment(
        &mut self,
        transaction_id: u64,
    ) -> Result<(), String> {
        RelayDriver::relay_accept_trusted_enrollment(self, transaction_id)
            .map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_reject_trusted_enrollment(
        &mut self,
        transaction_id: u64,
        reason: &str,
    ) -> Result<(), String> {
        RelayDriver::relay_reject_trusted_enrollment(self, transaction_id, reason)
            .map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_remove_trusted_peer(&mut self, peer_id: &str) -> Result<(), String> {
        RelayDriver::relay_remove_trusted_peer(self, peer_id).map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_events(&mut self) -> Vec<RelayEvent> {
        RelayDriver::relay_events(self)
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_discovery_start(&mut self) -> Result<(), String> {
        RelayDriver::relay_discovery_start(self).map_err(|error| error.to_string())
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_discovery_stop(&mut self) {
        RelayDriver::relay_discovery_stop(self)
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_discovery_usb_link_lost(&mut self) {
        RelayDriver::relay_discovery_usb_link_lost(self)
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_usb_link_present(&self) -> bool {
        RelayDriver::relay_usb_link_present(self)
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_peers(&self) -> Vec<RelayPeerInfo> {
        RelayDriver::relay_peers(self)
    }

    #[cfg(feature = "relay")]
    pub(crate) fn relay_local_links(&self) -> Vec<RelayLocalLink> {
        RelayDriver::relay_local_links(self)
    }
}

fn backend_name(availability: &BackendAvailability) -> String {
    #[cfg(target_os = "windows")]
    {
        match (availability.windows_audio, availability.windows_midi) {
            (true, true) => "windows-core-audio+winmm-midi".to_owned(),
            (true, false) => "windows-core-audio".to_owned(),
            (false, true) => "winmm-midi".to_owned(),
            (false, false) => "none".to_owned(),
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        match (availability.pipewire, availability.alsa) {
            (true, true) => "pipewire+alsa".to_owned(),
            (true, false) => "pipewire".to_owned(),
            (false, true) => "alsa".to_owned(),
            (false, false) => "none".to_owned(),
        }
    }
}

impl GraphDriver for ApplicationDriver {
    fn capabilities(&self) -> BackendCapabilities {
        match &self.backend {
            BackendKind::Demo(driver) => driver.capabilities(),
            BackendKind::Live(driver) => driver.capabilities(),
        }
    }

    fn is_link_mutable(&self, link: pw_graph_core::LinkId) -> bool {
        self.delegated_is_link_mutable(link)
    }

    fn refresh(&mut self) -> pw_graph_backend::BackendResult<Vec<Node>> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.refresh(),
            BackendKind::Live(driver) => driver.refresh(),
        }
    }

    fn refresh_if_needed(&mut self) -> pw_graph_backend::BackendResult<Vec<Node>> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.refresh_if_needed(),
            BackendKind::Live(driver) => driver.refresh_if_needed(),
        }
    }

    fn connect(
        &mut self,
        src: pw_graph_core::PortId,
        dst: pw_graph_core::PortId,
    ) -> pw_graph_backend::BackendResult<pw_graph_core::Link> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.connect(src, dst),
            BackendKind::Live(driver) => driver.connect(src, dst),
        }
    }

    fn disconnect(
        &mut self,
        link: pw_graph_core::LinkId,
    ) -> pw_graph_backend::BackendResult<pw_graph_core::Link> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.disconnect(link),
            BackendKind::Live(driver) => driver.disconnect(link),
        }
    }

    fn set_node_position(
        &mut self,
        node: NodeId,
        position: [f32; 2],
    ) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.set_node_position(node, position),
            BackendKind::Live(driver) => driver.set_node_position(node, position),
        }
    }

    fn node_audio_state(
        &self,
        node: NodeId,
    ) -> pw_graph_backend::BackendResult<pw_graph_backend::NodeAudioState> {
        match &self.backend {
            BackendKind::Demo(driver) => driver.node_audio_state(node),
            BackendKind::Live(driver) => driver.node_audio_state(node),
        }
    }

    fn node_capabilities(&self, node: NodeId) -> pw_graph_backend::NodeCapabilities {
        match &self.backend {
            BackendKind::Demo(driver) => driver.node_capabilities(node),
            BackendKind::Live(driver) => driver.node_capabilities(node),
        }
    }

    fn set_node_mute(&mut self, node: NodeId, muted: bool) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.set_node_mute(node, muted),
            BackendKind::Live(driver) => driver.set_node_mute(node, muted),
        }
    }

    fn set_node_volume(
        &mut self,
        node: NodeId,
        volume: f32,
    ) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.set_node_volume(node, volume),
            BackendKind::Live(driver) => driver.set_node_volume(node, volume),
        }
    }

    fn graph(&self) -> &Graph {
        match &self.backend {
            BackendKind::Demo(driver) => driver.graph(),
            BackendKind::Live(driver) => driver.graph(),
        }
    }

    fn graph_dirty(&self) -> bool {
        match &self.backend {
            BackendKind::Demo(driver) => driver.graph_dirty(),
            BackendKind::Live(driver) => driver.graph_dirty(),
        }
    }

    fn reports_graph_changes(&self) -> bool {
        match &self.backend {
            BackendKind::Demo(driver) => driver.reports_graph_changes(),
            BackendKind::Live(driver) => driver.reports_graph_changes(),
        }
    }

    fn is_node_type(&self, node_type: pw_graph_core::NodeType) -> bool {
        match &self.backend {
            BackendKind::Demo(driver) => driver.is_node_type(node_type),
            BackendKind::Live(driver) => driver.is_node_type(node_type),
        }
    }

    fn is_port_type(&self, port_type: PortType) -> bool {
        match &self.backend {
            BackendKind::Demo(driver) => driver.is_port_type(port_type),
            BackendKind::Live(driver) => driver.is_port_type(port_type),
        }
    }

    fn audio_meters(&mut self) -> pw_graph_backend::BackendResult<Vec<AudioMeter>> {
        match &mut self.backend {
            BackendKind::Demo(driver) => {
                if self.meter_policy == MeterPolicy::Disabled {
                    Ok(Vec::new())
                } else {
                    Ok(demo_meters(driver.graph(), self.meter_epoch.elapsed()))
                }
            }
            BackendKind::Live(driver) => driver.audio_meters(),
        }
    }

    fn set_meter_policy(&mut self, policy: MeterPolicy) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.set_meter_policy(policy),
            BackendKind::Live(driver) => driver.set_meter_policy(policy),
        }
    }

    fn request_meters(&mut self, nodes: &BTreeSet<NodeId>) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.request_meters(nodes),
            BackendKind::Live(driver) => driver.request_meters(nodes),
        }
    }

    fn reset_audio_config(&mut self) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.reset_audio_config(),
            BackendKind::Live(driver) => driver.reset_audio_config(),
        }
    }
}

impl EffectDriver for ApplicationDriver {
    fn effect_descriptors(&self) -> Vec<EffectDescriptor> {
        match &self.backend {
            BackendKind::Demo(driver) => driver.effect_descriptors(),
            BackendKind::Live(driver) => driver.effect_descriptors(),
        }
    }

    fn effect_instances(&self) -> Vec<EffectInstance> {
        match &self.backend {
            BackendKind::Demo(driver) => driver.effect_instances(),
            BackendKind::Live(driver) => driver.effect_instances(),
        }
    }

    fn supports_effect_nodes(&self) -> bool {
        match &self.backend {
            BackendKind::Demo(driver) => driver.supports_effect_nodes(),
            BackendKind::Live(driver) => driver.supports_effect_nodes(),
        }
    }

    fn create_effect_node(
        &mut self,
        request: EffectNodeRequest,
    ) -> pw_graph_backend::BackendResult<EffectInstance> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.create_effect_node(request),
            BackendKind::Live(driver) => driver.create_effect_node(request),
        }
    }

    fn insert_effect(
        &mut self,
        request: EffectInsertRequest,
    ) -> pw_graph_backend::BackendResult<EffectInstance> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.insert_effect(request),
            BackendKind::Live(driver) => driver.insert_effect(request),
        }
    }

    fn set_effect_enabled(
        &mut self,
        instance_id: &str,
        enabled: bool,
    ) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.set_effect_enabled(instance_id, enabled),
            BackendKind::Live(driver) => driver.set_effect_enabled(instance_id, enabled),
        }
    }

    fn set_effect_parameter(
        &mut self,
        instance_id: &str,
        parameter: &str,
        value: f32,
    ) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.set_effect_parameter(instance_id, parameter, value),
            BackendKind::Live(driver) => driver.set_effect_parameter(instance_id, parameter, value),
        }
    }

    fn remove_effect(&mut self, instance_id: &str) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.remove_effect(instance_id),
            BackendKind::Live(driver) => driver.remove_effect(instance_id),
        }
    }
}

#[cfg(feature = "relay")]
impl RelayDriver for ApplicationDriver {
    fn relay_available(&self) -> bool {
        match &self.backend {
            BackendKind::Demo(driver) => driver.relay_available(),
            BackendKind::Live(driver) => driver.relay_available(),
        }
    }

    fn relay_status(&self) -> RelayEngineStatus {
        match &self.backend {
            BackendKind::Demo(driver) => driver.relay_status(),
            BackendKind::Live(driver) => driver.relay_status(),
        }
    }

    fn relay_devices_active(&self) -> bool {
        match &self.backend {
            BackendKind::Demo(driver) => driver.relay_devices_active(),
            BackendKind::Live(driver) => driver.relay_devices_active(),
        }
    }

    fn relay_start_host(
        &mut self,
        request: RelayHostRequest,
    ) -> pw_graph_backend::BackendResult<u16> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.relay_start_host(request),
            BackendKind::Live(driver) => driver.relay_start_host(request),
        }
    }

    fn relay_stop_host(&mut self) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.relay_stop_host(),
            BackendKind::Live(driver) => driver.relay_stop_host(),
        }
    }

    fn relay_connect(
        &mut self,
        target: std::net::SocketAddr,
        pin: &str,
        direction: RelayDirection,
        direction_generation: u64,
    ) -> pw_graph_backend::BackendResult<RelaySessionId> {
        match &mut self.backend {
            BackendKind::Demo(driver) => {
                driver.relay_connect(target, pin, direction, direction_generation)
            }
            BackendKind::Live(driver) => {
                driver.relay_connect(target, pin, direction, direction_generation)
            }
        }
    }

    fn relay_connect_mode(
        &mut self,
        target: std::net::SocketAddr,
        pin: &str,
        mode: RelayMode,
        generation: u64,
    ) -> pw_graph_backend::BackendResult<RelaySessionId> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.relay_connect_mode(target, pin, mode, generation),
            BackendKind::Live(driver) => driver.relay_connect_mode(target, pin, mode, generation),
        }
    }

    fn relay_connect_trusted(
        &mut self,
        target: std::net::SocketAddr,
        peer_id: &str,
        secret: [u8; 32],
        direction: RelayDirection,
        direction_generation: u64,
    ) -> pw_graph_backend::BackendResult<RelaySessionId> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.relay_connect_trusted(
                target,
                peer_id,
                secret,
                direction,
                direction_generation,
            ),
            BackendKind::Live(driver) => driver.relay_connect_trusted(
                target,
                peer_id,
                secret,
                direction,
                direction_generation,
            ),
        }
    }

    fn relay_connect_trusted_mode(
        &mut self,
        target: std::net::SocketAddr,
        peer_id: &str,
        secret: [u8; 32],
        mode: RelayMode,
        generation: u64,
    ) -> pw_graph_backend::BackendResult<RelaySessionId> {
        match &mut self.backend {
            BackendKind::Demo(driver) => {
                driver.relay_connect_trusted_mode(target, peer_id, secret, mode, generation)
            }
            BackendKind::Live(driver) => {
                driver.relay_connect_trusted_mode(target, peer_id, secret, mode, generation)
            }
        }
    }

    fn relay_configure_identity(
        &mut self,
        device_id: String,
        trusted_peers: Vec<RelayTrustedPeer>,
        transport: pw_graph_backend::RelayTransportPreference,
    ) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => {
                driver.relay_configure_identity(device_id, trusted_peers, transport)
            }
            BackendKind::Live(driver) => {
                driver.relay_configure_identity(device_id, trusted_peers, transport)
            }
        }
    }

    fn relay_disconnect(&mut self, session: RelaySessionId) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.relay_disconnect(session),
            BackendKind::Live(driver) => driver.relay_disconnect(session),
        }
    }

    fn relay_offer_direction(
        &mut self,
        session: RelaySessionId,
        direction: RelayDirection,
        generation: u64,
    ) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => {
                driver.relay_offer_direction(session, direction, generation)
            }
            BackendKind::Live(driver) => {
                driver.relay_offer_direction(session, direction, generation)
            }
        }
    }

    fn relay_offer_flow(
        &mut self,
        session: RelaySessionId,
        flow: RelayFlow,
        generation: u64,
    ) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.relay_offer_flow(session, flow, generation),
            BackendKind::Live(driver) => driver.relay_offer_flow(session, flow, generation),
        }
    }

    fn relay_offer_mode(
        &mut self,
        session: RelaySessionId,
        mode: RelayMode,
        generation: u64,
    ) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.relay_offer_mode(session, mode, generation),
            BackendKind::Live(driver) => driver.relay_offer_mode(session, mode, generation),
        }
    }

    fn relay_trusted_enrollment_secret(
        &self,
        transaction_id: u64,
    ) -> pw_graph_backend::BackendResult<Option<[u8; 32]>> {
        match &self.backend {
            BackendKind::Demo(driver) => driver.relay_trusted_enrollment_secret(transaction_id),
            BackendKind::Live(driver) => driver.relay_trusted_enrollment_secret(transaction_id),
        }
    }

    fn relay_accept_trusted_enrollment(
        &mut self,
        transaction_id: u64,
    ) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.relay_accept_trusted_enrollment(transaction_id),
            BackendKind::Live(driver) => driver.relay_accept_trusted_enrollment(transaction_id),
        }
    }

    fn relay_reject_trusted_enrollment(
        &mut self,
        transaction_id: u64,
        reason: &str,
    ) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => {
                driver.relay_reject_trusted_enrollment(transaction_id, reason)
            }
            BackendKind::Live(driver) => {
                driver.relay_reject_trusted_enrollment(transaction_id, reason)
            }
        }
    }

    fn relay_remove_trusted_peer(&mut self, peer_id: &str) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.relay_remove_trusted_peer(peer_id),
            BackendKind::Live(driver) => driver.relay_remove_trusted_peer(peer_id),
        }
    }

    fn relay_events(&mut self) -> Vec<RelayEvent> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.relay_events(),
            BackendKind::Live(driver) => driver.relay_events(),
        }
    }

    fn relay_discovery_start(&mut self) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.relay_discovery_start(),
            BackendKind::Live(driver) => driver.relay_discovery_start(),
        }
    }

    fn relay_discovery_stop(&mut self) {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.relay_discovery_stop(),
            BackendKind::Live(driver) => driver.relay_discovery_stop(),
        }
    }

    fn relay_discovery_usb_link_lost(&mut self) {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.relay_discovery_usb_link_lost(),
            BackendKind::Live(driver) => driver.relay_discovery_usb_link_lost(),
        }
    }

    fn relay_usb_link_present(&self) -> bool {
        match &self.backend {
            BackendKind::Demo(driver) => driver.relay_usb_link_present(),
            BackendKind::Live(driver) => driver.relay_usb_link_present(),
        }
    }

    fn relay_peers(&self) -> Vec<RelayPeerInfo> {
        match &self.backend {
            BackendKind::Demo(driver) => driver.relay_peers(),
            BackendKind::Live(driver) => driver.relay_peers(),
        }
    }

    fn relay_local_links(&self) -> Vec<RelayLocalLink> {
        match &self.backend {
            BackendKind::Demo(driver) => driver.relay_local_links(),
            BackendKind::Live(driver) => driver.relay_local_links(),
        }
    }

    fn relay_send_sources(&self) -> Vec<RelayEndpointInfo> {
        match &self.backend {
            BackendKind::Demo(driver) => driver.relay_send_sources(),
            BackendKind::Live(driver) => driver.relay_send_sources(),
        }
    }

    fn relay_receive_sinks(&self) -> Vec<RelayEndpointInfo> {
        match &self.backend {
            BackendKind::Demo(driver) => driver.relay_receive_sinks(),
            BackendKind::Live(driver) => driver.relay_receive_sinks(),
        }
    }

    fn relay_set_send_source(
        &mut self,
        source: RelaySendSource,
    ) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.relay_set_send_source(source),
            BackendKind::Live(driver) => driver.relay_set_send_source(source),
        }
    }

    fn relay_set_receive_sink(
        &mut self,
        sink: RelayReceiveSink,
    ) -> pw_graph_backend::BackendResult<()> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.relay_set_receive_sink(sink),
            BackendKind::Live(driver) => driver.relay_set_receive_sink(sink),
        }
    }

    fn relay_ensure_local_route(
        &mut self,
        mode: RelayMode,
    ) -> pw_graph_backend::BackendResult<RelayLocalRouteState> {
        match &mut self.backend {
            BackendKind::Demo(driver) => driver.relay_ensure_local_route(mode),
            BackendKind::Live(driver) => driver.relay_ensure_local_route(mode),
        }
    }
}

fn demo_meters(graph: &Graph, elapsed: std::time::Duration) -> Vec<AudioMeter> {
    let elapsed = elapsed.as_secs_f32();
    graph
        .nodes
        .values()
        .filter(|node| {
            node.ports.iter().any(|port_id| {
                graph
                    .port(*port_id)
                    .is_some_and(|port| port.port_type == PortType::Audio)
            })
        })
        .map(|node| {
            let phase = elapsed * (1.3 + (node.id.0 % 5) as f32 * 0.19) + (node.id.0 % 17) as f32;
            let rms = (0.07 + phase.sin().abs() * 0.58).clamp(0.0, 1.0);
            let peak = (rms + 0.12 + (phase * 1.7).sin().abs() * 0.18).clamp(0.0, 1.0);
            AudioMeter {
                node_id: node.id,
                port_id: None,
                rms,
                peak,
                age_ms: 0,
                available: true,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pw_graph_backend::GraphDriver;
    use pw_graph_patchbay::Patchbay;

    #[test]
    fn observed_link_stays_immutable_through_application_driver() {
        let mut backend = DemoDriver::demo();
        let link = backend
            .connect(pw_graph_core::PortId(1), pw_graph_core::PortId(3))
            .unwrap();
        backend.mark_link_observed(link.id);

        let application = ApplicationDriver {
            backend: BackendKind::Demo(Box::new(backend)),
            backend_name: "test".into(),
            meter_policy: MeterPolicy::Disabled,
            meter_epoch: Instant::now(),
        };

        assert!(!application.is_link_mutable(link.id));
        assert!(!GraphDriver::is_link_mutable(&application, link.id));

        let mut patchbay = Patchbay::new("observed");
        patchbay.snapshot_driver(&application, true);
        assert!(patchbay.connections.is_empty());
    }
}
