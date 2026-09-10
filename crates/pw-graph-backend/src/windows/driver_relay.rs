//! Windows relay contract implementation.
//!
//! Relay methods are platform-specific policy and are kept separate from the
//! graph/effect driver lifecycle. The parent module still owns the state and
//! all public behavior is unchanged.

use super::*;

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
