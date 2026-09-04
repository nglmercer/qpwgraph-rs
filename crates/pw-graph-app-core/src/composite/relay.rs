//! The relay engine lives on one child backend per platform, so the composite
//! only has to find it and forward.

#[cfg(feature = "relay")]
use super::*;

/// Which child driver hosts the relay on this platform.
///
/// PipeWire carries it on Linux through virtual devices; Windows carries it
/// through WASAPI endpoints. Resolving the concrete type once here lets the
/// delegation below be written a single time, so the two platforms cannot
/// drift apart method by method.
#[cfg(feature = "relay")]
impl CompositeDriver {
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    fn relay_backend(&self) -> Option<&PipewireDriver> {
        self.pipewire.as_ref()
    }

    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    fn relay_backend_mut(&mut self) -> Option<&mut PipewireDriver> {
        self.pipewire.as_mut()
    }

    #[cfg(target_os = "windows")]
    fn relay_backend(&self) -> Option<&WindowsAudioDriver> {
        self.windows_audio.as_ref()
    }

    #[cfg(target_os = "windows")]
    fn relay_backend_mut(&mut self) -> Option<&mut WindowsAudioDriver> {
        self.windows_audio.as_mut()
    }

    // No relay-capable backend on this target; `DemoDriver` only supplies a
    // concrete type that implements the trait so the signatures line up.
    #[cfg(not(any(all(target_os = "linux", feature = "pipewire"), target_os = "windows")))]
    fn relay_backend(&self) -> Option<&pw_graph_backend::DemoDriver> {
        None
    }

    #[cfg(not(any(all(target_os = "linux", feature = "pipewire"), target_os = "windows")))]
    fn relay_backend_mut(&mut self) -> Option<&mut pw_graph_backend::DemoDriver> {
        None
    }

    fn relay_unavailable() -> BackendError {
        Self::unsupported("audio relay is not available for this backend")
    }
}

#[cfg(feature = "relay")]
impl pw_graph_backend::RelayDriver for CompositeDriver {
    fn relay_available(&self) -> bool {
        self.relay_backend()
            .is_some_and(|driver| driver.relay_available())
    }

    fn relay_status(&self) -> pw_graph_backend::RelayEngineStatus {
        self.relay_backend()
            .map(|driver| driver.relay_status())
            .unwrap_or_default()
    }

    fn relay_devices_active(&self) -> bool {
        self.relay_backend()
            .is_some_and(|driver| driver.relay_devices_active())
    }

    fn relay_start_host(
        &mut self,
        request: pw_graph_backend::RelayHostRequest,
    ) -> BackendResult<u16> {
        let port = self
            .relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_start_host(request)?;
        // Starting the host can add virtual nodes to the child graph, so the
        // merged view has to catch up. A no-op where it cannot.
        self.rebuild_after_native_mutation();
        Ok(port)
    }

    fn relay_stop_host(&mut self) -> BackendResult<()> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_stop_host()
    }

    fn relay_connect(
        &mut self,
        target: std::net::SocketAddr,
        pin: &str,
        direction: pw_graph_backend::RelayDirection,
        direction_generation: u64,
    ) -> BackendResult<pw_graph_backend::RelaySessionId> {
        let session = self
            .relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_connect(target, pin, direction, direction_generation)?;
        self.rebuild_after_native_mutation();
        Ok(session)
    }

    fn relay_connect_mode(
        &mut self,
        target: std::net::SocketAddr,
        pin: &str,
        mode: pw_graph_backend::RelayMode,
        generation: u64,
    ) -> BackendResult<pw_graph_backend::RelaySessionId> {
        let session = self
            .relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_connect_mode(target, pin, mode, generation)?;
        self.rebuild_after_native_mutation();
        Ok(session)
    }

    fn relay_connect_trusted(
        &mut self,
        target: std::net::SocketAddr,
        peer_id: &str,
        secret: [u8; 32],
        direction: pw_graph_backend::RelayDirection,
        direction_generation: u64,
    ) -> BackendResult<pw_graph_backend::RelaySessionId> {
        let session = self
            .relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_connect_trusted(target, peer_id, secret, direction, direction_generation)?;
        self.rebuild_after_native_mutation();
        Ok(session)
    }

    fn relay_connect_trusted_mode(
        &mut self,
        target: std::net::SocketAddr,
        peer_id: &str,
        secret: [u8; 32],
        mode: pw_graph_backend::RelayMode,
        generation: u64,
    ) -> BackendResult<pw_graph_backend::RelaySessionId> {
        let session = self
            .relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_connect_trusted_mode(target, peer_id, secret, mode, generation)?;
        self.rebuild_after_native_mutation();
        Ok(session)
    }

    fn relay_disconnect(&mut self, session: pw_graph_backend::RelaySessionId) -> BackendResult<()> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_disconnect(session)
    }

    fn relay_offer_direction(
        &mut self,
        session: pw_graph_backend::RelaySessionId,
        direction: pw_graph_backend::RelayDirection,
        generation: u64,
    ) -> BackendResult<()> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_offer_direction(session, direction, generation)
    }

    fn relay_offer_flow(
        &mut self,
        session: pw_graph_backend::RelaySessionId,
        flow: pw_graph_backend::RelayFlow,
        generation: u64,
    ) -> BackendResult<()> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_offer_flow(session, flow, generation)
    }

    fn relay_offer_mode(
        &mut self,
        session: pw_graph_backend::RelaySessionId,
        mode: pw_graph_backend::RelayMode,
        generation: u64,
    ) -> BackendResult<()> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_offer_mode(session, mode, generation)
    }

    fn relay_configure_identity(
        &mut self,
        device_id: String,
        trusted_peers: Vec<pw_graph_backend::RelayTrustedPeer>,
        transport: pw_graph_backend::RelayTransportPreference,
    ) -> BackendResult<()> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_configure_identity(device_id, trusted_peers, transport)
    }

    fn relay_trusted_enrollment_secret(
        &self,
        transaction_id: u64,
    ) -> BackendResult<Option<[u8; 32]>> {
        self.relay_backend()
            .ok_or_else(Self::relay_unavailable)?
            .relay_trusted_enrollment_secret(transaction_id)
    }

    fn relay_accept_trusted_enrollment(&mut self, transaction_id: u64) -> BackendResult<()> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_accept_trusted_enrollment(transaction_id)
    }

    fn relay_reject_trusted_enrollment(
        &mut self,
        transaction_id: u64,
        reason: &str,
    ) -> BackendResult<()> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_reject_trusted_enrollment(transaction_id, reason)
    }

    fn relay_remove_trusted_peer(&mut self, peer_id: &str) -> BackendResult<()> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_remove_trusted_peer(peer_id)
    }

    fn relay_events(&mut self) -> Vec<pw_graph_backend::RelayEvent> {
        let events = self
            .relay_backend_mut()
            .map(|driver| driver.relay_events())
            .unwrap_or_default();
        // When a session becomes ready the native graph (relay nodes/links)
        // has changed. Merge it into the composite view so the first connect
        // exposes a verified route without requiring a manual disconnect/reconnect.
        let resolved_mode = events.iter().find_map(|event| match event {
            pw_graph_backend::RelayEvent::FlowResolved { mode, .. } => Some(*mode),
            _ => None,
        });
        if events.iter().any(|event| {
            matches!(
                event,
                pw_graph_backend::RelayEvent::SessionEstablished { .. }
                    | pw_graph_backend::RelayEvent::DirectionResolved { .. }
                    | pw_graph_backend::RelayEvent::FlowResolved { .. }
            )
        }) {
            self.rebuild_after_native_mutation();
            // Ensure the backend's mode-specific route was verified after the
            // merged refresh. FlowResolved is authoritative for a live session;
            // the status fallback also covers an ordinary SessionEstablished.
            if let Some(driver) = self.relay_backend_mut() {
                let mode = resolved_mode
                    .or_else(|| {
                        driver
                            .relay_status()
                            .sessions
                            .first()
                            .and_then(|session| session.mode)
                    })
                    .unwrap_or(pw_graph_backend::RelayMode::Receiver);
                let _ = driver.relay_ensure_local_route(mode);
            }
        }
        events
    }

    fn relay_discovery_start(&mut self) -> BackendResult<()> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_discovery_start()
    }

    fn relay_discovery_stop(&mut self) {
        if let Some(driver) = self.relay_backend_mut() {
            driver.relay_discovery_stop();
        }
    }

    fn relay_discovery_usb_link_lost(&mut self) {
        if let Some(driver) = self.relay_backend_mut() {
            driver.relay_discovery_usb_link_lost();
        }
    }

    fn relay_usb_link_present(&self) -> bool {
        self.relay_backend()
            .is_some_and(|driver| driver.relay_usb_link_present())
    }

    fn relay_peers(&self) -> Vec<pw_graph_backend::RelayPeerInfo> {
        self.relay_backend()
            .map(|driver| driver.relay_peers())
            .unwrap_or_default()
    }

    fn relay_local_links(&self) -> Vec<pw_graph_backend::RelayLocalLink> {
        self.relay_backend()
            .map(|driver| driver.relay_local_links())
            .unwrap_or_default()
    }

    fn relay_playback_status(&self) -> pw_graph_backend::RelayPlaybackStatus {
        self.relay_backend()
            .map(|driver| driver.relay_playback_status())
            .unwrap_or_default()
    }

    fn relay_set_playback_enabled(&mut self, enabled: bool) -> BackendResult<()> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_set_playback_enabled(enabled)
    }

    fn relay_set_playback_gain(&mut self, gain: f32) -> BackendResult<()> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_set_playback_gain(gain)
    }

    fn relay_set_playback_mute(&mut self, muted: bool) -> BackendResult<()> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_set_playback_mute(muted)
    }

    fn relay_set_playback_sink(&mut self, sink: Option<String>) -> BackendResult<()> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_set_playback_sink(sink)
    }

    fn relay_playback_sinks(&self) -> Vec<pw_graph_backend::RelaySinkInfo> {
        self.relay_backend()
            .map(|driver| driver.relay_playback_sinks())
            .unwrap_or_default()
    }

    fn relay_ensure_playback_route(
        &mut self,
    ) -> BackendResult<pw_graph_backend::RelayPlaybackState> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_ensure_playback_route()
    }

    fn relay_send_sources(&self) -> Vec<pw_graph_backend::RelayEndpointInfo> {
        self.relay_backend()
            .map(|driver| driver.relay_send_sources())
            .unwrap_or_default()
    }

    fn relay_receive_sinks(&self) -> Vec<pw_graph_backend::RelayEndpointInfo> {
        self.relay_backend()
            .map(|driver| driver.relay_receive_sinks())
            .unwrap_or_default()
    }

    fn relay_set_send_source(
        &mut self,
        source: pw_graph_backend::RelaySendSource,
    ) -> BackendResult<()> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_set_send_source(source)
    }

    fn relay_set_receive_sink(
        &mut self,
        sink: pw_graph_backend::RelayReceiveSink,
    ) -> BackendResult<()> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_set_receive_sink(sink)
    }

    fn relay_ensure_local_route(
        &mut self,
        mode: pw_graph_backend::RelayMode,
    ) -> BackendResult<pw_graph_backend::RelayLocalRouteState> {
        self.relay_backend_mut()
            .ok_or_else(Self::relay_unavailable)?
            .relay_ensure_local_route(mode)
    }
}
