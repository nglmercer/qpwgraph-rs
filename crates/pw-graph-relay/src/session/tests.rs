use super::*;
use crate::audio::{seal_datagram, AudioHeader};
use crate::crypto::{pake_start, Side};
use crate::PAIRING_ATTEMPT_LIMIT;
use crate::RelayDirection;
use std::net::{IpAddr, TcpListener, TcpStream};

fn reject_worker_spawn(
    _name: String,
    _worker: Worker,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    Err(std::io::Error::other("worker spawn rejected by test"))
}

#[test]
fn outgoing_worker_spawn_failure_emits_session_lost() {
    let inner = EngineInner::new(crate::EngineConfig::default());
    let id = SessionId(7_001);
    connect_peer_with_spawner(
        &inner,
        id,
        "127.0.0.1:48123".parse().unwrap(),
        "123456".into(),
        Roles::emit_only(),
        reject_worker_spawn,
    );

    assert!(matches!(
        inner.drain_events().as_slice(),
        [RelayEvent::SessionLost { id: lost, reason }] if *lost == id
            && reason.contains("could not start relay connection worker")
    ));
}

#[test]
fn abandoned_valid_usb_probes_do_not_consume_pairing_attempts() {
    let inner = EngineInner::new(crate::EngineConfig {
        pin: "123456".into(),
        ..crate::EngineConfig::default()
    });
    let peer_ip = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

    for _ in 0..(PAIRING_ATTEMPT_LIMIT + 2) {
        let listener =
            TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).expect("probe test listener");
        let target = listener.local_addr().expect("probe test address");
        let server_inner = Arc::clone(&inner);
        let client_pake = pake_start(Side::Client, "123456");
        let client_message = hex_encode(&client_pake.message);
        let server = std::thread::spawn(move || {
            let (mut stream, peer) = listener.accept().expect("probe connects");
            host_pake_exchange(
                &server_inner,
                &mut stream,
                peer,
                &client_message,
                "probe-host".into(),
                "probe-host-id".into(),
            )
        });

        let mut client = TcpStream::connect(target).expect("probe TCP connection");
        assert!(matches!(
            read_frame(&mut client),
            Ok(ControlMessage::Challenge { .. })
        ));
        // A discovery probe intentionally stops here. It sent a valid
        // SPAKE2 message and must not be treated as an incorrect PIN.
        drop(client);
        assert!(server.join().expect("probe worker exits").is_none());
    }

    assert!(inner.pairing_allowed(peer_ip));
    assert!(inner.pairing_failures.lock().unwrap().is_empty());
}

fn audio_keys() -> (crate::crypto::Sealer, crate::crypto::Opener) {
    let client = pake_start(Side::Client, "123456");
    let host = pake_start(Side::Host, "123456");
    let client_message = client.message.clone();
    let host_message = host.message.clone();
    let client_keys = client.finish(&host_message).expect("client pairs");
    let host_keys = host.finish(&client_message).expect("host pairs");
    let (sealer, _) = client_keys.audio_channel().expect("client audio keys");
    let (_, opener) = host_keys.audio_channel().expect("host audio keys");
    (sealer, opener)
}

fn resumable_session(id: u64) -> Arc<SessionRecord> {
    resumable_session_with_udp(id, None)
}

fn resumable_session_with_udp(id: u64, udp_audio: Option<Arc<UdpAudioSlot>>) -> Arc<SessionRecord> {
    let client = pake_start(Side::Client, "123456");
    let host = pake_start(Side::Host, "123456");
    let client_message = client.message.clone();
    let host_message = host.message.clone();
    let client_keys = client.finish(&host_message).expect("client pairs");
    let host_keys = host.finish(&client_message).expect("host pairs");
    let (audio_sealer, _) = client_keys.audio_channel().expect("audio sealer");
    let (_, audio_opener) = host_keys.audio_channel().expect("audio opener");
    let format = AudioFormat::new(48_000, 1, 10);
    let capture_converter =
        Converter::with_capacity(48_000, 1, 48_000, 1, crate::MAX_REALTIME_QUANTUM_SAMPLES);
    let capture_destination = Vec::with_capacity(
        capture_converter.output_capacity_for(crate::MAX_REALTIME_QUANTUM_SAMPLES),
    );
    Arc::new(SessionRecord {
        id: SessionId(id),
        wire_id: id,
        peer: PeerInfo {
            id: "resume-peer-id".into(),
            name: "resume-peer".into(),
            kind: DeviceKind::Other,
            addr: "127.0.0.1:1".parse().expect("peer address"),
        },
        roles: Roles::both(),
        codec: CodecKind::Pcm,
        format,
        sending: true,
        receiving: true,
        stop: Arc::new(AtomicBool::new(false)),
        bye_requested: AtomicBool::new(false),
        control_generation: AtomicU64::new(1),
        resume_secret: client_keys.resume_auth_key(),
        trust_secret: Mutex::new(None),
        tcp_audio: None,
        udp_audio,
        control_peer_addr: Mutex::new("127.0.0.1:1".parse().unwrap()),
        control_state: Mutex::new(ControlState::Active),
        peer_audio_addr: Mutex::new(None),
        outgoing: crate::PcmQueue::new(crate::DEFAULT_QUEUE_CAPACITY),
        incoming: crate::PcmQueue::new(crate::DEFAULT_QUEUE_CAPACITY),
        capture_convert: Mutex::new((capture_converter, capture_destination)),
        audio_sealer: Mutex::new(audio_sealer),
        audio_opener: Mutex::new(audio_opener),
        direction: Mutex::new(DirectionNegotiation::new(DirectionOffer {
            generation: 1,
            direction: RelayDirection::MobileToDesktop,
            device_id: "resume-peer-id".into(),
        })),
    })
}

#[test]
fn resuming_on_the_same_link_keeps_the_existing_audio_socket() {
    // ADB pins the host bind address to loopback, so the interface
    // selected for the resume is exactly the one the socket already
    // holds. Rebinding it would ask the kernel for an address this very
    // session still owns and fail with EADDRINUSE.
    let config = crate::EngineConfig {
        transport: crate::TransportPreference::Adb,
        ..crate::EngineConfig::default()
    };
    let inner = EngineInner::new(config);
    let target: SocketAddr = "127.0.0.1:48123".parse().expect("target");
    let socket = bind_udp_audio_socket(&inner, target, true).expect("audio socket");
    let slot = UdpAudioSlot::new(socket).expect("audio slot");
    let before = slot.local_addr().expect("bound address");
    let record = resumable_session_with_udp(7_050, Some(Arc::clone(&slot)));

    migrate_udp_audio_socket(&inner, &record, target, true).expect("resume keeps the socket");

    assert_eq!(slot.local_addr(), Some(before));
}

/// The real binder used by `migrate_udp_audio_socket`, reproduced so the
/// lifecycle tests exercise the same kernel behaviour without depending
/// on netlink interface discovery.
fn real_udp_binder(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    let socket = UdpSocket::bind(addr)?;
    tune_audio_socket(&socket);
    Ok(socket)
}

#[test]
fn migrating_a_wildcard_socket_to_a_specific_address_keeps_the_port() {
    // Regression: `take_current` only unlinked the `Arc` from the slot.
    // Worker leases kept the wildcard socket open, so binding
    // `127.0.0.1:PORT` while `0.0.0.0:PORT` was still alive failed with
    // EADDRINUSE and the host lost its negotiated audio port.
    let slot =
        UdpAudioSlot::new(UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("wildcard socket"))
            .expect("audio slot");
    let port = slot.local_addr().expect("bound address").port();

    migrate_udp_slot(&slot, Ipv4Addr::LOCALHOST, Some(port), &real_udp_binder)
        .expect("wildcard migrates onto a specific address");

    let after = slot.local_addr().expect("migrated address");
    assert_eq!(after.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(after.port(), port);
}

#[test]
fn migrating_a_wildcard_socket_waits_for_outstanding_worker_leases() {
    // A worker holding a lease across its bounded `recv_from` must not
    // make the migration bind against a still-open wildcard socket.
    let slot = Arc::new(
        UdpAudioSlot::new(UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("wildcard socket"))
            .expect("audio slot"),
    );
    let port = slot.local_addr().expect("bound address").port();

    let lease = slot.current().expect("worker leases the socket");
    let holder = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(120));
        drop(lease);
    });

    migrate_udp_slot(&slot, Ipv4Addr::LOCALHOST, Some(port), &real_udp_binder)
        .expect("migration drains the lease and rebinds");
    holder.join().expect("lease holder finishes");

    let after = slot.local_addr().expect("migrated address");
    assert_eq!(after.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(after.port(), port);
}

#[test]
fn migrating_back_to_a_wildcard_socket_keeps_the_port() {
    // The reverse direction collides just as hard: `0.0.0.0:PORT` cannot
    // be bound while `127.0.0.1:PORT` is still open.
    let slot =
        UdpAudioSlot::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("specific socket"))
            .expect("audio slot");
    let port = slot.local_addr().expect("bound address").port();

    migrate_udp_slot(&slot, Ipv4Addr::UNSPECIFIED, Some(port), &real_udp_binder)
        .expect("specific address migrates back onto the wildcard");

    let after = slot.local_addr().expect("migrated address");
    assert!(after.ip().is_unspecified());
    assert_eq!(after.port(), port);
}

#[test]
fn a_failed_migration_keeps_a_wildcard_socket_on_the_negotiated_port() {
    // Rollback: once the old wildcard socket has been closed there is
    // nothing to put back, so the slot must be refilled on the *same*
    // negotiated port, and the caller must still learn that the move
    // failed. The session stays usable, so this is recoverable.
    let slot =
        UdpAudioSlot::new(UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("wildcard socket"))
            .expect("audio slot");
    let port = slot.local_addr().expect("bound address").port();

    // Injected failure for the desired address only; the wildcard
    // fallback still succeeds, exactly as a vanished interface behaves.
    let binder = |addr: SocketAddr| -> std::io::Result<UdpSocket> {
        if !addr.ip().is_unspecified() {
            return Err(std::io::Error::from(std::io::ErrorKind::AddrNotAvailable));
        }
        real_udp_binder(addr)
    };

    let error = migrate_udp_slot(&slot, Ipv4Addr::LOCALHOST, Some(port), &binder)
        .expect_err("the desired address cannot be bound");
    assert!(
        !error.is_fatal(),
        "the negotiated port was kept, so the session survives"
    );

    let after = slot.local_addr().expect("the slot is never left empty");
    assert!(after.ip().is_unspecified());
    assert_eq!(after.port(), port);
}

#[test]
fn a_host_migration_that_cannot_reopen_the_negotiated_port_is_fatal() {
    // Regression: the rollback used to fall back to `0.0.0.0:0` and
    // install that ephemeral port. The peer keeps sending to the port it
    // negotiated — there is no port renegotiation during a resume — so
    // the session would have kept a live control link with permanently
    // black-holed audio. The migration must fail fatally instead, and it
    // must never install an ephemeral port behind the caller's back.
    let slot =
        UdpAudioSlot::new(UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("wildcard socket"))
            .expect("audio slot");
    let port = slot.local_addr().expect("bound address").port();

    // Every bind on the negotiated port fails; only an ephemeral port
    // would succeed. That ephemeral escape hatch must not be taken.
    let binder = move |addr: SocketAddr| -> std::io::Result<UdpSocket> {
        if addr.port() == port {
            return Err(std::io::Error::from(std::io::ErrorKind::AddrNotAvailable));
        }
        real_udp_binder(addr)
    };

    let error = migrate_udp_slot(&slot, Ipv4Addr::LOCALHOST, Some(port), &binder)
        .expect_err("neither the desired address nor the negotiated port can be bound");
    assert!(
        error.is_fatal(),
        "losing the negotiated port must be reported as fatal, not papered over"
    );
    assert!(
        slot.local_addr().is_none(),
        "the slot must not hold an unannounced ephemeral port: {:?}",
        slot.local_addr()
    );
}

#[cfg(unix)]
#[test]
fn losing_the_negotiated_port_to_another_socket_is_fatal() {
    // The same failure with real sockets and no injection: a foreign
    // socket on 127.0.0.2:PORT blocks both the desired specific bind and
    // the wildcard rollback on that port once the old socket is closed.
    let slot = UdpAudioSlot::new(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("host socket"))
        .expect("audio slot");
    let port = slot.local_addr().expect("bound address").port();
    let Ok(_squatter) = UdpSocket::bind((Ipv4Addr::new(127, 0, 0, 2), port)) else {
        // A platform without the whole 127.0.0.0/8 loopback range cannot
        // stage this collision; the injected-binder test above covers the
        // same path deterministically everywhere.
        return;
    };

    let error = migrate_udp_slot(&slot, Ipv4Addr::UNSPECIFIED, Some(port), &real_udp_binder)
        .expect_err("the negotiated port is held by another socket");
    assert!(error.is_fatal());
    assert!(slot.local_addr().is_none());
}

#[test]
fn a_resume_whose_audio_port_is_lost_is_rejected_instead_of_acknowledged() {
    // End-to-end: a host resume that cannot restore its negotiated UDP
    // port must not answer `ResumeOk`. Doing so would report a healthy
    // session to the client while its audio went nowhere. The host sends
    // `PairFail` and tears the session down so the peer negotiates afresh.
    // Force a specific loopback bind so this test exercises migration even
    // on hosts whose interface inventory would make Auto keep the wildcard.
    let inner = EngineInner::new(crate::EngineConfig {
        transport: crate::TransportPreference::Adb,
        ..crate::EngineConfig::default()
    });
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("wildcard socket");
    let slot = UdpAudioSlot::new(socket).expect("audio slot");
    let audio_port = slot.local_addr().expect("bound address").port();
    let record = resumable_session_with_udp(7_060, Some(Arc::clone(&slot)));
    assert!(inner.insert_session(Arc::clone(&record)));
    assert!(record.mark_control_dropped());

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("resume listener");
    let target = listener.local_addr().expect("resume address");
    let server_inner = Arc::clone(&inner);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("resume client connects");
        let ControlMessage::ResumeHello {
            session_id,
            client_nonce,
        } = read_frame(&mut stream).expect("resume hello")
        else {
            panic!("resume client sent the wrong first message");
        };
        // Refuse only the negotiated port. An ephemeral bind would
        // still succeed, which is precisely the escape hatch that used to
        // hand the client a silent session.
        let binder = move |addr: SocketAddr| -> std::io::Result<UdpSocket> {
            if addr.port() == audio_port {
                return Err(std::io::Error::from(std::io::ErrorKind::AddrNotAvailable));
            }
            bind_audio_socket_at(addr)
        };
        resume_peer_session_with(
            &server_inner,
            SessionId(session_id),
            stream,
            &client_nonce,
            &binder,
        );
    });

    let resumed = resume_client_control(&inner, &record, target);
    server.join().expect("host resume worker finishes");

    assert!(
        resumed.is_none(),
        "the client must not see this resume succeed"
    );
    assert!(
        !inner.session_alive(record.id),
        "the host must tear down a session whose audio endpoint is gone"
    );
    assert!(
        slot.local_addr().is_none(),
        "no ephemeral port may be installed behind the peer's back"
    );
}

#[test]
fn a_migration_that_cannot_drain_restores_the_live_socket() {
    // If worker leases outlive the migration window the old socket is
    // still usable, so it must go back into the slot instead of being
    // closed on a guess.
    let slot =
        UdpAudioSlot::new(UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("wildcard socket"))
            .expect("audio slot");
    let before = slot.local_addr().expect("bound address");
    let lease = slot.current().expect("worker leases the socket");

    let taken = slot
        .take_exclusive(Duration::from_millis(20))
        .expect("the slot was populated");
    let still_leased = taken.expect_err("an outstanding lease blocks the take");
    slot.restore(still_leased);
    drop(lease);

    assert_eq!(slot.local_addr(), Some(before));
}

#[test]
fn migrating_between_two_specific_addresses_does_not_close_the_old_socket() {
    // Two different specific addresses do not contend for the port, so
    // the old socket may stay open while the new one is installed.
    let old: SocketAddr = "127.0.0.1:0".parse().expect("address");
    assert!(!udp_binds_collide(old, Ipv4Addr::new(127, 0, 0, 2), 40_000));
    let bound: SocketAddr = "127.0.0.1:40000".parse().expect("address");
    assert!(!udp_binds_collide(
        bound,
        Ipv4Addr::new(127, 0, 0, 2),
        40_000
    ));
    assert!(udp_binds_collide(
        "0.0.0.0:40000".parse().expect("address"),
        Ipv4Addr::new(127, 0, 0, 2),
        40_000
    ));
    assert!(udp_binds_collide(bound, Ipv4Addr::UNSPECIFIED, 40_000));
    // A client migration takes a fresh ephemeral port and never collides.
    assert!(!udp_binds_collide(bound, Ipv4Addr::UNSPECIFIED, 0));
}

#[test]
fn original_session_owner_can_resume_over_the_challenge_flow() {
    let inner = EngineInner::new(crate::EngineConfig::default());
    let record = resumable_session(7_004);
    assert!(inner.insert_session(Arc::clone(&record)));
    assert!(record.mark_control_dropped());

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("resume listener");
    let target = listener.local_addr().expect("resume address");
    let server_inner = Arc::clone(&inner);
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("resume client connects");
        let first = read_frame(&mut stream).expect("resume hello");
        let ControlMessage::ResumeHello {
            session_id,
            client_nonce,
        } = first
        else {
            panic!("resume client sent the wrong first message");
        };
        resume_peer_session(&server_inner, SessionId(session_id), stream, &client_nonce);
    });

    let (stream, cipher, _) = resume_client_control(&inner, &record, target)
        .expect("the original owner proves the session secret");
    assert_eq!(cipher.sealer.next_counter(), 0);
    assert_eq!(
        *record.control_state.lock().expect("control state"),
        ControlState::Active
    );
    drop(stream);
    teardown(&inner, record.id, "resume test complete".into());
    server.join().expect("resume worker exits");
    assert_eq!(record.control_generation.load(Ordering::Acquire), 2);
    assert!(!inner.session_alive(record.id));
}

#[test]
fn unauthenticated_resume_pairfail_tries_the_next_peer_address() {
    let inner = EngineInner::new(crate::EngineConfig::default());
    let record = resumable_session(7_008);
    assert!(inner.insert_session(Arc::clone(&record)));
    assert!(record.mark_control_dropped());

    // Bind two loopback addresses to the same port. The original target
    // is the malicious candidate; the discovered address is the real
    // host and is intentionally ranked later for this test.
    let attacker = TcpListener::bind(("127.0.0.2", 0)).unwrap();
    let port = attacker.local_addr().unwrap().port();
    let legitimate = TcpListener::bind(("127.0.0.1", port)).unwrap();
    let attacker_target = attacker.local_addr().unwrap();
    let legitimate_target = legitimate.local_addr().unwrap();

    let attacker_thread = std::thread::spawn(move || {
        let (mut stream, _) = attacker.accept().unwrap();
        let _ = read_frame(&mut stream).unwrap();
        write_frame(
            &mut stream,
            &ControlMessage::PairFail {
                reason: "unauthenticated candidate rejection".into(),
            },
        )
        .unwrap();
    });

    let server_inner = Arc::clone(&inner);
    let legitimate_thread = std::thread::spawn(move || {
        let (mut stream, _) = legitimate.accept().unwrap();
        let ControlMessage::ResumeHello {
            session_id,
            client_nonce,
        } = read_frame(&mut stream).unwrap()
        else {
            panic!("legitimate candidate received the wrong message");
        };
        resume_peer_session(&server_inner, SessionId(session_id), stream, &client_nonce);
    });

    let handle = crate::RelayHandle {
        inner: Arc::clone(&inner),
    };
    handle.update_discovered_peer_candidates(vec![(
        PeerInfo {
            id: record.peer.id.clone(),
            name: record.peer.name.clone(),
            kind: DeviceKind::Other,
            addr: legitimate_target,
        },
        Some(crate::LinkKind::Lan),
    )]);

    let (stream, _cipher, resumed_target) =
        resume_client_control(&inner, &record, attacker_target).expect("fallback resumes");
    assert_eq!(resumed_target, legitimate_target);
    assert_eq!(
        inner.last_successful_address(&record.peer.id),
        Some(legitimate_target)
    );
    assert!(!inner.candidate_allowed(&record.peer.id, attacker_target));

    drop(stream);
    teardown(&inner, record.id, "resume candidate test complete".into());
    attacker_thread.join().unwrap();
    legitimate_thread.join().unwrap();
}

#[test]
fn clear_trusted_ok_without_sealed_setup_does_not_learn_candidate() {
    let inner = EngineInner::new(crate::EngineConfig {
        device_id: "client-id".into(),
        ..crate::EngineConfig::default()
    });
    let target_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let target = target_listener.local_addr().unwrap();
    let host_id = "trusted-host".to_string();
    let secret = [0x27; 32];
    let server = std::thread::spawn({
        let host_id = host_id.clone();
        move || {
            let (mut stream, _) = target_listener.accept().unwrap();
            let (client_nonce, roles) = match read_frame(&mut stream).unwrap() {
                ControlMessage::TrustedHello {
                    client_nonce,
                    roles,
                    ..
                } => (decode_resume_nonce(&client_nonce).unwrap(), roles),
                message => panic!("unexpected trusted hello: {message:?}"),
            };
            let server_nonce = [0x28; RESUME_NONCE_LEN];
            write_frame(
                &mut stream,
                &ControlMessage::TrustedChallenge {
                    server_nonce: hex_encode(&server_nonce),
                    session_id: 9_009,
                    host_id: host_id.clone(),
                    host_name: "fake host".into(),
                },
            )
            .unwrap();
            assert!(matches!(
                read_frame(&mut stream).unwrap(),
                ControlMessage::TrustedProof { .. }
            ));
            write_frame(&mut stream, &ControlMessage::TrustedOk {}).unwrap();
            // The fake candidate cannot produce the sealed PairOk. The
            // client must reject it without recording this address.
            let _ = roles;
            drop(stream);
            let _ = client_nonce;
        }
    });

    trusted_client_thread(
        Arc::clone(&inner),
        SessionId(7_009),
        target,
        host_id.clone(),
        secret,
        Roles::emit_only(),
    );
    server.join().unwrap();
    assert_eq!(inner.last_successful_address(&host_id), None);
    assert!(matches!(
        inner.drain_events().as_slice(),
        [RelayEvent::SessionLost { id, .. }] if *id == SessionId(7_009)
    ));
}

#[test]
fn failed_resume_ok_has_no_ownerless_zombie_session() {
    let inner = EngineInner::new(crate::EngineConfig::default());
    let record = resumable_session(7_005);
    assert!(inner.insert_session(Arc::clone(&record)));

    // Model the host having committed generation 2, then losing the
    // socket before ResumeOk reached the client. The recovery helper is
    // the same one used by the real host resume path; zero deadlines keep
    // this deterministic and avoid a 15-second test.
    assert!(record.mark_control_dropped());
    let generation = record.begin_resume().expect("resume generation 2");
    assert_eq!(generation, 2);
    assert!(record.finish_resume(generation));
    assert!(matches!(
        *record.control_state.lock().expect("control state"),
        ControlState::Active
    ));

    assert!(!handle_failed_resume_ok_with_deadlines(
        &inner,
        &record,
        Duration::ZERO,
        Duration::ZERO,
    ));
    assert!(!inner.session_alive(record.id));
    assert!(matches!(
        inner.drain_events().as_slice(),
        [RelayEvent::SessionLost { id, reason }]
            if *id == record.id && reason.contains("could not deliver ResumeOk")
    ));
}

#[test]
fn failed_resume_ok_allows_one_bounded_replacement_resume() {
    let inner = EngineInner::new(crate::EngineConfig::default());
    let record = resumable_session(7_006);
    assert!(inner.insert_session(Arc::clone(&record)));
    assert!(record.mark_control_dropped());
    let generation = record.begin_resume().expect("resume generation 2");
    assert!(record.finish_resume(generation));

    let waiter_inner = Arc::clone(&inner);
    let waiter_record = Arc::clone(&record);
    let waiter = std::thread::spawn(move || {
        handle_failed_resume_ok_with_deadlines(
            &waiter_inner,
            &waiter_record,
            Duration::from_millis(250),
            Duration::from_millis(250),
        )
    });

    for _ in 0..1_000 {
        if matches!(
            *record.control_state.lock().expect("control state"),
            ControlState::ResumeEligible { generation: 2 }
        ) {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let replacement = record.begin_resume().expect("one replacement resume");
    assert_eq!(replacement, 3);
    assert!(record.finish_resume(replacement));

    assert!(waiter.join().expect("resume grace watcher exits"));
    assert!(inner.session_alive(record.id));
    assert_eq!(
        *record.control_state.lock().expect("control state"),
        ControlState::Active
    );
    teardown(&inner, record.id, "resume replacement test complete".into());
}

#[test]
fn resume_targets_follow_the_same_stable_peer_across_addresses() {
    let inner = EngineInner::new(crate::EngineConfig::default());
    let record = resumable_session(7_007);
    assert!(inner.insert_session(Arc::clone(&record)));
    let original: SocketAddr = "192.168.1.20:48123".parse().unwrap();
    let handle = crate::RelayHandle {
        inner: Arc::clone(&inner),
    };
    handle.update_discovered_peer_candidates(vec![
        (
            PeerInfo {
                id: "resume-peer-id".into(),
                name: "resume-peer".into(),
                kind: DeviceKind::Other,
                addr: "192.168.42.129:48123".parse().unwrap(),
            },
            Some(crate::LinkKind::Usb),
        ),
        (
            PeerInfo {
                id: "unrelated-peer".into(),
                name: "resume-peer".into(),
                kind: DeviceKind::Other,
                addr: "10.0.0.5:48123".parse().unwrap(),
            },
            None,
        ),
    ]);

    let targets = resume_targets(&inner, &record, original);
    assert_eq!(
        targets,
        vec!["192.168.42.129:48123".parse().unwrap(), original,]
    );
    assert!(targets.len() <= crate::MAX_TRUSTED_CANDIDATE_ADDRESSES);
    teardown(&inner, record.id, "resume target test complete".into());
}

#[test]
fn replacing_tcp_audio_closes_the_stale_forwarded_stream() {
    use std::io::Read as _;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let target = listener.local_addr().unwrap();
    let first_client = TcpStream::connect(target).unwrap();
    let (mut first_server, _) = listener.accept().unwrap();
    first_server
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();

    let slot = TcpAudioSlot::new();
    slot.install(first_client).unwrap();
    let first = slot.current().expect("first forwarded stream is installed");

    let second_client = TcpStream::connect(target).unwrap();
    let (_second_server, _) = listener.accept().unwrap();
    slot.install(second_client).unwrap();
    let second = slot.current().expect("replacement stream is installed");
    assert!(!Arc::ptr_eq(&first, &second));

    let mut byte = [0u8; 1];
    assert_eq!(
        first_server.read(&mut byte).unwrap(),
        0,
        "installing a resumed ADB stream must wake workers on the old one"
    );
}

#[test]
fn adb_audio_secondary_stream_runs_the_production_authenticated_handshake() {
    let inner = EngineInner::new(crate::EngineConfig {
        transport: crate::TransportPreference::Adb,
        ..crate::EngineConfig::default()
    });
    let record = resumable_session(7_004);
    let secret = record.resume_secret;
    let wire_id = record.wire_id;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let target = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let client_nonce = match read_frame(&mut stream).unwrap() {
            ControlMessage::AudioHello {
                session_id,
                client_nonce,
            } => {
                assert_eq!(session_id, wire_id);
                decode_resume_nonce(&client_nonce).unwrap()
            }
            message => panic!("unexpected ADB hello: {message:?}"),
        };
        let server_nonce = [0x42; RESUME_NONCE_LEN];
        write_frame(
            &mut stream,
            &ControlMessage::AudioChallenge {
                server_nonce: hex_encode(&server_nonce),
            },
        )
        .unwrap();
        let proof = match read_frame(&mut stream).unwrap() {
            ControlMessage::AudioProof { proof } => hex_decode(&proof).unwrap(),
            message => panic!("unexpected ADB proof: {message:?}"),
        };
        let expected = crate::crypto::tcp_audio_proof(
            &secret,
            wire_id,
            &client_nonce,
            &server_nonce,
            Side::Client,
        );
        assert!(bool::from(expected.ct_eq(&proof)));
        let host_proof = crate::crypto::tcp_audio_proof(
            &secret,
            wire_id,
            &client_nonce,
            &server_nonce,
            Side::Host,
        );
        write_frame(
            &mut stream,
            &ControlMessage::AudioReady {
                proof: hex_encode(&host_proof),
            },
        )
        .unwrap();
    });

    let slot = TcpAudioSlot::new();
    open_tcp_audio_once(&inner, &record, &slot, target).unwrap();
    assert!(slot.is_active());
    server.join().unwrap();
}

#[test]
fn adb_audio_wrong_host_proof_cannot_replace_the_active_slot() {
    let inner = EngineInner::new(crate::EngineConfig {
        transport: crate::TransportPreference::Adb,
        ..crate::EngineConfig::default()
    });
    let record = resumable_session(7_005);
    let wire_id = record.wire_id;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let target = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let client_nonce = match read_frame(&mut stream).unwrap() {
            ControlMessage::AudioHello { client_nonce, .. } => {
                decode_resume_nonce(&client_nonce).unwrap()
            }
            message => panic!("unexpected ADB hello: {message:?}"),
        };
        let server_nonce = [0x43; RESUME_NONCE_LEN];
        write_frame(
            &mut stream,
            &ControlMessage::AudioChallenge {
                server_nonce: hex_encode(&server_nonce),
            },
        )
        .unwrap();
        let _ = read_frame(&mut stream).unwrap();
        let wrong = [0u8; crate::crypto::CONFIRM_LEN];
        write_frame(
            &mut stream,
            &ControlMessage::AudioReady {
                proof: hex_encode(&wrong),
            },
        )
        .unwrap();
        let _ = (client_nonce, wire_id);
    });
    let slot = TcpAudioSlot::new();
    assert!(open_tcp_audio_once(&inner, &record, &slot, target).is_err());
    assert!(!slot.is_active());
    server.join().unwrap();
}

#[test]
fn adb_tx_transport_loss_clears_the_slot_as_a_recoverable_write() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let target = listener.local_addr().unwrap();
    let client = TcpStream::connect(target).unwrap();
    let (server, _) = listener.accept().unwrap();
    let slot = TcpAudioSlot::new();
    slot.install(client).unwrap();

    // Force the installed writer into the same terminal state observed
    // after BrokenPipe/ConnectionReset, without relying on peer timing.
    slot.current()
        .unwrap()
        .writer
        .lock()
        .unwrap()
        .shutdown(Shutdown::Both)
        .unwrap();
    drop(server);

    let error = send_tcp_audio_datagram(&slot, &[0x01]).expect_err("write must fail");
    assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
    assert!(!slot.is_active(), "supervisor must see an empty slot");
}

#[test]
fn adb_tcp_audio_transport_errors_are_distinct_from_fatal_errors() {
    for kind in [
        std::io::ErrorKind::BrokenPipe,
        std::io::ErrorKind::ConnectionReset,
        std::io::ErrorKind::ConnectionAborted,
        std::io::ErrorKind::NotConnected,
        std::io::ErrorKind::UnexpectedEof,
        std::io::ErrorKind::TimedOut,
        std::io::ErrorKind::WouldBlock,
    ] {
        assert!(is_recoverable_tcp_audio_error(&std::io::Error::from(kind)));
    }
    assert!(!is_recoverable_tcp_audio_error(&std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "locally invalid frame",
    )));
}

#[test]
fn adb_tx_reconnect_signal_does_not_emit_session_loss() {
    let inner = EngineInner::new(crate::EngineConfig {
        transport: crate::TransportPreference::Adb,
        ..crate::EngineConfig::default()
    });
    let record = resumable_session(7_010);
    let frame_samples = record.format.frame_samples();
    record.outgoing.push(&vec![0.0; frame_samples]);
    assert!(inner.insert_session(Arc::clone(&record)));

    let attempted = Arc::new(AtomicBool::new(false));
    let attempted_by_worker = Arc::clone(&attempted);
    let worker_inner = Arc::clone(&inner);
    let worker_record = Arc::clone(&record);
    let worker = std::thread::spawn(move || {
        run_tx_source(
            worker_inner,
            worker_record,
            None,
            || true,
            move |_| {
                attempted_by_worker.store(true, Ordering::Release);
                Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "ADB audio connection is reconnecting",
                ))
            },
        );
    });

    for _ in 0..100 {
        if attempted.load(Ordering::Acquire) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(attempted.load(Ordering::Acquire));
    assert!(inner.session_alive(record.id));
    assert!(inner.drain_events().is_empty());

    teardown(&inner, record.id, "ADB TX reconnect test complete".into());
    worker.join().unwrap();
}

#[test]
fn tcp_audio_framing_rejects_empty_and_oversized_payloads() {
    let mut output = Vec::new();
    assert!(write_tcp_audio_frame(&mut output, &[]).is_err());
    assert!(write_tcp_audio_frame(&mut output, &vec![0; MAX_DATAGRAM + 1]).is_err());
    let mut oversized = (u32::try_from(MAX_DATAGRAM + 1).unwrap())
        .to_be_bytes()
        .to_vec();
    oversized.extend_from_slice(&[0; 4]);
    let mut destination = vec![0; MAX_DATAGRAM];
    assert!(read_tcp_audio_frame(&mut &oversized[..], &mut destination).is_err());
}

#[test]
fn session_worker_spawn_failures_are_reported_and_stop_the_record() {
    let stop = AtomicBool::new(false);
    let mut reject = reject_worker_spawn;
    let error =
        spawn_worker_with_report(&mut reject, &stop, SessionId(7_002), "RX", Box::new(|| {}))
            .expect_err("RX spawn is refused");
    assert!(error.contains("RX worker"));
    assert!(stop.load(Ordering::Relaxed));

    let stop = AtomicBool::new(false);
    let mut calls = 0;
    let mut spawn = |name, worker| {
        calls += 1;
        if calls == 2 {
            reject_worker_spawn(name, worker)
        } else {
            Ok(std::thread::Builder::new()
                .name(name)
                .spawn(worker)
                .unwrap())
        }
    };
    spawn_worker_with_report(&mut spawn, &stop, SessionId(7_003), "RX", Box::new(|| {}))
        .expect("RX spawn succeeds");
    let error =
        spawn_worker_with_report(&mut spawn, &stop, SessionId(7_003), "TX", Box::new(|| {}))
            .expect_err("TX spawn is refused after RX starts");
    assert!(error.contains("TX worker"));
    assert!(stop.load(Ordering::Relaxed));
}

#[test]
fn handshake_slot_is_released_when_worker_ownership_ends() {
    let inner = EngineInner::new(crate::EngineConfig {
        max_pending_handshakes: 1,
        ..crate::EngineConfig::default()
    });
    let slot = inner.claim_handshake().expect("first slot is available");
    assert!(inner.claim_handshake().is_none());
    drop(slot);
    assert!(inner.claim_handshake().is_some());
}

/// Seal a frame with the given header metadata, then take it back apart
/// the way `run_rx` does: parse, authenticate, and only then judge the
/// header against the negotiated format.
fn authenticated_packet_is_accepted(
    header_codec: CodecKind,
    header_stereo: bool,
    negotiated_codec: CodecKind,
    negotiated: AudioFormat,
) -> bool {
    let (mut sealer, mut opener) = audio_keys();
    let datagram = seal_datagram(
        &mut sealer,
        &AudioHeader {
            stereo: header_stereo,
            keyframe: true,
            codec: header_codec,
            sequence: 0,
            timestamp_ms: 0,
        },
        &[1, 2, 3],
    )
    .expect("frame seals");
    let packet = AudioPacket::parse(&datagram).expect("parses");
    let payload = packet.open(&mut opener).expect("authenticates");
    assert!(
        !payload.is_empty(),
        "this is an audio frame, not an announce"
    );
    packet_matches_negotiation(&packet, negotiated_codec, negotiated)
}

#[test]
fn an_authenticated_packet_matching_the_negotiation_is_accepted() {
    let format = AudioFormat::new(48_000, 1, 20);
    assert!(authenticated_packet_is_accepted(
        CodecKind::Opus,
        false,
        CodecKind::Opus,
        format
    ));
    let stereo = AudioFormat::new(48_000, 2, 20);
    assert!(authenticated_packet_is_accepted(
        CodecKind::Opus,
        true,
        CodecKind::Opus,
        stereo
    ));
}

#[test]
fn an_authenticated_packet_with_the_wrong_codec_is_rejected() {
    // The peer is paired — the packet opens — but it is now claiming a
    // codec the session never agreed to. Handing that to a decoder built
    // for the negotiated codec is at best an error per packet.
    let format = AudioFormat::new(48_000, 1, 20);
    assert!(!authenticated_packet_is_accepted(
        CodecKind::Pcm,
        false,
        CodecKind::Opus,
        format
    ));
    assert!(!authenticated_packet_is_accepted(
        CodecKind::Opus,
        false,
        CodecKind::Pcm,
        format
    ));
}

#[test]
fn an_authenticated_packet_with_the_wrong_stereo_flag_is_rejected() {
    // A stereo flag that disagrees with the negotiated channel count means
    // every frame would be de-interleaved against the wrong geometry.
    let mono = AudioFormat::new(48_000, 1, 20);
    assert!(!authenticated_packet_is_accepted(
        CodecKind::Opus,
        true,
        CodecKind::Opus,
        mono
    ));
    let stereo = AudioFormat::new(48_000, 2, 20);
    assert!(!authenticated_packet_is_accepted(
        CodecKind::Opus,
        false,
        CodecKind::Opus,
        stereo
    ));
}

#[test]
fn prepared_capture_converters_are_sized_for_the_realtime_quantum() {
    // Session setup must leave nothing for `broadcast_capture` to grow.
    let quantum = crate::MAX_REALTIME_QUANTUM_SAMPLES;
    for local_channels in [1u16, 2] {
        for wire_rate in crate::SAMPLE_RATES_HZ {
            for wire_channels in [1u16, 2] {
                let local = AudioFormat::new(48_000, local_channels, 20);
                let wire = AudioFormat::new(wire_rate, wire_channels, 20);
                let (mut converter, mut out) = prepared_capture_converter(local, wire);
                if converter.is_identity() {
                    continue;
                }
                let mapped_before = converter.output_capacity_for(quantum);
                assert!(out.capacity() >= mapped_before);
                let out_capacity = out.capacity();
                let input = vec![0.1f32; quantum];
                for _ in 0..4 {
                    converter.convert(&input, &mut out);
                    assert_eq!(
                        out.capacity(),
                        out_capacity,
                        "{local:?} -> {wire:?} grew its transmit buffer"
                    );
                }
            }
        }
    }
}

#[test]
fn enrollment_wait_keeps_the_session_alive_while_the_host_decides() {
    let client_pake = pake_start(Side::Client, "123456");
    let host_pake = pake_start(Side::Host, "123456");
    let client_message = client_pake.message.clone();
    let host_message = host_pake.message.clone();
    let client_keys = client_pake.finish(&host_message).expect("client pairs");
    let host_keys = host_pake.finish(&client_message).expect("host pairs");
    let (client_sealer, client_opener) = client_keys.control_channel().expect("client cipher");
    let (host_sealer, host_opener) = host_keys.control_channel().expect("host cipher");

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let target = listener.local_addr().unwrap();
    let host = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        let mut cipher = ControlCipher {
            sealer: host_sealer,
            opener: host_opener,
        };
        assert!(matches!(
            cipher.receive(&mut stream).unwrap(),
            ControlMessage::TrustEnroll { .. }
        ));
        // Simulate the host embedding's accept/decline dialog staying open
        // well past one keepalive interval: the client must keep the control
        // channel alive or the host's session timeout ends the session
        // before any decision can be acknowledged.
        std::thread::sleep(Duration::from_millis(2_400));
        let mut saw_keepalive = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match cipher.receive(&mut stream) {
                Ok(ControlMessage::Keepalive {}) => {
                    saw_keepalive = true;
                    break;
                }
                Ok(_) => {}
                Err(error) if is_timeout(&error) => continue,
                Err(_) => break,
            }
        }
        assert!(
            saw_keepalive,
            "client stopped keepalives while waiting for the enrollment decision"
        );
        cipher
            .send(&mut stream, &ControlMessage::TrustAccepted {})
            .unwrap();
    });

    let inner = EngineInner::new(crate::EngineConfig {
        device_id: "client-id".into(),
        ..crate::EngineConfig::default()
    });
    let record = resumable_session(7_021);
    assert!(inner.insert_session(Arc::clone(&record)));
    let mut client_stream = TcpStream::connect(target).unwrap();
    let _ = client_stream.set_read_timeout(Some(Duration::from_millis(250)));
    let mut cipher = ControlCipher {
        sealer: client_sealer,
        opener: client_opener,
    };
    enroll_trusted_peer(
        &inner,
        &record,
        &mut client_stream,
        &mut cipher,
        "client-id",
        [0x2a; 32],
    );
    host.join().unwrap();
    assert!(matches!(
        inner.drain_events().as_slice(),
        [RelayEvent::TrustedPeerAvailable { peer_id, .. }] if peer_id == "resume-peer-id"
    ));
}
