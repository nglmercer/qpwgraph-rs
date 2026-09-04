//! Authenticating a peer: PIN/PAKE pairing, trusted-credential hello, and
//! enrolling a credential once pairing succeeds.

use super::*;

pub(super) fn fresh_trust_secret() -> [u8; 32] {
    let mut secret = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut secret);
    secret
}

/// Everything a `TrustedHello` frame carries that the trusted reconnect path
/// actually needs. The codec parameters the frame also advertises are not
/// kept here on purpose: `host_session_after_auth` re-negotiates them from
/// the sealed `SessionStart` frame so a trusted reconnect gets exactly the
/// same validation as a PIN pairing.
pub(super) struct TrustedHelloContext {
    /// Stable peer identity the client claims; must match an enrolled
    /// trusted credential before anything else happens.
    pub(super) client_id: String,
    pub(super) peer_name: String,
    pub(super) peer_kind: DeviceKind,
    /// Host identity the client believes it is reconnecting to.
    pub(super) host_id: String,
    pub(super) transport: String,
    /// Client half of the resume challenge, hex encoded.
    pub(super) client_nonce: String,
}

pub(super) fn trusted_peer_thread(
    inner: &Arc<EngineInner>,
    mut stream: TcpStream,
    peer_addr: SocketAddr,
    control_port: u16,
    hello: TrustedHelloContext,
) {
    let TrustedHelloContext {
        client_id,
        peer_name,
        peer_kind,
        host_id,
        transport,
        client_nonce,
    } = hello;
    let config = inner.config();
    if host_id != config.device_id {
        let _ = write_frame(
            &mut stream,
            &ControlMessage::PairFail {
                reason: "trusted host identity did not match".into(),
            },
        );
        return;
    }
    let Some(secret) = inner.trusted_secret(&client_id) else {
        let _ = write_frame(
            &mut stream,
            &ControlMessage::PairFail {
                reason: "peer is not trusted on this host".into(),
            },
        );
        return;
    };
    let Some(client_nonce) = decode_resume_nonce(&client_nonce) else {
        return;
    };
    let id = inner.next_session_id();
    let server_nonce = fresh_resume_nonce();
    if write_frame(
        &mut stream,
        &ControlMessage::TrustedChallenge {
            server_nonce: hex_encode(&server_nonce),
            session_id: id.0,
            host_id: config.device_id.clone(),
            host_name: config.device_name.clone(),
        },
    )
    .is_err()
    {
        return;
    }
    let proof = match read_frame(&mut stream) {
        Ok(ControlMessage::TrustedProof { proof }) => hex_decode(&proof).ok(),
        _ => None,
    };
    let valid = proof.as_deref().is_some_and(|proof| {
        crate::crypto::verify_trusted_proof(
            &secret,
            &client_id,
            &config.device_id,
            id.0,
            &client_nonce,
            &server_nonce,
            proof,
        )
    });
    if !valid {
        let _ = write_frame(
            &mut stream,
            &ControlMessage::PairFail {
                reason: "trusted authentication failed".into(),
            },
        );
        return;
    }
    let keys = crate::crypto::trusted_session_keys(
        &secret,
        &client_id,
        &config.device_id,
        id.0,
        &client_nonce,
        &server_nonce,
        Side::Host,
    );
    if write_frame(&mut stream, &ControlMessage::TrustedOk {}).is_err() {
        return;
    }
    let audio_over_tcp = transport.eq_ignore_ascii_case("adb");
    host_session_after_auth(
        Arc::clone(inner),
        stream,
        peer_addr,
        HostAuthenticatedPeer {
            peer_id: client_id,
            peer_name,
            peer_kind,
            keys,
            audio_over_tcp,
            control_port,
            requested_id: Some(id),
        },
    );
}

/// Run the host's half of the SPAKE2 exchange and both key confirmations.
///
/// Returns the derived keys only when the client proved it holds the same
/// PIN. A mismatch is recorded against the source address so repeated
/// guessing runs into the lockout.
pub(super) fn host_pake_exchange(
    inner: &Arc<EngineInner>,
    stream: &mut TcpStream,
    peer_addr: SocketAddr,
    client_pake: &str,
    host_name: String,
    host_id: String,
) -> Option<SessionKeys> {
    let Ok(client_message) = hex_decode(client_pake) else {
        return None;
    };
    let pin = inner.config().pin;
    let host = pake_start(Side::Host, &pin);
    let host_message = host.message.clone();
    if write_frame(
        stream,
        &ControlMessage::Challenge {
            protocol: PROTOCOL_VERSION as u32,
            pake: hex_encode(&host_message),
            host_name,
            device_id: host_id,
        },
    )
    .is_err()
    {
        return None;
    }
    let keys = match host.finish(&client_message) {
        Ok(keys) => keys,
        Err(_) => {
            // A malformed SPAKE2 message is as much a failed attempt as a
            // wrong PIN, and must count against the same budget.
            reject_pairing(inner, stream, peer_addr.ip(), "pairing exchange failed");
            return None;
        }
    };
    let confirm = match read_frame(stream) {
        Ok(ControlMessage::Pair { confirm, .. }) => confirm,
        _ => return None,
    };
    let Ok(confirm) = hex_decode(&confirm) else {
        reject_pairing(inner, stream, peer_addr.ip(), "PIN did not match");
        return None;
    };
    if !keys.verify_confirmation(&confirm) {
        reject_pairing(inner, stream, peer_addr.ip(), "PIN did not match");
        return None;
    }
    if write_frame(
        stream,
        &ControlMessage::PairConfirm {
            confirm: hex_encode(&keys.confirmation()),
        },
    )
    .is_err()
    {
        return None;
    }
    Some(keys)
}

pub(super) fn reject_pairing(
    inner: &Arc<EngineInner>,
    stream: &mut TcpStream,
    addr: IpAddr,
    reason: &str,
) {
    inner.note_pairing_failure(addr);
    let _ = write_frame(
        stream,
        &ControlMessage::PairFail {
            reason: reason.into(),
        },
    );
}

/// Ask the host to retain the credential generated by an explicit PIN
/// pairing. The embedding is notified only after the authenticated host
/// acknowledges the write; otherwise it could persist a credential that the
/// host never actually accepted and every later auto-connect would fail.
///
/// The wait spans the host embedding's whole decision window (a UI may hold
/// the accept/decline dialog open for many seconds), so keepalives are sent
/// here too: without them the host's session timeout would tear down an
/// otherwise healthy session while its own user is still deciding.
pub(super) fn enroll_trusted_peer(
    inner: &Arc<EngineInner>,
    record: &Arc<SessionRecord>,
    stream: &mut TcpStream,
    cipher: &mut ControlCipher,
    local_peer_id: &str,
    secret: [u8; 32],
) {
    if cipher
        .send(
            stream,
            &ControlMessage::TrustEnroll {
                peer_id: local_peer_id.to_owned(),
                secret: hex_encode(&secret),
            },
        )
        .is_err()
    {
        return;
    }
    if flush_pending_direction_offer(record, stream, cipher).is_err() {
        return;
    }
    if flush_pending_flow_offer(record, stream, cipher).is_err() {
        return;
    }
    // The handshake's 5s read timeout would let keepalive sends drift to the
    // edge of the host's 6s session timeout; poll finely so they keep the
    // cadence the control loop normally maintains. The control loop sets its
    // own timeout when this wait ends, stretched or not.
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let deadline = Instant::now() + ENROLLMENT_ACK_TIMEOUT;
    let mut last_keepalive = Instant::now();
    while Instant::now() < deadline {
        // A disconnect during the wait must end the wait immediately: the
        // control loop that would otherwise deliver `bye` only runs after
        // this returns, and a keepalive-fed host would never time out.
        if record.bye_requested.load(Ordering::Relaxed)
            || record.stop.load(Ordering::Relaxed)
            || !inner.session_alive(record.id)
        {
            return;
        }
        // A direction switch may be requested while the embedding is still
        // deciding whether to persist the freshly enrolled credential. Keep
        // the same queued-offer/retry contract as the normal control watcher.
        if flush_pending_direction_offer(record, stream, cipher).is_err() {
            return;
        }
        if flush_pending_flow_offer(record, stream, cipher).is_err() {
            return;
        }
        match cipher.receive(stream) {
            Ok(ControlMessage::TrustAccepted {}) => {
                inner.emit(RelayEvent::TrustedPeerAvailable {
                    peer_id: record.peer.id.clone(),
                    peer: record.peer.clone(),
                    secret,
                });
                return;
            }
            Ok(ControlMessage::TrustRejected { .. }) => return,
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
                match apply_direction_offer(record, stream, cipher, offer) {
                    Ok(Some(resolution)) => emit_direction_resolution(inner, record.id, resolution),
                    Ok(None) => {}
                    Err(_) => return,
                }
            }
            Ok(ControlMessage::DirectionAck {
                generation,
                direction,
            }) => {
                if let Some(resolution) = record.receive_direction_ack(DirectionAck {
                    generation,
                    direction,
                }) {
                    emit_direction_resolution(inner, record.id, resolution);
                }
            }
            Ok(ControlMessage::FlowOffer {
                generation,
                emitter_id,
                proposer_id,
            }) => {
                if let Ok(Some(resolution)) = apply_flow_offer(
                    record,
                    stream,
                    cipher,
                    FlowOffer {
                        generation,
                        emitter_id,
                        proposer_id,
                    },
                ) {
                    emit_flow_resolution(inner, record, resolution);
                }
            }
            Ok(ControlMessage::FlowAck {
                generation,
                emitter_id,
            }) => {
                if let Some(resolution) = record.receive_flow_ack(FlowAck {
                    generation,
                    emitter_id,
                }) {
                    emit_flow_resolution(inner, record, resolution);
                }
            }
            // Keepalive is legal immediately after SessionReady. It is a
            // one-way liveness hint, so consuming it while waiting for the
            // enrollment acknowledgement does not require a response.
            Ok(_) => {}
            Err(error) if is_timeout(&error) => {}
            Err(_) => return,
        }
        let now = Instant::now();
        if now.duration_since(last_keepalive) >= KEEPALIVE_INTERVAL {
            if cipher.send(stream, &ControlMessage::Keepalive {}).is_err() {
                return;
            }
            last_keepalive = now;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
