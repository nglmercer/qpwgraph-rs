//! End-to-end loopback tests: host and client engines in one process,
//! connected over localhost. PCM codec keeps the payload deterministic.

use pw_graph_relay::{
    CodecKind, EngineConfig, RelayDirection, RelayEngine, RelayEvent, RelayHandle, Roles,
    SessionId, TransportPreference, TrustedPeer,
};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(8);

fn await_event(
    handle: &RelayHandle,
    predicate: impl Fn(&RelayEvent) -> bool,
) -> Option<RelayEvent> {
    let start = Instant::now();
    while start.elapsed() < TIMEOUT {
        for event in handle.events() {
            if predicate(&event) {
                return Some(event);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < TIMEOUT {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn host_engine(pin: &str) -> (RelayEngine, RelayHandle, u16) {
    host_engine_with(EngineConfig {
        pin: pin.into(),
        bind_addr: Some(Ipv4Addr::LOCALHOST),
        ..EngineConfig::default()
    })
}

fn host_engine_with(mut config: EngineConfig) -> (RelayEngine, RelayHandle, u16) {
    // The generic API makes the host a Receiver endpoint. Keep this helper's
    // old call sites focused on transport/pairing behavior while making that
    // role explicit instead of relying on the client-oriented EngineConfig
    // default.
    config.mode = pw_graph_relay::RelayMode::Receiver;
    config.client_roles = Roles::receive_only();
    let engine = RelayEngine::start(config).expect("host engine starts");
    let handle = engine.handle();
    let port = handle.host_start().expect("host listens");
    (engine, handle, port)
}

fn client_engine() -> (RelayEngine, RelayHandle) {
    client_engine_with(EngineConfig {
        codec: CodecKind::Pcm,
        device_name: "loopback-client".into(),
        ..EngineConfig::default()
    })
}

fn client_engine_with(config: EngineConfig) -> (RelayEngine, RelayHandle) {
    let engine = RelayEngine::start(config).expect("client engine starts");
    let handle = engine.handle();
    (engine, handle)
}

fn ramp(samples: usize) -> Vec<f32> {
    (0..samples).map(|i| i as f32 / samples as f32).collect()
}

/// One codec frame at the default configuration (10 ms, mono, 48 kHz).
const FRAME: usize = 480;
/// Frames the producer may run ahead of what the consumer has taken
/// delivery of. The relay keeps only the freshest couple of frames, so a
/// test that races further ahead than this would see its own surplus
/// discarded — correctly — as stale backlog.
const PIPELINE_SLACK: usize = 2;

/// Empty a playback queue completely, appending everything to `received`.
///
/// The relay caps its queues at a small multiple of the frame size on
/// purpose, so a consumer that takes only one buffer per poll can fall
/// behind and lose audio that the transport delivered correctly.
fn drain_playback(handle: &RelayHandle, received: &mut Vec<f32>) {
    let mut buffer = [0.0f32; FRAME];
    loop {
        let count = handle.pull_playback(&mut buffer);
        if count == 0 {
            return;
        }
        received.extend_from_slice(&buffer[..count]);
    }
}

/// Feed a signal the way a capture callback does: one frame at a time,
/// draining the far end as we go.
///
/// Pushing the whole signal in a single call would be dropped as stale
/// backlog — the queues keep only the freshest couple of frames so that a
/// stalled consumer costs a glitch instead of permanent added latency. The
/// producer therefore paces itself against actual delivery rather than a
/// fixed sleep, which also keeps the test honest on a loaded machine.
fn stream_frames(push: impl Fn(&[f32]), receiver: &RelayHandle, signal: &[f32]) -> Vec<f32> {
    let mut received = Vec::new();
    for (index, chunk) in signal.chunks(FRAME).enumerate() {
        push(chunk);
        let settled = index.saturating_sub(PIPELINE_SLACK) * FRAME;
        wait_until(|| {
            drain_playback(receiver, &mut received);
            received.len() >= settled
        });
    }
    wait_until(|| {
        drain_playback(receiver, &mut received);
        received.len() >= signal.len()
    });
    received
}

fn establish(
    host: &RelayHandle,
    client: &RelayHandle,
    port: u16,
    pin: &str,
    roles: Roles,
) -> SessionId {
    let target: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let session = client.connect(target, pin, roles);
    assert!(
        await_event(client, |event| matches!(
            event,
            RelayEvent::SessionEstablished { id, .. } if *id == session
        ))
        .is_some(),
        "client session should establish"
    );
    assert!(
        await_event(host, |event| matches!(
            event,
            RelayEvent::SessionEstablished { .. }
        ))
        .is_some(),
        "host session should establish"
    );
    session
}

#[test]
fn bidirectional_sessions_are_rejected_before_audio_starts() {
    let (_host, host_handle, port) = host_engine("123456");
    let (_client, client_handle) = client_engine();
    let target: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let session = client_handle.connect(target, "123456", Roles::both());

    let event = await_event(
        &client_handle,
        |event| matches!(event, RelayEvent::SessionLost { id, .. } if *id == session),
    );
    match event {
        Some(RelayEvent::SessionLost { reason, .. }) => assert!(
            reason.contains("bidirectional relay sessions are disabled"),
            "unexpected rejection: {reason}"
        ),
        other => panic!("expected one-way rejection, got {other:?}"),
    }
    assert!(host_handle.status().sessions.is_empty());
}

#[test]
#[allow(deprecated)]
fn authenticated_direction_offers_resolve_and_reject_stale_reversals() {
    let (_host, host_handle, port) = host_engine_with(EngineConfig {
        device_id: "desktop".into(),
        pin: "123456".into(),
        direction: RelayDirection::MobileToDesktop,
        ..EngineConfig::default()
    });
    let (_client, client_handle) = client_engine_with(EngineConfig {
        device_id: "phone".into(),
        direction: RelayDirection::MobileToDesktop,
        ..EngineConfig::default()
    });
    let session = establish(
        &host_handle,
        &client_handle,
        port,
        "123456",
        Roles::emit_only(),
    );

    client_handle
        .offer_direction(session, RelayDirection::DesktopToMobile, 1)
        .expect("the first direction offer should be queued");
    let client_resolution = await_event(&client_handle, |event| {
        matches!(
            event,
            RelayEvent::DirectionResolved {
                id,
                generation: 1,
                direction: RelayDirection::DesktopToMobile,
                ..
            } if *id == session
        )
    });
    assert!(
        client_resolution.is_some(),
        "client should observe the winner"
    );
    let host_resolution = await_event(&host_handle, |event| {
        matches!(
            event,
            RelayEvent::DirectionResolved {
                generation: 1,
                direction: RelayDirection::DesktopToMobile,
                ..
            }
        )
    });
    assert!(host_resolution.is_some(), "host should observe the winner");

    assert!(client_handle
        .offer_direction(session, RelayDirection::MobileToDesktop, 1)
        .is_err());
    client_handle
        .offer_direction(session, RelayDirection::MobileToDesktop, 2)
        .expect("a newer generation may switch back");
    assert!(await_event(&client_handle, |event| matches!(
        event,
        RelayEvent::DirectionResolved {
            id,
            generation: 2,
            direction: RelayDirection::MobileToDesktop,
            ..
        } if *id == session
    ))
    .is_some());
}

#[test]
fn client_emit_delivers_audio_to_host() {
    let (_host, host_handle, port) = host_engine("123456");
    let (_client, client_handle) = client_engine();
    let session = establish(
        &host_handle,
        &client_handle,
        port,
        "123456",
        Roles::emit_only(),
    );

    let signal = ramp(FRAME * 10);
    let received = stream_frames(
        |frame| client_handle.push_capture(frame),
        &host_handle,
        &signal,
    );
    assert!(
        received.len() >= signal.len(),
        "host should receive all emitted samples, got {}",
        received.len()
    );
    assert_eq!(&received[..signal.len()], &signal[..]);

    // Graceful disconnect surfaces on the host side.
    client_handle.disconnect(session).unwrap();
    assert!(
        await_event(&host_handle, |event| matches!(
            event,
            RelayEvent::SessionLost { id, reason } if *id == session && reason.contains("peer left")
        ))
        .is_some(),
        "host should see the client leave"
    );
}

#[test]
fn receiver_host_plays_audio_emitted_by_a_client() {
    let (_host, host_handle, port) = host_engine("123456");
    let (_client, client_handle) = client_engine();
    establish(
        &host_handle,
        &client_handle,
        port,
        "123456",
        Roles::emit_only(),
    );

    let signal = ramp(FRAME * 8);
    let received = stream_frames(
        |frame| client_handle.push_capture(frame),
        &host_handle,
        &signal,
    );
    assert!(
        received.len() >= signal.len(),
        "receiver host should play all emitted samples, got {}",
        received.len()
    );
    assert_eq!(&received[..signal.len()], &signal[..]);
}

#[test]
fn receiver_host_mixes_multiple_emitters() {
    let (_host, host_handle, port) = host_engine("123456");
    let (_client_a, client_a_handle) = client_engine();
    let (_client_b, client_b_handle) = client_engine();
    establish(
        &host_handle,
        &client_a_handle,
        port,
        "123456",
        Roles::emit_only(),
    );
    establish(
        &host_handle,
        &client_b_handle,
        port,
        "123456",
        Roles::emit_only(),
    );

    let mut received = Vec::new();
    for index in 0..8usize {
        client_a_handle.push_capture(&[0.25f32; FRAME]);
        client_b_handle.push_capture(&[0.5f32; FRAME]);
        let settled = index.saturating_sub(PIPELINE_SLACK) * FRAME;
        wait_until(|| {
            drain_playback(&host_handle, &mut received);
            received.len() >= settled
        });
    }
    assert!(wait_until(|| {
        drain_playback(&host_handle, &mut received);
        received.len() >= FRAME * 8
    }));
    assert!(received[..FRAME * 8]
        .iter()
        .all(|sample| (*sample - 0.75).abs() < 0.01));
}

#[test]
fn frames_traverse_the_relay_promptly() {
    let (_host, host_handle, port) = host_engine("123456");
    let (_client, client_handle) = client_engine();
    establish(
        &host_handle,
        &client_handle,
        port,
        "123456",
        Roles::emit_only(),
    );

    // Prime the path: the jitter buffer holds the first frames back until it
    // has an anchor, which is startup cost rather than steady-state delay.
    let warmup = ramp(FRAME * 8);
    stream_frames(
        |frame| client_handle.push_capture(frame),
        &host_handle,
        &warmup,
    );

    // Now time one frame from capture to playback. Over loopback this is
    // dominated by the relay's own buffering, which is exactly what the
    // queue depths and jitter tolerance are there to bound.
    let mut worst = Duration::ZERO;
    for _ in 0..10 {
        let mut received = Vec::new();
        let sent = Instant::now();
        client_handle.push_capture(&ramp(FRAME));
        assert!(
            wait_until(|| {
                drain_playback(&host_handle, &mut received);
                received.len() >= FRAME
            }),
            "a frame must arrive within the test timeout"
        );
        worst = worst.max(sent.elapsed());
    }

    // Generous next to the ~10 ms design target: this runs alongside the
    // rest of the suite on shared cores, and the point is to catch a
    // regression back to the hundreds of milliseconds an unbounded queue
    // used to accumulate, not to measure the network.
    assert!(
        worst < Duration::from_millis(150),
        "worst-case capture-to-playback delay was {worst:?}"
    );
}

#[test]
fn wrong_pin_is_rejected() {
    let (_host, _host_handle, port) = host_engine("123456");
    let (_client, client_handle) = client_engine();
    let target: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let session = client_handle.connect(target, "999999", Roles::emit_only());

    let event = await_event(&client_handle, |event| {
        matches!(
            event,
            RelayEvent::SessionLost { id, .. } if *id == session
        )
    });
    match event {
        Some(RelayEvent::SessionLost { reason, .. }) => {
            assert!(
                reason.contains("rejected pairing"),
                "reason should mention the rejection, got: {reason}"
            );
        }
        other => panic!("expected SessionLost, got {other:?}"),
    }
}

#[test]
fn host_requires_a_pin_before_listening() {
    let engine = RelayEngine::start(EngineConfig::default()).unwrap();
    let handle = engine.handle();
    assert!(handle.host_start().is_err());
}

#[test]
fn status_reflects_host_and_sessions() {
    let (_host, host_handle, port) = host_engine("123456");
    let (_client, client_handle) = client_engine();

    let status = host_handle.status();
    assert!(status.host_active);
    assert_eq!(status.host_port, Some(port));
    assert_eq!(status.host_addr, Some(Ipv4Addr::LOCALHOST));
    assert!(status.sessions.is_empty());

    establish(
        &host_handle,
        &client_handle,
        port,
        "123456",
        Roles::emit_only(),
    );
    let status = host_handle.status();
    assert_eq!(status.sessions.len(), 1);
    assert!(status.sessions[0].receiving);
    assert!(!status.sessions[0].sending);

    host_handle.host_stop().unwrap();
    assert!(!host_handle.status().host_active);
}

#[test]
fn trusted_pairing_allows_a_later_pinless_connection() {
    let (_host, host_handle, port) = host_engine_with(EngineConfig {
        pin: "123456".into(),
        device_id: "host-installation".into(),
        bind_addr: Some(Ipv4Addr::LOCALHOST),
        ..EngineConfig::default()
    });
    let (_first_client, first_handle) = client_engine_with(EngineConfig {
        codec: CodecKind::Pcm,
        device_name: "first-client".into(),
        device_id: "client-installation".into(),
        trust_new_peers: true,
        ..EngineConfig::default()
    });
    let target: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let first_session = first_handle.connect(target, "123456", Roles::emit_only());

    let mut client_established = false;
    let mut trusted = None;
    let mut host_established = false;
    let mut host_enrolled = false;
    assert!(wait_until(|| {
        for event in first_handle.events() {
            match event {
                RelayEvent::TrustedPeerAvailable {
                    peer_id, secret, ..
                } => trusted = Some(TrustedPeer { peer_id, secret }),
                RelayEvent::SessionEstablished { id, .. } if id == first_session => {
                    client_established = true
                }
                _ => {}
            }
        }
        // Raw engine users are the durable owner of trusted credentials.  The
        // test simulates that owner by committing the transaction before the
        // engine is allowed to send TrustAccepted.
        for event in host_handle.events() {
            match event {
                RelayEvent::TrustedPeerEnrollmentRequested { transaction_id, .. } => {
                    let secret = host_handle
                        .trusted_enrollment_secret(transaction_id)
                        .expect("pending enrollment should retain the secret");
                    host_handle
                        .accept_trusted_enrollment(transaction_id)
                        .expect("simulated durable enrollment should succeed");
                    assert_eq!(secret.len(), 32);
                    host_enrolled = true;
                }
                RelayEvent::SessionEstablished { .. } => host_established = true,
                _ => {}
            }
        }
        client_established && trusted.is_some() && host_enrolled
    }));
    let trusted = trusted.expect("explicit PIN pairing should produce a credential");
    assert_eq!(trusted.peer_id, "host-installation");

    // Wait until the host has accepted and stored the encrypted enrollment;
    // the second connection must be able to authenticate without its PIN.
    assert!(wait_until(|| {
        for event in host_handle.events() {
            if let RelayEvent::SessionEstablished { .. } = event {
                host_established = true;
            }
        }
        host_established && host_enrolled
    }));

    first_handle.disconnect(first_session).unwrap();
    assert!(wait_until(|| {
        host_handle
            .events()
            .into_iter()
            .any(|event| matches!(event, RelayEvent::SessionLost { .. }))
    }));

    let (_second_client, second_handle) = client_engine_with(EngineConfig {
        codec: CodecKind::Pcm,
        device_name: "second-client".into(),
        device_id: "client-installation".into(),
        trust_new_peers: false,
        ..EngineConfig::default()
    });
    let second_session = second_handle.connect_trusted(
        target,
        "host-installation",
        trusted.secret,
        Roles::emit_only(),
    );
    assert!(await_event(&second_handle, |event| matches!(
        event,
        RelayEvent::SessionEstablished { id, .. } if *id == second_session
    ))
    .is_some());
    assert!(await_event(&host_handle, |event| matches!(
        event,
        RelayEvent::SessionEstablished { peer, .. } if peer.id == "client-installation"
    ))
    .is_some());
}

#[test]
fn pin_only_host_rejects_trusted_enrollment_without_delaying_the_session() {
    let (_host, host_handle, port) = host_engine_with(EngineConfig {
        pin: "123456".into(),
        device_id: "pin-only-host".into(),
        bind_addr: Some(Ipv4Addr::LOCALHOST),
        trust_new_peers: false,
        ..EngineConfig::default()
    });
    let (_client, client_handle) = client_engine_with(EngineConfig {
        codec: CodecKind::Pcm,
        device_id: "would-be-trusted-client".into(),
        trust_new_peers: true,
        ..EngineConfig::default()
    });
    let target: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let session = client_handle.connect(target, "123456", Roles::emit_only());
    let started = Instant::now();
    let mut established = false;
    let mut credential_offered = false;
    assert!(wait_until(|| {
        for event in client_handle.events() {
            match event {
                RelayEvent::SessionEstablished { id, .. } if id == session => established = true,
                RelayEvent::TrustedPeerAvailable { .. } => credential_offered = true,
                _ => {}
            }
        }
        established
    }));
    assert!(!credential_offered);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "an authenticated rejection should not wait for the handshake timeout"
    );

    let mut host_enrolled = false;
    assert!(wait_until(|| {
        let mut host_established = false;
        for event in host_handle.events() {
            match event {
                RelayEvent::SessionEstablished { .. } => host_established = true,
                RelayEvent::TrustedPeerAvailable { .. } => host_enrolled = true,
                _ => {}
            }
        }
        host_established
    }));
    assert!(!host_enrolled);
}

#[test]
fn adb_forwarding_uses_the_authenticated_tcp_audio_channel() {
    let (_host, host_handle, port) = host_engine_with(EngineConfig {
        pin: "123456".into(),
        device_id: "adb-host".into(),
        bind_addr: Some(Ipv4Addr::LOCALHOST),
        ..EngineConfig::default()
    });
    let (_client, client_handle) = client_engine_with(EngineConfig {
        codec: CodecKind::Pcm,
        device_name: "adb-client".into(),
        device_id: "adb-client-id".into(),
        transport: TransportPreference::Adb,
        trust_new_peers: false,
        ..EngineConfig::default()
    });
    let session = establish(
        &host_handle,
        &client_handle,
        port,
        "123456",
        Roles::emit_only(),
    );
    let signal = ramp(FRAME * 6);
    let received = stream_frames(
        |frame| client_handle.push_capture(frame),
        &host_handle,
        &signal,
    );
    assert!(received.len() >= signal.len());
    assert_eq!(&received[..signal.len()], &signal[..]);

    client_handle.disconnect(session).unwrap();
}

#[test]
fn a_running_host_rebinds_when_its_configured_link_changes() {
    let initial_addr = Ipv4Addr::new(127, 0, 0, 2);
    let next_addr = Ipv4Addr::new(127, 0, 0, 3);
    let (_host, host_handle, port) = host_engine_with(EngineConfig {
        pin: "123456".into(),
        bind_addr: Some(initial_addr),
        ..EngineConfig::default()
    });
    let old_target = SocketAddr::new(initial_addr.into(), port);
    assert!(std::net::TcpStream::connect_timeout(&old_target, Duration::from_secs(1)).is_ok());

    let mut config = host_handle.config();
    config.bind_addr = Some(next_addr);
    host_handle.update_config(config);
    assert!(wait_until(
        || host_handle.status().host_addr == Some(next_addr)
    ));

    let new_target = SocketAddr::new(next_addr.into(), port);
    assert!(std::net::TcpStream::connect_timeout(&new_target, Duration::from_secs(1)).is_ok());
    assert!(std::net::TcpStream::connect_timeout(&old_target, Duration::from_millis(100)).is_err());
}

#[test]
fn unauthenticated_datagrams_cannot_inject_audio_or_move_the_peer_address() {
    use std::net::UdpSocket;

    let (_host, host_handle, port) = host_engine("123456");
    let (_client, client_handle) = client_engine();
    establish(
        &host_handle,
        &client_handle,
        port,
        "123456",
        Roles::emit_only(),
    );

    // Deliver real audio first so the path is known good, then drain it.
    let warmup = ramp(FRAME * 4);
    let delivered = stream_frames(
        |frame| client_handle.push_capture(frame),
        &host_handle,
        &warmup,
    );
    assert!(!delivered.is_empty(), "the honest path must work first");
    let mut received = Vec::new();
    drain_playback(&host_handle, &mut received);
    received.clear();

    // Now play the attacker: forge datagrams in the documented wire format
    // from an unrelated socket. Without the session key they cannot be
    // sealed, so none of them may reach the playback queue.
    let attacker = UdpSocket::bind("127.0.0.1:0").expect("attacker socket binds");
    for candidate in 1024..1200u16 {
        let mut forged = vec![0u8; 20 + 64];
        forged[0..2].copy_from_slice(&0xA1E5u16.to_le_bytes());
        forged[2] = 2 | 0x20; // version 2, keyframe
        forged[3] = 0; // PCM
        let _ = attacker.send_to(&forged, ("127.0.0.1", candidate));
    }
    std::thread::sleep(Duration::from_millis(200));
    drain_playback(&host_handle, &mut received);
    assert!(
        received.is_empty(),
        "forged datagrams must never reach playback, got {} samples",
        received.len()
    );

    // The session must still work afterwards: rejecting the forgeries is not
    // allowed to disturb the real stream.
    let signal = ramp(FRAME * 4);
    let delivered = stream_frames(
        |frame| client_handle.push_capture(frame),
        &host_handle,
        &signal,
    );
    assert!(delivered.len() >= signal.len());
}

#[test]
fn repeated_wrong_pins_lock_the_source_out() {
    let (_host, host_handle, port) = host_engine("123456");
    let target: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    // Burn through the attempt budget. A PAKE makes guessing online-only, and
    // the lockout is what makes an online guessing run hopeless.
    for _ in 0..pw_graph_relay::PAIRING_ATTEMPT_LIMIT {
        let (_client, client_handle) = client_engine();
        let session = client_handle.connect(target, "999999", Roles::emit_only());
        assert!(await_event(&client_handle, |event| matches!(
            event,
            RelayEvent::SessionLost { id, .. } if *id == session
        ))
        .is_some());
    }

    // Even the correct PIN is now refused while the lockout stands.
    let (_client, client_handle) = client_engine();
    let session = client_handle.connect(target, "123456", Roles::emit_only());
    let event = await_event(
        &client_handle,
        |event| matches!(event, RelayEvent::SessionLost { id, .. } if *id == session),
    );
    match event {
        Some(RelayEvent::SessionLost { reason, .. }) => assert!(
            reason.contains("failed pairing attempts"),
            "expected a lockout, got: {reason}"
        ),
        other => panic!("expected SessionLost, got {other:?}"),
    }
    let _ = host_handle;
}

#[test]
fn a_session_at_a_different_rate_is_converted_not_misread() {
    // A 16 kHz mono peer used to have its samples handed to a 48 kHz endpoint
    // untouched, which plays back at three times the pitch.
    let (_host, host_handle, port) = host_engine("123456");
    let engine = RelayEngine::start(EngineConfig {
        codec: CodecKind::Pcm,
        device_name: "narrowband-client".into(),
        sample_rate: 16_000,
        local_sample_rate: 16_000,
        frame_ms: 10,
        ..EngineConfig::default()
    })
    .expect("client engine starts");
    let client_handle = engine.handle();
    establish(
        &host_handle,
        &client_handle,
        port,
        "123456",
        Roles::emit_only(),
    );

    // One second of 16 kHz audio must arrive as about one second of 48 kHz
    // audio on the host, whose local format is the default 48 kHz.
    let narrow_frame = 160;
    let signal = ramp(narrow_frame * 20);
    let mut received = Vec::new();
    for (index, chunk) in signal.chunks(narrow_frame).enumerate() {
        client_handle.push_capture(chunk);
        let settled = index.saturating_sub(PIPELINE_SLACK) * narrow_frame * 3;
        wait_until(|| {
            drain_playback(&host_handle, &mut received);
            received.len() >= settled
        });
    }
    wait_until(|| {
        drain_playback(&host_handle, &mut received);
        received.len() >= signal.len() * 3 - FRAME
    });
    let expected = signal.len() * 3;
    assert!(
        received.len() as i64 > (expected as i64 * 8) / 10,
        "16 kHz audio should arrive as ~{expected} samples at 48 kHz, got {}",
        received.len()
    );
}

#[test]
fn two_peers_mix_instead_of_interleaving() {
    // One shared playback queue used to concatenate two peers' audio; each
    // session now decodes into its own queue and the engine sums them.
    let (_host, host_handle, port) = host_engine("123456");
    let (_a, a_handle) = client_engine();
    let (_b, b_handle) = client_engine();
    establish(&host_handle, &a_handle, port, "123456", Roles::emit_only());
    establish(&host_handle, &b_handle, port, "123456", Roles::emit_only());

    // Both peers send a constant, so a correct mix is their sum and a
    // concatenation would alternate between the two values.
    let mut received = Vec::new();
    for index in 0..12usize {
        a_handle.push_capture(&[0.25f32; FRAME]);
        b_handle.push_capture(&[0.5f32; FRAME]);
        // Pace against actual delivery, allowing the usual pipeline slack:
        // waiting for the full total on every iteration would sit out the
        // whole timeout on the first few.
        let settled = index.saturating_sub(PIPELINE_SLACK) * FRAME;
        wait_until(|| {
            drain_playback(&host_handle, &mut received);
            received.len() >= settled
        });
    }
    wait_until(|| {
        drain_playback(&host_handle, &mut received);
        received.len() >= FRAME * 4
    });
    let mixed = received
        .iter()
        .filter(|sample| (**sample - 0.75).abs() < 1e-3)
        .count();
    assert!(
        mixed > 0,
        "expected summed samples of 0.75, saw values: {:?}",
        &received[..received.len().min(8)]
    );
}
