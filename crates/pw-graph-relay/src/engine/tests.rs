use super::*;
use crate::DEFAULT_QUEUE_CAPACITY;
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener};

#[test]
fn default_config_is_usable() {
    let config = EngineConfig::default();
    assert_eq!(config.frame_ms, 10);
    assert_eq!(config.sample_rate, 48_000);
    assert!(config.client_roles.emit);
    let format = AudioFormat::new(config.sample_rate, config.channels, config.frame_ms);
    assert_eq!(format.frame_samples(), 480);
}

#[test]
fn discovery_start_accepts_each_working_mechanism_and_rejects_both_failures() {
    let ok = Ok(());
    let mdns_error = Err(RelayError::Engine("mDNS unavailable".into()));
    let usb_error = Err(RelayError::Engine("USB scanner unavailable".into()));

    assert!(matches!(
        discovery_start_outcome(&ok, &ok),
        Ok(DiscoveryStartOutcome::MdnsAndUsb)
    ));
    assert!(matches!(
        discovery_start_outcome(&ok, &usb_error),
        Ok(DiscoveryStartOutcome::MdnsOnly)
    ));
    assert!(matches!(
        discovery_start_outcome(&mdns_error, &ok),
        Ok(DiscoveryStartOutcome::UsbOnly)
    ));
    assert!(matches!(
        discovery_start_outcome(&mdns_error, &usb_error),
        Err(RelayError::Engine(message))
            if message.contains("mDNS unavailable")
                && message.contains("USB scanner unavailable")
    ));
}

#[test]
fn host_start_requires_a_pin() {
    let engine = RelayEngine::start(EngineConfig::default()).unwrap();
    let handle = engine.handle();
    assert!(handle.host_start().is_err());
}

#[test]
fn host_start_reports_a_port_conflict_without_falling_back_to_another_port() {
    let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = occupied.local_addr().unwrap().port();
    let engine = RelayEngine::start(EngineConfig {
        pin: "123456".into(),
        port,
        bind_addr: Some(Ipv4Addr::LOCALHOST),
        mode: RelayMode::Receiver,
        client_roles: Roles::receive_only(),
        ..EngineConfig::default()
    })
    .unwrap();
    let handle = engine.handle();
    let error = handle
        .host_start()
        .expect_err("the occupied port must fail");
    assert!(error.to_string().contains("control port"));
    assert!(error.to_string().contains(&port.to_string()));
    assert!(!handle.status().host_active);
    assert_eq!(handle.status().host_port, None);
    engine.shutdown();
}

#[test]
fn host_stop_releases_an_explicit_port_before_returning() {
    let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    drop(reservation);
    let engine = RelayEngine::start(EngineConfig {
        pin: "123456".into(),
        port,
        bind_addr: Some(Ipv4Addr::LOCALHOST),
        mode: RelayMode::Receiver,
        client_roles: Roles::receive_only(),
        ..EngineConfig::default()
    })
    .unwrap();
    let handle = engine.handle();
    assert_eq!(handle.host_start().unwrap(), port);
    handle.host_stop().unwrap();
    assert_eq!(handle.host_start().unwrap(), port);
    handle.host_stop().unwrap();
    engine.shutdown();
}

#[test]
fn resume_is_only_eligible_after_control_drop_and_consumes_generations() {
    let record = mixing_session(1, false);

    // A live control owner cannot be replaced by a connection that merely
    // knows the session id.
    assert_eq!(record.begin_resume(), None);
    assert_eq!(*record.control_state.lock().unwrap(), ControlState::Active);

    assert!(record.mark_control_dropped());
    let first = record.begin_resume().expect("dropped control is resumable");
    assert_eq!(first, 2);
    // One challenge can be in flight at a time.
    assert_eq!(record.begin_resume(), None);

    // A failed challenge returns to eligibility. The next challenge uses
    // a fresh server nonce, so an old proof cannot be replayed even though
    // the live control-key generation has not changed yet.
    record.cancel_resume(first);
    let second = record.begin_resume().expect("retry remains eligible");
    assert_eq!(second, 2);
    assert!(record.finish_resume(second));
    assert_eq!(*record.control_state.lock().unwrap(), ControlState::Active);
    assert_eq!(record.begin_resume(), None);
}

#[test]
fn resume_grace_expiry_cannot_race_a_successful_resume() {
    let record = mixing_session(2, false);
    assert!(record.mark_control_dropped());
    let generation = record.begin_resume().expect("resume is eligible");
    assert_eq!(
        record.expire_resume_grace(1),
        ResumeGraceResult::InProgress { generation }
    );
    assert_eq!(
        *record.control_state.lock().unwrap(),
        ControlState::Resuming { generation }
    );
    assert!(record.finish_resume(generation));
    // The old watcher's generation is stale after the successful resume.
    assert_eq!(record.expire_resume_grace(1), ResumeGraceResult::Resumed);

    let record = mixing_session(3, false);
    assert!(record.mark_control_dropped());
    let generation = record.begin_resume().expect("resume is eligible");
    record.cancel_resume(generation);
    assert_eq!(record.expire_resume_grace(1), ResumeGraceResult::Expired);
    // Expiry wins only while the old generation is still current.
    assert!(!record.finish_resume(generation));
}

#[test]
fn resume_expiry_aborts_the_in_progress_generation_not_the_stale_owner() {
    let record = mixing_session(4, false);
    assert!(record.mark_control_dropped());
    let generation = record.begin_resume().expect("resume is eligible");
    assert_eq!(generation, 2);

    // The old control watcher still owns generation 1. Once the grace
    // deadline is reached, the result names generation 2 so the stalled
    // challenge can be cancelled precisely.
    assert_eq!(
        record.expire_resume_grace(1),
        ResumeGraceResult::InProgress { generation: 2 }
    );
    assert!(record.abort_resume(2));
    assert_eq!(*record.control_state.lock().unwrap(), ControlState::Active);
    assert!(!record.abort_resume(1));
}

#[test]
fn realtime_push_reports_no_acceptor_when_no_session_exists() {
    let handle = RelayHandle {
        inner: mixing_engine(Vec::new()),
    };
    assert!(!handle.try_push_capture(&[]));
}

#[test]
fn realtime_push_rejects_oversized_quantum_before_converter_work() {
    let session = mixing_session(1, false);
    let inner = mixing_engine(vec![Arc::clone(&session)]);
    let before = session.capture_convert.lock().unwrap().1.capacity();
    let oversized = vec![0.0f32; MAX_REALTIME_QUANTUM_SAMPLES + 1];

    let handle = RelayHandle { inner };
    assert!(!handle.try_push_capture(&oversized));
    assert_eq!(session.capture_convert.lock().unwrap().1.capacity(), before);
}

#[test]
fn established_session_admission_is_atomically_bounded() {
    let inner = EngineInner::new(EngineConfig {
        max_sessions: 1,
        ..EngineConfig::default()
    });
    assert!(inner.insert_session(mixing_session(1, false)));
    assert!(!inner.insert_session(mixing_session(2, false)));
    assert_eq!(inner.session_count(), 1);
}

#[test]
fn pairing_failure_table_has_a_hard_bound() {
    let inner = EngineInner::new(EngineConfig::default());
    let now = Instant::now();
    {
        let mut failures = inner.pairing_failures.lock().unwrap();
        for index in 0..MAX_PAIRING_FAILURE_RECORDS {
            failures.insert(
                IpAddr::V6(Ipv6Addr::from(index as u128)),
                FailureRecord {
                    count: PAIRING_ATTEMPT_LIMIT,
                    locked_until: now + PAIRING_LOCKOUT,
                    last_seen: now,
                },
            );
        }
    }

    inner.note_pairing_failure(IpAddr::V6(Ipv6Addr::from(99_999u128)));
    assert_eq!(
        inner.pairing_failures.lock().unwrap().len(),
        MAX_PAIRING_FAILURE_RECORDS
    );
}

fn enrollment_peer(id: &str) -> PeerInfo {
    PeerInfo {
        id: id.into(),
        name: format!("{id}-name"),
        kind: DeviceKind::Other,
        addr: "192.168.42.2:48123".parse().unwrap(),
    }
}

#[test]
fn trusted_enrollment_is_not_imported_until_the_embedding_commits() {
    let inner = EngineInner::new(EngineConfig::default());
    let peer = enrollment_peer("phone");
    let secret = [7u8; 32];
    let transaction = inner
        .begin_trusted_enrollment(SessionId(9), peer.id.clone(), peer.clone(), secret)
        .unwrap();

    assert_eq!(inner.trusted_secret(&peer.id), None);
    assert_eq!(inner.trusted_enrollment_secret(transaction), Some(secret));
    inner.accept_trusted_enrollment(transaction).unwrap();
    let resolution = inner.take_trusted_enrollment(SessionId(9)).unwrap();
    assert!(resolution.accepted);
    inner.remember_trusted_peer(resolution.peer_id, resolution.secret);
    assert_eq!(inner.trusted_secret(&peer.id), Some(secret));
}

#[test]
fn failed_or_rejected_enrollment_preserves_the_previous_credential() {
    let inner = EngineInner::new(EngineConfig::default());
    let peer = enrollment_peer("phone");
    let old = [3u8; 32];
    let new = [4u8; 32];
    inner.remember_trusted_peer(peer.id.clone(), old);
    let transaction = inner
        .begin_trusted_enrollment(SessionId(10), peer.id.clone(), peer, new)
        .unwrap();
    inner
        .reject_trusted_enrollment(transaction, "simulated persistence failure".into())
        .unwrap();
    let resolution = inner.take_trusted_enrollment(SessionId(10)).unwrap();
    assert!(!resolution.accepted);
    assert_eq!(inner.trusted_secret("phone"), Some(old));
}

#[test]
fn enrollment_rejects_malformed_duplicate_and_mismatched_requests() {
    let inner = EngineInner::new(EngineConfig::default());
    let peer = enrollment_peer("phone");
    assert!(inner
        .begin_trusted_enrollment(SessionId(1), peer.id.clone(), peer.clone(), [0u8; 32],)
        .is_err());
    assert!(inner
        .begin_trusted_enrollment(SessionId(1), "other".into(), peer.clone(), [1u8; 32],)
        .is_err());
    let transaction = inner
        .begin_trusted_enrollment(SessionId(1), peer.id.clone(), peer.clone(), [1u8; 32])
        .unwrap();
    assert!(inner
        .begin_trusted_enrollment(SessionId(1), peer.id.clone(), peer, [2u8; 32])
        .is_err());
    assert!(inner
        .begin_trusted_enrollment(
            SessionId(2),
            "another".into(),
            enrollment_peer("another"),
            [2u8; 32],
        )
        .is_ok());
    inner
        .reject_trusted_enrollment(transaction, "duplicate".into())
        .unwrap();
}

#[test]
fn enrollment_expiry_is_bounded_and_does_not_expose_the_secret() {
    let inner = EngineInner::new(EngineConfig::default());
    let peer = enrollment_peer("phone");
    let peer_id = peer.id.clone();
    let transaction = inner
        .begin_trusted_enrollment(SessionId(3), peer_id, peer, [9u8; 32])
        .unwrap();
    inner
        .pending_enrollments
        .lock()
        .unwrap()
        .get_mut(&transaction)
        .unwrap()
        .created = Instant::now() - TRUST_ENROLLMENT_TIMEOUT - Duration::from_secs(1);
    assert_eq!(inner.trusted_enrollment_secret(transaction), None);
    assert!(inner.accept_trusted_enrollment(transaction).is_err());
}

#[test]
fn revocation_removes_the_live_credential_without_affecting_other_peers() {
    let inner = EngineInner::new(EngineConfig::default());
    inner.remember_trusted_peer("one".into(), [1u8; 32]);
    inner.remember_trusted_peer("two".into(), [2u8; 32]);
    assert!(inner.remove_trusted_peer("one"));
    assert_eq!(inner.trusted_secret("one"), None);
    assert_eq!(inner.trusted_secret("two"), Some([2u8; 32]));
}

#[test]
fn candidate_backoff_is_scoped_to_one_peer_and_address() {
    let inner = EngineInner::new(EngineConfig::default());
    let fake: SocketAddr = "192.168.1.66:48123".parse().unwrap();
    let real: SocketAddr = "192.168.42.1:48123".parse().unwrap();
    inner.note_candidate_failure("host", fake);
    assert!(!inner.candidate_allowed("host", fake));
    assert!(inner.candidate_allowed("host", real));
    assert!(inner.candidate_allowed("other", fake));
    inner.note_candidate_success("host", real);
    assert_eq!(inner.last_successful_address("host"), Some(real));
    assert!(!inner
        .candidate_failures
        .lock()
        .unwrap()
        .contains_key(&("host".into(), real)));
}

#[test]
fn trusted_peer_debug_redacts_the_secret() {
    let secret = [0xabu8; 32];
    let text = format!(
        "{:?}",
        TrustedPeer {
            peer_id: "phone".into(),
            secret
        }
    );
    assert!(!text.contains("ab".repeat(32).as_str()));
    assert!(text.contains("redacted"));
}

/// A session record with just enough filled in to exercise mixing. The
/// crypto halves are real, because `SessionRecord` has nowhere to put a
/// placeholder, but nothing in the mix path touches them.
fn mixing_session(id: u64, receiving: bool) -> Arc<SessionRecord> {
    use crate::crypto::{pake_start, Side};
    let client = pake_start(Side::Client, "123456");
    let host = pake_start(Side::Host, "123456");
    let client_message = client.message.clone();
    let host_message = host.message.clone();
    let keys = client.finish(&host_message).expect("client pairs");
    let peer_keys = host.finish(&client_message).expect("host pairs");
    let (sealer, _) = keys.audio_channel().expect("audio keys");
    let (_, opener) = peer_keys.audio_channel().expect("peer audio keys");
    let format = AudioFormat::new(48_000, 1, 10);
    let capture_converter =
        Converter::with_capacity(48_000, 1, 48_000, 1, MAX_REALTIME_QUANTUM_SAMPLES);
    let capture_destination =
        Vec::with_capacity(capture_converter.output_capacity_for(MAX_REALTIME_QUANTUM_SAMPLES));
    Arc::new(SessionRecord {
        id: SessionId(id),
        wire_id: id,
        peer: PeerInfo {
            id: format!("peer-{id}"),
            name: format!("peer-{id}"),
            kind: DeviceKind::Other,
            addr: "127.0.0.1:1".parse().unwrap(),
        },
        roles: Roles::both(),
        codec: CodecKind::Pcm,
        format,
        sending: true,
        receiving,
        active_roles: AtomicU8::new(SessionRecord::role_bits(Roles {
            emit: true,
            receive: receiving,
        })),
        stop: Arc::new(AtomicBool::new(false)),
        bye_requested: AtomicBool::new(false),
        control_generation: AtomicU64::new(1),
        resume_secret: keys.resume_auth_key(),
        trust_secret: Mutex::new(None),
        tcp_audio: None,
        udp_audio: None,
        control_peer_addr: Mutex::new("127.0.0.1:1".parse().unwrap()),
        control_state: Mutex::new(ControlState::Active),
        peer_audio_addr: Mutex::new(None),
        outgoing: PcmQueue::new(DEFAULT_QUEUE_CAPACITY),
        incoming: PcmQueue::new(DEFAULT_QUEUE_CAPACITY),
        capture_convert: Mutex::new((capture_converter, capture_destination)),
        audio_sealer: Mutex::new(sealer),
        audio_opener: Mutex::new(opener),
        direction: Mutex::new(DirectionNegotiation::new(DirectionOffer {
            generation: 1,
            direction: RelayDirection::MobileToDesktop,
            device_id: format!("peer-{id}"),
        })),
    })
}

fn mixing_engine(sessions: Vec<Arc<SessionRecord>>) -> Arc<EngineInner> {
    let inner = EngineInner::new(EngineConfig::default());
    for record in sessions {
        inner.insert_session(record);
    }
    inner
}

#[test]
fn mixing_with_no_receiving_sessions_produces_nothing() {
    let inner = mixing_engine(vec![mixing_session(1, false)]);
    let mut out = [9.0f32; 4];
    assert_eq!(inner.mix_playback(&mut out, true), 0);
    // Producing nothing must also mean touching nothing: the caller fills
    // the untouched tail with silence itself.
    assert_eq!(out, [9.0; 4]);
    assert_eq!(mixing_engine(vec![]).mix_playback(&mut out, true), 0);
}

#[test]
fn mixing_one_receiving_session_passes_its_audio_through() {
    let session = mixing_session(1, true);
    session.incoming.push(&[0.1, 0.2, 0.3]);
    let inner = mixing_engine(vec![session, mixing_session(2, false)]);
    let mut out = [0.0f32; 4];
    assert_eq!(inner.mix_playback(&mut out, true), 3);
    assert_eq!(&out[..3], &[0.1, 0.2, 0.3]);
}

#[test]
fn mixing_several_receiving_sessions_sums_them() {
    let first = mixing_session(1, true);
    let second = mixing_session(2, true);
    let third = mixing_session(3, true);
    first.incoming.push(&[0.1, 0.1, 0.1, 0.1]);
    second.incoming.push(&[0.2, 0.2]);
    third.incoming.push(&[0.3, 0.3, 0.3]);
    let inner = mixing_engine(vec![first, second, third]);
    let mut out = [0.0f32; 4];
    // The count is the longest contributor, not the sum of the lengths:
    // peers are mixed, not concatenated.
    assert_eq!(inner.mix_playback(&mut out, true), 4);
    assert!((out[0] - 0.6).abs() < 1e-6, "{out:?}");
    assert!((out[1] - 0.6).abs() < 1e-6, "{out:?}");
    assert!((out[2] - 0.4).abs() < 1e-6, "{out:?}");
    assert!((out[3] - 0.1).abs() < 1e-6, "{out:?}");
}

#[test]
fn a_summed_mix_is_clamped_to_full_scale() {
    let first = mixing_session(1, true);
    let second = mixing_session(2, true);
    first.incoming.push(&[0.9, -0.9]);
    second.incoming.push(&[0.9, -0.9]);
    let inner = mixing_engine(vec![first, second]);
    let mut out = [0.0f32; 2];
    assert_eq!(inner.mix_playback(&mut out, true), 2);
    assert_eq!(out, [1.0, -1.0]);
}

#[test]
fn realtime_mixing_does_not_grow_the_scratch_buffer() {
    // This is the regression the `Vec` collect used to cause: every
    // realtime callback allocated, on the one thread that must not.
    let first = mixing_session(1, true);
    let second = mixing_session(2, true);
    let inner = mixing_engine(vec![Arc::clone(&first), Arc::clone(&second)]);
    let capacity = inner.mix_scratch.lock().unwrap().capacity();
    assert!(capacity >= MAX_REALTIME_QUANTUM_SAMPLES);
    let block = vec![0.25f32; 1_024];
    let mut out = vec![0.0f32; 1_024];
    for _ in 0..64 {
        first.incoming.push(&block);
        second.incoming.push(&block);
        inner.mix_playback(&mut out, true);
        assert_eq!(inner.mix_scratch.lock().unwrap().capacity(), capacity);
    }
}

#[test]
fn realtime_mixing_of_an_oversized_buffer_serves_what_it_can_without_growing() {
    // A caller ignoring `MAX_REALTIME_QUANTUM_SAMPLES` gets a short read
    // rather than an allocation on the audio thread.
    let first = mixing_session(1, true);
    let second = mixing_session(2, true);
    let oversized = MAX_REALTIME_QUANTUM_SAMPLES + 512;
    first.incoming.push(&vec![0.5f32; oversized]);
    second.incoming.push(&vec![0.5f32; oversized]);
    let inner = mixing_engine(vec![first, second]);
    let capacity = inner.mix_scratch.lock().unwrap().capacity();
    let mut out = vec![0.0f32; oversized];
    let produced = inner.mix_playback(&mut out, true);
    assert!(produced <= MAX_REALTIME_QUANTUM_SAMPLES);
    assert_eq!(inner.mix_scratch.lock().unwrap().capacity(), capacity);
}
