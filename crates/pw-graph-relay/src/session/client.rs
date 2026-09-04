//! Connecting outwards: the attempt threads, the post-authentication session
//! setup, and opening the TCP audio channel.

use super::*;

pub(crate) fn connect_peer(
    inner: &Arc<EngineInner>,
    id: SessionId,
    target: SocketAddr,
    pin: String,
    roles: Roles,
) {
    connect_peer_with_spawner(inner, id, target, pin, roles, spawn_named);
}

pub(crate) fn connect_trusted_peer(
    inner: &Arc<EngineInner>,
    id: SessionId,
    target: SocketAddr,
    peer_id: String,
    secret: [u8; 32],
    roles: Roles,
) {
    let worker_inner = Arc::clone(inner);
    let failure_inner = Arc::clone(inner);
    let worker: Worker =
        Box::new(move || trusted_client_thread(worker_inner, id, target, peer_id, secret, roles));
    if let Err(error) = spawn_named(format!("relay-trusted-client-{target}"), worker) {
        fail_attempt(
            &failure_inner,
            id,
            format!("could not start trusted connection worker: {error}"),
        );
    }
}

pub(super) fn connect_peer_with_spawner(
    inner: &Arc<EngineInner>,
    id: SessionId,
    target: SocketAddr,
    pin: String,
    roles: Roles,
    spawn: WorkerSpawner,
) {
    let worker_inner = Arc::clone(inner);
    let failure_inner = Arc::clone(inner);
    let worker: Worker = Box::new(move || client_thread(worker_inner, id, target, pin, roles));
    if let Err(error) = spawn(format!("relay-client-{target}"), worker) {
        fail_attempt(
            &failure_inner,
            id,
            format!("could not start relay connection worker: {error}"),
        );
    }
}

pub(super) fn fail_trusted_attempt(
    inner: &Arc<EngineInner>,
    id: SessionId,
    target: SocketAddr,
    host_id: &str,
    reason: String,
) {
    inner.note_candidate_failure(host_id, target);
    fail_attempt(inner, id, reason);
}

#[allow(deprecated)]
pub(super) fn trusted_client_thread(
    inner: Arc<EngineInner>,
    id: SessionId,
    target: SocketAddr,
    host_id: String,
    secret: [u8; 32],
    roles: Roles,
) {
    if roles.is_empty() {
        fail_attempt(&inner, id, "no audio direction requested".into());
        return;
    }
    if !roles.is_one_way() {
        fail_attempt(
            &inner,
            id,
            "bidirectional relay sessions are disabled; choose one audio direction".into(),
        );
        return;
    }
    let config = inner.config();
    let bind = netlink::outbound_bind_addr(&netlink::local_links(), target, config.transport);
    let mut stream = match connect_control_tcp(target, bind, config.transport) {
        Ok(stream) => stream,
        Err(error) => {
            fail_trusted_attempt(
                &inner,
                id,
                target,
                &host_id,
                format!("trusted connection failed: {error}"),
            );
            return;
        }
    };
    let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
    let client_nonce = fresh_resume_nonce();
    if write_frame(
        &mut stream,
        &ControlMessage::TrustedHello {
            protocol: PROTOCOL_VERSION as u32,
            device_id: config.device_id.clone(),
            device_name: config.device_name.clone(),
            device_kind: config.device_kind,
            host_id: host_id.clone(),
            transport: config.transport.as_str().into(),
            roles,
            sample_rate: config.sample_rate,
            channels: config.channels,
            client_nonce: hex_encode(&client_nonce),
        },
    )
    .is_err()
    {
        fail_trusted_attempt(
            &inner,
            id,
            target,
            &host_id,
            "trusted handshake failed while sending hello".into(),
        );
        return;
    }
    let (server_nonce, wire_id, returned_host_id, host_name) = match read_frame(&mut stream) {
        Ok(ControlMessage::TrustedChallenge {
            server_nonce,
            session_id,
            host_id: challenge_host_id,
            host_name,
        }) => match decode_resume_nonce(&server_nonce) {
            Some(server_nonce) => (server_nonce, session_id, challenge_host_id, host_name),
            None => {
                fail_trusted_attempt(
                    &inner,
                    id,
                    target,
                    &host_id,
                    "trusted host sent a malformed challenge".into(),
                );
                return;
            }
        },
        Ok(ControlMessage::PairFail { reason }) => {
            fail_trusted_attempt(
                &inner,
                id,
                target,
                &host_id,
                format!("host rejected trusted connection: {reason}"),
            );
            return;
        }
        Ok(_) | Err(_) => {
            fail_trusted_attempt(
                &inner,
                id,
                target,
                &host_id,
                "trusted handshake response was malformed".into(),
            );
            return;
        }
    };
    if returned_host_id != host_id {
        fail_trusted_attempt(
            &inner,
            id,
            target,
            &host_id,
            "trusted host identity did not match".into(),
        );
        return;
    }
    let proof = crate::crypto::trusted_proof(
        &secret,
        &config.device_id,
        &host_id,
        wire_id,
        &client_nonce,
        &server_nonce,
    );
    if write_frame(
        &mut stream,
        &ControlMessage::TrustedProof {
            proof: hex_encode(&proof),
        },
    )
    .is_err()
    {
        fail_trusted_attempt(
            &inner,
            id,
            target,
            &host_id,
            "trusted handshake failed while proving credential".into(),
        );
        return;
    }
    match read_frame(&mut stream) {
        Ok(ControlMessage::TrustedOk {}) => {}
        Ok(ControlMessage::PairFail { reason }) => {
            fail_trusted_attempt(
                &inner,
                id,
                target,
                &host_id,
                format!("host rejected trusted connection: {reason}"),
            );
            return;
        }
        Ok(_) | Err(_) => {
            fail_trusted_attempt(
                &inner,
                id,
                target,
                &host_id,
                "trusted handshake was not accepted".into(),
            );
            return;
        }
    }
    let keys = crate::crypto::trusted_session_keys(
        &secret,
        &config.device_id,
        &host_id,
        wire_id,
        &client_nonce,
        &server_nonce,
        Side::Client,
    );
    let Ok((sealer, opener)) = keys.control_channel() else {
        fail_trusted_attempt(
            &inner,
            id,
            target,
            &host_id,
            "trusted control keys could not be prepared".into(),
        );
        return;
    };
    client_session_after_auth(
        inner,
        stream,
        ControlCipher { sealer, opener },
        ClientAuthenticatedSession {
            id,
            target,
            roles,
            config,
            host_name,
            host_id,
            keys,
        },
    );
}

/// Client side: connect, pair, negotiate, then keepalive watcher.
#[allow(deprecated)]
pub(super) fn client_thread(
    inner: Arc<EngineInner>,
    id: SessionId,
    target: SocketAddr,
    pin: String,
    roles: Roles,
) {
    if roles.is_empty() {
        fail_attempt(&inner, id, "no audio direction requested".into());
        return;
    }
    if !roles.is_one_way() {
        fail_attempt(
            &inner,
            id,
            "bidirectional relay sessions are disabled; choose one audio direction".into(),
        );
        return;
    }
    let config = inner.config();

    // Bind outgoing sockets to the best local link for this target, honouring
    // the configured transport preference.
    let links = netlink::local_links();
    let bind = netlink::outbound_bind_addr(&links, target, config.transport);

    let mut stream = match connect_control_tcp(target, bind, config.transport) {
        Ok(stream) => stream,
        Err(error) => {
            fail_attempt(&inner, id, format!("connection failed: {error}"));
            return;
        }
    };
    let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));

    let client = pake_start(Side::Client, &pin);
    let hello = ControlMessage::Hello {
        protocol: PROTOCOL_VERSION as u32,
        device_id: config.device_id.clone(),
        transport: config.transport.as_str().into(),
        device_name: config.device_name.clone(),
        device_kind: config.device_kind,
        roles,
        sample_rate: config.sample_rate,
        channels: config.channels,
        pake: hex_encode(&client.message),
    };
    if write_frame(&mut stream, &hello).is_err() {
        fail_attempt(&inner, id, "handshake failed while sending hello".into());
        return;
    }

    let (host_pake, host_name, host_id) = match read_frame(&mut stream) {
        Ok(ControlMessage::Challenge {
            protocol,
            pake,
            host_name,
            device_id,
        }) if protocol == PROTOCOL_VERSION as u32 => (pake, host_name, device_id),
        Ok(ControlMessage::PairFail { reason }) => {
            fail_attempt(&inner, id, format!("host rejected pairing: {reason}"));
            return;
        }
        Ok(_) => {
            fail_attempt(&inner, id, "host sent an unexpected message".into());
            return;
        }
        Err(error) => {
            fail_attempt(&inner, id, format!("handshake failed: {error}"));
            return;
        }
    };
    let Ok(host_message) = hex_decode(&host_pake) else {
        fail_attempt(&inner, id, "host sent a malformed pairing message".into());
        return;
    };
    let keys = match client.finish(&host_message) {
        Ok(keys) => keys,
        Err(error) => {
            fail_attempt(&inner, id, format!("pairing failed: {error}"));
            return;
        }
    };
    if write_frame(
        &mut stream,
        &ControlMessage::Pair {
            pake: String::new(),
            confirm: hex_encode(&keys.confirmation()),
        },
    )
    .is_err()
    {
        fail_attempt(&inner, id, "handshake failed while pairing".into());
        return;
    }
    // The host's confirmation is what tells the client its PIN was right —
    // without it a wrong PIN would only show up as traffic that never
    // decrypts, several messages later.
    match read_frame(&mut stream) {
        Ok(ControlMessage::PairConfirm { confirm }) => {
            let matched = hex_decode(&confirm)
                .map(|confirm| keys.verify_confirmation(&confirm))
                .unwrap_or(false);
            if !matched {
                fail_attempt(&inner, id, "the PIN did not match the host".into());
                return;
            }
        }
        Ok(ControlMessage::PairFail { reason }) => {
            fail_attempt(&inner, id, format!("host rejected pairing: {reason}"));
            return;
        }
        Ok(_) | Err(_) => {
            fail_attempt(&inner, id, "pairing response was malformed".into());
            return;
        }
    }
    // PIN pairing is also authenticated and establishes the address that
    // should win if discovery later offers several addresses for this host.
    inner.note_candidate_success(&host_id, target);
    let Ok((sealer, opener)) = keys.control_channel() else {
        fail_attempt(&inner, id, "session keys could not be prepared".into());
        return;
    };
    let mut cipher = ControlCipher { sealer, opener };

    let (audio_port, wire_id) = match cipher.receive(&mut stream) {
        Ok(ControlMessage::PairOk {
            audio_port,
            session_id,
        }) => (audio_port, session_id),
        Ok(ControlMessage::PairFail { reason }) => {
            fail_attempt(&inner, id, format!("host rejected pairing: {reason}"));
            return;
        }
        Ok(_) | Err(_) => {
            fail_attempt(&inner, id, "pairing response was malformed".into());
            return;
        }
    };

    let start = ControlMessage::SessionStart {
        roles,
        codec: config.codec,
        frame_ms: config.frame_ms,
        sample_rate: config.sample_rate,
        channels: config.channels,
    };
    if cipher.send(&mut stream, &start).is_err() {
        fail_attempt(&inner, id, "handshake failed during session setup".into());
        return;
    }
    match cipher.receive(&mut stream) {
        Ok(ControlMessage::SessionReady {}) => {}
        Ok(ControlMessage::PairFail { reason }) => {
            fail_attempt(&inner, id, format!("host rejected session: {reason}"));
            return;
        }
        Ok(_) => {
            fail_attempt(
                &inner,
                id,
                "host sent an unexpected session response".into(),
            );
            return;
        }
        Err(error) => {
            fail_attempt(&inner, id, format!("session setup failed: {error}"));
            return;
        }
    }
    let Ok((audio_sealer, audio_opener)) = keys.audio_channel() else {
        fail_attempt(&inner, id, "audio keys could not be prepared".into());
        return;
    };
    let audio_over_tcp = config.transport == crate::TransportPreference::Adb;
    // A legacy v3 host has no stable installation identity and does not know
    // the enrollment messages. Do not turn a successful connection to one
    // into an avoidable five-second wait for an acknowledgement that can
    // never arrive, and never persist a name-only bearer credential.
    let trust_secret =
        (config.trust_new_peers && !host_id.trim().is_empty()).then(fresh_trust_secret);

    let socket = if audio_over_tcp {
        None
    } else {
        match bind_udp_audio_socket(&inner, target, false) {
            Ok(socket) => {
                tune_audio_socket(&socket);
                match UdpAudioSlot::new(socket) {
                    Ok(slot) => Some(slot),
                    Err(error) => {
                        fail_attempt(
                            &inner,
                            id,
                            format!("could not prepare audio socket: {error}"),
                        );
                        return;
                    }
                }
            }
            Err(error) => {
                fail_attempt(&inner, id, format!("could not open audio socket: {error}"));
                return;
            }
        }
    };
    let host_audio_addr = SocketAddr::new(target.ip(), audio_port);

    let format = AudioFormat::new(config.sample_rate, config.channels, config.frame_ms);
    let local = config.local_format();
    let mut audio_sealer = audio_sealer;
    // Teach the host our UDP address before real audio flows. The announce is
    // sealed with the session key, so only the paired client can move it.
    if !audio_over_tcp {
        if let Ok(announce) = announce_packet(&mut audio_sealer, config.codec) {
            if let Some(socket) = socket.as_ref().and_then(|slot| slot.current()) {
                let _ = socket.send_to(&announce, host_audio_addr);
            }
        }
    }
    let tcp_audio = audio_over_tcp.then(TcpAudioSlot::new);
    let peer_id = if host_id.trim().is_empty() {
        host_name.clone()
    } else {
        host_id.clone()
    };
    let record = Arc::new(SessionRecord {
        id,
        wire_id,
        peer: PeerInfo {
            id: peer_id.clone(),
            name: host_name,
            kind: DeviceKind::Other,
            addr: target,
        },
        roles,
        codec: config.codec,
        format,
        active_roles: AtomicU8::new(SessionRecord::role_bits(Roles {
            emit: roles.emit,
            receive: roles.receive,
        })),
        stop: Arc::new(AtomicBool::new(false)),
        bye_requested: AtomicBool::new(false),
        control_generation: AtomicU64::new(1),
        resume_secret: keys.resume_auth_key(),
        trust_secret: Mutex::new(trust_secret),
        tcp_audio: tcp_audio.clone(),
        udp_audio: socket.clone(),
        control_peer_addr: Mutex::new(target),
        control_state: Mutex::new(ControlState::Active),
        peer_audio_addr: Mutex::new(Some(host_audio_addr)),
        outgoing: crate::PcmQueue::new(crate::DEFAULT_QUEUE_CAPACITY),
        incoming: crate::PcmQueue::new(crate::DEFAULT_QUEUE_CAPACITY),
        capture_convert: Mutex::new(prepared_capture_converter(local, format)),
        audio_sealer: Mutex::new(audio_sealer),
        audio_opener: Mutex::new(audio_opener),
        direction: Mutex::new(DirectionNegotiation::with_initial_flow(
            DirectionOffer {
                generation: config.direction_generation,
                direction: roles.direction().unwrap_or(config.direction),
                device_id: config.device_id.clone(),
            },
            &config.device_id,
            &peer_id,
            roles.emit,
            config.direction_generation,
        )),
    });
    if !inner.insert_session(Arc::clone(&record)) {
        let reason = "the local session limit was reached".to_string();
        let _ = cipher.send(
            &mut stream,
            &ControlMessage::Bye {
                reason: reason.clone(),
            },
        );
        fail_attempt(&inner, id, reason);
        return;
    }
    if let Err(reason) = spawn_session_workers(&inner, &record, &socket, false) {
        let _ = cipher.send(
            &mut stream,
            &ControlMessage::Bye {
                reason: reason.clone(),
            },
        );
        // `spawn_worker_with_report` has already raised the stop flag so a
        // worker that did start will exit. Startup failure still needs an
        // explicit removal: `fail_session` treats an already-set stop flag as
        // an orderly shutdown and would otherwise leave this record in the
        // session map without a SessionLost event.
        inner.emit(RelayEvent::Error {
            message: reason.clone(),
        });
        teardown(&inner, id, reason);
        return;
    }

    if !inner.session_alive(id) {
        return;
    }

    // ADB audio is supervised independently. A temporary missing forwarding
    // rule must not turn an otherwise healthy authenticated control session
    // into a false disconnect.

    // The session is fully established here — audio workers are running and
    // the sealed setup completed. Reporting it before the enrollment exchange
    // keeps the UI from showing "connecting" for the whole host-side
    // accept/decline window; the trust save is a background addendum and its
    // own event (TrustedPeerAvailable) reports the outcome.
    inner.emit(RelayEvent::SessionEstablished {
        id,
        peer: record.peer.clone(),
        roles,
        codec: config.codec,
    });

    let trusted_secret = record.trust_secret.lock().ok().and_then(|slot| *slot);
    if let Some(secret) = trusted_secret {
        enroll_trusted_peer(
            &inner,
            &record,
            &mut stream,
            &mut cipher,
            &config.device_id,
            secret,
        );
    }

    client_control_loop(inner, record, stream, cipher, socket, target);
}

/// Shared post-authentication client setup used by both PIN and trusted
/// handshakes.
/// The locally decided session facts a client carries into the shared
/// post-authentication setup, once the host has been authenticated.
pub(super) struct ClientAuthenticatedSession {
    pub(super) id: SessionId,
    pub(super) target: SocketAddr,
    pub(super) roles: Roles,
    pub(super) config: crate::EngineConfig,
    pub(super) host_name: String,
    pub(super) host_id: String,
    /// Directional keys derived by the completed handshake, moved into the
    /// session record below.
    pub(super) keys: SessionKeys,
}

#[allow(deprecated)]
pub(super) fn client_session_after_auth(
    inner: Arc<EngineInner>,
    mut stream: TcpStream,
    mut cipher: ControlCipher,
    session: ClientAuthenticatedSession,
) {
    let ClientAuthenticatedSession {
        id,
        target,
        roles,
        config,
        host_name,
        host_id,
        keys,
    } = session;
    let (audio_port, wire_id) = match cipher.receive(&mut stream) {
        Ok(ControlMessage::PairOk {
            audio_port,
            session_id,
        }) => (audio_port, session_id),
        Ok(ControlMessage::PairFail { reason }) => {
            fail_trusted_attempt(
                &inner,
                id,
                target,
                &host_id,
                format!("host rejected pairing: {reason}"),
            );
            return;
        }
        Ok(_) | Err(_) => {
            fail_trusted_attempt(
                &inner,
                id,
                target,
                &host_id,
                "pairing response was malformed".into(),
            );
            return;
        }
    };
    if cipher
        .send(
            &mut stream,
            &ControlMessage::SessionStart {
                roles,
                codec: config.codec,
                frame_ms: config.frame_ms,
                sample_rate: config.sample_rate,
                channels: config.channels,
            },
        )
        .is_err()
    {
        fail_trusted_attempt(
            &inner,
            id,
            target,
            &host_id,
            "handshake failed during session setup".into(),
        );
        return;
    }
    match cipher.receive(&mut stream) {
        Ok(ControlMessage::SessionReady {}) => {}
        Ok(ControlMessage::PairFail { reason }) => {
            fail_trusted_attempt(
                &inner,
                id,
                target,
                &host_id,
                format!("host rejected session: {reason}"),
            );
            return;
        }
        Ok(_) | Err(_) => {
            fail_trusted_attempt(
                &inner,
                id,
                target,
                &host_id,
                "host sent an unexpected session response".into(),
            );
            return;
        }
    }
    let Ok((audio_sealer, audio_opener)) = keys.audio_channel() else {
        fail_trusted_attempt(
            &inner,
            id,
            target,
            &host_id,
            "audio keys could not be prepared".into(),
        );
        return;
    };
    let audio_over_tcp = config.transport == crate::TransportPreference::Adb;
    // This helper is used for an already authenticated trusted reconnect;
    // trusted credentials are enrolled only by the explicit PIN path.
    let trust_secret = None;
    let socket = if audio_over_tcp {
        None
    } else {
        match bind_udp_audio_socket(&inner, target, false) {
            Ok(socket) => {
                tune_audio_socket(&socket);
                match UdpAudioSlot::new(socket) {
                    Ok(slot) => Some(slot),
                    Err(error) => {
                        fail_trusted_attempt(
                            &inner,
                            id,
                            target,
                            &host_id,
                            format!("could not prepare audio socket: {error}"),
                        );
                        return;
                    }
                }
            }
            Err(error) => {
                fail_trusted_attempt(
                    &inner,
                    id,
                    target,
                    &host_id,
                    format!("could not open audio socket: {error}"),
                );
                return;
            }
        }
    };
    let host_audio_addr = SocketAddr::new(target.ip(), audio_port);
    let format = AudioFormat::new(config.sample_rate, config.channels, config.frame_ms);
    let local = config.local_format();
    let mut audio_sealer = audio_sealer;
    if !audio_over_tcp {
        if let Ok(announce) = announce_packet(&mut audio_sealer, config.codec) {
            if let Some(socket) = socket.as_ref().and_then(|slot| slot.current()) {
                let _ = socket.send_to(&announce, host_audio_addr);
            }
        }
    }
    let tcp_audio = audio_over_tcp.then(TcpAudioSlot::new);
    let peer_id = if host_id.trim().is_empty() {
        host_name.clone()
    } else {
        host_id.clone()
    };
    let record = Arc::new(SessionRecord {
        id,
        wire_id,
        peer: PeerInfo {
            id: peer_id.clone(),
            name: host_name,
            kind: DeviceKind::Other,
            addr: target,
        },
        roles,
        codec: config.codec,
        format,
        active_roles: AtomicU8::new(SessionRecord::role_bits(Roles {
            emit: roles.emit,
            receive: roles.receive,
        })),
        stop: Arc::new(AtomicBool::new(false)),
        bye_requested: AtomicBool::new(false),
        control_generation: AtomicU64::new(1),
        resume_secret: keys.resume_auth_key(),
        trust_secret: Mutex::new(trust_secret),
        tcp_audio: tcp_audio.clone(),
        udp_audio: socket.clone(),
        control_peer_addr: Mutex::new(target),
        control_state: Mutex::new(ControlState::Active),
        peer_audio_addr: Mutex::new(Some(host_audio_addr)),
        outgoing: crate::PcmQueue::new(crate::DEFAULT_QUEUE_CAPACITY),
        incoming: crate::PcmQueue::new(crate::DEFAULT_QUEUE_CAPACITY),
        capture_convert: Mutex::new(prepared_capture_converter(local, format)),
        audio_sealer: Mutex::new(audio_sealer),
        audio_opener: Mutex::new(audio_opener),
        direction: Mutex::new(DirectionNegotiation::with_initial_flow(
            DirectionOffer {
                generation: config.direction_generation,
                direction: roles.direction().unwrap_or(config.direction),
                device_id: config.device_id.clone(),
            },
            &config.device_id,
            &peer_id,
            roles.emit,
            config.direction_generation,
        )),
    });
    if !inner.insert_session(Arc::clone(&record)) {
        let reason = "the local session limit was reached".to_string();
        let _ = cipher.send(
            &mut stream,
            &ControlMessage::Bye {
                reason: reason.clone(),
            },
        );
        fail_trusted_attempt(&inner, id, target, &record.peer.id, reason);
        return;
    }
    if let Err(reason) = spawn_session_workers(&inner, &record, &socket, false) {
        let _ = cipher.send(
            &mut stream,
            &ControlMessage::Bye {
                reason: reason.clone(),
            },
        );
        inner.note_candidate_failure(&record.peer.id, target);
        teardown(&inner, id, reason);
        return;
    }
    if !inner.session_alive(id) {
        return;
    }
    // A clear TrustedOk only proves that the candidate followed the wire
    // shape. Record the address only after the sealed session setup and all
    // requested workers have succeeded, which proves possession of the
    // trusted credential on the host side.
    inner.note_candidate_success(&record.peer.id, target);
    inner.emit(RelayEvent::SessionEstablished {
        id,
        peer: record.peer.clone(),
        roles,
        codec: config.codec,
    });
    client_control_loop(inner, record, stream, cipher, socket, target);
}

pub(super) fn open_tcp_audio(
    inner: &Arc<EngineInner>,
    record: &Arc<SessionRecord>,
    slot: &Arc<TcpAudioSlot>,
    target: SocketAddr,
) -> std::io::Result<()> {
    if !slot.begin_connect() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "ADB audio reconnect is already in progress",
        ));
    }
    let result = open_tcp_audio_once(inner, record, slot, target);
    slot.end_connect();
    result
}

pub(super) fn open_tcp_audio_once(
    inner: &Arc<EngineInner>,
    record: &Arc<SessionRecord>,
    slot: &Arc<TcpAudioSlot>,
    target: SocketAddr,
) -> std::io::Result<()> {
    let config = inner.config();
    let bind = netlink::outbound_bind_addr(&netlink::local_links(), target, config.transport);
    let mut stream = connect_control_tcp(target, bind, config.transport)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let client_nonce = fresh_resume_nonce();
    write_frame(
        &mut stream,
        &ControlMessage::AudioHello {
            session_id: record.wire_id,
            client_nonce: hex_encode(&client_nonce),
        },
    )?;
    let server_nonce = match read_frame(&mut stream)? {
        ControlMessage::AudioChallenge { server_nonce } => decode_resume_nonce(&server_nonce)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "malformed TCP audio challenge",
                )
            })?,
        ControlMessage::PairFail { reason } => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                reason,
            ))
        }
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unexpected TCP audio challenge",
            ))
        }
    };
    let proof = crate::crypto::tcp_audio_proof(
        &record.resume_secret,
        record.wire_id,
        &client_nonce,
        &server_nonce,
        Side::Client,
    );
    write_frame(
        &mut stream,
        &ControlMessage::AudioProof {
            proof: hex_encode(&proof),
        },
    )?;
    let server_proof = match read_frame(&mut stream)? {
        ControlMessage::AudioReady { proof } => hex_decode(&proof).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "malformed TCP audio response",
            )
        })?,
        ControlMessage::PairFail { reason } => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                reason,
            ))
        }
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unexpected TCP audio response",
            ))
        }
    };
    let expected = crate::crypto::tcp_audio_proof(
        &record.resume_secret,
        record.wire_id,
        &client_nonce,
        &server_nonce,
        Side::Host,
    );
    if !bool::from(expected.ct_eq(&server_proof)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "TCP audio host authentication failed",
        ));
    }
    stream.set_read_timeout(Some(Duration::from_millis(500)))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    slot.install(stream)
}
