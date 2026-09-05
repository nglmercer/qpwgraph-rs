//! The control channel: its cipher, the keepalive/watch loops both sides run,
//! and the teardown paths.

use super::*;

/// The two ends of an encrypted control channel, owned by one control thread.
pub(super) struct ControlCipher {
    pub(super) sealer: Sealer,
    pub(super) opener: Opener,
}

impl ControlCipher {
    pub(super) fn send(
        &mut self,
        stream: &mut TcpStream,
        message: &ControlMessage,
    ) -> std::io::Result<()> {
        write_sealed_frame(stream, &mut self.sealer, message)
    }

    pub(super) fn receive(&mut self, stream: &mut TcpStream) -> std::io::Result<ControlMessage> {
        read_sealed_frame(stream, &mut self.opener)
    }
}

/// Send the currently queued direction offer, keeping it queued until the
/// sealed write succeeds. This is also called by the short trusted-enrollment
/// wait, which temporarily owns the control stream before the normal watcher
/// starts.
pub(super) fn flush_pending_direction_offer(
    record: &Arc<SessionRecord>,
    stream: &mut TcpStream,
    cipher: &mut ControlCipher,
) -> std::io::Result<()> {
    if let Some(offer) = record.pending_direction_offer() {
        cipher.send(
            stream,
            &ControlMessage::DirectionOffer {
                generation: offer.generation,
                direction: offer.direction,
                device_id: offer.device_id.clone(),
            },
        )?;
        record.mark_direction_offer_sent(&offer);
    }
    Ok(())
}

/// Send the canonical emitter offer, retaining it until the peer's
/// authenticated acknowledgement arrives so a control reconnect can resend
/// the request safely.
pub(super) fn flush_pending_flow_offer(
    record: &Arc<SessionRecord>,
    stream: &mut TcpStream,
    cipher: &mut ControlCipher,
) -> std::io::Result<()> {
    if let Some(offer) = record.pending_flow_offer() {
        cipher.send(
            stream,
            &ControlMessage::FlowOffer {
                generation: offer.generation,
                emitter_id: offer.emitter_id.clone(),
                proposer_id: offer.proposer_id.clone(),
            },
        )?;
        record.mark_flow_offer_sent(&offer);
    }
    Ok(())
}

/// Apply a remote offer and send the deterministic acknowledgement while the
/// caller still owns the control cipher.
pub(super) fn apply_direction_offer(
    record: &Arc<SessionRecord>,
    stream: &mut TcpStream,
    cipher: &mut ControlCipher,
    offer: DirectionOffer,
) -> Result<Option<DirectionResolution>, String> {
    let (ack, resolution) = record.receive_direction_offer(offer)?;
    cipher
        .send(
            stream,
            &ControlMessage::DirectionAck {
                generation: ack.generation,
                direction: ack.direction,
            },
        )
        .map_err(|error| error.to_string())?;
    Ok(resolution)
}

pub(super) fn emit_direction_resolution(
    inner: &Arc<EngineInner>,
    id: SessionId,
    resolution: DirectionResolution,
) {
    inner.emit(RelayEvent::DirectionResolved {
        id,
        generation: resolution.winner.generation,
        direction: resolution.winner.direction,
        winner_device_id: resolution.winner.device_id,
    });
}

pub(super) fn apply_flow_offer(
    record: &Arc<SessionRecord>,
    stream: &mut TcpStream,
    cipher: &mut ControlCipher,
    offer: FlowOffer,
) -> Result<Option<FlowResolution>, String> {
    let (ack, resolution) = record.receive_flow_offer(offer)?;
    cipher
        .send(
            stream,
            &ControlMessage::FlowAck {
                generation: ack.generation,
                emitter_id: ack.emitter_id,
            },
        )
        .map_err(|error| error.to_string())?;
    Ok(resolution)
}

pub(super) fn emit_flow_resolution(
    inner: &Arc<EngineInner>,
    record: &Arc<SessionRecord>,
    resolution: FlowResolution,
) {
    let flow = resolution.winner.flow();
    record.apply_flow(&flow);
    let local_id = record.local_device_id().unwrap_or_default();
    let Some(mode) = flow.mode_for(&local_id) else {
        return;
    };
    inner.emit(RelayEvent::FlowResolved {
        id: record.id,
        generation: resolution.winner.generation,
        flow,
        mode,
    });
}

/// Ask a session's control thread to send `bye` and tear down. Only the
/// bye flag is set: the control thread checks it before its stop condition
/// so the farewell frame actually goes out.
pub(crate) fn request_bye(inner: &EngineInner, id: SessionId) {
    if let Some(record) = inner.session(id) {
        record.bye_requested.store(true, Ordering::Relaxed);
    }
}

/// Remove a session and announce its loss. Idempotent.
pub(crate) fn teardown(inner: &EngineInner, id: SessionId, reason: String) {
    if inner.remove_session(id).is_some() {
        inner.emit(RelayEvent::SessionLost { id, reason });
    }
}

/// Report a connection attempt that failed before any session existed. The
/// caller owns the id (it came from `connect`), so the loss must always be
/// announced even though nothing was registered.
pub(crate) fn fail_attempt(inner: &EngineInner, id: SessionId, reason: String) {
    inner.remove_session(id);
    inner.emit(RelayEvent::SessionLost { id, reason });
}

/// Handle a clean control-watch exit that ends the session. Returns `true`
/// if the session was torn down (so the caller can return).
pub(super) fn handle_teardown_exit(
    inner: &Arc<EngineInner>,
    record: &Arc<SessionRecord>,
    exit: ControlExit,
    on_drop: impl FnOnce(String) -> bool,
) -> bool {
    match exit {
        ControlExit::Stopped => {
            teardown(inner, record.id, "session stopped".into());
            true
        }
        ControlExit::PeerBye(reason) => {
            teardown(inner, record.id, format!("peer left: {reason}"));
            true
        }
        ControlExit::Dropped(reason) => on_drop(reason),
    }
}

/// Watch the control channel, waiting out link drops for [`RESUME_GRACE`]
/// so a reconnecting client can take over without losing the session. The
/// resuming thread runs its own watch, so every outcome ends this one.
pub(super) fn host_control_loop(
    inner: Arc<EngineInner>,
    record: Arc<SessionRecord>,
    stream: TcpStream,
    cipher: ControlCipher,
) {
    let result = watch_control(Arc::clone(&inner), Arc::clone(&record), stream, cipher);
    handle_teardown_exit(&inner, &record, result, |reason| {
        if await_resume_grace(&inner, &record) {
            return false;
        }
        teardown(&inner, record.id, reason);
        true
    });
}

/// Client-side control watch: on a link drop the host is re-dialed and the
/// session resumed, so Wi-Fi roaming or brief outages do not end the session.
pub(super) fn client_control_loop(
    inner: Arc<EngineInner>,
    record: Arc<SessionRecord>,
    stream: TcpStream,
    cipher: ControlCipher,
    socket: Option<Arc<UdpAudioSlot>>,
    target: SocketAddr,
) {
    let socket_codec = record.codec;
    let mut stream = Some((stream, cipher));
    loop {
        let (taken, cipher) = stream.take().expect("stream is set between iterations");
        let result = watch_control(Arc::clone(&inner), Arc::clone(&record), taken, cipher);
        let torn_down = handle_teardown_exit(&inner, &record, result, |reason| {
            if record.bye_requested.load(Ordering::Relaxed) || !inner.session_alive(record.id) {
                teardown(&inner, record.id, reason);
                return true;
            }
            inner.emit(RelayEvent::Error {
                message: format!("control link to host lost ({reason}); attempting to resume"),
            });
            match resume_client_control(&inner, &record, target) {
                Some((resumed_stream, resumed_cipher, resumed_target)) => {
                    if let Ok(mut current) = record.control_peer_addr.lock() {
                        *current = resumed_target;
                    }
                    if record.tcp_audio.is_none() {
                        // Re-announce our UDP address from the real audio socket:
                        // the route may have changed link (e.g. Wi-Fi to USB
                        // tethering), and the host must learn the new source
                        // address. The announce is sealed with the session's
                        // unchanged audio key, which is exactly what authorises
                        // the host to follow us.
                        let audio_port = record
                            .peer_audio_addr
                            .lock()
                            .ok()
                            .and_then(|slot| slot.map(|addr| addr.port()));
                        match migrate_udp_audio_socket(&inner, &record, resumed_target, false) {
                            Ok(()) => {
                                if let (
                                    Some(socket),
                                    Some(audio_port),
                                    Ok(mut slot),
                                    Ok(mut sealer),
                                ) = (
                                    socket.as_ref().and_then(|slot| slot.current()),
                                    audio_port,
                                    record.peer_audio_addr.lock(),
                                    record.audio_sealer.lock(),
                                ) {
                                    let addr = SocketAddr::new(resumed_target.ip(), audio_port);
                                    *slot = Some(addr);
                                    if let Ok(announce) = announce_packet(&mut sealer, socket_codec)
                                    {
                                        let _ = socket.send_to(&announce, addr);
                                    }
                                }
                            }
                            Err(error) => {
                                inner.emit(RelayEvent::Error {
                                    message: format!(
                                        "UDP audio interface migration failed: {error}"
                                    ),
                                });
                                if error.is_fatal() {
                                    // A client normally takes a fresh
                                    // ephemeral port and announces it, so it
                                    // should never reach here; if its audio
                                    // socket is nonetheless unrecoverable the
                                    // session can only go silent, so end it.
                                    teardown(
                                        &inner,
                                        record.id,
                                        format!("relay audio socket was lost: {error}"),
                                    );
                                    return true;
                                }
                            }
                        }
                    }
                    stream = Some((resumed_stream, resumed_cipher));
                    false
                }
                None => {
                    teardown(&inner, record.id, reason);
                    true
                }
            }
        });
        if torn_down {
            return;
        }
    }
}

/// Tear a session down after an unrecoverable worker failure, surfacing the
/// reason once. Both are idempotent, so a deliberate shutdown that races a
/// worker failure still produces exactly one `SessionLost`.
pub(super) fn fail_session(inner: &Arc<EngineInner>, record: &Arc<SessionRecord>, reason: String) {
    // A stop that was already requested is an orderly shutdown, not a fault.
    if record.stop.load(Ordering::Relaxed) || !inner.session_alive(record.id) {
        return;
    }
    inner.emit(RelayEvent::Error {
        message: reason.clone(),
    });
    teardown(inner, record.id, reason);
}

/// Why a control watch ended. Teardown decisions belong to the caller so
/// host and client loops can attempt a resume first.
pub(super) enum ControlExit {
    /// Engine shutdown or local bye request (farewell already sent).
    Stopped,
    /// The peer said goodbye.
    PeerBye(String),
    /// The link broke (closed or keepalive timeout); may be resumable.
    Dropped(String),
}

/// Keepalive watcher; runs on the session's control thread until the watch
/// ends. Returns the reason so the caller can decide about resuming.
pub(super) fn watch_control(
    inner: Arc<EngineInner>,
    record: Arc<SessionRecord>,
    mut stream: TcpStream,
    mut cipher: ControlCipher,
) -> ControlExit {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    // A control owner may be a resumed TCP stream. Any offer that was written
    // just before the previous stream dropped remains authoritative, but it
    // must be sent again on this authenticated replacement channel.
    record.reset_direction_offer_send();
    record.reset_flow_offer_send();
    let mut last_seen = Instant::now();
    let mut last_keepalive = Instant::now();

    loop {
        if !inner.running.load(Ordering::Relaxed) || record.stop.load(Ordering::Relaxed) {
            return ControlExit::Stopped;
        }
        if record.bye_requested.load(Ordering::Relaxed) {
            let sent = cipher.send(
                &mut stream,
                &ControlMessage::Bye {
                    reason: "user disconnected".into(),
                },
            );
            // A Windows close can abort a socket that still has unread data,
            // causing the peer to miss an otherwise-flushed final frame.
            // Keep the control stream alive until the peer echoes Bye (or a
            // bounded timeout expires), so graceful disconnect is observable
            // without weakening the reconnect grace period for real drops.
            if sent.is_ok() {
                let deadline = Instant::now() + Duration::from_secs(1);
                loop {
                    match cipher.receive(&mut stream) {
                        Ok(ControlMessage::Bye { .. }) => break,
                        Ok(_) => {}
                        Err(error) if is_timeout(&error) && Instant::now() < deadline => {}
                        Err(_) => break,
                    }
                    if Instant::now() >= deadline {
                        break;
                    }
                }
            }
            return ControlExit::Stopped;
        }
        if let Err(error) = flush_pending_direction_offer(&record, &mut stream, &mut cipher) {
            return ControlExit::Dropped(format!("direction offer could not be sent: {error}"));
        }
        if let Err(error) = flush_pending_flow_offer(&record, &mut stream, &mut cipher) {
            return ControlExit::Dropped(format!("flow offer could not be sent: {error}"));
        }
        if let Some(resolution) = inner.take_trusted_enrollment(record.id) {
            if resolution.accepted {
                // The embedding has already committed the secret to durable
                // storage before it can mark this transaction accepted. Only
                // now import it into the live map and acknowledge the client.
                inner.remember_trusted_peer(resolution.peer_id, resolution.secret);
                if let Ok(mut enrolled) = record.trust_secret.lock() {
                    *enrolled = Some(resolution.secret);
                }
                if cipher
                    .send(&mut stream, &ControlMessage::TrustAccepted {})
                    .is_err()
                {
                    return ControlExit::Dropped("control channel closed".into());
                }
            } else if cipher
                .send(
                    &mut stream,
                    &ControlMessage::TrustRejected {
                        reason: resolution
                            .reason
                            .unwrap_or_else(|| "trusted enrollment rejected".into()),
                    },
                )
                .is_err()
            {
                return ControlExit::Dropped("control channel closed".into());
            }
        }
        match cipher.receive(&mut stream) {
            Ok(ControlMessage::Bye { reason }) => {
                let _ = cipher.send(
                    &mut stream,
                    &ControlMessage::Bye {
                        reason: "bye acknowledged".into(),
                    },
                );
                return ControlExit::PeerBye(reason);
            }
            Ok(ControlMessage::TrustEnroll { peer_id, secret }) => {
                let rejected = if !inner.config().trust_new_peers {
                    Some("this host requires explicit PIN pairing".to_string())
                } else if peer_id != record.peer.id {
                    Some("trusted peer identity did not match the session".to_string())
                } else {
                    match hex_decode(&secret)
                        .ok()
                        .and_then(|bytes| <[u8; 32]>::try_from(bytes.as_slice()).ok())
                    {
                        Some(secret) => {
                            match inner.begin_trusted_enrollment(
                                record.id,
                                peer_id,
                                record.peer.clone(),
                                secret,
                            ) {
                                Ok(transaction_id) => {
                                    inner.emit(RelayEvent::TrustedPeerEnrollmentRequested {
                                        transaction_id,
                                        peer_id: record.peer.id.clone(),
                                        peer: record.peer.clone(),
                                    });
                                    None
                                }
                                Err(reason) => Some(reason),
                            }
                        }
                        None => Some("trusted credential was malformed".to_string()),
                    }
                };
                if let Some(reason) = rejected {
                    let _ = cipher.send(&mut stream, &ControlMessage::TrustRejected { reason });
                }
                last_seen = Instant::now();
            }
            Ok(ControlMessage::DirectionOffer {
                generation,
                direction,
                device_id,
            }) => {
                let offer = DirectionOffer {
                    generation,
                    direction,
                    device_id,
                };
                match apply_direction_offer(&record, &mut stream, &mut cipher, offer) {
                    Ok(Some(resolution)) => {
                        emit_direction_resolution(&inner, record.id, resolution)
                    }
                    Ok(None) => {}
                    Err(reason) => {
                        return ControlExit::Dropped(format!(
                            "invalid direction negotiation message: {reason}"
                        ));
                    }
                }
                last_seen = Instant::now();
            }
            Ok(ControlMessage::DirectionAck {
                generation,
                direction,
            }) => {
                let resolution = record.receive_direction_ack(DirectionAck {
                    generation,
                    direction,
                });
                if let Some(resolution) = resolution {
                    emit_direction_resolution(&inner, record.id, resolution);
                }
                last_seen = Instant::now();
            }
            Ok(ControlMessage::FlowOffer {
                generation,
                emitter_id,
                proposer_id,
            }) => {
                let offer = FlowOffer {
                    generation,
                    emitter_id,
                    proposer_id,
                };
                match apply_flow_offer(&record, &mut stream, &mut cipher, offer) {
                    Ok(Some(resolution)) => emit_flow_resolution(&inner, &record, resolution),
                    Ok(None) => {}
                    Err(reason) => {
                        return ControlExit::Dropped(format!(
                            "invalid flow negotiation message: {reason}"
                        ));
                    }
                }
                last_seen = Instant::now();
            }
            Ok(ControlMessage::FlowAck {
                generation,
                emitter_id,
            }) => {
                if let Some(resolution) = record.receive_flow_ack(FlowAck {
                    generation,
                    emitter_id,
                }) {
                    emit_flow_resolution(&inner, &record, resolution);
                }
                last_seen = Instant::now();
            }
            Ok(_) => last_seen = Instant::now(),
            Err(error) if is_timeout(&error) => {}
            Err(_) => return ControlExit::Dropped("control channel closed".into()),
        }
        let now = Instant::now();
        if now.duration_since(last_seen) > SESSION_TIMEOUT {
            return ControlExit::Dropped("keepalive timeout".into());
        }
        if now.duration_since(last_keepalive) >= KEEPALIVE_INTERVAL {
            if cipher
                .send(&mut stream, &ControlMessage::Keepalive {})
                .is_err()
            {
                return ControlExit::Dropped("control channel closed".into());
            }
            last_keepalive = now;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
