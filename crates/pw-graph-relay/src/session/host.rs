//! Hosting: the control listener, the accept loop, and the per-peer threads
//! a host runs once a peer is authenticated.

use super::*;

/// Bookkeeping for a running host listener.
pub(crate) struct HostRecord {
    pub port: u16,
    /// The exact address the TCP listener was bound to. `None` is the
    /// documented no-link fallback, which intentionally uses INADDR_ANY.
    pub(super) bind_addr: Arc<Mutex<Option<Ipv4Addr>>>,
    pub stop: Arc<AtomicBool>,
    pub(super) worker: Option<std::thread::JoinHandle<()>>,
}

impl HostRecord {
    pub(crate) fn bind_addr(&self) -> Option<Ipv4Addr> {
        self.bind_addr.lock().ok().and_then(|addr| *addr)
    }

    /// Stop the accept loop and wait for its listener to be dropped. Returning
    /// only after the join makes an immediate same-port restart deterministic
    /// and prevents callers from being tempted to hide the race by falling
    /// back to an ephemeral port.
    pub(crate) fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) fn start_host(inner: &Arc<EngineInner>, port: u16) -> RelayResult<HostRecord> {
    // Binding every interface exposes pairing on the LAN, on any VPN, and on
    // whatever else happens to be up. Honour the configured address, or the
    // transport preference when no address is pinned, so the relay is offered
    // on the link it is meant to serve.
    let config = inner.config();
    let bind_ip = host_bind_addr(&config);
    if config.transport != crate::TransportPreference::Auto
        && config.transport != crate::TransportPreference::Adb
        && bind_ip.is_none()
    {
        return Err(RelayError::Engine(
            "the selected relay interface is not available".into(),
        ));
    }
    let (listener, bound_addr, bound) = bind_control_listener(bind_ip, port)?;
    let mut listeners = vec![(listener, bound_addr)];
    // Keep a loopback listener alongside a link-specific listener so an ADB
    // forward/reverse can reach the same host without changing the host's
    // network exposure policy. A wildcard listener already includes loopback.
    if bound_addr.is_some_and(|addr| addr != Ipv4Addr::LOCALHOST) {
        if let Ok((loopback, _, _)) = bind_control_listener(Some(Ipv4Addr::LOCALHOST), bound) {
            listeners.push((loopback, Some(Ipv4Addr::LOCALHOST)));
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let inner = Arc::clone(inner);
    let current_addr = Arc::new(Mutex::new(bound_addr));
    let thread_current_addr = Arc::clone(&current_addr);
    let worker = std::thread::Builder::new()
        .name("relay-host".into())
        .spawn(move || accept_loop(inner, listeners, thread_stop, thread_current_addr, bound))?;
    Ok(HostRecord {
        port: bound,
        bind_addr: current_addr,
        stop,
        worker: Some(worker),
    })
}

pub(super) fn bind_control_listener(
    bind_ip: Option<Ipv4Addr>,
    port: u16,
) -> RelayResult<(TcpListener, Option<Ipv4Addr>, u16)> {
    let bind_address = bind_ip.unwrap_or(Ipv4Addr::UNSPECIFIED);
    let listener = TcpListener::bind((bind_address, port)).map_err(|error| {
        RelayError::Engine(format!(
            "could not bind relay control port {port} on {bind_address}: {error}"
        ))
    })?;
    let local_addr = listener.local_addr()?;
    let bound_addr = match local_addr.ip() {
        IpAddr::V4(addr) if !addr.is_unspecified() => Some(addr),
        _ => None,
    };
    let bound = local_addr.port();
    listener.set_nonblocking(true)?;
    Ok((listener, bound_addr, bound))
}

/// The address a host listens on: an explicitly configured one wins, then the
/// selected active link. `None` is reserved for the documented Auto/no-link
/// fallback where the OS must provide a wildcard listener.
pub(super) fn host_bind_addr(config: &crate::EngineConfig) -> Option<Ipv4Addr> {
    if config.transport == crate::TransportPreference::Adb {
        return Some(Ipv4Addr::LOCALHOST);
    }
    if config.bind_addr.is_some() {
        return config.bind_addr;
    }
    let links = netlink::local_links();
    if config.transport == crate::TransportPreference::Auto {
        netlink::listen_bind_addr(&links, config.transport)
    } else {
        netlink::select_links(&links, config.transport)
            .first()
            .map(|link| link.addr)
    }
}

pub(crate) fn stop_host(inner: &EngineInner) {
    let taken = inner.host.lock().ok().and_then(|mut host| host.take());
    if let Some(record) = taken {
        record.stop();
    }
}

pub(super) fn accept_loop(
    inner: Arc<EngineInner>,
    mut listeners: Vec<(TcpListener, Option<Ipv4Addr>)>,
    stop: Arc<AtomicBool>,
    current_addr: Arc<Mutex<Option<Ipv4Addr>>>,
    port: u16,
) {
    let mut bind_addr = current_addr.lock().ok().and_then(|addr| *addr);
    loop {
        if !inner.running.load(Ordering::Relaxed) || stop.load(Ordering::Relaxed) {
            break;
        }
        // Network interfaces can appear after the host starts (most notably
        // USB tethering). Add the new listener before dropping the old one,
        // preserving all session/PIN state while the control endpoint moves
        // to the preferred address.
        let desired = host_bind_addr(&inner.config());
        if desired != bind_addr {
            // A link-specific host also keeps a loopback listener for ADB.
            // If the configured preference changes to `adb`, that secondary
            // listener is already the desired endpoint; reuse it instead of
            // trying to bind the same address a second time.
            if listeners.iter().any(|(_, address)| *address == desired) {
                listeners.retain(|(_, address)| *address != bind_addr);
                bind_addr = desired;
                if let Ok(mut current) = current_addr.lock() {
                    *current = desired;
                }
                inner.start_advertiser(port, desired);
                continue;
            }
            // A wildcard listener conflicts with every specific-address
            // bind. Remove it only for the duration of this migration and
            // restore the wildcard fallback if the new bind fails.
            let replacing_wildcard = desired.is_none();
            let replacing_current_wildcard = !replacing_wildcard && bind_addr.is_none();
            if replacing_wildcard {
                listeners.clear();
            } else if replacing_current_wildcard {
                listeners.retain(|(_, address)| *address != bind_addr);
            }
            match bind_control_listener(desired, port) {
                Ok((next, next_addr, _)) => {
                    if !replacing_wildcard && bind_addr.is_some() {
                        listeners.retain(|(_, address)| *address != bind_addr);
                    }
                    listeners.push((next, next_addr));
                    if next_addr.is_some_and(|addr| addr != Ipv4Addr::LOCALHOST)
                        && !listeners
                            .iter()
                            .any(|(_, address)| *address == Some(Ipv4Addr::LOCALHOST))
                    {
                        if let Ok((loopback, _, _)) =
                            bind_control_listener(Some(Ipv4Addr::LOCALHOST), port)
                        {
                            listeners.push((loopback, Some(Ipv4Addr::LOCALHOST)));
                        }
                    }
                    if next_addr.is_none() {
                        listeners.retain(|(_, address)| *address != Some(Ipv4Addr::LOCALHOST));
                    }
                    bind_addr = next_addr;
                    if let Ok(mut current) = current_addr.lock() {
                        *current = next_addr;
                    }
                    inner.start_advertiser(port, next_addr);
                }
                Err(error) => {
                    if replacing_wildcard || replacing_current_wildcard {
                        if let Ok((fallback, fallback_addr, _)) =
                            bind_control_listener(bind_addr, port)
                        {
                            listeners.push((fallback, fallback_addr));
                            if fallback_addr.is_some_and(|addr| addr != Ipv4Addr::LOCALHOST) {
                                if let Ok((loopback, _, _)) =
                                    bind_control_listener(Some(Ipv4Addr::LOCALHOST), port)
                                {
                                    listeners.push((loopback, Some(Ipv4Addr::LOCALHOST)));
                                }
                            }
                        }
                    }
                    inner.emit(RelayEvent::Error {
                        message: format!("relay listener migration failed: {error}"),
                    });
                }
            }
        }
        let mut accepted = false;
        for (listener, listener_addr) in &listeners {
            match listener.accept() {
                Ok((mut stream, addr)) => {
                    accepted = true;
                    let listener_addr = *listener_addr;
                    let _ = stream.set_nonblocking(false);
                    // Every accepted connection costs a thread that can sit in a
                    // handshake read timeout before proving anything, so refuse
                    // rather than let an unauthenticated peer spawn without bound.
                    let Some(slot) = inner.claim_handshake() else {
                        let _ = write_frame(
                            &mut stream,
                            &ControlMessage::PairFail {
                                reason: "the host is busy; try again shortly".into(),
                            },
                        );
                        continue;
                    };
                    // Keep a duplicate only for the exceptional path: the
                    // worker owns the accepted stream, but a failed spawn should
                    // still get one best-effort PairFail onto the peer.
                    let mut failure_stream = stream.try_clone().ok();
                    let worker_inner = Arc::clone(&inner);
                    match spawn_named(
                        format!("relay-peer-{addr}"),
                        Box::new(move || {
                            host_peer_thread(worker_inner, stream, addr, slot, listener_addr, port)
                        }),
                    ) {
                        Ok(_) => {}
                        Err(error) => {
                            if let Some(mut stream) = failure_stream.take() {
                                let _ = write_frame(
                                    &mut stream,
                                    &ControlMessage::PairFail {
                                        reason: "the host could not start a handshake worker"
                                            .into(),
                                    },
                                );
                            }
                            inner.emit(RelayEvent::Error {
                                message: format!("could not start peer handshake worker: {error}"),
                            });
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => {}
            }
            if accepted {
                break;
            }
        }
        if !accepted {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Host side of one peer connection: either a fresh handshake or a resume of
/// an existing session, then the keepalive watcher.
///
/// `_slot` is the pre-authentication admission ticket; holding it for the
/// whole thread is what bounds concurrent handshakes.
pub(super) fn host_peer_thread(
    inner: Arc<EngineInner>,
    mut stream: TcpStream,
    peer_addr: SocketAddr,
    _slot: crate::HandshakeSlot,
    _bind_addr: Option<Ipv4Addr>,
    control_port: u16,
) {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));

    // Guessing a short PIN is now an online-only game, so make each round of
    // that game cost the guesser a lockout.
    if !inner.pairing_allowed(peer_addr.ip()) {
        let _ = write_frame(
            &mut stream,
            &ControlMessage::PairFail {
                reason: "too many failed pairing attempts; wait and retry".into(),
            },
        );
        #[cfg(windows)]
        {
            // Winsock may reset a socket when the server closes immediately
            // after a final cleartext response. Keep the write half open for
            // one bounded scheduling interval so the client can read the
            // lockout reason instead of seeing WSAECONNABORTED.
            let _ = stream.shutdown(Shutdown::Write);
            std::thread::sleep(Duration::from_millis(100));
        }
        return;
    }

    let first = match read_frame(&mut stream) {
        Ok(message) => message,
        Err(_) => return,
    };
    let (peer_id, peer_name, peer_kind, client_pake, audio_over_tcp) = match first {
        ControlMessage::ResumeHello {
            session_id,
            client_nonce,
        } => {
            resume_peer_session(&inner, SessionId(session_id), stream, &client_nonce);
            return;
        }
        ControlMessage::AudioHello {
            session_id,
            client_nonce,
        } => {
            host_audio_thread(&inner, stream, SessionId(session_id), &client_nonce);
            return;
        }
        ControlMessage::TrustedHello {
            protocol,
            device_id,
            device_name,
            device_kind,
            host_id,
            transport,
            client_nonce,
            // The codec parameters advertised here are deliberately dropped:
            // the shared post-authentication path negotiates them from the
            // sealed `SessionStart` frame, exactly as PIN pairing does.
            ..
        } if protocol == PROTOCOL_VERSION as u32 => {
            trusted_peer_thread(
                &inner,
                stream,
                peer_addr,
                control_port,
                TrustedHelloContext {
                    client_id: device_id,
                    peer_name: device_name,
                    peer_kind: device_kind,
                    host_id,
                    transport,
                    client_nonce,
                },
            );
            return;
        }
        ControlMessage::Hello {
            protocol,
            device_id,
            transport,
            device_name,
            device_kind,
            pake,
            ..
        } if protocol == PROTOCOL_VERSION as u32 => (
            if device_id.trim().is_empty() {
                device_name.clone()
            } else {
                device_id
            },
            device_name,
            device_kind,
            pake,
            transport.eq_ignore_ascii_case("adb"),
        ),
        _ => return,
    };

    if inner.session_count() >= inner.config().max_sessions {
        let _ = write_frame(
            &mut stream,
            &ControlMessage::PairFail {
                reason: "the host is already at its session limit".into(),
            },
        );
        return;
    }

    let host_config = inner.config();
    let host_name = host_config.device_name;
    let Some(keys) = host_pake_exchange(
        &inner,
        &mut stream,
        peer_addr,
        &client_pake,
        host_name,
        host_config.device_id,
    ) else {
        return;
    };
    inner.clear_pairing_failures(peer_addr.ip());
    host_session_after_auth(
        inner,
        stream,
        peer_addr,
        HostAuthenticatedPeer {
            peer_id,
            peer_name,
            peer_kind,
            keys,
            audio_over_tcp,
            control_port,
            requested_id: None,
        },
    );
}

/// Authenticate and install the secondary TCP stream opened by an ADB
/// client. It is tied to the existing session's resume secret, so merely
/// reaching the forwarded localhost port cannot inject audio.
pub(super) fn host_audio_thread(
    inner: &Arc<EngineInner>,
    mut stream: TcpStream,
    id: SessionId,
    client_nonce: &str,
) {
    let Some(record) = inner.session(id) else {
        let _ = write_frame(
            &mut stream,
            &ControlMessage::PairFail {
                reason: "unknown or expired audio session".into(),
            },
        );
        return;
    };
    let Some(slot) = record.tcp_audio.clone() else {
        let _ = write_frame(
            &mut stream,
            &ControlMessage::PairFail {
                reason: "session does not use the TCP audio transport".into(),
            },
        );
        return;
    };
    let Some(client_nonce) = decode_resume_nonce(client_nonce) else {
        return;
    };
    let server_nonce = fresh_resume_nonce();
    if write_frame(
        &mut stream,
        &ControlMessage::AudioChallenge {
            server_nonce: hex_encode(&server_nonce),
        },
    )
    .is_err()
    {
        return;
    }
    let proof = match read_frame(&mut stream) {
        Ok(ControlMessage::AudioProof { proof }) => hex_decode(&proof).ok(),
        _ => None,
    };
    let valid = proof.as_deref().is_some_and(|proof| {
        crate::crypto::tcp_audio_proof(
            &record.resume_secret,
            record.wire_id,
            &client_nonce,
            &server_nonce,
            Side::Client,
        )
        .ct_eq(proof)
        .into()
    });
    if !valid {
        let _ = write_frame(
            &mut stream,
            &ControlMessage::PairFail {
                reason: "TCP audio authentication failed".into(),
            },
        );
        return;
    }
    let server_proof = crate::crypto::tcp_audio_proof(
        &record.resume_secret,
        record.wire_id,
        &client_nonce,
        &server_nonce,
        Side::Host,
    );
    if write_frame(
        &mut stream,
        &ControlMessage::AudioReady {
            proof: hex_encode(&server_proof),
        },
    )
    .is_err()
    {
        return;
    }
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    if !inner.session_alive(record.id) {
        return;
    }
    if let Err(error) = slot.install(stream) {
        inner.emit(RelayEvent::Error {
            message: format!("could not install authenticated ADB audio stream: {error}"),
        });
    }
}

/// Finish a fresh or trusted authentication with one common negotiated
/// session setup. Keeping this after-auth path shared is important: a trusted
/// reconnect must receive exactly the same role/codec validation and worker
/// startup guarantees as a PIN pairing.
/// A peer that has proved possession of either the PIN or its trusted
/// credential, together with the transport facts the shared setup path needs.
pub(super) struct HostAuthenticatedPeer {
    pub(super) peer_id: String,
    pub(super) peer_name: String,
    pub(super) peer_kind: DeviceKind,
    /// Directional keys derived by the completed handshake. Moved, never
    /// cloned, so the session record keeps sole ownership.
    pub(super) keys: SessionKeys,
    /// ADB peers carry audio on the authenticated secondary TCP stream
    /// instead of UDP.
    pub(super) audio_over_tcp: bool,
    /// Control port this listener accepted on, reported back to ADB peers as
    /// their audio port.
    pub(super) control_port: u16,
    /// Session id already allocated by a trusted reconnect, if any.
    pub(super) requested_id: Option<SessionId>,
}

#[allow(deprecated)]
pub(super) fn host_session_after_auth(
    inner: Arc<EngineInner>,
    mut stream: TcpStream,
    peer_addr: SocketAddr,
    peer: HostAuthenticatedPeer,
) {
    let HostAuthenticatedPeer {
        peer_id,
        peer_name,
        peer_kind,
        keys,
        audio_over_tcp,
        control_port,
        requested_id,
    } = peer;
    let Ok((control_sealer, control_opener)) = keys.control_channel() else {
        return;
    };
    let mut cipher = ControlCipher {
        sealer: control_sealer,
        opener: control_opener,
    };

    // ADB uses only its authenticated TCP secondary stream. Normal relay
    // audio is bound to the selected interface; wildcard is reserved for the
    // documented no-link fallback in `bind_udp_audio_socket`.
    let socket = if audio_over_tcp {
        None
    } else {
        match bind_udp_audio_socket(&inner, peer_addr, true) {
            Ok(socket) => {
                tune_audio_socket(&socket);
                match UdpAudioSlot::new(socket) {
                    Ok(slot) => Some(slot),
                    Err(error) => {
                        let _ = cipher.send(
                            &mut stream,
                            &ControlMessage::PairFail {
                                reason: format!("could not prepare audio socket: {error}"),
                            },
                        );
                        return;
                    }
                }
            }
            Err(error) => {
                let _ = cipher.send(
                    &mut stream,
                    &ControlMessage::PairFail {
                        reason: format!("could not open audio socket: {error}"),
                    },
                );
                return;
            }
        }
    };
    let udp_audio_port = socket
        .as_ref()
        .and_then(|socket| socket.local_addr())
        .map(|addr| addr.port())
        .unwrap_or(0);
    let audio_port = if audio_over_tcp {
        control_port
    } else {
        udp_audio_port
    };
    let id = requested_id.unwrap_or_else(|| inner.next_session_id());
    if cipher
        .send(
            &mut stream,
            &ControlMessage::PairOk {
                audio_port,
                session_id: id.0,
            },
        )
        .is_err()
    {
        return;
    }

    let start = match cipher.receive(&mut stream) {
        Ok(ControlMessage::SessionStart {
            roles,
            codec,
            frame_ms,
            sample_rate,
            channels,
        }) => (roles, codec, frame_ms, sample_rate, channels),
        Ok(_) | Err(_) => return,
    };
    let (roles, codec, frame_ms, sample_rate, channels) = start;
    if let Err(error) = validate_negotiation(codec, frame_ms, sample_rate, channels) {
        let _ = cipher.send(
            &mut stream,
            &ControlMessage::PairFail {
                reason: error.to_string(),
            },
        );
        return;
    }
    if roles.is_empty() {
        let _ = cipher.send(
            &mut stream,
            &ControlMessage::PairFail {
                reason: "no audio direction requested".into(),
            },
        );
        return;
    }
    if !roles.is_one_way() {
        let _ = cipher.send(
            &mut stream,
            &ControlMessage::PairFail {
                reason: "bidirectional relay sessions are disabled; choose one audio direction"
                    .into(),
            },
        );
        return;
    }

    let format = AudioFormat::new(sample_rate, channels, frame_ms);
    if let Err(error) =
        make_encoder(codec, format).and_then(|_| make_decoder(codec, format).map(|_| ()))
    {
        let _ = cipher.send(
            &mut stream,
            &ControlMessage::PairFail {
                reason: error.to_string(),
            },
        );
        return;
    }
    let Ok((audio_sealer, audio_opener)) = keys.audio_channel() else {
        return;
    };
    let local = inner.config().local_format();
    let tcp_audio = audio_over_tcp.then(TcpAudioSlot::new);
    let local_config = inner.config();
    let peer_id_for_flow = peer_id.clone();
    let record = Arc::new(SessionRecord {
        id,
        wire_id: id.0,
        peer: PeerInfo {
            id: peer_id,
            name: peer_name,
            kind: peer_kind,
            addr: peer_addr,
        },
        roles,
        codec,
        format,
        active_roles: AtomicU8::new(SessionRecord::role_bits(Roles {
            emit: roles.receive,
            receive: roles.emit,
        })),
        stop: Arc::new(AtomicBool::new(false)),
        bye_requested: AtomicBool::new(false),
        control_generation: AtomicU64::new(1),
        resume_secret: keys.resume_auth_key(),
        trust_secret: Mutex::new(None),
        tcp_audio,
        udp_audio: socket.clone(),
        control_peer_addr: Mutex::new(peer_addr),
        control_state: Mutex::new(ControlState::Active),
        peer_audio_addr: Mutex::new(None),
        outgoing: crate::PcmQueue::new(crate::DEFAULT_QUEUE_CAPACITY),
        incoming: crate::PcmQueue::new(crate::DEFAULT_QUEUE_CAPACITY),
        capture_convert: Mutex::new(prepared_capture_converter(local, format)),
        audio_sealer: Mutex::new(audio_sealer),
        audio_opener: Mutex::new(audio_opener),
        direction: Mutex::new(DirectionNegotiation::with_initial_flow(
            DirectionOffer {
                generation: local_config.direction_generation,
                direction: local_config.direction,
                device_id: local_config.device_id.clone(),
            },
            &local_config.device_id,
            &peer_id_for_flow,
            roles.receive,
            local_config.direction_generation,
        )),
    });
    if !inner.insert_session(Arc::clone(&record)) {
        let _ = cipher.send(
            &mut stream,
            &ControlMessage::PairFail {
                reason: "the host is already at its session limit".into(),
            },
        );
        return;
    }
    if let Err(reason) = spawn_session_workers(&inner, &record, &socket, true) {
        let _ = cipher.send(
            &mut stream,
            &ControlMessage::PairFail {
                reason: reason.clone(),
            },
        );
        teardown(&inner, id, reason);
        return;
    }
    if !inner.session_alive(id) {
        return;
    }
    if cipher
        .send(&mut stream, &ControlMessage::SessionReady {})
        .is_err()
    {
        teardown(
            &inner,
            id,
            "handshake failed while starting the session".into(),
        );
        return;
    }
    inner.emit(RelayEvent::SessionEstablished {
        id,
        peer: record.peer.clone(),
        roles,
        codec,
    });
    host_control_loop(inner, record, stream, cipher);
}
