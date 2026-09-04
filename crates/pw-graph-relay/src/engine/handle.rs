//! The public engine and the handle an embedder holds.
//!
//! The handle owns the engine's lifetime; dropping it stops the workers.

use super::*;

/// Releases a pre-authentication handshake slot when the handshake thread
/// ends, however it ends.
pub(crate) struct HandshakeSlot {
    pub(crate) inner: Arc<EngineInner>,
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        self.inner.pending_handshakes.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The relay engine. Owns background threads until [`RelayEngine::shutdown`].
pub struct RelayEngine {
    pub(crate) inner: Arc<EngineInner>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiscoveryStartOutcome {
    MdnsAndUsb,
    MdnsOnly,
    UsbOnly,
}

/// Decide whether discovery can remain usable when one of its independent
/// mechanisms fails. Keeping this decision separate from worker startup makes
/// the partial-failure contract explicit and testable.
pub(crate) fn discovery_start_outcome(
    mdns: &RelayResult<()>,
    usb: &RelayResult<()>,
) -> RelayResult<DiscoveryStartOutcome> {
    match (mdns.is_ok(), usb.is_ok()) {
        (true, true) => Ok(DiscoveryStartOutcome::MdnsAndUsb),
        (true, false) => Ok(DiscoveryStartOutcome::MdnsOnly),
        (false, true) => Ok(DiscoveryStartOutcome::UsbOnly),
        (false, false) => Err(RelayError::Engine(format!(
            "all relay discovery mechanisms failed (mDNS: {}; USB: {})",
            mdns.as_ref().unwrap_err(),
            usb.as_ref().unwrap_err()
        ))),
    }
}

impl RelayEngine {
    /// Create the engine. No sockets open until `host_start`/`connect`.
    pub fn start(config: EngineConfig) -> RelayResult<Self> {
        Ok(Self {
            inner: EngineInner::new(config),
        })
    }

    pub fn handle(&self) -> RelayHandle {
        RelayHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Stop the host, end all sessions, and mark the engine stopped.
    /// Background threads observe the stop within about a second.
    pub fn shutdown(&self) {
        self.inner.stop_all();
    }
}

impl Drop for RelayEngine {
    fn drop(&mut self) {
        self.inner.stop_all();
    }
}

/// Cheap cloneable handle used by the embedding application.
#[derive(Clone)]
pub struct RelayHandle {
    pub(crate) inner: Arc<EngineInner>,
}

impl RelayHandle {
    /// Replace the engine configuration. Safe to call while idle; hosts
    /// re-read the PIN per pairing attempt.
    pub fn update_config(&self, config: EngineConfig) {
        if let Ok(mut slot) = self.inner.config.lock() {
            *slot = config;
        }
        if let Ok(mut peers) = self.inner.trusted_peers.lock() {
            peers.clear();
            for peer in self
                .inner
                .config()
                .trusted_peers
                .into_iter()
                .take(MAX_TRUSTED_PEERS)
            {
                peers.insert(peer.peer_id, peer.secret);
            }
        }
    }

    pub fn config(&self) -> EngineConfig {
        self.inner.config()
    }

    /// Start listening for clients. Returns the bound TCP control port.
    pub fn host_start(&self) -> RelayResult<u16> {
        let config = self.inner.config();
        let pin = config.pin.trim();
        if pin.is_empty() {
            return Err(RelayError::Engine(
                "a pairing PIN must be configured before hosting".into(),
            ));
        }
        if pin.len() < pairing::PIN_LENGTH {
            return Err(RelayError::Engine(format!(
                "the pairing PIN must be at least {} characters",
                pairing::PIN_LENGTH
            )));
        }
        let mut host = self
            .inner
            .host
            .lock()
            .map_err(|_| RelayError::Engine("host state is locked".into()))?;
        if host.is_some() {
            return Err(RelayError::Engine(
                "the relay host is already running".into(),
            ));
        }
        let record = session::start_host(&self.inner, config.port)?;
        let port = record.port;
        let bind_addr = record.bind_addr();
        *host = Some(record);
        drop(host);
        // Advertise over mDNS so peers can find us (best-effort).
        self.inner.start_advertiser(port, bind_addr);
        self.inner.emit(RelayEvent::HostStarted { port });
        Ok(port)
    }

    pub fn host_stop(&self) -> RelayResult<()> {
        let removed = {
            let mut host = self
                .inner
                .host
                .lock()
                .map_err(|_| RelayError::Engine("host state is locked".into()))?;
            host.take()
        };
        if let Some(record) = removed {
            record.stop();
            self.inner.stop_advertiser();
            self.inner.emit(RelayEvent::HostStopped);
        }
        Ok(())
    }

    /// Connect to a host as a client. The handshake runs on a background
    /// thread; success or failure arrives as a [`RelayEvent`]. The returned
    /// id is valid immediately.
    pub fn connect(&self, target: SocketAddr, pin: &str, roles: Roles) -> SessionId {
        let id = self.inner.next_session_id();
        session::connect_peer(&self.inner, id, target, pin.to_owned(), roles);
        id
    }

    /// Connect using a credential created by a previous explicit PIN pairing.
    /// The stable peer id is part of the authenticated transcript, so a
    /// credential cannot be replayed against an unrelated discovered host.
    pub fn connect_trusted(
        &self,
        target: SocketAddr,
        peer_id: &str,
        secret: [u8; 32],
        roles: Roles,
    ) -> SessionId {
        let id = self.inner.next_session_id();
        session::connect_trusted_peer(&self.inner, id, target, peer_id.to_owned(), secret, roles);
        id
    }

    /// Queue an authenticated direction offer for an established session.
    ///
    /// The control watcher sends it on its next tick and retains it until the
    /// peer acknowledges or supersedes it. The embedding owns persistence of
    /// `generation`; reusing a generation for a different direction is
    /// rejected so an old offline request cannot reverse a newer choice.
    pub fn offer_direction(
        &self,
        id: SessionId,
        direction: RelayDirection,
        generation: u64,
    ) -> RelayResult<()> {
        let config = self.inner.config();
        if config.device_id.trim().is_empty() {
            return Err(RelayError::Config(
                "direction offers require a stable device id".into(),
            ));
        }
        let record = self
            .inner
            .session(id)
            .ok_or_else(|| RelayError::Engine(format!("unknown relay session {id}")))?;
        let result = record
            .direction
            .lock()
            .map_err(|_| RelayError::Engine("direction state is locked".into()))?
            .queue(DirectionOffer {
                generation,
                direction,
                device_id: config.device_id,
            })
            .map_err(RelayError::Config);
        result
    }

    /// Return the secret held by a pending host enrollment transaction. The
    /// embedding must durably commit this value before accepting the
    /// transaction. It is intentionally not carried in the request event.
    pub fn trusted_enrollment_secret(&self, transaction_id: u64) -> Option<[u8; 32]> {
        self.inner.trusted_enrollment_secret(transaction_id)
    }

    /// Commit a pending trusted enrollment after durable application
    /// persistence has succeeded. Only after the control thread observes this
    /// decision does it import the credential and send TrustAccepted.
    pub fn accept_trusted_enrollment(&self, transaction_id: u64) -> RelayResult<()> {
        self.inner.accept_trusted_enrollment(transaction_id)
    }

    /// Reject a pending trusted enrollment. The client is not told to retain
    /// the credential and the live trusted map is unchanged.
    pub fn reject_trusted_enrollment(
        &self,
        transaction_id: u64,
        reason: impl Into<String>,
    ) -> RelayResult<()> {
        self.inner
            .reject_trusted_enrollment(transaction_id, reason.into())
    }

    /// Remove a trusted identity immediately from the live engine. An
    /// embedding should remove the same record from its durable store too.
    pub fn remove_trusted_peer(&self, peer_id: &str) -> RelayResult<()> {
        if peer_id.trim().is_empty() {
            return Err(RelayError::Config(
                "trusted peer id must not be empty".into(),
            ));
        }
        self.inner.remove_trusted_peer(peer_id);
        Ok(())
    }

    /// Snapshot of live trusted identities. Secrets are returned only because
    /// this API is used to rebuild an engine's authenticated configuration;
    /// status and event JSON never expose them.
    pub fn trusted_peers(&self) -> Vec<TrustedPeer> {
        self.inner.trusted_peers()
    }

    /// Begin browsing for relay hosts on the local network. Discovered peers
    /// arrive as [`RelayEvent::PeerDiscovered`]. Runs mDNS alongside a direct
    /// probe of USB tether subnets, because mDNS often does not cross a USB
    /// tether. Idempotent.
    pub fn discovery_start(&self) -> RelayResult<()> {
        // mDNS and direct USB probing are independent mechanisms. A multicast
        // failure is expected on some tethered networks and must not prevent
        // the mechanism designed for exactly that case from starting.
        let mdns = self.inner.start_browser();
        let usb = self.inner.start_usb_scanner();
        match discovery_start_outcome(&mdns, &usb)? {
            DiscoveryStartOutcome::MdnsAndUsb => Ok(()),
            DiscoveryStartOutcome::MdnsOnly => {
                let error = usb.expect_err("mDNS-only discovery must have a USB error");
                self.inner.emit(RelayEvent::Error {
                    message: format!("USB relay discovery unavailable: {error}"),
                });
                Ok(())
            }
            DiscoveryStartOutcome::UsbOnly => {
                let error = mdns.expect_err("USB-only discovery must have an mDNS error");
                self.inner.emit(RelayEvent::Error {
                    message: format!("mDNS relay discovery unavailable: {error}"),
                });
                Ok(())
            }
        }
    }

    /// Stop browsing for relay hosts. Idempotent.
    pub fn discovery_stop(&self) {
        self.inner.stop_browser();
        self.inner.stop_usb_scanner();
        self.inner.clear_discovered();
    }

    /// Forget direct USB-probe results while leaving mDNS browsing active.
    /// Platform link watchers call this as soon as a tether disappears; the
    /// scanner also performs the same refresh when its next loop observes it.
    pub fn discovery_usb_link_lost(&self) {
        self.inner.lost_peer(usb_probe::USB_PROBE_SERVICE);
    }

    /// Snapshot of relay hosts discovered so far.
    pub fn discovered_peers(&self) -> Vec<PeerInfo> {
        self.inner.discovered_peers()
    }

    /// Discovery snapshot with non-authoritative link classification for
    /// candidate ranking and diagnostics. The link is public metadata; only a
    /// successful authenticated handshake establishes peer identity.
    pub fn discovered_peer_candidates(&self) -> Vec<(PeerInfo, Option<LinkKind>)> {
        let peers = self.inner.discovered_peers();
        let links = self.inner.peer_links.lock().ok();
        peers
            .into_iter()
            .map(|peer| {
                let link = links
                    .as_ref()
                    .and_then(|links| links.get(&peer.addr).copied());
                (peer, link)
            })
            .collect()
    }

    /// Supply peer addresses discovered by an embedding-owned browser.
    ///
    /// Some platform adapters keep discovery in a separate engine (for
    /// example, an Android browser handle that must outlive client handles).
    /// Feeding its identity-tagged snapshot into the client engine lets an
    /// in-progress session resume over a newly visible address without
    /// allowing an unrelated host with the same port to become a target.
    pub fn update_discovered_peers(&self, peers: Vec<PeerInfo>) {
        self.inner
            .refresh_service("embedding-discovery._qpw-relay._udp.local.", peers);
    }

    /// Supply an embedding-owned discovery snapshot with link hints for
    /// candidate ranking. The hints are public routing metadata only; resume
    /// and trusted authentication still prove the peer identity.
    pub fn update_discovered_peer_candidates(&self, peers: Vec<(PeerInfo, Option<LinkKind>)>) {
        self.inner.refresh_embedding_candidates(peers);
    }

    /// End a session gracefully.
    pub fn disconnect(&self, id: SessionId) -> RelayResult<()> {
        session::request_bye(&self.inner, id);
        Ok(())
    }

    /// Drain pending events. Call once per update tick.
    pub fn events(&self) -> Vec<RelayEvent> {
        self.inner.drain_events()
    }

    /// Publish a non-fatal error from an embedding audio endpoint or worker.
    ///
    /// Relay transports already surface their background failures through the
    /// event queue; platform adapters use the same path so a stopped capture
    /// or render thread cannot look like a healthy, silent session.
    pub fn report_error(&self, message: impl Into<String>) {
        self.inner.emit(RelayEvent::Error {
            message: message.into(),
        });
    }

    /// Feed audio to transmit (e.g. the virtual relay sink tap). Oldest
    /// samples are dropped when the queue overflows.
    pub fn push_capture(&self, samples: &[f32]) {
        self.inner.broadcast_capture(samples, false);
    }

    /// Realtime-safe variant of [`Self::push_capture`].
    ///
    /// Returns `false` without touching the input when `samples` exceeds
    /// [`MAX_REALTIME_QUANTUM_SAMPLES`], when a realtime lock is busy, or
    /// when no session accepts capture. A successful call enqueues the whole
    /// quantum for each available session; bounded queues may drop their
    /// oldest samples when full.
    pub fn try_push_capture(&self, samples: &[f32]) -> bool {
        if samples.len() > MAX_REALTIME_QUANTUM_SAMPLES {
            return false;
        }
        self.inner.broadcast_capture(samples, true)
    }

    /// Take decoded audio received from peers (e.g. into the virtual relay
    /// microphone), mixed across sessions and converted into the engine's
    /// local format. Returns the number of samples written to `out`.
    pub fn pull_playback(&self, out: &mut [f32]) -> usize {
        self.inner.mix_playback(out, false)
    }

    /// Realtime-safe variant of [`Self::pull_playback`].
    ///
    /// Returns zero when a realtime lock is busy or no audio is available.
    /// At most [`MAX_REALTIME_QUANTUM_SAMPLES`] samples are produced; an
    /// oversized output slice is short-served and its tail is untouched.
    pub fn try_pull_playback(&self, out: &mut [f32]) -> usize {
        let usable = out.len().min(MAX_REALTIME_QUANTUM_SAMPLES);
        self.inner.mix_playback(&mut out[..usable], true)
    }

    /// Decoded audio currently waiting for playback, summed across receiving
    /// sessions, paired with the depth those queues are trimmed to.
    ///
    /// A pull loop that runs on its own consumer clock — the Windows render
    /// endpoint, unlike PipeWire's clock-following streams — compares this
    /// backlog against the target over time to learn whether the peer's
    /// capture clock runs ahead of or behind the local render clock, and
    /// micro-adjusts its read rate accordingly. Locks the session table, so
    /// call it at control rate (a few times a second), never from a realtime
    /// callback.
    pub fn playback_levels(&self) -> (usize, usize) {
        self.inner.playback_levels()
    }

    pub fn status(&self) -> EngineStatus {
        self.inner.status()
    }
}
