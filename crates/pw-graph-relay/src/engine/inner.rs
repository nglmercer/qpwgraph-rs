//! The engine's shared interior.
//!
//! `EngineInner` is what the session threads hold: the session table, the
//! trusted credential store, pairing rate limits, and the event queue. It is
//! deliberately separate from the public handle so a worker thread can never
//! reach a public API that would deadlock it.

use super::*;
use std::cell::RefCell;

thread_local! {
    static REALTIME_SNAPSHOT_CACHE: RefCell<Option<Arc<Vec<Arc<SessionRecord>>>>> = const { RefCell::new(None) };
}

pub(crate) struct EngineInner {
    pub(crate) config: Mutex<EngineConfig>,
    pub(crate) events: Mutex<VecDeque<RelayEvent>>,
    /// Scratch used while summing the per-session receive queues.
    pub(crate) mix_scratch: Mutex<Vec<f32>>,
    pub(crate) sessions: Mutex<BTreeMap<SessionId, Arc<SessionRecord>>>,
    /// Lock-free snapshot for realtime callbacks. Updated only when sessions
    /// are inserted/removed (rare), read via try_lock plus thread-local cache
    /// so a contended control-plane lock never turns audio into silence.
    pub(crate) realtime_snapshot: Mutex<Arc<Vec<Arc<SessionRecord>>>>,
    /// Recent failed pairing attempts per source address. A PAKE makes
    /// guessing an online-only game; this is what makes that game slow.
    pub(crate) pairing_failures: Mutex<BTreeMap<IpAddr, FailureRecord>>,
    /// Imported persistent credentials, keyed by the remote stable identity.
    pub(crate) trusted_peers: Mutex<BTreeMap<String, [u8; 32]>>,
    /// Connections currently inside the pre-authentication handshake.
    pub(crate) pending_handshakes: AtomicU64,
    /// Durable-enrollment transactions awaiting an embedding decision.
    /// Secrets remain private to this map and are never included in the
    /// request event or diagnostics.
    pub(crate) pending_enrollments: Mutex<BTreeMap<u64, PendingEnrollment>>,
    pub(crate) next_enrollment: AtomicU64,
    pub(crate) host: Mutex<Option<session::HostRecord>>,
    /// Discovered (not necessarily connected) relay hosts, keyed by address.
    pub(crate) peers: Mutex<BTreeMap<SocketAddr, PeerInfo>>,
    /// Resolved addresses grouped by mDNS service identity.
    pub(crate) peer_services: Mutex<BTreeMap<String, BTreeMap<SocketAddr, PeerInfo>>>,
    /// Discovery metadata used only to rank candidate addresses. Identity is
    /// still proved by the trusted/resume handshake, never by this metadata.
    pub(crate) peer_links: Mutex<BTreeMap<SocketAddr, LinkKind>>,
    pub(crate) candidate_failures: Mutex<BTreeMap<(String, SocketAddr), FailureRecord>>,
    pub(crate) last_successful_addresses: Mutex<BTreeMap<String, SocketAddr>>,
    pub(crate) advertiser: Mutex<Option<discovery::Advertiser>>,
    pub(crate) browser: Mutex<Option<discovery::Browser>>,
    pub(crate) usb_scanner: Mutex<Option<usb_probe::UsbScanner>>,
    pub(crate) next_session: AtomicU64,
    pub(crate) running: AtomicBool,
}

impl EngineInner {
    pub(crate) fn new(config: EngineConfig) -> Arc<Self> {
        let trusted_peers = config
            .trusted_peers
            .iter()
            .take(MAX_TRUSTED_PEERS)
            .map(|peer| (peer.peer_id.clone(), peer.secret))
            .collect();
        Arc::new(Self {
            config: Mutex::new(config),
            events: Mutex::new(VecDeque::new()),
            // Allocated once, at the largest quantum the realtime callback
            // will ever present, so `mix_playback` never grows it. 64 KiB.
            mix_scratch: Mutex::new(Vec::with_capacity(MAX_REALTIME_QUANTUM_SAMPLES)),
            sessions: Mutex::new(BTreeMap::new()),
            realtime_snapshot: Mutex::new(Arc::new(Vec::new())),
            pairing_failures: Mutex::new(BTreeMap::new()),
            trusted_peers: Mutex::new(trusted_peers),
            pending_handshakes: AtomicU64::new(0),
            pending_enrollments: Mutex::new(BTreeMap::new()),
            next_enrollment: AtomicU64::new(1),
            host: Mutex::new(None),
            peers: Mutex::new(BTreeMap::new()),
            peer_services: Mutex::new(BTreeMap::new()),
            peer_links: Mutex::new(BTreeMap::new()),
            candidate_failures: Mutex::new(BTreeMap::new()),
            last_successful_addresses: Mutex::new(BTreeMap::new()),
            advertiser: Mutex::new(None),
            browser: Mutex::new(None),
            usb_scanner: Mutex::new(None),
            next_session: AtomicU64::new(1),
            running: AtomicBool::new(true),
        })
    }

    pub(crate) fn emit(&self, event: RelayEvent) {
        let Ok(mut events) = self.events.lock() else {
            return;
        };
        // Meter updates and repeated identical errors are replaceable rather
        // than cumulative: a consumer only ever wants the latest. Coalescing
        // them keeps a noisy session from pushing everything else out of a
        // bounded queue.
        let replaceable = match &event {
            RelayEvent::AudioLevel { id, .. } => {
                let id = *id;
                events.iter_mut().find(|queued| {
                    matches!(queued, RelayEvent::AudioLevel { id: queued, .. } if *queued == id)
                })
            }
            RelayEvent::Error { message } => {
                // Only a recent identical error is folded away; a genuinely
                // new message always gets through.
                let recent = events.len().saturating_sub(8);
                events.iter_mut().skip(recent).find(|queued| {
                    matches!(queued, RelayEvent::Error { message: queued } if queued.as_str() == message.as_str())
                })
            }
            _ => None,
        };
        if let Some(slot) = replaceable {
            *slot = event;
            return;
        }
        events.push_back(event);
        while events.len() > MAX_QUEUED_EVENTS {
            events.pop_front();
        }
    }

    /// Whether `addr` may attempt a pairing right now.
    pub(crate) fn pairing_allowed(&self, addr: IpAddr) -> bool {
        let Ok(mut failures) = self.pairing_failures.lock() else {
            return true;
        };
        match failures.get(&addr) {
            Some(record) if record.locked_until > Instant::now() => false,
            Some(record)
                if record.locked_until <= Instant::now()
                    && record.count >= PAIRING_ATTEMPT_LIMIT =>
            {
                // The lockout expired; give the peer a fresh budget.
                failures.remove(&addr);
                true
            }
            _ => true,
        }
    }

    /// Record a failed pairing, locking the source out once it has burned
    /// through its budget.
    pub(crate) fn note_pairing_failure(&self, addr: IpAddr) {
        let Ok(mut failures) = self.pairing_failures.lock() else {
            return;
        };
        let now = Instant::now();
        // Keep the table bounded if many addresses probe. Expired records are
        // discarded first; if a hostile burst has filled the table with
        // active lockouts, evict the least recently seen source so a new
        // address cannot grow the map past the limit.
        if !failures.contains_key(&addr) && failures.len() >= MAX_PAIRING_FAILURE_RECORDS {
            failures.retain(|_, record| record.locked_until > now);
            if failures.len() >= MAX_PAIRING_FAILURE_RECORDS {
                let oldest = failures
                    .iter()
                    .min_by_key(|(_, record)| record.last_seen)
                    .map(|(address, _)| *address);
                if let Some(oldest) = oldest {
                    failures.remove(&oldest);
                }
            }
        }
        let record = failures.entry(addr).or_insert(FailureRecord {
            count: 0,
            locked_until: now,
            last_seen: now,
        });
        record.count += 1;
        record.last_seen = now;
        if record.count >= PAIRING_ATTEMPT_LIMIT {
            record.locked_until = now + PAIRING_LOCKOUT;
        }
    }

    /// Forget a source's failures after it pairs successfully.
    pub(crate) fn clear_pairing_failures(&self, addr: IpAddr) {
        if let Ok(mut failures) = self.pairing_failures.lock() {
            failures.remove(&addr);
        }
    }

    pub(crate) fn candidate_allowed(&self, peer_id: &str, addr: SocketAddr) -> bool {
        let Ok(mut failures) = self.candidate_failures.lock() else {
            return true;
        };
        let key = (peer_id.to_owned(), addr);
        let Some(record) = failures.get(&key) else {
            return true;
        };
        if record.locked_until <= Instant::now() {
            failures.remove(&key);
            true
        } else {
            false
        }
    }

    pub(crate) fn note_candidate_failure(&self, peer_id: &str, addr: SocketAddr) {
        let Ok(mut failures) = self.candidate_failures.lock() else {
            return;
        };
        let now = Instant::now();
        let key = (peer_id.to_owned(), addr);
        if !failures.contains_key(&key) && failures.len() >= MAX_TRUSTED_CANDIDATE_FAILURES {
            failures.retain(|_, record| record.locked_until > now);
            if failures.len() >= MAX_TRUSTED_CANDIDATE_FAILURES {
                if let Some(oldest) = failures
                    .iter()
                    .min_by_key(|(_, record)| record.last_seen)
                    .map(|(key, _)| key.clone())
                {
                    failures.remove(&oldest);
                }
            }
        }
        let record = failures.entry(key).or_insert(FailureRecord {
            count: 0,
            locked_until: now,
            last_seen: now,
        });
        record.count = record.count.saturating_add(1);
        record.last_seen = now;
        let exponent = record.count.saturating_sub(1).min(6);
        let delay = Duration::from_millis(500u64.saturating_mul(1u64 << exponent));
        record.locked_until = now + delay.min(Duration::from_secs(30));
    }

    pub(crate) fn note_candidate_success(&self, peer_id: &str, addr: SocketAddr) {
        if let Ok(mut failures) = self.candidate_failures.lock() {
            failures.remove(&(peer_id.to_owned(), addr));
        }
        if let Ok(mut addresses) = self.last_successful_addresses.lock() {
            if !addresses.contains_key(peer_id)
                && addresses.len() >= MAX_TRUSTED_SUCCESSFUL_ADDRESSES
            {
                if let Some(oldest) = addresses.keys().next().cloned() {
                    addresses.remove(&oldest);
                }
            }
            addresses.insert(peer_id.to_owned(), addr);
        }
    }

    pub(crate) fn last_successful_address(&self, peer_id: &str) -> Option<SocketAddr> {
        self.last_successful_addresses
            .lock()
            .ok()
            .and_then(|addresses| addresses.get(peer_id).copied())
    }

    pub(crate) fn discovered_link(&self, addr: SocketAddr) -> Option<LinkKind> {
        self.peer_links.lock().ok()?.get(&addr).copied()
    }

    pub(crate) fn drain_events(&self) -> Vec<RelayEvent> {
        self.events
            .lock()
            .map(|mut events| events.drain(..).collect())
            .unwrap_or_default()
    }

    pub(crate) fn config(&self) -> EngineConfig {
        self.config
            .lock()
            .map(|config| config.clone())
            .unwrap_or_default()
    }

    pub(crate) fn trusted_secret(&self, peer_id: &str) -> Option<[u8; 32]> {
        self.trusted_peers.lock().ok()?.get(peer_id).copied()
    }

    pub(crate) fn remember_trusted_peer(&self, peer_id: String, secret: [u8; 32]) {
        if let Ok(mut peers) = self.trusted_peers.lock() {
            peers.insert(peer_id, secret);
        }
    }

    /// Start a host-side durable enrollment transaction. The secret is kept
    /// only in the bounded transaction table until the embedding confirms
    /// that its own durable store has committed it.
    pub(crate) fn begin_trusted_enrollment(
        &self,
        session_id: SessionId,
        peer_id: String,
        peer: PeerInfo,
        secret: [u8; 32],
    ) -> Result<u64, String> {
        if peer_id.trim().is_empty() || peer_id != peer.id {
            return Err("trusted peer identity did not match the session".into());
        }
        if secret.iter().all(|byte| *byte == 0) {
            return Err("trusted credential was malformed".into());
        }
        if self.trusted_secret(&peer_id).is_none()
            && self
                .trusted_peers
                .lock()
                .map(|peers| peers.len() >= MAX_TRUSTED_PEERS)
                .unwrap_or(true)
        {
            return Err("trusted credential capacity has been reached".into());
        }
        let now = Instant::now();
        let mut pending = self
            .pending_enrollments
            .lock()
            .map_err(|_| "trusted enrollment state is locked".to_string())?;
        pending.retain(|_, enrollment| {
            now.duration_since(enrollment.created) < TRUST_ENROLLMENT_TIMEOUT
        });
        if pending.len() >= MAX_PENDING_TRUST_ENROLLMENTS {
            return Err("too many trusted enrollments are pending".into());
        }
        if pending
            .values()
            .any(|enrollment| enrollment.peer_id == peer_id || enrollment.session_id == session_id)
        {
            return Err("a trusted enrollment for this peer is already pending".into());
        }
        let transaction_id = self.next_enrollment.fetch_add(1, Ordering::Relaxed);
        pending.insert(
            transaction_id,
            PendingEnrollment {
                session_id,
                peer_id,
                secret,
                created: now,
                decision: EnrollmentDecision::Pending,
            },
        );
        Ok(transaction_id)
    }

    /// Return a transaction's secret to the embedding that owns durable
    /// storage. Callers should copy it only for the persistence operation and
    /// then immediately accept or reject the transaction.
    pub(crate) fn trusted_enrollment_secret(&self, transaction_id: u64) -> Option<[u8; 32]> {
        let pending = self.pending_enrollments.lock().ok()?;
        let enrollment = pending.get(&transaction_id)?;
        (enrollment.created.elapsed() < TRUST_ENROLLMENT_TIMEOUT).then_some(enrollment.secret)
    }

    pub(crate) fn accept_trusted_enrollment(&self, transaction_id: u64) -> RelayResult<()> {
        let mut pending = self
            .pending_enrollments
            .lock()
            .map_err(|_| RelayError::Engine("trusted enrollment state is locked".into()))?;
        let enrollment = pending
            .get_mut(&transaction_id)
            .ok_or_else(|| RelayError::Engine("trusted enrollment expired or is unknown".into()))?;
        if enrollment.created.elapsed() >= TRUST_ENROLLMENT_TIMEOUT {
            pending.remove(&transaction_id);
            return Err(RelayError::Engine("trusted enrollment expired".into()));
        }
        match &enrollment.decision {
            EnrollmentDecision::Pending => enrollment.decision = EnrollmentDecision::Accepted,
            EnrollmentDecision::Accepted => {}
            EnrollmentDecision::Rejected(_) => {
                return Err(RelayError::Engine("trusted enrollment was rejected".into()))
            }
        }
        Ok(())
    }

    pub(crate) fn reject_trusted_enrollment(
        &self,
        transaction_id: u64,
        reason: String,
    ) -> RelayResult<()> {
        let mut pending = self
            .pending_enrollments
            .lock()
            .map_err(|_| RelayError::Engine("trusted enrollment state is locked".into()))?;
        let enrollment = pending
            .get_mut(&transaction_id)
            .ok_or_else(|| RelayError::Engine("trusted enrollment expired or is unknown".into()))?;
        if enrollment.created.elapsed() >= TRUST_ENROLLMENT_TIMEOUT {
            pending.remove(&transaction_id);
            return Err(RelayError::Engine("trusted enrollment expired".into()));
        }
        if matches!(&enrollment.decision, EnrollmentDecision::Accepted) {
            return Err(RelayError::Engine(
                "trusted enrollment was already accepted".into(),
            ));
        }
        enrollment.decision = EnrollmentDecision::Rejected(if reason.trim().is_empty() {
            "trusted enrollment rejected".into()
        } else {
            reason
        });
        Ok(())
    }

    /// Resolve one transaction belonging to a session. Expiry is resolved as
    /// a rejection, so the client never waits forever for a host callback.
    pub(crate) fn take_trusted_enrollment(
        &self,
        session_id: SessionId,
    ) -> Option<EnrollmentResolution> {
        let mut pending = self.pending_enrollments.lock().ok()?;
        let transaction_id = pending
            .iter()
            .filter(|(_, enrollment)| enrollment.session_id == session_id)
            .min_by_key(|(_, enrollment)| enrollment.created)
            .map(|(id, _)| *id)?;
        let enrollment = pending.get(&transaction_id)?;
        let expired = enrollment.created.elapsed() >= TRUST_ENROLLMENT_TIMEOUT;
        let accepted = matches!(&enrollment.decision, EnrollmentDecision::Accepted);
        let reason = match &enrollment.decision {
            EnrollmentDecision::Rejected(reason) => Some(reason.clone()),
            EnrollmentDecision::Pending if expired => Some("trusted enrollment timed out".into()),
            _ => None,
        };
        if !accepted && reason.is_none() {
            return None;
        }
        let enrollment = pending.remove(&transaction_id)?;
        Some(EnrollmentResolution {
            peer_id: enrollment.peer_id,
            secret: enrollment.secret,
            accepted,
            reason,
        })
    }

    pub(crate) fn remove_trusted_peer(&self, peer_id: &str) -> bool {
        let removed = self
            .trusted_peers
            .lock()
            .ok()
            .and_then(|mut peers| peers.remove(peer_id))
            .is_some();
        let ids = self
            .sessions
            .lock()
            .map(|sessions| {
                sessions
                    .values()
                    .filter(|record| record.peer.id == peer_id)
                    .map(|record| record.id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for id in &ids {
            session::teardown(self, *id, "trusted peer was revoked".into());
        }
        if let Ok(mut pending) = self.pending_enrollments.lock() {
            pending.retain(|_, enrollment| enrollment.peer_id != peer_id);
        }
        removed || !ids.is_empty()
    }

    pub(crate) fn trusted_peers(&self) -> Vec<TrustedPeer> {
        self.trusted_peers
            .lock()
            .map(|peers| {
                peers
                    .iter()
                    .map(|(peer_id, secret)| TrustedPeer {
                        peer_id: peer_id.clone(),
                        secret: *secret,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn next_session_id(&self) -> SessionId {
        SessionId(self.next_session.fetch_add(1, Ordering::Relaxed))
    }

    pub(crate) fn insert_session(&self, record: Arc<SessionRecord>) -> bool {
        let limit = self.config().max_sessions;
        let Ok(mut sessions) = self.sessions.lock() else {
            return false;
        };
        // Session IDs are allocated monotonically, but rejecting a duplicate
        // here also prevents a test/backend mistake from replacing a live
        // record and bypassing the bound.
        if sessions.contains_key(&record.id) || sessions.len() >= limit {
            return false;
        }
        let id = record.id;
        sessions.insert(id, Arc::clone(&record));
        // Publish snapshot for realtime readers. This is rare (session
        // insert) so a brief extra lock is not on the hot audio path.
        if let Ok(mut snap) = self.realtime_snapshot.lock() {
            *snap = Arc::new(sessions.values().cloned().collect());
        }
        true
    }

    pub(crate) fn session(&self, id: SessionId) -> Option<Arc<SessionRecord>> {
        self.sessions.lock().ok()?.get(&id).cloned()
    }

    pub(crate) fn session_count(&self) -> usize {
        self.sessions
            .lock()
            .map(|sessions| sessions.len())
            .unwrap_or(0)
    }

    /// Claim a pre-authentication handshake slot, or `None` when the host is
    /// already handling as many as it allows.
    pub(crate) fn claim_handshake(self: &Arc<Self>) -> Option<HandshakeSlot> {
        let limit = self.config().max_pending_handshakes.max(1) as u64;
        let mut current = self.pending_handshakes.load(Ordering::Relaxed);
        loop {
            if current >= limit {
                return None;
            }
            match self.pending_handshakes.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(HandshakeSlot {
                        inner: Arc::clone(self),
                    })
                }
                Err(observed) => current = observed,
            }
        }
    }

    pub(crate) fn session_alive(&self, id: SessionId) -> bool {
        self.running.load(Ordering::Relaxed)
            && self
                .sessions
                .lock()
                .map(|sessions| sessions.contains_key(&id))
                .unwrap_or(false)
    }

    /// Fan captured audio out to every transmitting session, converting the
    /// engine's local geometry into each session's negotiated one.
    ///
    /// `samples` are interleaved in the local format
    /// ([`EngineConfig::local_sample_rate`] / `local_channels`). Sessions may
    /// each have negotiated something different, so the conversion is
    /// per-session and stateful; its buffers are reused, so the realtime path
    /// does not allocate after warm-up.
    pub(crate) fn broadcast_capture(&self, samples: &[f32], realtime: bool) -> bool {
        // Realtime path avoids the global sessions mutex entirely by reading
        // a pre-published snapshot. The snapshot is updated only on session
        // insert/remove (rare), so try_lock almost always succeeds. On
        // contention we reuse the thread-local cached snapshot rather than
        // turning audio into silence.
        let snapshot: Arc<Vec<Arc<SessionRecord>>> = if realtime {
            match self.realtime_snapshot.try_lock() {
                Ok(guard) => {
                    let arc = Arc::clone(&*guard);
                    REALTIME_SNAPSHOT_CACHE.with(|c| *c.borrow_mut() = Some(Arc::clone(&arc)));
                    arc
                }
                Err(_) => {
                    if let Some(cached) = REALTIME_SNAPSHOT_CACHE.with(|c| c.borrow().clone()) {
                        cached
                    } else {
                        return false;
                    }
                }
            }
        } else {
            let Ok(sessions) = self.sessions.lock() else {
                return false;
            };
            // Build a temporary snapshot for the non-realtime path to share
            // the same iteration logic without holding the map lock across
            // per-session work.
            Arc::new(sessions.values().cloned().collect())
        };
        let mut accepted = true;
        let mut found = false;
        for record in snapshot.iter().filter(|record| record.sending) {
            found = true;
            let converted = if realtime {
                record.capture_convert.try_lock().ok()
            } else {
                record.capture_convert.lock().ok()
            };
            let Some(mut converted) = converted else {
                accepted = false;
                continue;
            };
            let (converter, buffer) = &mut *converted;
            if converter.is_identity() {
                // Avoid the copy on the common matched-geometry path.
                accepted &= if realtime {
                    record.outgoing.try_push(samples)
                } else {
                    record.outgoing.push(samples);
                    true
                };
                continue;
            }
            if realtime {
                if !converter.try_convert_prepared(samples, buffer) {
                    accepted = false;
                    continue;
                }
            } else {
                converter.convert(samples, buffer);
            }
            accepted &= if realtime {
                record.outgoing.try_push(buffer)
            } else {
                record.outgoing.push(buffer);
                true
            };
        }
        found && accepted
    }

    /// Total decoded audio waiting for playback across receiving sessions,
    /// paired with the queue depth the receive workers trim to.
    ///
    /// This is the drift-control signal for a playback consumer that runs on
    /// its own clock (the Windows render endpoint): the peer's capture clock
    /// and the local render clock disagree by tens of parts per million, and
    /// an uncorrected consumer drifts into repeated underruns or into the
    /// queues' drop-oldest trim. It locks the session table, so it belongs to
    /// control-rate callers, not realtime callbacks.
    pub(crate) fn playback_levels(&self) -> (usize, usize) {
        let Ok(sessions) = self.sessions.lock() else {
            return (0, 0);
        };
        let mut depth = 0usize;
        let mut target = 0usize;
        for record in sessions.values().filter(|record| record.receiving) {
            depth += record.incoming.len();
            target += record.incoming.target_depth();
        }
        (depth, target)
    }

    /// Sum every receiving session's decoded audio into `out`.
    ///
    /// Each session decodes into its own queue in the engine's local format,
    /// so mixing is a plain sum. Sharing one queue — as an earlier version
    /// did — concatenated two peers' audio into one stream rather than
    /// mixing it, and let either peer resize the other's playback buffer.
    pub(crate) fn mix_playback(&self, out: &mut [f32], realtime: bool) -> usize {
        if out.is_empty() {
            return 0;
        }
        let snapshot: Arc<Vec<Arc<SessionRecord>>> = if realtime {
            match self.realtime_snapshot.try_lock() {
                Ok(guard) => {
                    let arc = Arc::clone(&*guard);
                    REALTIME_SNAPSHOT_CACHE.with(|c| *c.borrow_mut() = Some(Arc::clone(&arc)));
                    arc
                }
                Err(_) => {
                    if let Some(cached) = REALTIME_SNAPSHOT_CACHE.with(|c| c.borrow().clone()) {
                        cached
                    } else {
                        return 0;
                    }
                }
            }
        } else {
            let Ok(sessions) = self.sessions.lock() else {
                return 0;
            };
            Arc::new(sessions.values().cloned().collect())
        };
        // Iterate the snapshot directly. Collecting the receiving sessions into a
        // `Vec` first — as this used to — allocated on the PipeWire process
        // callback, on a path whose entire contract is that it does not.
        let mut receiving = snapshot.iter().filter(|record| record.receiving);
        let Some(first) = receiving.next() else {
            return 0;
        };
        let Some(second) = receiving.next() else {
            // One session is the overwhelmingly common case; skip the scratch
            // buffer and the summing loop entirely.
            return if realtime {
                first.incoming.try_pull(out)
            } else {
                first.incoming.pull(out)
            };
        };

        let scratch = if realtime {
            self.mix_scratch.try_lock().ok()
        } else {
            self.mix_scratch.lock().ok()
        };
        let Some(mut scratch) = scratch else {
            return 0;
        };
        // `mix_scratch` is allocated at [`MAX_REALTIME_QUANTUM_SAMPLES`] when
        // the engine is built, so for any realtime caller this resize is a
        // length change inside existing capacity. A caller that ignores that
        // bound and hands over something longer would otherwise reallocate
        // here, so instead it mixes into the part that is already backed and
        // reports only that much.
        let usable = if realtime {
            if scratch.capacity() < out.len() {
                scratch.capacity()
            } else {
                out.len()
            }
        } else {
            out.len()
        };
        if usable == 0 {
            return 0;
        }
        scratch.clear();
        scratch.resize(usable, 0.0);
        out[..usable].fill(0.0);

        let mut produced = 0;
        // `first` and `second` are already pulled off the iterator; chaining
        // them back on keeps one loop body without building a collection.
        for record in [first, second].into_iter().chain(receiving) {
            let count = if realtime {
                record.incoming.try_pull(&mut scratch[..])
            } else {
                record.incoming.pull(&mut scratch[..])
            };
            for (slot, sample) in out.iter_mut().zip(scratch.iter()).take(count) {
                *slot += *sample;
            }
            produced = produced.max(count);
        }
        // Summed peers can exceed full scale; clamping is far less
        // objectionable than the wraparound a raw sum would hand to an
        // integer conversion downstream.
        for sample in out.iter_mut().take(produced) {
            *sample = sample.clamp(-1.0, 1.0);
        }
        produced
    }

    pub(crate) fn remove_session(&self, id: SessionId) -> Option<Arc<SessionRecord>> {
        let record = {
            let mut sessions = self.sessions.lock().ok()?;
            let record = sessions.remove(&id);
            if let Some(record) = &record {
                record.stop.store(true, Ordering::Relaxed);
            }
            // Update realtime snapshot after removal.
            if let Ok(mut snap) = self.realtime_snapshot.lock() {
                *snap = Arc::new(sessions.values().cloned().collect());
            }
            record
        };
        record
    }

    pub(crate) fn status(&self) -> EngineStatus {
        let host = self.host.lock().ok().and_then(|host| {
            host.as_ref()
                .map(|record| (record.port, record.bind_addr()))
        });
        let (host_port, host_addr) = host
            .map(|(port, addr)| (Some(port), addr))
            .unwrap_or((None, None));
        let config = self.config();
        let sessions = self
            .sessions
            .lock()
            .map(|sessions| {
                sessions
                    .values()
                    .map(|record| {
                        let control_state = record
                            .control_state
                            .lock()
                            .map(|state| match *state {
                                ControlState::Active => "active",
                                ControlState::ResumeEligible { .. } => "resume-eligible",
                                ControlState::Resuming { .. } => "resuming",
                            })
                            .unwrap_or("unknown")
                            .to_string();
                        let audio_over_tcp = record.tcp_audio.is_some();
                        let audio_channel_state = if let Some(audio) = &record.tcp_audio {
                            if audio.is_active() {
                                "active"
                            } else {
                                "reconnecting"
                            }
                        } else {
                            "active"
                        };
                        let remote_addr = record
                            .control_peer_addr
                            .lock()
                            .ok()
                            .map(|addr| *addr)
                            .unwrap_or(record.peer.addr);
                        let link = if config.transport == TransportPreference::Adb {
                            "loopback"
                        } else {
                            self.discovered_link(remote_addr)
                                .map(LinkKind::as_str)
                                .unwrap_or("unknown")
                        };
                        let local_addr = record
                            .udp_audio
                            .as_ref()
                            .and_then(|socket| socket.local_addr());
                        let trusted = record
                            .trust_secret
                            .lock()
                            .ok()
                            .and_then(|secret| *secret)
                            .is_some()
                            || self.trusted_secret(&record.peer.id).is_some();
                        SessionStatus {
                            id: record.id,
                            peer: record.peer.clone(),
                            roles: record.roles,
                            codec: record.codec,
                            sending: record.sending,
                            receiving: record.receiving,
                            transport: if audio_over_tcp { "adb-tcp" } else { "udp" }.into(),
                            link: link.into(),
                            local_addr,
                            remote_addr,
                            control_state,
                            audio_channel_state: audio_channel_state.into(),
                            trusted,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        EngineStatus {
            host_active: host_port.is_some(),
            host_port,
            host_addr,
            sessions,
        }
    }

    pub(crate) fn stop_all(&self) {
        self.running.store(false, Ordering::Relaxed);
        self.stop_advertiser();
        self.stop_browser();
        self.stop_usb_scanner();
        session::stop_host(self);
        let ids: Vec<SessionId> = self
            .sessions
            .lock()
            .map(|sessions| sessions.keys().copied().collect())
            .unwrap_or_default();
        for id in ids {
            session::teardown(self, id, "engine stopped".into());
        }
    }
}
