//! One live session's shared state.
//!
//! Every worker thread for a session -- control, receive, transmit -- holds
//! an `Arc` of this record and checks its stop flag, which is what bounds
//! shutdown to roughly one socket timeout.

use super::*;

/// Canonical emitter negotiation state. The legacy direction state below is
/// kept beside it for one compatibility period so an upgraded peer can still
/// resume a session established by the previous protocol surface.
#[derive(Clone, Debug)]
pub(crate) struct FlowNegotiation {
    pub local: FlowOffer,
    pub resolved: FlowOffer,
    pub remote: Option<FlowOffer>,
    pub pending: Option<FlowOffer>,
    pub sent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FlowResolution {
    pub winner: FlowOffer,
    pub changed: bool,
}

impl FlowNegotiation {
    fn new(local: FlowOffer, pending: bool) -> Self {
        Self {
            resolved: local.clone(),
            local: local.clone(),
            remote: None,
            pending: pending.then_some(local),
            sent: false,
        }
    }

    fn queue(&mut self, offer: FlowOffer) -> Result<(), String> {
        if !offer.is_valid() {
            return Err("flow offer requires stable emitter and proposer ids".into());
        }
        if offer.proposer_id != self.local.proposer_id {
            return Err("flow offer identity does not match the session owner".into());
        }
        if offer.generation < self.local.generation {
            return Err("flow offer generation is older than the local offer".into());
        }
        if offer.generation == self.local.generation && offer.emitter_id != self.local.emitter_id {
            return Err("flow changes must increment the offer generation".into());
        }
        if offer == self.local {
            if self.pending.is_none() && self.resolved != offer {
                self.pending = Some(offer);
                self.sent = false;
            }
            return Ok(());
        }
        self.local = offer.clone();
        self.pending = Some(offer);
        self.sent = false;
        Ok(())
    }

    fn pending(&self) -> Option<FlowOffer> {
        (!self.sent).then(|| self.pending.clone()).flatten()
    }

    fn mark_sent(&mut self, offer: &FlowOffer) {
        if self.pending.as_ref() == Some(offer) {
            self.sent = true;
        }
    }

    fn reset_send(&mut self) {
        if self.pending.is_some() {
            self.sent = false;
        }
    }

    fn receive_offer(
        &mut self,
        remote: FlowOffer,
    ) -> Result<(FlowAck, Option<FlowResolution>), String> {
        if !remote.is_valid() {
            return Err("flow offer requires stable emitter and proposer ids".into());
        }
        self.remote = Some(match self.remote.take() {
            Some(previous) => resolve_flow_offers(&previous, &remote),
            None => remote.clone(),
        });
        let candidate = resolve_flow_offers(&self.local, &remote);
        let winner = resolve_flow_offers(&self.resolved, &candidate);
        let changed = winner != self.resolved;
        self.resolved = winner.clone();
        if winner != self.local {
            self.pending = None;
            self.sent = false;
        }
        Ok((
            FlowAck {
                generation: winner.generation,
                emitter_id: winner.emitter_id.clone(),
            },
            changed.then_some(FlowResolution { winner, changed }),
        ))
    }

    fn receive_ack(&mut self, ack: FlowAck, authenticated_peer_id: &str) -> Option<FlowResolution> {
        if !ack.is_valid() {
            return None;
        }
        let pending = self.pending.clone()?;
        let matches_local =
            ack.generation == pending.generation && ack.emitter_id == pending.emitter_id;
        let remote = FlowOffer {
            generation: ack.generation,
            emitter_id: ack.emitter_id,
            proposer_id: authenticated_peer_id.to_owned(),
        };
        let remote_is_current = match self.remote.as_ref() {
            Some(known) if known.generation > remote.generation => false,
            Some(known) if known.generation == remote.generation => {
                known.emitter_id == remote.emitter_id
            }
            _ => true,
        };
        let matches_remote =
            !matches_local && remote_is_current && resolve_flow_offers(&pending, &remote) == remote;
        if !matches_local && !matches_remote {
            return None;
        }
        let previous = self.resolved.clone();
        let winner = if matches_remote {
            resolve_flow_offers(&pending, &remote)
        } else {
            pending
        };
        let winner = resolve_flow_offers(&previous, &winner);
        let changed = winner != previous;
        self.resolved = winner.clone();
        self.pending = None;
        self.sent = false;
        Some(FlowResolution { winner, changed })
    }
}

/// Per-session state for the authenticated direction control plane.
///
/// `resolved` is the last deterministic winner. `local` is this installation's
/// newest proposal, while `pending` remains queued until the control thread
/// has put it on the wire. Keeping the proposal in the session record means a
/// brief control-link drop cannot lose a direction request before resume.
#[derive(Clone, Debug)]
pub(crate) struct DirectionNegotiation {
    pub local: DirectionOffer,
    pub resolved: DirectionOffer,
    pub remote: Option<DirectionOffer>,
    pub pending: Option<DirectionOffer>,
    /// Whether the queued offer has been written on the current control
    /// channel. It stays retained until its matching acknowledgement arrives
    /// so a reconnect can resend it without losing an offline choice.
    pub sent: bool,
    /// Canonical emitter negotiation. `None` means this record has only
    /// spoken to a legacy peer so far.
    pub flow: Option<FlowNegotiation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirectionResolution {
    pub winner: DirectionOffer,
    pub changed: bool,
}

impl DirectionNegotiation {
    pub(crate) fn new(local: DirectionOffer) -> Self {
        Self {
            resolved: local.clone(),
            local: local.clone(),
            remote: None,
            pending: Some(local),
            sent: false,
            flow: None,
        }
    }

    pub(crate) fn with_initial_flow(
        local: DirectionOffer,
        local_device_id: &str,
        peer_id: &str,
        local_is_emitter: bool,
        generation: u64,
    ) -> Self {
        let mut state = Self::new(local);
        let emitter_id = if local_is_emitter {
            local_device_id
        } else {
            peer_id
        };
        let offer = FlowOffer {
            generation,
            emitter_id: emitter_id.to_owned(),
            proposer_id: local_device_id.to_owned(),
        };
        // The initial flow is derived from the authenticated session roles;
        // it does not need a second control round trip. Later user changes
        // set `pending` and use the authenticated FlowOffer/FlowAck exchange.
        state.flow = Some(FlowNegotiation::new(offer, false));
        state
    }

    fn ensure_flow(
        &mut self,
        local_device_id: &str,
        peer_id: &str,
        local_is_emitter: bool,
        generation: u64,
    ) -> &mut FlowNegotiation {
        self.flow.get_or_insert_with(|| {
            let emitter_id = if local_is_emitter {
                local_device_id
            } else {
                peer_id
            };
            FlowNegotiation::new(
                FlowOffer {
                    generation,
                    emitter_id: emitter_id.to_owned(),
                    proposer_id: local_device_id.to_owned(),
                },
                false,
            )
        })
    }

    pub(crate) fn queue(&mut self, offer: DirectionOffer) -> Result<(), String> {
        if !offer.is_valid() {
            return Err("direction offer requires a stable device id".into());
        }
        if offer.device_id != self.local.device_id {
            return Err("direction offer identity does not match the session owner".into());
        }
        if offer.generation < self.local.generation {
            return Err("direction offer generation is older than the local offer".into());
        }
        if offer.generation == self.local.generation && offer.direction != self.local.direction {
            return Err("direction changes must increment the offer generation".into());
        }
        if offer == self.local {
            if self.pending.is_none() && self.resolved != offer {
                self.pending = Some(offer);
                self.sent = false;
            }
            return Ok(());
        }
        self.local = offer.clone();
        self.pending = Some(offer);
        self.sent = false;
        Ok(())
    }

    pub(crate) fn pending(&self) -> Option<DirectionOffer> {
        (!self.sent).then(|| self.pending.clone()).flatten()
    }

    pub(crate) fn mark_sent(&mut self, offer: &DirectionOffer) {
        if self.pending.as_ref() == Some(offer) {
            self.sent = true;
        }
    }

    pub(crate) fn reset_send(&mut self) {
        if self.pending.is_some() {
            self.sent = false;
        }
    }

    pub(crate) fn receive_offer(
        &mut self,
        remote: DirectionOffer,
    ) -> Result<(DirectionAck, Option<DirectionResolution>), String> {
        if !remote.is_valid() {
            return Err("direction offer requires a stable device id".into());
        }
        self.remote = Some(match self.remote.take() {
            Some(previous) => resolve_direction_offers(&previous, &remote),
            None => remote.clone(),
        });
        let candidate = resolve_direction_offers(&self.local, &remote);
        let winner = resolve_direction_offers(&self.resolved, &candidate);
        let changed = winner != self.resolved;
        self.resolved = winner.clone();
        // A remote winner supersedes a queued local proposal. If the local
        // proposal wins, retain it until the peer acknowledges that result.
        if winner != self.local {
            self.pending = None;
            self.sent = false;
        }
        Ok((
            DirectionAck {
                generation: winner.generation,
                direction: winner.direction,
            },
            changed.then_some(DirectionResolution { winner, changed }),
        ))
    }

    pub(crate) fn receive_ack(
        &mut self,
        ack: DirectionAck,
        authenticated_peer_id: &str,
    ) -> Option<DirectionResolution> {
        let pending = self.pending.clone()?;
        let matches_local =
            ack.generation == pending.generation && ack.direction == pending.direction;
        let remote = DirectionOffer {
            generation: ack.generation,
            direction: ack.direction,
            device_id: authenticated_peer_id.to_owned(),
        };
        let remote_is_current = remote.is_valid()
            && match self.remote.as_ref() {
                Some(known) if known.generation > remote.generation => false,
                Some(known) if known.generation == remote.generation => {
                    known.direction == remote.direction
                }
                _ => true,
            };
        let matches_remote = !matches_local
            && remote_is_current
            && resolve_direction_offers(&pending, &remote) == remote;
        if !matches_local && !matches_remote {
            return None;
        }
        let previous = self.resolved.clone();
        let winner = if matches_remote {
            resolve_direction_offers(&pending, &remote)
        } else {
            pending
        };
        let winner = resolve_direction_offers(&previous, &winner);
        let changed = winner != previous;
        self.resolved = winner.clone();
        self.pending = None;
        self.sent = false;
        Some(DirectionResolution { winner, changed })
    }
}

/// Internal session bookkeeping shared with worker threads.
pub(crate) struct SessionRecord {
    pub id: SessionId,
    /// Session identifier assigned by the host and used on resume.
    pub wire_id: u64,
    pub peer: PeerInfo,
    pub roles: Roles,
    pub codec: CodecKind,
    pub format: AudioFormat,
    /// This side sends audio (peer receives).
    pub sending: bool,
    /// This side receives audio (peer sends).
    pub receiving: bool,
    /// Current local one-way role. The legacy booleans above describe the
    /// handshake that created the record; this atomic is updated after an
    /// authenticated flow switch so realtime audio paths change without
    /// taking the session-table lock.
    pub active_roles: AtomicU8,
    pub stop: Arc<AtomicBool>,
    /// Set by `disconnect`; the control thread sends `bye` and tears down.
    pub bye_requested: AtomicBool,
    /// Identifies the control-key generation currently in use. Host-side
    /// grace waits compare generations to notice a replacement.
    pub control_generation: AtomicU64,
    /// Derived from the original PAKE shared secret. It is never transmitted
    /// and is used only for challenge-response resumption.
    pub resume_secret: [u8; 32],
    /// Credential generated by the client for the peer, or learned by the
    /// host from an authenticated enrollment message.
    pub trust_secret: Mutex<Option<[u8; 32]>>,
    /// Authenticated replacement stream for ADB-only operation. `None`
    /// means the session uses UDP audio.
    pub tcp_audio: Option<Arc<session::TcpAudioSlot>>,
    /// Interface-scoped UDP socket, replaceable after authenticated resume.
    pub udp_audio: Option<Arc<session::UdpAudioSlot>>,
    /// Current authenticated control peer address. The stable discovery
    /// address in `peer` remains the identity label; this field reports the
    /// path actually carrying the live control session.
    pub control_peer_addr: Mutex<SocketAddr>,
    /// Explicitly gates resume takeover and serializes racing attempts.
    pub control_state: Mutex<ControlState>,
    /// UDP address of the peer's audio socket.
    ///
    /// Only ever updated from a datagram that authenticated against this
    /// session's audio key. Learning it from any syntactically valid packet,
    /// as an earlier version did, let anyone who could reach the port
    /// redirect our outbound audio to themselves.
    pub peer_audio_addr: Mutex<Option<SocketAddr>>,
    /// Per-session transmit queue so one capture stream fans out to every
    /// receiving peer without competing consumers. Holds audio already
    /// converted into *this session's* negotiated format.
    pub outgoing: PcmQueue,
    /// Per-session receive queue holding audio converted into the engine's
    /// local format, ready to be mixed with the other sessions'.
    pub incoming: PcmQueue,
    /// Local-format-to-session-format conversion for the transmit path, with
    /// its reusable output buffer. One converter per session: they have
    /// independent geometries and independent interpolation state.
    pub capture_convert: Mutex<(Converter, Vec<f32>)>,
    /// Seals this session's outgoing datagrams. Shared between the transmit
    /// worker and the announce path, because a single nonce counter per key
    /// is what keeps the AEAD safe.
    pub audio_sealer: Mutex<Sealer>,
    /// Opens this session's incoming datagrams and tracks its replay window.
    pub audio_opener: Mutex<Opener>,
    /// Authenticated direction proposals and the last deterministic winner.
    pub direction: Mutex<DirectionNegotiation>,
}

impl SessionRecord {
    pub(crate) fn role_bits(roles: Roles) -> u8 {
        (roles.emit as u8) | ((roles.receive as u8) << 1)
    }

    pub(crate) fn local_roles(&self) -> Roles {
        match self.active_roles.load(Ordering::Acquire) {
            1 => Roles::emit_only(),
            2 => Roles::receive_only(),
            3 => Roles::both(),
            _ => Roles::default(),
        }
    }

    pub(crate) fn set_local_roles(&self, roles: Roles) {
        // Never publish a bidirectional active state, even if a future caller
        // accidentally supplies one. A session is always exactly one-way.
        let roles = if roles.is_one_way() {
            roles
        } else {
            Roles::default()
        };
        self.active_roles
            .store(Self::role_bits(roles), Ordering::Release);
    }

    pub(crate) fn local_device_id(&self) -> Option<String> {
        self.direction
            .lock()
            .ok()
            .map(|direction| direction.local.device_id.clone())
    }

    pub(crate) fn flow(&self) -> Option<RelayFlow> {
        self.direction
            .lock()
            .ok()
            .and_then(|direction| direction.flow.as_ref().map(|flow| flow.resolved.flow()))
    }

    pub(crate) fn local_mode(&self) -> Option<RelayMode> {
        let local_id = self.local_device_id()?;
        self.flow()?.mode_for(&local_id)
    }

    pub(crate) fn queue_flow_offer(&self, offer: FlowOffer) -> Result<(), String> {
        let local_id = self
            .local_device_id()
            .ok_or_else(|| "flow state is locked".to_string())?;
        let mut direction = self
            .direction
            .lock()
            .map_err(|_| "flow state is locked".to_string())?;
        let current_generation = direction
            .flow
            .as_ref()
            .map(|flow| flow.local.generation)
            .unwrap_or(direction.local.generation);
        let local_is_emitter = self.local_roles().emit;
        direction
            .ensure_flow(
                &local_id,
                &self.peer.id,
                local_is_emitter,
                current_generation,
            )
            .queue(offer)
    }

    pub(crate) fn pending_flow_offer(&self) -> Option<FlowOffer> {
        let local_id = self.local_device_id()?;
        let mut direction = self.direction.lock().ok()?;
        let current_generation = direction
            .flow
            .as_ref()
            .map(|flow| flow.local.generation)
            .unwrap_or(direction.local.generation);
        let local_is_emitter = self.local_roles().emit;
        direction
            .ensure_flow(
                &local_id,
                &self.peer.id,
                local_is_emitter,
                current_generation,
            )
            .pending()
    }

    pub(crate) fn mark_flow_offer_sent(&self, offer: &FlowOffer) {
        if let Ok(mut direction) = self.direction.lock() {
            if let Some(flow) = direction.flow.as_mut() {
                flow.mark_sent(offer);
            }
        }
    }

    pub(crate) fn reset_flow_offer_send(&self) {
        if let Ok(mut direction) = self.direction.lock() {
            if let Some(flow) = direction.flow.as_mut() {
                flow.reset_send();
            }
        }
    }

    pub(crate) fn receive_flow_offer(
        &self,
        offer: FlowOffer,
    ) -> Result<(FlowAck, Option<FlowResolution>), String> {
        if offer.proposer_id != self.peer.id {
            return Err("flow offer identity does not match the authenticated peer".into());
        }
        let local_id = self
            .local_device_id()
            .ok_or_else(|| "flow state is locked".to_string())?;
        let mut direction = self
            .direction
            .lock()
            .map_err(|_| "flow state is locked".to_string())?;
        let current_generation = direction
            .flow
            .as_ref()
            .map(|flow| flow.local.generation)
            .unwrap_or(direction.local.generation);
        let local_is_emitter = self.local_roles().emit;
        direction
            .ensure_flow(
                &local_id,
                &self.peer.id,
                local_is_emitter,
                current_generation,
            )
            .receive_offer(offer)
    }

    pub(crate) fn receive_flow_ack(&self, ack: FlowAck) -> Option<FlowResolution> {
        self.direction
            .lock()
            .ok()?
            .flow
            .as_mut()?
            .receive_ack(ack, &self.peer.id)
    }

    pub(crate) fn apply_flow(&self, flow: &RelayFlow) {
        let Some(local_id) = self.local_device_id() else {
            return;
        };
        if let Some(mode) = flow.mode_for(&local_id) {
            self.set_local_roles(mode.roles());
        }
    }

    pub(crate) fn pending_direction_offer(&self) -> Option<DirectionOffer> {
        self.direction.lock().ok()?.pending()
    }

    pub(crate) fn mark_direction_offer_sent(&self, offer: &DirectionOffer) {
        if let Ok(mut direction) = self.direction.lock() {
            direction.mark_sent(offer);
        }
    }

    pub(crate) fn reset_direction_offer_send(&self) {
        if let Ok(mut direction) = self.direction.lock() {
            direction.reset_send();
        }
    }

    pub(crate) fn receive_direction_offer(
        &self,
        offer: DirectionOffer,
    ) -> Result<(DirectionAck, Option<DirectionResolution>), String> {
        if offer.device_id != self.peer.id {
            return Err("direction offer identity does not match the authenticated peer".into());
        }
        self.direction
            .lock()
            .map_err(|_| "direction state is locked".to_string())?
            .receive_offer(offer)
    }

    pub(crate) fn receive_direction_ack(&self, ack: DirectionAck) -> Option<DirectionResolution> {
        self.direction.lock().ok()?.receive_ack(ack, &self.peer.id)
    }

    /// Mark the current control owner as gone. Calling this more than once is
    /// harmless while the grace period is in progress.
    pub(crate) fn mark_control_dropped(&self) -> bool {
        let Ok(mut state) = self.control_state.lock() else {
            return false;
        };
        match *state {
            ControlState::Active => {
                let generation = self.control_generation.load(Ordering::Acquire);
                *state = ControlState::ResumeEligible { generation };
                true
            }
            ControlState::ResumeEligible { .. } => true,
            ControlState::Resuming { .. } => false,
        }
    }

    /// Claim one eligible resume generation. A session whose old control
    /// channel is active cannot be claimed, and only one challenge may be in
    /// flight at once.
    pub(crate) fn begin_resume(&self) -> Option<u64> {
        let Ok(mut state) = self.control_state.lock() else {
            return None;
        };
        let ControlState::ResumeEligible { generation } = *state else {
            return None;
        };
        let next = generation.checked_add(1)?;
        *state = ControlState::Resuming { generation: next };
        Some(next)
    }

    /// Return a failed resume attempt to the eligible state without rotating
    /// the live control generation.
    pub(crate) fn cancel_resume(&self, generation: u64) {
        if let Ok(mut state) = self.control_state.lock() {
            if *state == (ControlState::Resuming { generation }) {
                let current = self.control_generation.load(Ordering::Acquire);
                *state = ControlState::ResumeEligible {
                    generation: current,
                };
            }
        }
    }

    /// Commit a successful resume and rotate the control-key generation.
    pub(crate) fn finish_resume(&self, generation: u64) -> bool {
        let Ok(mut state) = self.control_state.lock() else {
            return false;
        };
        if *state != (ControlState::Resuming { generation }) {
            return false;
        }
        self.control_generation.store(generation, Ordering::Release);
        *state = ControlState::Active;
        true
    }

    /// End a grace period without allowing its old control watcher to tear
    /// down a session that has already been resumed. The state transition is
    /// serialized with `finish_resume`, so the watcher and the new owner
    /// cannot both decide the session's fate. An in-flight challenge remains
    /// in progress until it succeeds or cancels itself.
    pub(crate) fn expire_resume_grace(&self, generation: u64) -> ResumeGraceResult {
        let Ok(mut state) = self.control_state.lock() else {
            return ResumeGraceResult::InProgress {
                generation: self.control_generation.load(Ordering::Acquire),
            };
        };
        if self.control_generation.load(Ordering::Acquire) != generation {
            return ResumeGraceResult::Resumed;
        }
        match *state {
            ControlState::ResumeEligible {
                generation: current,
            } if current == generation => {
                *state = ControlState::Active;
                ResumeGraceResult::Expired
            }
            ControlState::Resuming { generation } => ResumeGraceResult::InProgress { generation },
            _ => ResumeGraceResult::Resumed,
        }
    }

    /// Abort a challenge that has remained in flight beyond the bounded
    /// handshake timeout. The caller will tear down the session; a successful
    /// finisher racing this method wins or loses under the same state lock.
    pub(crate) fn abort_resume(&self, generation: u64) -> bool {
        let Ok(mut state) = self.control_state.lock() else {
            return false;
        };
        if *state == (ControlState::Resuming { generation }) {
            *state = ControlState::Active;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod direction_tests {
    use super::*;

    fn offer(generation: u64, direction: RelayDirection, device_id: &str) -> DirectionOffer {
        DirectionOffer {
            generation,
            direction,
            device_id: device_id.into(),
        }
    }

    #[test]
    fn an_offer_stays_queued_until_its_matching_acknowledgement() {
        let local = offer(0, RelayDirection::MobileToDesktop, "phone");
        let mut state = DirectionNegotiation::new(local.clone());
        assert_eq!(state.pending(), Some(local.clone()));

        state.mark_sent(&local);
        assert_eq!(state.pending(), None);
        state.reset_send();
        assert_eq!(state.pending(), Some(local.clone()));

        let resolution = state.receive_ack(
            DirectionAck {
                generation: 0,
                direction: RelayDirection::MobileToDesktop,
            },
            "desktop",
        );
        assert_eq!(resolution.unwrap().winner, local);
        assert_eq!(state.pending(), None);
        assert!(state
            .receive_ack(
                DirectionAck {
                    generation: 0,
                    direction: RelayDirection::DesktopToMobile,
                },
                "a-desktop",
            )
            .is_none());
    }

    #[test]
    fn a_losing_local_offer_adopts_the_authenticated_remote_ack() {
        let mut state =
            DirectionNegotiation::new(offer(4, RelayDirection::MobileToDesktop, "phone"));
        let resolution = state.receive_ack(
            DirectionAck {
                generation: 5,
                direction: RelayDirection::DesktopToMobile,
            },
            "desktop",
        );

        assert_eq!(
            resolution.unwrap().winner,
            offer(5, RelayDirection::DesktopToMobile, "desktop")
        );
        assert!(state.pending().is_none());
    }

    #[test]
    fn stale_and_same_generation_direction_reversals_are_rejected() {
        let mut state =
            DirectionNegotiation::new(offer(3, RelayDirection::DesktopToMobile, "phone"));
        assert!(state
            .queue(offer(2, RelayDirection::MobileToDesktop, "phone"))
            .is_err());
        assert!(state
            .queue(offer(3, RelayDirection::MobileToDesktop, "phone"))
            .is_err());
    }

    #[test]
    fn a_newer_remote_offer_cannot_be_reversed_by_an_older_message() {
        let mut state =
            DirectionNegotiation::new(offer(2, RelayDirection::DesktopToMobile, "phone"));
        let (ack, resolution) = state
            .receive_offer(offer(1, RelayDirection::MobileToDesktop, "desktop"))
            .unwrap();
        assert_eq!(ack.generation, 2);
        assert_eq!(ack.direction, RelayDirection::DesktopToMobile);
        assert!(resolution.is_none());
        assert_eq!(state.resolved.direction, RelayDirection::DesktopToMobile);
    }

    #[test]
    fn simultaneous_offers_converge_on_the_same_authenticated_winner() {
        let left_offer = offer(7, RelayDirection::MobileToDesktop, "aaa");
        let right_offer = offer(7, RelayDirection::DesktopToMobile, "bbb");
        let mut left = DirectionNegotiation::new(left_offer.clone());
        let mut right = DirectionNegotiation::new(right_offer.clone());

        let (left_ack, _) = left.receive_offer(right_offer.clone()).unwrap();
        let (right_ack, _) = right.receive_offer(left_offer).unwrap();
        assert_eq!(left_ack.generation, right_ack.generation);
        assert_eq!(left_ack.direction, right_ack.direction);
        assert_eq!(left.resolved, right.resolved);
        assert_eq!(left.resolved, right_offer);
    }
}
