//! PipeWire relay driver integration.
//!
//! Relay control and route reconciliation are kept out of the graph driver
//! composition module. All methods remain on PipewireDriver so the public
//! backend contract does not change, but the relay-specific policy is now
//! isolated behind the relay feature.

use super::*;

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

    // `EngineConfig::direction` is deprecated but kept synchronized with
    // `mode` as a wire/trusted-peer compatibility shim.
    #[allow(deprecated)]
    fn relay_start_host(&mut self, request: RelayHostRequest) -> BackendResult<u16> {
        if request.mode != RelayMode::Receiver {
            return Err(BackendError::unsupported(
                "a local relay host must run in Receiver mode",
            ));
        }
        self.with_loop(|driver| {
            let set = driver.ensure_relay_devices_locked(&request.device_name)?;
            set.local_router.set_mode(request.mode);
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
                mode: request.mode,
                mode_generation: request.mode_generation,
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

    // `EngineConfig::direction` is deprecated but kept synchronized with
    // `mode` as a wire/trusted-peer compatibility shim.
    #[allow(deprecated)]
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
            config.mode = match direction {
                RelayDirection::MobileToDesktop => RelayMode::Receiver,
                RelayDirection::DesktopToMobile => RelayMode::Emitter,
            };
            config.mode_generation = direction_generation;
            let roles = super::api::desktop_relay_client_roles(direction);
            config.client_roles = roles;
            set.local_router.set_mode(match direction {
                RelayDirection::MobileToDesktop => RelayMode::Receiver,
                RelayDirection::DesktopToMobile => RelayMode::Emitter,
            });
            set.handle().update_config(config);
            Ok(set.handle().connect(target, pin, roles))
        })
    }

    // `EngineConfig::direction` is deprecated but kept synchronized with
    // `mode` as a wire/trusted-peer compatibility shim.
    #[allow(deprecated)]
    fn relay_connect_mode(
        &mut self,
        target: std::net::SocketAddr,
        pin: &str,
        mode: RelayMode,
        generation: u64,
    ) -> BackendResult<RelaySessionId> {
        self.with_loop(|driver| {
            let device_name = driver
                .relay
                .as_ref()
                .map(|set| set.handle().config().device_name)
                .unwrap_or_else(|| "qpwgraph-rs".into());
            let set = driver.ensure_relay_devices_locked(&device_name)?;
            set.local_router.set_mode(mode);
            let mut config = set.handle().config();
            config.client_roles = mode.roles();
            config.direction = match mode {
                RelayMode::Emitter => RelayDirection::DesktopToMobile,
                RelayMode::Receiver => RelayDirection::MobileToDesktop,
            };
            config.direction_generation = generation;
            config.mode = mode;
            config.mode_generation = generation;
            set.handle().update_config(config);
            Ok(set.handle().connect_mode(target, pin, mode))
        })
    }

    // `EngineConfig::direction` is deprecated but kept synchronized with
    // `mode` as a wire/trusted-peer compatibility shim.
    #[allow(deprecated)]
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
            config.mode = match direction {
                RelayDirection::MobileToDesktop => RelayMode::Receiver,
                RelayDirection::DesktopToMobile => RelayMode::Emitter,
            };
            config.mode_generation = direction_generation;
            let roles = super::api::desktop_relay_client_roles(direction);
            config.client_roles = roles;
            set.local_router.set_mode(match direction {
                RelayDirection::MobileToDesktop => RelayMode::Receiver,
                RelayDirection::DesktopToMobile => RelayMode::Emitter,
            });
            set.handle().update_config(config);
            Ok(set.handle().connect_trusted(target, peer_id, secret, roles))
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

    fn relay_offer_flow(
        &mut self,
        session: RelaySessionId,
        flow: RelayFlow,
        generation: u64,
    ) -> BackendResult<()> {
        let Some(set) = self.relay.as_ref() else {
            return Err(BackendError::native("no relay session exists"));
        };
        set.handle()
            .offer_flow(session, flow, generation)
            .map_err(|error| BackendError::native(format!("relay flow offer failed: {error}")))
    }

    fn relay_offer_mode(
        &mut self,
        session: RelaySessionId,
        mode: RelayMode,
        generation: u64,
    ) -> BackendResult<()> {
        let Some(set) = self.relay.as_ref() else {
            return Err(BackendError::native("no relay session exists"));
        };
        set.handle()
            .offer_mode(session, mode, generation)
            .map_err(|error| BackendError::native(format!("relay mode offer failed: {error}")))
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
        let resolved_mode = events.iter().find_map(|event| match event {
            RelayEvent::FlowResolved { mode, .. } => Some(*mode),
            _ => None,
        });
        if events.iter().any(|e| {
            matches!(
                e,
                RelayEvent::SessionEstablished { .. } | RelayEvent::FlowResolved { .. }
            )
        }) {
            let _ = self.with_loop(|driver| {
                driver.rebuild_graph_locked()?;
                if let Some(mode) = resolved_mode {
                    if let Some(set) = driver.relay.as_mut() {
                        set.local_router.set_mode(mode);
                    }
                    let _ = driver.ensure_relay_local_route_locked(mode)?;
                } else if let Some(mode) =
                    driver.relay.as_ref().and_then(|set| set.local_router.mode)
                {
                    let _ = driver.ensure_relay_local_route_locked(mode)?;
                }
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
            set.local_router.enabled = enabled;
            // Log major state transitions only, not per frame
            if enabled {
                eprintln!("Relay playback starting");
            } else {
                eprintln!("Relay playback disabled");
            }
        }
        // Propagate routing failures so the UI does not show a healthy state
        // while the PipeWire link could not be created.
        self.with_loop(|driver| {
            let mode = driver
                .relay
                .as_ref()
                .and_then(|set| set.local_router.mode)
                .unwrap_or(RelayMode::Receiver);
            driver.ensure_relay_local_route_locked(mode).map(|_| ())
        })?;
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
        let receive_sink = sink
            .clone()
            .map(RelayReceiveSink::OutputDevice)
            .unwrap_or(RelayReceiveSink::DefaultOutput);
        self.relay_receive_sink = receive_sink.clone();
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
            set.local_router.set_receive_sink(receive_sink);
            if let Some(name) = &sink {
                eprintln!("Relay playback sink selected: {name}");
            } else {
                eprintln!("Relay playback sink selected: Default");
            }
        }
        self.with_loop(|driver| {
            let mode = driver
                .relay
                .as_ref()
                .and_then(|set| set.local_router.mode)
                .unwrap_or(RelayMode::Receiver);
            driver.ensure_relay_local_route_locked(mode).map(|_| ())
        })?;
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
            let mode = driver
                .relay
                .as_ref()
                .and_then(|set| set.local_router.mode)
                .unwrap_or(RelayMode::Receiver);
            driver.ensure_relay_local_route_locked(mode)?;
            let status = driver.relay_playback_status();
            Ok(status.state)
        })
    }

    fn relay_send_sources(&self) -> Vec<RelayEndpointInfo> {
        let mut sources = vec![RelayEndpointInfo {
            id: "default-input".into(),
            name: "Default input".into(),
            description: "PipeWire/WirePlumber default.audio.source".into(),
        }];
        sources.push(RelayEndpointInfo {
            id: "manual".into(),
            name: "Manual graph".into(),
            description: "Leave Emitter routing to the PipeWire graph".into(),
        });
        sources.extend(self.pipewire_relay_endpoint_infos(true, false));
        sources.extend(self.pipewire_relay_endpoint_infos(false, true));
        sources
    }

    fn relay_receive_sinks(&self) -> Vec<RelayEndpointInfo> {
        let mut sinks = vec![RelayEndpointInfo {
            id: "default-output".into(),
            name: "Default output".into(),
            description: "PipeWire/WirePlumber default.audio.sink".into(),
        }];
        sinks.push(RelayEndpointInfo {
            id: "manual".into(),
            name: "Manual graph".into(),
            description: "Leave Receiver routing to the PipeWire graph".into(),
        });
        sinks.extend(self.pipewire_relay_endpoint_infos(false, false));
        sinks
    }

    fn relay_set_send_source(&mut self, source: RelaySendSource) -> BackendResult<()> {
        if matches!(&source, RelaySendSource::Application(_)) {
            return Err(BackendError::unsupported(
                "application relay selectors are available only on Windows",
            ));
        }
        self.relay_send_source = source.clone();
        self.with_loop(|driver| {
            if let Some(set) = driver.relay.as_mut() {
                set.local_router.set_send_source(source);
                if let Some(mode) = set.local_router.mode {
                    driver.ensure_relay_local_route_locked(mode)?;
                }
            }
            Ok(())
        })
    }

    fn relay_set_receive_sink(&mut self, sink: RelayReceiveSink) -> BackendResult<()> {
        self.relay_receive_sink = sink.clone();
        self.with_loop(|driver| {
            if let Some(set) = driver.relay.as_mut() {
                set.local_router.set_receive_sink(sink);
                if let Some(mode) = set.local_router.mode {
                    driver.ensure_relay_local_route_locked(mode)?;
                }
            }
            Ok(())
        })
    }

    fn relay_ensure_local_route(&mut self, mode: RelayMode) -> BackendResult<RelayLocalRouteState> {
        self.with_loop(|driver| {
            let Some(set) = driver.relay.as_mut() else {
                return Ok(RelayLocalRouteState::default());
            };
            set.local_router.set_mode(mode);
            driver.ensure_relay_local_route_locked(mode)
        })
    }
}

#[cfg(all(target_os = "linux", feature = "relay"))]
impl PipewireDriver {
    fn pipewire_relay_endpoint_infos(
        &self,
        input_source: bool,
        output_monitor: bool,
    ) -> Vec<RelayEndpointInfo> {
        let registry = self.state.lock().unwrap().clone();
        self.graph
            .nodes
            .values()
            .filter_map(|node| {
                if node.name == relay::RELAY_SOURCE_NAME || node.name == relay::RELAY_SINK_NAME {
                    return None;
                }
                let record = registry.nodes.get(&native_node_id(node.id))?;
                let media_class = record.media_class.to_ascii_lowercase();
                let has_source = node.ports.iter().any(|port_id| {
                    self.graph.port(*port_id).is_some_and(|port| {
                        port.direction.is_source()
                            && (port.port_type == PortType::Audio
                                || port.port_type == PortType::Unknown)
                    })
                });
                let has_sink = node.ports.iter().any(|port_id| {
                    self.graph.port(*port_id).is_some_and(|port| {
                        port.direction.is_sink()
                            && (port.port_type == PortType::Audio
                                || port.port_type == PortType::Unknown)
                    })
                });
                let matches = if input_source {
                    // Physical sources and application playback streams both
                    // expose source ports. The latter are commonly classified
                    // as Stream/Output/Audio rather than Audio/Source.
                    (media_class.contains("source")
                        || media_class.contains("stream/output")
                        || media_class.contains("audio/output"))
                        && has_source
                } else if output_monitor {
                    media_class.contains("sink") && has_source
                } else {
                    media_class.contains("sink") && has_sink
                };
                matches.then(|| RelayEndpointInfo {
                    id: if input_source {
                        format!("input:{}", node.name)
                    } else if output_monitor {
                        format!("monitor:{}", node.name)
                    } else {
                        format!("output:{}", node.name)
                    },
                    name: record.name.clone(),
                    description: format!("{} ({})", record.name, node.name),
                })
            })
            .collect()
    }

    /// Create the relay engine and virtual devices on first use. The caller
    /// must hold the ThreadLoop lock.
    fn ensure_relay_devices_locked(
        &mut self,
        device_name: &str,
    ) -> BackendResult<&mut relay::RelayRuntimeSet> {
        if self.relay.is_none() {
            let mut set = relay::RelayRuntimeSet::create(&self.thread_loop, device_name)?;
            set.local_router
                .set_send_source(self.relay_send_source.clone());
            set.local_router
                .set_receive_sink(self.relay_receive_sink.clone());
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

    /// Reconcile exactly one qpwgraph-owned local route. The route is
    /// destroyed before a mode/endpoint change is created, so a rapid
    /// Emitter ↔ Receiver transition can never leave both automatic paths
    /// connected at once.
    pub(super) fn ensure_relay_local_route_locked(
        &mut self,
        mode: RelayMode,
    ) -> BackendResult<RelayLocalRouteState> {
        if self.relay.is_none() {
            return Ok(RelayLocalRouteState::default());
        }
        self.rebuild_graph_locked()?;
        let registry = self.state.lock().unwrap().clone();
        let (default_source, default_sink, enabled, manual, previous_ids, desired) = {
            let set = self.relay.as_mut().expect("relay set exists");
            set.local_router.set_mode(mode);
            set.local_router
                .set_send_source(self.relay_send_source.clone());
            set.local_router
                .set_receive_sink(self.relay_receive_sink.clone());
            let manual = match mode {
                RelayMode::Emitter => {
                    matches!(
                        set.local_router.send_source,
                        RelaySendSource::ManualGraph | RelaySendSource::Application(_)
                    )
                }
                RelayMode::Receiver => {
                    matches!(
                        set.local_router.receive_sink,
                        RelayReceiveSink::ManualGraph | RelayReceiveSink::VirtualMicrophone
                    )
                }
            };
            let mut previous_ids = set.local_router.link_ids.clone();
            previous_ids.extend(set.router.link_ids.iter().copied());
            previous_ids.sort_unstable();
            previous_ids.dedup();
            let desired = set.local_router.desired_links(
                &self.graph,
                &registry.nodes,
                registry.default_source.as_ref(),
                registry.default_sink.as_ref(),
            );
            (
                registry.default_source,
                registry.default_sink,
                set.local_router.enabled,
                manual,
                previous_ids,
                desired,
            )
        };
        let _ = (default_source, default_sink);

        let existing_owned_pairs: std::collections::BTreeSet<(PortId, PortId)> = previous_ids
            .iter()
            .filter_map(|id| self.graph.link(LinkId(*id)))
            .map(|link| (link.output_port, link.input_port))
            .collect();
        let desired_pairs = desired.as_ref().map(|(pairs, _, _, _)| {
            pairs
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>()
        });

        // Idempotent fast path: the exact owned route is already connected.
        if enabled
            && desired_pairs.as_ref().is_some_and(|pairs| {
                *pairs == existing_owned_pairs
                    && previous_ids.len() == pairs.len()
                    && self
                        .relay
                        .as_ref()
                        .is_some_and(|set| set.local_router.mode == Some(mode))
            })
        {
            let state = desired.as_ref().map_or_else(
                RelayLocalRouteState::default,
                |(_, source, sink, description)| RelayLocalRouteState {
                    mode: Some(mode),
                    active: true,
                    source_id: Some(source.clone()),
                    sink_id: Some(sink.clone()),
                    description: description.clone(),
                },
            );
            if let Some(set) = self.relay.as_mut() {
                set.local_router.state = state.clone();
            }
            return Ok(state);
        }

        // Tear down only links this router created (the old playback router
        // is included for one-release compatibility). Ordinary user links
        // remain untouched.
        for id in previous_ids {
            let link_id = LinkId(id);
            if self.graph.link(link_id).is_some() {
                let _ = self.disconnect_locked(link_id);
            }
        }
        if let Some(set) = self.relay.as_mut() {
            set.local_router.link_ids.clear();
            set.router.link_ids.clear();
            set.router.current_sink_name = None;
            set.router.current_sink_serial = None;
        }

        let Some((pairs, source_id, sink_id, description)) = desired else {
            let legacy_state = if enabled {
                match mode {
                    RelayMode::Emitter => relay::RelayPlaybackState::Disabled,
                    RelayMode::Receiver => relay::RelayPlaybackState::WaitingForSink,
                }
            } else {
                relay::RelayPlaybackState::Disabled
            };
            let state = RelayLocalRouteState {
                mode: Some(mode),
                active: false,
                source_id: None,
                sink_id: None,
                description: if !enabled {
                    "local relay route disabled".into()
                } else {
                    if manual {
                        "manual graph routing".into()
                    } else {
                        match mode {
                            RelayMode::Emitter => "waiting for an input source".into(),
                            RelayMode::Receiver => "waiting for an output sink".into(),
                        }
                    }
                },
            };
            if let Some(set) = self.relay.as_mut() {
                set.local_router.state = state.clone();
                set.router.state = legacy_state;
            }
            return Ok(state);
        };

        let mut owned_ids = Vec::new();
        for (output, input) in pairs.iter().copied() {
            if let Some(existing) = self
                .graph
                .links
                .values()
                .find(|link| link.output_port == output && link.input_port == input)
            {
                // A pre-existing user link is valid, but it is not ours to
                // remove on a later mode switch.
                if existing_owned_pairs.contains(&(output, input)) {
                    owned_ids.push(existing.id.0);
                }
                continue;
            }
            match self.connect_locked(output, input) {
                Ok(link) => owned_ids.push(link.id.0),
                Err(error) => {
                    for id in owned_ids.iter().copied() {
                        if self.graph.link(LinkId(id)).is_some() {
                            let _ = self.disconnect_locked(LinkId(id));
                        }
                    }
                    let state = RelayLocalRouteState {
                        mode: Some(mode),
                        active: false,
                        source_id: None,
                        sink_id: None,
                        description: format!("local relay route failed: {error}"),
                    };
                    if let Some(set) = self.relay.as_mut() {
                        set.local_router.state = state;
                    }
                    return Err(BackendError::native(format!(
                        "could not create local relay route: {error}"
                    )));
                }
            }
        }
        let state = RelayLocalRouteState {
            mode: Some(mode),
            active: true,
            source_id: Some(source_id),
            sink_id: Some(sink_id),
            description,
        };
        if let Some(set) = self.relay.as_mut() {
            set.local_router.link_ids = owned_ids.clone();
            set.local_router.state = state.clone();
            if mode == RelayMode::Receiver {
                set.router.link_ids = owned_ids;
                set.router.state = relay::RelayPlaybackState::Connected;
                set.router.current_sink_name = state.sink_id.clone();
            } else {
                set.router.state = relay::RelayPlaybackState::Disabled;
                set.router.current_sink_name = None;
            }
        }
        Ok(state)
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
    ///
    /// Legacy path superseded by `ensure_relay_local_route_locked`, retained
    /// for one-release compatibility and exercised by unit tests.
    #[allow(dead_code)]
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
