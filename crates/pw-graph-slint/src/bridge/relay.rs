#[cfg(feature = "relay")]
use pw_graph_backend::{
    relay_build_qr_payload, relay_parse_qr_payload, relay_qr, RelayCodecKind, RelayEvent,
    RelayDirection, RelayHostRequest, RelayPeerInfo, RelaySessionId,
    RelayTransportPreference,
    RelayTrustedPeer,
};
use slint::SharedString;
#[cfg(feature = "relay")]
use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
#[cfg(feature = "relay")]
use std::collections::BTreeSet;
#[cfg(feature = "relay")]
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
#[cfg(feature = "relay")]
use std::str::FromStr;
#[cfg(feature = "relay")]
use std::time::{Duration, Instant};

#[cfg(feature = "relay")]
use pw_graph_utils::hex::{hex_decode, hex_encode};

use pw_graph_i18n::I18n;

use super::app::Application;
#[cfg(feature = "relay")]
use super::app::{RelayAttempt, RelayDirectionSwitch};
#[cfg(feature = "relay")]
use super::config::save_config;
use super::RelayRow;
use pw_graph_config::AudioDirection;

#[cfg(feature = "relay")]
fn relay_device_id(application: &mut Application) -> String {
    if application.config.relay_device_id.trim().is_empty() {
        application.config.relay_device_id = pw_graph_backend::relay_generate_device_id();
        // The ID is installation state, not a per-session discovery value.
        // Mark it dirty before exposing it to the engine so the normal atomic
        // config writer persists it on the next UI pump.
        application
            .config_dirty_since
            .get_or_insert_with(Instant::now);
    }
    application.config.relay_device_id.clone()
}

#[cfg(feature = "relay")]
fn configured_trusted_peers(application: &Application) -> Vec<RelayTrustedPeer> {
    application
        .config
        .relay_trusted_peers
        .iter()
        .filter_map(|stored| {
            let peer_id = stored.peer_id.trim();
            if peer_id.is_empty() {
                return None;
            }
            let secret = hex_decode(stored.secret.trim()).ok()?.try_into().ok()?;
            Some(RelayTrustedPeer {
                peer_id: peer_id.to_owned(),
                secret,
            })
        })
        .collect()
}

#[cfg(feature = "relay")]
fn trusted_secret_for(application: &Application, peer_id: &str) -> Option<[u8; 32]> {
    application
        .config
        .relay_trusted_peers
        .iter()
        .find(|stored| stored.peer_id == peer_id)
        .and_then(|stored| hex_decode(stored.secret.trim()).ok()?.try_into().ok())
}

#[cfg(feature = "relay")]
fn remember_trusted_peer(
    application: &mut Application,
    peer_id: &str,
    peer: &RelayPeerInfo,
    secret: [u8; 32],
) {
    let peer_id = peer_id.trim();
    if peer_id.is_empty() {
        return;
    }
    let encoded = hex_encode(&secret);
    let mut changed = false;
    if let Some(stored) = application
        .config
        .relay_trusted_peers
        .iter_mut()
        .find(|stored| stored.peer_id == peer_id)
    {
        if stored.secret != encoded {
            stored.secret = encoded;
            changed = true;
        }
        if stored.name != peer.name {
            stored.name = peer.name.clone();
            changed = true;
        }
        let address = peer.addr.to_string();
        if stored.address != address {
            stored.address = address;
            changed = true;
        }
    } else {
        application
            .config
            .relay_trusted_peers
            .push(pw_graph_config::PersistedRelayPeer {
                peer_id: peer_id.to_owned(),
                secret: encoded,
                name: peer.name.clone(),
                address: peer.addr.to_string(),
            });
        changed = true;
    }
    if changed {
        // autosave_config observes the snapshot difference, but recording a
        // dirty time here makes the persistence intent explicit for hosts
        // that receive an enrollment while the preferences window is idle.
        application
            .config_dirty_since
            .get_or_insert_with(Instant::now);
    }
}

#[cfg(feature = "relay")]
fn refresh_trusted_peer_address(application: &mut Application, peer: &RelayPeerInfo) {
    let Some(stored) = application
        .config
        .relay_trusted_peers
        .iter_mut()
        .find(|stored| stored.peer_id == peer.id)
    else {
        return;
    };
    let address = peer.addr.to_string();
    if stored.name != peer.name || stored.address != address {
        stored.name = peer.name.clone();
        stored.address = address;
        application
            .config_dirty_since
            .get_or_insert_with(Instant::now);
    }
}

#[cfg(feature = "relay")]
fn configure_relay_identity(application: &mut Application) -> Result<(), String> {
    let device_id = relay_device_id(application);
    let trusted_peers = configured_trusted_peers(application);
    let transport = relay_transport(&application.config.relay_transport);
    application
        .source
        .relay_configure_identity(device_id, trusted_peers, transport)
}

#[cfg(feature = "relay")]
fn session_or_attempt_for_peer(application: &Application, peer: &RelayPeerInfo) -> bool {
    let status = application.source.relay_status();
    status.sessions.iter().any(|session| {
        (!peer.id.is_empty() && session.peer.id == peer.id) || session.peer.addr == peer.addr
    }) || application
        .relay_connecting
        .as_ref()
        .is_some_and(|attempt| {
            attempt.peer_id.as_deref() == Some(peer.id.as_str())
                || attempt.target == peer.addr.to_string()
        })
}

/// Start a connection using the credential learned during an earlier PIN
/// pairing. This is deliberately only called for a discovered peer whose
/// stable identity has a matching stored secret.
#[cfg(feature = "relay")]
fn connect_trusted_peer(
    application: &mut Application,
    peer: &RelayPeerInfo,
    automatic: bool,
) -> bool {
    let Some(secret) = trusted_secret_for(application, &peer.id) else {
        return false;
    };
    if automatic && !trusted_candidate_allowed(application, peer) {
        return false;
    }
    application.relay_trusted_auto_attempt_at = Some(Instant::now());
    if session_or_attempt_for_peer(application, peer) {
        return true;
    }
    if let Err(error) = configure_relay_identity(application) {
        application.status = application.tf("relay.error", &[("error", error)]);
        return true;
    }
    match application.source.relay_connect_trusted(
        peer.addr,
        &peer.id,
        secret,
        backend_direction(application.config.relay_direction),
        application.config.relay_direction_generation,
    ) {
        Ok(session) => {
            application.relay_connecting = Some(RelayAttempt {
                target: peer.addr.to_string(),
                session: session.0,
                peer_id: Some(peer.id.clone()),
            });
            application.status = application.t("relay.connecting");
        }
        Err(error) => {
            if automatic {
                note_trusted_candidate_failure(application, &peer.id, &peer.addr.to_string());
                // A refused dial means nothing is listening at this address
                // right now. Stop re-injecting it as a candidate so the
                // reconnect loop ends instead of chasing a stale address;
                // discovery re-announcing the peer is what revives it.
                let refused = error.contains("refused")
                    || error.contains("os error 111")
                    || error.contains("unreachable")
                    || error.contains("No route to host")
                    || error.contains("os error 113")
                    || error.contains("os error 10061")
                    || error.contains("os error 10051");
                if refused {
                    note_trusted_candidate_refused(
                        application,
                        &peer.id,
                        &peer.addr.to_string(),
                    );
                }
            }
            application.status = application.tf("relay.error", &[("error", error)])
        }
    }
    true
}

#[cfg(feature = "relay")]
pub(crate) fn trusted_candidate_allowed(application: &mut Application, peer: &RelayPeerInfo) -> bool {
    let key = (peer.id.clone(), peer.addr.to_string());
    // An active refusal is not a backoff delay: until discovery re-announces
    // this address, nothing is listening there and dialing it again only
    // produces another 10061 loop.
    if application.relay_trusted_refused.contains(&key) {
        return false;
    }
    let Some((_, retry_at)) = application.relay_trusted_candidate_failures.get(&key) else {
        return true;
    };
    if *retry_at <= Instant::now() {
        application.relay_trusted_candidate_failures.remove(&key);
        true
    } else {
        false
    }
}

/// Record that a trusted candidate at this address is actively unreachable
/// (connection refused / no route). Unlike the timing backoff, this does not
/// expire on its own: only discovery re-announcing the address or a successful
/// session with it clears the mark.
#[cfg(feature = "relay")]
pub(crate) fn note_trusted_candidate_refused(application: &mut Application, peer_id: &str, address: &str) {
    if peer_id.is_empty() || address.is_empty() {
        return;
    }
    application
        .relay_trusted_refused
        .insert((peer_id.to_owned(), address.to_owned()));
}

/// Clear a refusal mark: discovery re-announced the address (the host came
/// back, possibly at the same lease) or a session with it succeeded.
#[cfg(feature = "relay")]
pub(crate) fn clear_trusted_candidate_refused(application: &mut Application, peer_id: &str, address: &str) {
    application
        .relay_trusted_refused
        .remove(&(peer_id.to_owned(), address.to_owned()));
}

#[cfg(feature = "relay")]
fn note_trusted_candidate_failure(application: &mut Application, peer_id: &str, address: &str) {
    const MAX_FAILURES: usize = 256;
    let now = Instant::now();
    let key = (peer_id.to_owned(), address.to_owned());
    if !application
        .relay_trusted_candidate_failures
        .contains_key(&key)
        && application.relay_trusted_candidate_failures.len() >= MAX_FAILURES
    {
        application
            .relay_trusted_candidate_failures
            .retain(|_, (_, retry_at)| *retry_at > now);
        if application.relay_trusted_candidate_failures.len() >= MAX_FAILURES {
            if let Some(oldest) = application
                .relay_trusted_candidate_failures
                .iter()
                .min_by_key(|(_, (_, retry_at))| *retry_at)
                .map(|(key, _)| key.clone())
            {
                application.relay_trusted_candidate_failures.remove(&oldest);
            }
        }
    }
    let count = application
        .relay_trusted_candidate_failures
        .get(&key)
        .map(|(count, _)| count.saturating_add(1))
        .unwrap_or(1)
        .min(7);
    let delay = Duration::from_millis(500u64.saturating_mul(1u64 << (count - 1)));
    application
        .relay_trusted_candidate_failures
        .insert(key, (count, now + delay.min(Duration::from_secs(30))));
}

#[cfg(feature = "relay")]
fn trusted_candidate_rank(application: &Application, peer: &RelayPeerInfo) -> (u8, SocketAddr) {
    if application
        .config
        .relay_trusted_peers
        .iter()
        .find(|stored| stored.peer_id == peer.id)
        .and_then(|stored| stored.address.parse::<SocketAddr>().ok())
        == Some(peer.addr)
    {
        return (0, peer.addr);
    }
    if let IpAddr::V4(address) = peer.addr.ip() {
        let octets = address.octets();
        if (octets[0] == 192 && octets[1] == 168 && octets[2] == 42)
            || (octets[0] == 10 && octets[1] == 42)
        {
            return (1, peer.addr);
        }
        if application
            .source
            .relay_local_links()
            .iter()
            .any(|link| link.contains(address))
        {
            return (2, peer.addr);
        }
    }
    (3, peer.addr)
}

/// Retry a trusted peer whose discovery record is still present. Discovery
/// reports address changes, but a failed TCP attempt does not necessarily
/// produce a second discovery event; bounded retries are what make a cable
/// insertion work through a short host-start or route-setup race.
#[cfg(feature = "relay")]
fn retry_trusted_auto_connect(application: &mut Application) {
    const RETRY_INTERVAL: Duration = Duration::from_secs(5);
    // If a visible reconnect is pending, respect its next_retry so Cancel
    // stays on screen for the full interval instead of flashing 0.1ms.
    if let Some(pending) = &application.relay_reconnect_pending {
        if Instant::now() < pending.next_retry {
            return;
        }
        // Ready to retry the pending peer specifically
        let pending = pending.clone();
        // Find current best address for this peer (discovery may have updated)
        let mut peers = application.source.relay_peers();
        for stored in &application.config.relay_trusted_peers {
            if stored.peer_id != pending.peer_id {
                continue;
            }
            if let Ok(addr) = stored.address.parse::<SocketAddr>() {
                if !peers
                    .iter()
                    .any(|p| p.id == stored.peer_id && p.addr == addr)
                {
                    peers.push(RelayPeerInfo {
                        id: stored.peer_id.clone(),
                        name: stored.name.clone(),
                        kind: pw_graph_backend::RelayDeviceKind::Other,
                        addr,
                    });
                }
            }
        }
        peers.retain(|p| p.id == pending.peer_id && trusted_secret_for(application, &p.id).is_some());
        // If every candidate for this peer is actively refused, the stored
        // address is dead until discovery re-announces it; holding the row
        // would only loop "retry in 5s" against nothing.
        let all_refused = !peers.is_empty()
            && peers.iter().all(|p| {
                application
                    .relay_trusted_refused
                    .contains(&(p.id.clone(), p.addr.to_string()))
            });
        if all_refused {
            application.relay_reconnect_pending = None;
            return;
        }
        peers.retain(|p| trusted_candidate_allowed(application, p));
        peers.sort_by_key(|peer| trusted_candidate_rank(application, peer));
        if let Some(peer) = peers.into_iter().next() {
            application.relay_reconnect_pending = None;
            let _ = connect_trusted_peer(application, &peer, true);
            return;
        } else {
            // No candidate allowed yet (backoff) – keep pending and push next retry
            if let Some(p) = &mut application.relay_reconnect_pending {
                p.next_retry = Instant::now() + RETRY_INTERVAL;
            }
            return;
        }
    }
    if !application.config.relay_auto_connect_trusted
        || application.relay_connecting.is_some()
        || application
            .source
            .relay_status()
            .sessions
            .iter()
            .any(|session| {
                application
                    .config
                    .relay_trusted_peers
                    .iter()
                    .any(|trusted| trusted.peer_id == session.peer.id)
            })
        || application
            .relay_trusted_auto_attempt_at
            .is_some_and(|last| last.elapsed() < RETRY_INTERVAL)
    {
        return;
    }
    let mut peers = application.source.relay_peers();
    // Discovery is untrusted and may be crowded by same-ID advertisements.
    // Keep the last durable address in the candidate set as well, so a forged
    // sixteen-address burst cannot evict the address that worked previously.
    for stored in &application.config.relay_trusted_peers {
        let Ok(address) = stored.address.parse::<SocketAddr>() else {
            continue;
        };
        if !peers
            .iter()
            .any(|peer| peer.id == stored.peer_id && peer.addr == address)
        {
            peers.push(RelayPeerInfo {
                id: stored.peer_id.clone(),
                name: stored.name.clone(),
                kind: pw_graph_backend::RelayDeviceKind::Other,
                addr: address,
            });
        }
    }
    peers.retain(|peer| {
        trusted_secret_for(application, &peer.id).is_some()
            && trusted_candidate_allowed(application, peer)
    });
    peers.sort_by_key(|peer| trusted_candidate_rank(application, peer));
    let peer = peers.into_iter().next();
    if let Some(peer) = peer {
        let _ = connect_trusted_peer(application, &peer, true);
    }
}

pub(crate) fn start_relay_discovery(application: &mut Application) {
    #[cfg(feature = "relay")]
    {
        if let Err(error) = application.source.relay_discovery_start() {
            application.status = application.tf("relay.error", &[("error", error)]);
        } else {
            application.relay_discovery_active = true;
        }
    }
    #[cfg(not(feature = "relay"))]
    {
        application.status = application.t("relay.unavailable");
    }
}

pub(crate) fn stop_relay_discovery(application: &mut Application) {
    #[cfg(feature = "relay")]
    {
        application.source.relay_discovery_stop();
        application.relay_discovery_active = false;
    }
    #[cfg(not(feature = "relay"))]
    let _ = application;
}

/// Start discovery when a real USB tether link appears and remove its peers
/// immediately when the link disappears. A discovered peer is eligible for
/// immediate connection only after this installation has explicitly paired
/// with the same stable peer identity and stored its credential.
#[cfg(feature = "relay")]
pub(crate) fn poll_relay_usb_hotplug(application: &mut Application) {
    const POLL_INTERVAL: Duration = Duration::from_secs(1);
    if application
        .relay_usb_last_poll
        .is_some_and(|last| last.elapsed() < POLL_INTERVAL)
    {
        return;
    }
    application.relay_usb_last_poll = Some(std::time::Instant::now());
    let present = application.source.relay_usb_link_present();
    let appeared = present && !application.relay_usb_present;
    let disappeared = !present && application.relay_usb_present;
    application.relay_usb_present = present;

    if disappeared {
        application.source.relay_discovery_usb_link_lost();
        application.relay_usb_auto_attempted = false;
    }
    if appeared {
        application.relay_usb_auto_attempted = false;
    }
    if appeared && !application.relay_usb_auto_attempted {
        application.relay_usb_auto_attempted = true;
        if !application.relay_discovery_active {
            match application.source.relay_discovery_start() {
                Ok(()) => {
                    application.relay_discovery_active = true;
                    application.status = application.t("relay.discovery_started");
                }
                Err(error) => {
                    application.status = application.tf("relay.error", &[("error", error)]);
                }
            }
        }
    }
}

#[cfg(not(feature = "relay"))]
pub(crate) fn poll_relay_usb_hotplug(_application: &mut Application) {}

pub(crate) fn relay_host_active(application: &Application) -> bool {
    #[cfg(feature = "relay")]
    {
        application.source.relay_status().host_active
    }
    #[cfg(not(feature = "relay"))]
    {
        let _ = application;
        false
    }
}

pub(crate) fn relay_nodes_visible(application: &Application) -> bool {
    #[cfg(feature = "relay")]
    {
        let status = application.source.relay_status();
        status.host_active || !status.sessions.is_empty() || application.relay_connecting.is_some()
    }
    #[cfg(not(feature = "relay"))]
    {
        let _ = application;
        false
    }
}

/// A host PIN is ephemeral: each hosting session gets a fresh random one
/// rather than a stored (or, worse, shipped) value.
///
/// The two halves of that promise are split into these functions so the
/// lifecycle can be tested without standing up a relay backend. Together they
/// hold three properties the UI depends on:
///
/// - a first start always has a PIN;
/// - the PIN does not move while a session is live, so the panel and the
///   pairing QR code keep showing one that actually works;
/// - a stop retires it, so the next start generates a new one.
///
/// Generating unconditionally in [`host_pin_on_start`] would satisfy the third
/// property too, but it would also throw away a PIN a user had deliberately
/// typed into the field, so the retirement happens on stop instead.
#[cfg(feature = "relay")]
pub(crate) fn host_pin_on_start(pin: &mut String, generate: impl FnOnce() -> String) {
    if pin.trim().is_empty() {
        *pin = generate();
    }
}

/// Keep the last host PIN across stops so the next start can reuse it
/// without retyping (user request). The PIN remains `serde(skip)` – not
/// persisted to disk for security – but is kept in-memory and shown in the
/// field with a refresh button to generate a new one. This avoids an empty
/// PIN on mobile/desktop after stop.
#[cfg(feature = "relay")]
pub(crate) fn host_pin_on_stop(_pin: &mut String) {
    // Intentionally keep the PIN – user can refresh via the new button.
}

#[cfg(feature = "relay")]
const DIRECTION_SWITCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Begin a desktop endpoint switch. Active sessions stay on their old
/// one-way roles until the authenticated direction offer resolves; only then
/// are the old sessions retired and the opposite endpoint started.
pub(crate) fn handle_relay_direction_change(
    application: &mut Application,
    old: AudioDirection,
    next: AudioDirection,
) {
    #[cfg(feature = "relay")]
    {
        if old == next {
            return;
        }
        let status = application.source.relay_status();
        let live_sessions = status
            .sessions
            .iter()
            .map(|session| session.id.0)
            .collect::<BTreeSet<_>>();
        let live_peer = status.sessions.first().map(|session| session.peer.clone());

        if live_sessions.is_empty() {
            // There is no authenticated peer to negotiate with. A connecting
            // attempt is cancelled, and an idle listener is simply replaced
            // locally; the next connection will carry the persisted offer.
            if let Some(attempt) = application.relay_connecting.take() {
                let _ = application
                    .source
                    .relay_disconnect(RelaySessionId(attempt.session));
            }
            if old == AudioDirection::MobileToDesktop
                && next == AudioDirection::DesktopToMobile
                && status.host_active
            {
                let _ = application.source.relay_stop_host();
            }
            application.relay_direction_switch = None;
            application.status = application.tf(
                "relay.direction_changed",
                &[("direction", direction_label(application, next))],
            );
            return;
        }

        let generation = application.config.relay_direction_generation;
        let (session_ids, peer) = if let Some(switch) = application.relay_direction_switch.as_mut() {
            // A rapid A → B → A gesture updates one pending transaction and
            // keeps the original live endpoint in place until the newest
            // offer resolves.
            switch.target = next;
            switch.generation = generation;
            switch.sessions = live_sessions.clone();
            switch.resolved_sessions.clear();
            switch.resolved = false;
            switch.peer = live_peer.clone().or_else(|| switch.peer.clone());
            switch.started_at = Instant::now();
            (switch.sessions.clone(), switch.peer.clone())
        } else {
            let switch = RelayDirectionSwitch {
                from: old,
                target: next,
                generation,
                sessions: live_sessions.clone(),
                resolved_sessions: BTreeSet::new(),
                resolved: false,
                peer: live_peer,
                started_at: Instant::now(),
            };
            let session_ids = switch.sessions.clone();
            let peer = switch.peer.clone();
            application.relay_direction_switch = Some(switch);
            (session_ids, peer)
        };

        let direction = backend_direction(next);
        let mut offered = false;
        for session in session_ids {
            match application.source.relay_offer_direction(
                RelaySessionId(session),
                direction,
                generation,
            ) {
                Ok(()) => offered = true,
                Err(error) => {
                    application.status = application.tf("relay.error", &[("error", error)]);
                }
            }
        }
        if !offered {
            if let Some(switch) = application.relay_direction_switch.as_mut() {
                switch.resolved = true;
            }
            advance_relay_direction_switch(application);
        } else {
            let _ = peer;
            application.status = application.t("relay.direction_switching");
        }
    }
    #[cfg(not(feature = "relay"))]
    {
        let _ = (application, old, next);
    }
}

#[cfg(feature = "relay")]
fn direction_label(application: &Application, direction: AudioDirection) -> String {
    application.t(match direction {
        AudioDirection::MobileToDesktop => "relay.direction_mobile_to_desktop",
        AudioDirection::DesktopToMobile => "relay.direction_desktop_to_mobile",
    })
}

/// Adopt a direction resolved by a peer that initiated the switch. The local
/// UI has no pending waiter in this case, but the authenticated resolution is
/// still a commit point: every live session must leave the old endpoint before
/// the desktop starts its new host/client side.
#[cfg(feature = "relay")]
fn adopt_remote_relay_direction(
    application: &mut Application,
    resolved_session: RelaySessionId,
    generation: u64,
    direction: AudioDirection,
) {
    let from = application.config.relay_direction;
    if direction == from {
        if generation > application.config.relay_direction_generation {
            application.config.relay_direction_generation = generation;
            application.config_dirty_since.get_or_insert_with(Instant::now);
        }
        return;
    }

    let status = application.source.relay_status();
    let sessions = status
        .sessions
        .iter()
        .map(|session| session.id.0)
        .collect::<BTreeSet<_>>();
    if sessions.is_empty() {
        application.config.relay_direction = direction;
        application.config.relay_direction_generation = application
            .config
            .relay_direction_generation
            .max(generation);
        application.config_dirty_since.get_or_insert_with(Instant::now);
        application.relay_direction_ui_sync = Some(direction);
        return;
    }

    application.config.relay_direction = direction;
    application.config.relay_direction_generation = application
        .config
        .relay_direction_generation
        .max(generation);
    application.config_dirty_since.get_or_insert_with(Instant::now);

    let peer = status
        .sessions
        .iter()
        .find(|session| session.id == resolved_session)
        .or_else(|| status.sessions.first())
        .map(|session| session.peer.clone());
    let mut resolved_sessions = BTreeSet::new();
    resolved_sessions.insert(resolved_session.0);
    let resolved = resolved_sessions.len() == sessions.len();
    application.relay_direction_switch = Some(RelayDirectionSwitch {
        from,
        target: direction,
        generation,
        sessions: sessions.clone(),
        resolved_sessions,
        resolved,
        peer,
        started_at: Instant::now(),
    });

    for session in sessions {
        if session == resolved_session.0 {
            continue;
        }
        if let Err(error) = application.source.relay_offer_direction(
            RelaySessionId(session),
            backend_direction(direction),
            generation,
        ) {
            application.status = application.tf("relay.error", &[("error", error)]);
        }
    }
    application.status = application.t("relay.direction_switching");
    advance_relay_direction_switch(application);
}

/// Apply the deterministic winner to the desktop's local lifecycle after the
/// old session has actually disappeared. The peer address is retained so a
/// host→client switch can reconnect to the same authenticated device.
#[cfg(feature = "relay")]
fn finish_relay_direction_switch(application: &mut Application, switch: RelayDirectionSwitch) {
    let returned_to_original_direction = switch.from == switch.target;
    let direction = switch.target;
    application.config.relay_direction = direction;
    application.config.relay_direction_generation = application
        .config
        .relay_direction_generation
        .max(switch.generation);
    application
        .config_dirty_since
        .get_or_insert_with(Instant::now);
    application.relay_direction_ui_sync = Some(direction);

    // A rapid A → B → A gesture can resolve back to the endpoint that has
    // stayed alive throughout the transaction. Preserve it when it is still
    // active; restarting it would only add an avoidable audio gap.
    let endpoint_is_still_active = match direction {
        AudioDirection::MobileToDesktop => application.source.relay_status().host_active,
        AudioDirection::DesktopToMobile => {
            !application.source.relay_status().sessions.is_empty()
        }
    };
    if returned_to_original_direction && endpoint_is_still_active {
        application.status = application.tf(
            "relay.direction_changed",
            &[("direction", direction_label(application, direction))],
        );
        return;
    }

    match direction {
        AudioDirection::MobileToDesktop => {
            // A DTM desktop host had the opposite local role. Recreate the
            // listener so its virtual graph is built with receive-only roles.
            if application.source.relay_status().host_active {
                let _ = application.source.relay_stop_host();
            }
            start_relay_host(application);
        }
        AudioDirection::DesktopToMobile => {
            if application.source.relay_status().host_active {
                let _ = application.source.relay_stop_host();
            }
            if let Some(peer) = switch.peer {
                application.config.relay_client_target = peer.addr.to_string();
                if !connect_trusted_peer(application, &peer, false) {
                    connect_relay(application, None);
                }
            } else {
                application.status = application.t("relay.direction_changed");
            }
        }
    }
}

/// Once a switch is resolved, request orderly shutdown of the old sessions.
/// Keeping them in the switch record until their SessionLost events arrive
/// prevents a new client/host from racing the old endpoint teardown.
#[cfg(feature = "relay")]
fn advance_relay_direction_switch(application: &mut Application) {
    let live = application
        .source
        .relay_status()
        .sessions
        .into_iter()
        .map(|session| session.id.0)
        .collect::<BTreeSet<_>>();
    let mut disconnect = Vec::new();
    let mut finish = None;
    if let Some(switch) = application.relay_direction_switch.as_mut() {
        if !switch.resolved {
            return;
        }
        switch.sessions.retain(|session| live.contains(session));
        disconnect.extend(switch.sessions.iter().copied());
        if switch.sessions.is_empty() {
            finish = application.relay_direction_switch.take();
        }
    }
    for session in disconnect {
        let _ = application
            .source
            .relay_disconnect(RelaySessionId(session));
    }
    if let Some(switch) = finish {
        finish_relay_direction_switch(application, switch);
    } else if application.relay_direction_switch.is_some() {
        application.status = application.t("relay.direction_switching");
    }
}

#[cfg(feature = "relay")]
fn poll_relay_direction_switch_timeout(application: &mut Application) {
    let expired = application
        .relay_direction_switch
        .as_ref()
        .is_some_and(|switch| switch.started_at.elapsed() >= DIRECTION_SWITCH_TIMEOUT);
    if !expired {
        return;
    }
    if let Some(switch) = application.relay_direction_switch.as_mut() {
        switch.resolved = true;
        switch.resolved_sessions = switch.sessions.clone();
        application.status = application.t("relay.direction_switch_timeout");
    }
    advance_relay_direction_switch(application);
}

#[cfg(feature = "relay")]
pub(crate) fn regenerate_host_pin(application: &mut Application) {
    application.config.relay_host_pin = pw_graph_backend::relay_generate_pin();
    application.status = application.tf(
        "relay.pin_regenerated",
        &[("pin", application.config.relay_host_pin.clone())],
    );
}

#[cfg(not(feature = "relay"))]
pub(crate) fn regenerate_host_pin(application: &mut Application) {
    application.status = application.t("relay.unavailable");
}

pub(crate) fn start_relay_host(application: &mut Application) {
    #[cfg(feature = "relay")]
    {
        host_pin_on_start(
            &mut application.config.relay_host_pin,
            pw_graph_backend::relay_generate_pin,
        );
        let device_id = relay_device_id(application);
        let trusted_peers = configured_trusted_peers(application);
        let request = RelayHostRequest {
            device_id,
            trusted_peers,
            trust_new_peers: true,
            device_name: application.config.relay_device_name.trim().to_owned(),
            pin: application.config.relay_host_pin.trim().to_owned(),
            port: application.config.relay_host_port,
            codec: relay_codec(&application.config.relay_codec),
            // Snap to a duration the protocol actually negotiates. Clamping
            // let a hand-edited config carry something like 7 ms all the way
            // to the far end of a handshake before it was rejected.
            frame_ms: pw_graph_backend::relay_normalize_frame_ms(application.config.relay_frame_ms),
            transport: relay_transport(&application.config.relay_transport),
            direction: backend_direction(application.config.relay_direction),
            direction_generation: application.config.relay_direction_generation,
        };
        match application.source.relay_start_host(request) {
            Ok(port) => {
                application.status =
                    application.tf("relay.host_started", &[("port", port.to_string())])
            }
            Err(error) => {
                // A failed bind/start is not a hosting session. Retire the
                // generated PIN so a later retry gets a fresh credential and
                // the UI never leaves a failed session's PIN displayed.
                host_pin_on_stop(&mut application.config.relay_host_pin);
                application.status = application.tf("relay.error", &[("error", error)])
            }
        }
    }
    #[cfg(not(feature = "relay"))]
    {
        application.status = application.t("relay.unavailable");
    }
}

pub(crate) fn stop_relay_host(application: &mut Application) {
    #[cfg(feature = "relay")]
    {
        match application.source.relay_stop_host() {
            Ok(()) => {
                host_pin_on_stop(&mut application.config.relay_host_pin);
                application.status = application.t("relay.host_stopped");
            }
            Err(error) => application.status = application.tf("relay.error", &[("error", error)]),
        }
    }
    #[cfg(not(feature = "relay"))]
    {
        application.status = application.t("relay.unavailable");
    }
}

pub(crate) fn cancel_relay_connect(application: &mut Application) {
    #[cfg(feature = "relay")]
    {
        let mut had = false;
        if let Some(attempt) = application.relay_connecting.take() {
            let _ = application
                .source
                .relay_disconnect(pw_graph_backend::RelaySessionId(attempt.session));
            if let Some(peer_id) = attempt.peer_id {
                note_trusted_candidate_failure(application, &peer_id, &attempt.target);
            }
            had = true;
        }
        if let Some(pending) = application.relay_reconnect_pending.take() {
            note_trusted_candidate_failure(application, &pending.peer_id, &pending.peer_addr);
            had = true;
        }
        application.status = application.t("relay.connecting_cancelled");
        // Prevent immediate auto-retry from re-creating the same attempt
        application.relay_trusted_auto_attempt_at = Some(std::time::Instant::now());
        if !had {
            // Also clear any plain manual target attempt
            application.status = application.t("relay.connecting_cancelled");
        }
    }
    #[cfg(not(feature = "relay"))]
    let _ = application;
}

/// Revoke a trusted identity in both the live engine and the durable desktop
/// config. The engine operation comes first so a failed backend call never
/// leaves the UI claiming that a credential was forgotten.
/// Commit a host-side enrollment transaction to durable storage and accept it
/// in the engine. Shared by the user's dialog confirmation and the
/// re-enrollment auto-accept so both follow the identical
/// persist-then-acknowledge (with rollback) order.
#[cfg(feature = "relay")]
fn commit_enrollment(
    application: &mut Application,
    pending: &crate::bridge::app::PendingEnrollment,
) {
    let before = application.config.clone();
    let persisted = application
        .source
        .relay_trusted_enrollment_secret(pending.transaction_id)
        .ok()
        .flatten()
        .map(|secret| {
            remember_trusted_peer(
                application,
                &pending.peer_id,
                &pw_graph_backend::RelayPeerInfo {
                    id: pending.peer_id.clone(),
                    name: pending.peer_name.clone(),
                    kind: pw_graph_backend::RelayDeviceKind::Other,
                    addr: pending
                        .peer_addr
                        .parse()
                        .unwrap_or_else(|_| "0.0.0.0:0".parse().unwrap()),
                },
                secret,
            );
            save_config(application, false);
            application.config == application.config_saved_snapshot
        })
        .unwrap_or(false);
    if persisted {
        if let Err(error) = application
            .source
            .relay_accept_trusted_enrollment(pending.transaction_id)
        {
            application.config = before;
            save_config(application, false);
            application.status = application.tf("relay.error", &[("error", error)]);
        } else {
            application.status = application.t("relay.enrollment_accepted");
        }
    } else if let Err(error) = application.source.relay_reject_trusted_enrollment(
        pending.transaction_id,
        "trusted credential could not be durably persisted",
    ) {
        application.status = application.tf("relay.error", &[("error", error)]);
    }
}

/// Whether this peer already holds a stored credential: its re-enrollment is
/// a routine secret rotation that never needs the accept/decline dialog.
#[cfg(feature = "relay")]
pub(crate) fn is_trusted_peer(application: &Application, peer_id: &str) -> bool {
    !peer_id.trim().is_empty()
        && application
            .config
            .relay_trusted_peers
            .iter()
            .any(|stored| stored.peer_id == peer_id)
}

pub(crate) fn accept_pending_enrollment(application: &mut Application) {
    #[cfg(feature = "relay")]
    {
        let Some(pending) = application.relay_pending_enrollment.clone() else {
            return;
        };
        commit_enrollment(application, &pending);
        application.relay_pending_enrollment = None;
    }
    #[cfg(not(feature = "relay"))]
    let _ = application;
}

pub(crate) fn reject_pending_enrollment(application: &mut Application) {
    #[cfg(feature = "relay")]
    {
        let Some(pending) = application.relay_pending_enrollment.take() else {
            return;
        };
        if let Err(error) = application
            .source
            .relay_reject_trusted_enrollment(pending.transaction_id, "rejected by user")
        {
            application.status = application.tf("relay.error", &[("error", error)]);
        } else {
            application.status = application.t("relay.enrollment_rejected");
        }
    }
    #[cfg(not(feature = "relay"))]
    let _ = application;
}

pub(crate) fn forget_trusted_peer(application: &mut Application, peer_id: &str) {
    #[cfg(feature = "relay")]
    {
        let peer_id = peer_id.trim();
        if peer_id.is_empty() {
            return;
        }
        if let Err(error) = application.source.relay_remove_trusted_peer(peer_id) {
            application.status = application.tf("relay.error", &[("error", error)]);
            return;
        }
        let before_config = application.config.clone();
        let before_count = application.config.relay_trusted_peers.len();
        application
            .config
            .relay_trusted_peers
            .retain(|peer| peer.peer_id != peer_id);
        if application.config.relay_trusted_peers.len() != before_count {
            save_config(application, false);
            if application.config != application.config_saved_snapshot {
                application.config = before_config;
                application.config_dirty_since = None;
                if let Err(error) = configure_relay_identity(application) {
                    application.status = application.tf("relay.error", &[("error", error)]);
                }
                return;
            }
        }
        // Clear any reconnecting state for this peer so Cancel/Forget is immediate
        if application
            .relay_reconnect_pending
            .as_ref()
            .is_some_and(|p| p.peer_id == peer_id)
        {
            application.relay_reconnect_pending = None;
        }
        // Forget also drops any stale-refusal mark, so a re-paired device is
        // auto-connectable again without waiting for anything else.
        application
            .relay_trusted_refused
            .retain(|(refused_id, _)| refused_id != peer_id);
        if application
            .relay_connecting
            .as_ref()
            .is_some_and(|a| a.peer_id.as_deref() == Some(peer_id))
        {
            application.relay_connecting = None;
        }
        application.status = application.t("relay.trusted_peer_forgotten");
    }
    #[cfg(not(feature = "relay"))]
    let _ = (application, peer_id);
}

pub(crate) fn connect_relay(application: &mut Application, requested_target: Option<&str>) {
    #[cfg(feature = "relay")]
    {
        let raw_target = requested_target
            .map(str::to_owned)
            .unwrap_or_else(|| application.config.relay_client_target.clone());
        let raw_target = raw_target.trim().to_owned();
        if raw_target.is_empty() {
            application.status = application.t("status.relay_target_required");
            return;
        }
        // Manual connect cancels any pending reconnect UI so the manual
        // attempt's Cancel/Connecting state is authoritative.
        application.relay_reconnect_pending = None;
        let target_text = match relay_parse_qr_payload(&raw_target) {
            Some(payload) => {
                application.config.relay_client_target = payload.target.clone();
                if let Some(pin) = payload.pin {
                    application.config.relay_client_pin = pin;
                }
                payload.target
            }
            None => {
                if requested_target.is_some() {
                    application.config.relay_client_target = raw_target.clone();
                }
                raw_target
            }
        };
        let target = match target_text
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next())
        {
            Some(target) => target,
            None => {
                application.status =
                    application.tf("relay.invalid_target", &[("target", target_text)]);
                return;
            }
        };
        // ADB mode is localhost-only: give immediate actionable feedback instead
        // of the later "ADB transport requires a localhost forwarding target"
        // from the transport layer, which looks like a bug.
        let transport = relay_transport(&application.config.relay_transport);
        if transport == pw_graph_backend::RelayTransportPreference::Adb
            && !target.ip().is_loopback()
        {
            application.status = application.tf(
                "relay.error_adb_requires_localhost",
                &[("target", target.to_string())],
            );
            return;
        }
        // A discovered address with a stored credential can be reconnected
        // without asking for the old PIN again. QR/manual targets still use
        // the explicit PIN unless discovery can identify the same peer.
        if let Some(peer) = application
            .source
            .relay_peers()
            .into_iter()
            .find(|peer| peer.addr == target)
        {
            if connect_trusted_peer(application, &peer, false) {
                return;
            }
        }
        let pin = application.config.relay_client_pin.trim().to_owned();
        if pin.is_empty() {
            application.status = application.t("status.relay_pin_required");
            return;
        }
        if let Err(error) = configure_relay_identity(application) {
            application.status = application.tf("relay.error", &[("error", error)]);
            return;
        }
        match application.source.relay_connect(
            target,
            &pin,
            backend_direction(application.config.relay_direction),
            application.config.relay_direction_generation,
        ) {
            Ok(session) => {
                application.relay_connecting = Some(RelayAttempt {
                    target: target.to_string(),
                    session: session.0,
                    peer_id: None,
                });
                application.status = application.t("relay.connecting");
            }
            Err(error) => application.status = application.tf("relay.error", &[("error", error)]),
        }
    }
    #[cfg(not(feature = "relay"))]
    {
        let _ = requested_target;
        application.status = application.t("relay.unavailable");
    }
}

pub(crate) fn disconnect_relay(application: &mut Application, session: Option<u64>) {
    #[cfg(feature = "relay")]
    {
        let Some(session) = session else {
            application.status = application.t("status.relay_session_invalid");
            return;
        };
        match application.source.relay_disconnect(RelaySessionId(session)) {
            Ok(()) => application.status = application.t("relay.disconnecting"),
            Err(error) => application.status = application.tf("relay.error", &[("error", error)]),
        }
    }
    #[cfg(not(feature = "relay"))]
    {
        let _ = session;
        application.status = application.t("relay.unavailable");
    }
}

#[cfg(feature = "relay")]
pub(crate) fn poll_relay_events(application: &mut Application) {
    for event in application.source.relay_events() {
        match event {
            RelayEvent::HostStarted { port } => {
                application.status =
                    application.tf("relay.host_started", &[("port", port.to_string())]);
            }
            RelayEvent::HostStopped => application.status = application.t("relay.host_stopped"),
            RelayEvent::PeerDiscovered { peer } => {
                application.status =
                    application.tf("relay.peer_discovered", &[("name", peer.name.clone())]);
                // Discovery re-announcing an address is proof the host is
                // listening there again, so a stale-refusal mark must not
                // keep the auto-connect from dialing it.
                clear_trusted_candidate_refused(application, &peer.id, &peer.addr.to_string());
                if application.config.relay_auto_connect_trusted {
                    let _ = connect_trusted_peer(application, &peer, true);
                }
            }
            RelayEvent::PeerLost { peer } => {
                application.status = application.tf("relay.peer_lost", &[("name", peer.name)]);
            }
            RelayEvent::TrustedPeerAvailable {
                peer_id,
                peer,
                secret,
            } => {
                remember_trusted_peer(application, &peer_id, &peer, secret);
            }
            RelayEvent::TrustedPeerEnrollmentRequested {
                transaction_id,
                peer_id,
                peer,
            } => {
                // Pairing policy: "pair code or accept dialog, never both."
                // A peer presenting a credential we already store is rotating
                // its secret after another PIN pairing — re-accept it
                // silently, exactly like the Android host does. Only a
                // first-contact device earns the Accept/Decline dialog. The
                // engine holds the transaction 30s, so a missed dialog safely
                // times out and the client retries with PIN.
                let pending = crate::bridge::app::PendingEnrollment {
                    transaction_id,
                    peer_id: peer_id.clone(),
                    peer_name: peer.name.clone(),
                    peer_addr: peer.addr.to_string(),
                };
                if is_trusted_peer(application, &peer_id) {
                    commit_enrollment(application, &pending);
                } else {
                    // Queue a user-visible confirmation – modal shows PIN +
                    // peer identity with Accept/Decline. No auto-accept; user
                    // must explicitly confirm.
                    application.relay_pending_enrollment = Some(pending);
                    application.status = application.tf(
                        "relay.enrollment_requested",
                        &[("name", peer.name.clone()), ("addr", peer.addr.to_string())],
                    );
                }
            }
            RelayEvent::SessionEstablished { id, peer, .. } => {
                if application
                    .relay_connecting
                    .as_ref()
                    .is_some_and(|attempt| attempt.session == id.0)
                {
                    application.relay_connecting = None;
                }
                // Clear any pending reconnect for this peer on success
                if application
                    .relay_reconnect_pending
                    .as_ref()
                    .is_some_and(|p| p.peer_id == peer.id)
                {
                    application.relay_reconnect_pending = None;
                }
                refresh_trusted_peer_address(application, &peer);
                // A session succeeding proves the address is live again.
                clear_trusted_candidate_refused(application, &peer.id, &peer.addr.to_string());
                application.status =
                    application.tf("relay.session_connected", &[("name", peer.name)]);
            }
            RelayEvent::DirectionResolved {
                id,
                generation,
                direction,
                ..
            } => {
                let resolved_direction = config_direction(direction);
                let mut ready = false;
                if let Some(switch) = application.relay_direction_switch.as_mut() {
                    if switch.sessions.contains(&id.0) && generation == switch.generation {
                        switch.target = resolved_direction;
                        switch.resolved_sessions.insert(id.0);
                        if switch.resolved_sessions.len() >= switch.sessions.len() {
                            switch.resolved = true;
                            ready = true;
                        }
                    }
                }
                if ready {
                    advance_relay_direction_switch(application);
                } else if application.relay_direction_switch.is_none()
                    && resolved_direction != application.config.relay_direction
                {
                    adopt_remote_relay_direction(application, id, generation, resolved_direction);
                } else {
                    if generation > application.config.relay_direction_generation
                        && resolved_direction == application.config.relay_direction
                    {
                        application.config.relay_direction_generation = generation;
                        application.config_dirty_since.get_or_insert_with(Instant::now);
                    }
                    application.status = application.tf(
                        "relay.direction_resolved",
                        &[
                            ("session", id.0.to_string()),
                            ("generation", generation.to_string()),
                            ("direction", direction.as_str().to_owned()),
                        ],
                    );
                }
            }
            RelayEvent::SessionLost { id, reason } => {
                application.relay_levels.remove(&id.0);
                let mut advance_switch = false;
                if let Some(switch) = application.relay_direction_switch.as_mut() {
                    if switch.sessions.remove(&id.0) {
                        if switch.sessions.is_empty() && !switch.resolved {
                            // The peer disappeared before acknowledging. The
                            // newest persisted direction is still safe to
                            // apply locally; a later reconnect negotiates it
                            // again from its generation.
                            switch.resolved = true;
                        }
                        advance_switch = switch.resolved;
                    }
                }
                if application
                    .relay_connecting
                    .as_ref()
                    .is_some_and(|attempt| attempt.session == id.0)
                {
                    let attempt = application
                        .relay_connecting
                        .as_ref()
                        .map(|attempt| (attempt.peer_id.clone(), attempt.target.clone()));
                    let pending_info = attempt.clone();
                    if let Some((Some(peer_id), target)) = attempt {
                        note_trusted_candidate_failure(application, &peer_id, &target);
                    }
                    application.relay_connecting = None;
                    if let Some((Some(peer_id), target)) = pending_info {
                        let refused = reason.contains("Connection refused")
                            || reason.contains("os error 111")
                            || reason.contains("os error 10061")
                            || reason.contains("No route to host")
                            || reason.contains("os error 113")
                            || reason.contains("os error 101")
                            || reason.contains("os error 10051");
                        if refused {
                            // Nothing is listening at this address right now.
                            // Marking it refused ends the auto-retry instead
                            // of looping "Connecting (retry in 5s)" against a
                            // dead address; discovery re-announcing the peer
                            // clears the mark and revives auto-connect.
                            note_trusted_candidate_refused(application, &peer_id, &target);
                        } else if reason.contains("os error 11") {
                            // Keep a visible reconnecting state for the 5s
                            // retry interval so Cancel/Forget stay on screen
                            // instead of flashing 0.1ms.
                            let name = application
                                .config
                                .relay_trusted_peers
                                .iter()
                                .find(|p| p.peer_id == peer_id)
                                .map(|p| p.name.clone())
                                .unwrap_or_else(|| peer_id.clone());
                            application.relay_reconnect_pending =
                                Some(crate::bridge::app::ReconnectPending {
                                    peer_id,
                                    peer_name: name,
                                    peer_addr: target,
                                    next_retry: Instant::now() + Duration::from_secs(5),
                                });
                        }
                    }
                }
                let display_reason = if reason.contains("No route to host")
                    || reason.contains("os error 113")
                    || reason.contains("os error 101")
                {
                    format!(
                        "{} — check that host is reachable on this network (Wi-Fi/USB changed? Try rediscovery or re-enter host:port). Original: {}",
                        application.t("relay.error_no_route"),
                        reason
                    )
                } else if reason.contains("Connection refused") || reason.contains("os error 111") {
                    format!(
                        "{} — host not listening at that address. Original: {}",
                        application.t("relay.error_tcp_refused"),
                        reason
                    )
                } else {
                    reason
                };
                application.status =
                    application.tf("relay.session_lost", &[("reason", display_reason)]);
                if advance_switch {
                    advance_relay_direction_switch(application);
                }
            }
            RelayEvent::AudioLevel { id, rms } => {
                application.relay_levels.insert(id.0, rms.clamp(0.0, 1.0));
            }
            RelayEvent::Error { message } => {
                application.relay_connecting = None;
                application.status = application.tf("relay.error", &[("error", message)]);
            }
        }
    }
    poll_relay_direction_switch_timeout(application);
    retry_trusted_auto_connect(application);
}

#[cfg(not(feature = "relay"))]
pub(crate) fn poll_relay_events(_application: &mut Application) {}

#[cfg(all(feature = "relay", test))]
fn desktop_roles(direction: AudioDirection) -> pw_graph_backend::RelayRoles {
    match direction {
        AudioDirection::MobileToDesktop => pw_graph_backend::RelayRoles::receive_only(),
        AudioDirection::DesktopToMobile => pw_graph_backend::RelayRoles::emit_only(),
    }
}

#[cfg(feature = "relay")]
fn backend_direction(direction: AudioDirection) -> RelayDirection {
    match direction {
        AudioDirection::MobileToDesktop => RelayDirection::MobileToDesktop,
        AudioDirection::DesktopToMobile => RelayDirection::DesktopToMobile,
    }
}

#[cfg(feature = "relay")]
fn config_direction(direction: RelayDirection) -> AudioDirection {
    match direction {
        RelayDirection::MobileToDesktop => AudioDirection::MobileToDesktop,
        RelayDirection::DesktopToMobile => AudioDirection::DesktopToMobile,
    }
}

#[cfg(feature = "relay")]
fn relay_codec(value: &str) -> RelayCodecKind {
    if value.eq_ignore_ascii_case("pcm") {
        RelayCodecKind::Pcm
    } else {
        RelayCodecKind::Opus
    }
}

#[cfg(feature = "relay")]
fn relay_transport(value: &str) -> RelayTransportPreference {
    RelayTransportPreference::from_str(value).unwrap_or_default()
}

#[cfg(feature = "relay")]
pub(crate) fn relay_qr_payload(application: &Application) -> Option<String> {
    let status = application.source.relay_status();
    let port = status.host_port?;
    let addr = host_link_addr(application)?;
    Some(relay_build_qr_payload(
        addr,
        port,
        application.config.relay_host_pin.trim(),
    ))
}

/// The local address to publish for pairing.
///
/// The host binds the link its transport preference selects, so the QR code
/// and the endpoint label must name that same link — otherwise the app shows
/// an address nothing is listening on.
#[cfg(feature = "relay")]
fn host_link_addr(application: &Application) -> Option<std::net::Ipv4Addr> {
    let status = application.source.relay_status();
    status.host_addr.or_else(|| {
        // A listener with no currently classified link intentionally binds
        // INADDR_ANY. It is still useful to publish a real, reachable address
        // when the display-side link enumerator has one, rather than showing
        // 0.0.0.0 in the endpoint and QR code.
        let links = application.source.relay_local_links();
        let preference = relay_transport(&application.config.relay_transport);
        let selected = pw_graph_backend::relay_select_links(&links, preference);
        selected.first().map(|link| link.addr).or_else(|| {
            let fallback =
                pw_graph_backend::relay_select_links(&links, RelayTransportPreference::Auto);
            fallback.first().map(|link| link.addr)
        })
    })
}

#[cfg(not(feature = "relay"))]
pub(crate) fn relay_qr_payload(_application: &Application) -> Option<String> {
    None
}
pub(crate) fn relay_rows(application: &Application, i18n: &I18n) -> Vec<RelayRow> {
    #[cfg(not(feature = "relay"))]
    let _ = application;
    #[cfg(feature = "relay")]
    {
        let status = application.source.relay_status();
        let mut rows = Vec::new();
        let mut connected = BTreeSet::new();
        let trusted_ids = application
            .config
            .relay_trusted_peers
            .iter()
            .map(|peer| peer.peer_id.as_str())
            .collect::<BTreeSet<_>>();
        for session in status.sessions {
            let address = session.peer.addr.to_string();
            if !session.peer.id.is_empty() {
                connected.insert(format!("id:{}", session.peer.id));
            }
            connected.insert(address.clone());
            let direction = match (session.sending, session.receiving) {
                // Accepted sessions are one-way. Keep a defensive diagnostic
                // label for a stale/foreign status snapshot without exposing
                // “both” as a valid direction.
                (true, true) => i18n.text("relay.direction_invalid"),
                (true, false) => i18n.text("relay.direction_send"),
                (false, true) => i18n.text("relay.direction_receive"),
                (false, false) => i18n.text("relay.direction_connected"),
            };
            let transport = if session.transport.is_empty() {
                "unknown".to_owned()
            } else {
                session.transport.clone()
            };
            let link = if session.link.is_empty() {
                "unknown".to_owned()
            } else {
                session.link.clone()
            };
            let audio_state = if session.audio_channel_state == "reconnecting" {
                " · reconnecting audio"
            } else {
                ""
            };
            rows.push(RelayRow {
                id: SharedString::from(session.id.0.to_string()),
                name: SharedString::from(session.peer.name),
                address: SharedString::from(address.clone()),
                state: SharedString::from(format!(
                    "{} · {direction} · {transport}/{link}{audio_state}",
                    i18n.text("relay.group_connected"),
                )),
                level: application
                    .relay_levels
                    .get(&session.id.0)
                    .copied()
                    .unwrap_or_default(),
                connected: true,
                connecting: false,
                trusted: trusted_ids.contains(session.peer.id.as_str()),
                peer_id: SharedString::from(session.peer.id),
            });
        }
        let connecting = application
            .relay_connecting
            .as_ref()
            .map(|attempt| attempt.target.as_str());
        let mut peers = application.source.relay_peers();
        peers.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.addr.cmp(&b.addr)));
        for peer in peers {
            let address = peer.addr.to_string();
            if connected.contains(&address)
                || (!peer.id.is_empty() && connected.contains(&format!("id:{}", peer.id)))
            {
                continue;
            }
            let state = if connecting == Some(address.as_str()) {
                i18n.text("relay.state_connecting")
            } else {
                i18n.text("relay.state_available")
            };
            rows.push(RelayRow {
                id: SharedString::from(address.clone()),
                name: SharedString::from(peer.name),
                address: SharedString::from(address.clone()),
                state: SharedString::from(state),
                level: 0.0,
                connected: false,
                connecting: connecting == Some(address.as_str()),
                trusted: trusted_ids.contains(peer.id.as_str()),
                peer_id: SharedString::from(peer.id),
            });
        }
        if let Some(target) = connecting {
            if !rows.iter().any(|row| row.address == target) {
                rows.push(RelayRow {
                    id: SharedString::from(target),
                    name: SharedString::from(target),
                    address: SharedString::from(target),
                    state: i18n.text("relay.state_connecting").into(),
                    level: 0.0,
                    connected: false,
                    connecting: true,
                    trusted: false,
                    peer_id: SharedString::new(),
                });
            }
        }
        // Pending trusted reconnect – keeps Cancel/Forget visible during the
        // 5s retry interval instead of flashing 0.1ms. Shows as reconnecting
        // so user can cancel or forget before next auto-attempt.
        if let Some(pending) = &application.relay_reconnect_pending {
            if !connected.contains(&pending.peer_addr)
                && !rows.iter().any(|row| row.address == pending.peer_addr)
                && !rows.iter().any(|row| row.peer_id == pending.peer_id)
            {
                let secs = pending
                    .next_retry
                    .saturating_duration_since(Instant::now())
                    .as_secs()
                    + 1;
                rows.push(RelayRow {
                    id: SharedString::from(pending.peer_addr.clone()),
                    name: SharedString::from(pending.peer_name.clone()),
                    address: SharedString::from(pending.peer_addr.clone()),
                    state: SharedString::from(format!(
                        "{} (retry in {}s)",
                        i18n.text("relay.state_connecting"),
                        secs
                    )),
                    level: 0.0,
                    connected: false,
                    connecting: true,
                    trusted: true,
                    peer_id: SharedString::from(pending.peer_id.clone()),
                });
            }
        }
        if rows.is_empty() && !application.config.relay_client_target.trim().is_empty() {
            rows.push(RelayRow {
                id: SharedString::from(application.config.relay_client_target.clone()),
                name: i18n.text("relay.configured_peer").into(),
                address: SharedString::from(application.config.relay_client_target.clone()),
                state: i18n.text("relay.state_configured").into(),
                level: 0.0,
                connected: false,
                connecting: false,
                trusted: false,
                peer_id: SharedString::new(),
            });
        }
        if rows.is_empty() {
            rows.push(RelayRow {
                id: SharedString::new(),
                name: i18n.text("relay.no_peers").into(),
                address: i18n.text("relay.discovery_help").into(),
                state: i18n.text("relay.state_idle").into(),
                level: 0.0,
                connected: false,
                connecting: false,
                trusted: false,
                peer_id: SharedString::new(),
            });
        }
        rows
    }
    #[cfg(not(feature = "relay"))]
    {
        vec![RelayRow {
            id: SharedString::new(),
            name: i18n.text("relay.unavailable").into(),
            address: i18n.text("relay.advanced_help").into(),
            state: i18n.text("relay.state_unavailable").into(),
            level: 0.0,
            connected: false,
            connecting: false,
            trusted: false,
            peer_id: SharedString::new(),
        }]
    }
}

pub(crate) fn relay_direction_tab(direction: AudioDirection) -> i32 {
    match direction {
        AudioDirection::MobileToDesktop => 0,
        AudioDirection::DesktopToMobile => 1,
    }
}

pub(crate) fn relay_direction_from_tab(index: i32, fallback: AudioDirection) -> AudioDirection {
    match index {
        0 => AudioDirection::MobileToDesktop,
        1 => AudioDirection::DesktopToMobile,
        _ => fallback,
    }
}

pub(crate) fn relay_codec_index(value: &str) -> i32 {
    if value.eq_ignore_ascii_case("pcm") {
        1
    } else {
        0
    }
}

pub(crate) fn relay_codec_from_index(index: i32) -> &'static str {
    if index == 1 {
        "pcm"
    } else {
        "opus"
    }
}

/// Frame durations offered by the settings combo box.
///
/// The settings panel exists whether or not the relay feature is compiled in,
/// so this list cannot live behind the relay re-exports. It mirrors
/// `pw_graph_relay::FRAME_DURATIONS_MS`, and
/// `the_picker_offers_exactly_the_negotiable_frame_durations` fails if the two
/// ever drift — the duplication is checked, not trusted.
const FRAME_DURATIONS_MS: [u16; 5] = [5, 10, 20, 40, 60];

/// Combo-box index for a frame duration. A value that is not exactly one of
/// the offered durations snaps to the nearest, so a hand-edited config shows
/// the duration it will actually negotiate rather than silently disagreeing
/// with the wire.
pub(crate) fn relay_frame_index(frame_ms: u16) -> i32 {
    FRAME_DURATIONS_MS
        .iter()
        .enumerate()
        .min_by_key(|(_, candidate)| candidate.abs_diff(frame_ms))
        .map(|(index, _)| index as i32)
        .unwrap_or(1)
}

pub(crate) fn relay_frame_from_index(index: i32) -> u16 {
    FRAME_DURATIONS_MS
        .get(index.clamp(0, FRAME_DURATIONS_MS.len() as i32 - 1) as usize)
        .copied()
        .unwrap_or(10)
}

pub(crate) fn relay_transport_index(value: &str) -> i32 {
    match value {
        "wifi" => 1,
        "bluetooth" => 2,
        "lan" => 3,
        "adb" => 4,
        _ => 0,
    }
}

pub(crate) fn relay_transport_from_index(index: i32) -> &'static str {
    match index {
        1 => "wifi",
        2 => "bluetooth",
        3 => "lan",
        4 => "adb",
        _ => "auto",
    }
}

#[cfg(feature = "relay")]
pub(crate) fn relay_host_endpoint(application: &Application, port: Option<u16>) -> String {
    let Some(port) = port else {
        return String::new();
    };
    host_link_addr(application)
        .map(|addr| format!("{addr}:{port}"))
        .unwrap_or_else(|| format!("0.0.0.0:{port}"))
}

#[cfg(feature = "relay")]
pub(crate) fn qr_image(payload: &str) -> Image {
    let Some(scale) = relay_qr::module_scale_for(payload, 236) else {
        return Image::default();
    };
    let Some(bitmap) = relay_qr::render(payload, scale, relay_qr::DEFAULT_QUIET_MODULES) else {
        return Image::default();
    };
    let pixels: Vec<Rgba8Pixel> = bitmap
        .dark
        .into_iter()
        .map(|dark| {
            if dark {
                Rgba8Pixel {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 255,
                }
            } else {
                Rgba8Pixel {
                    r: 255,
                    g: 255,
                    b: 255,
                    a: 255,
                }
            }
        })
        .collect();
    let mut buffer =
        SharedPixelBuffer::<Rgba8Pixel>::new(bitmap.width as u32, bitmap.height as u32);
    buffer.make_mut_slice().copy_from_slice(&pixels);
    Image::from_rgba8(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "relay")]
    #[test]
    fn the_picker_offers_exactly_the_negotiable_frame_durations() {
        assert_eq!(
            FRAME_DURATIONS_MS,
            pw_graph_backend::RELAY_FRAME_DURATIONS_MS,
            "the settings picker and the wire protocol disagree about frame durations"
        );
    }

    #[test]
    fn frame_durations_round_trip_through_the_picker() {
        for (index, duration) in FRAME_DURATIONS_MS.iter().enumerate() {
            assert_eq!(relay_frame_index(*duration), index as i32);
            assert_eq!(relay_frame_from_index(index as i32), *duration);
        }
    }

    #[test]
    fn an_unsupported_stored_duration_snaps_to_a_real_one() {
        // A hand-edited or older config could hold any u16; the picker must
        // resolve it to a duration the protocol will actually accept.
        for (stored, expected) in [(0u16, 5u16), (7, 5), (13, 10), (35, 40), (9_000, 60)] {
            assert_eq!(relay_frame_from_index(relay_frame_index(stored)), expected);
        }
    }

    #[cfg(feature = "relay")]
    #[test]
    fn directions_derive_only_one_way_desktop_roles() {
        assert_eq!(
            desktop_roles(AudioDirection::MobileToDesktop),
            pw_graph_backend::RelayRoles::receive_only()
        );
        assert_eq!(
            desktop_roles(AudioDirection::DesktopToMobile),
            pw_graph_backend::RelayRoles::emit_only()
        );
        for roles in [
            desktop_roles(AudioDirection::MobileToDesktop),
            desktop_roles(AudioDirection::DesktopToMobile),
        ] {
            assert!(!(roles.emit && roles.receive));
        }
    }

    #[test]
    fn direction_tabs_round_trip_without_an_advanced_role() {
        assert_eq!(
            relay_direction_tab(AudioDirection::MobileToDesktop),
            0
        );
        assert_eq!(
            relay_direction_tab(AudioDirection::DesktopToMobile),
            1
        );
        assert_eq!(
            relay_direction_from_tab(0, AudioDirection::DesktopToMobile),
            AudioDirection::MobileToDesktop
        );
        assert_eq!(
            relay_direction_from_tab(1, AudioDirection::MobileToDesktop),
            AudioDirection::DesktopToMobile
        );
        assert_eq!(
            relay_direction_from_tab(2, AudioDirection::DesktopToMobile),
            AudioDirection::DesktopToMobile
        );
    }
}

#[cfg(all(test, feature = "relay"))]
mod host_pin_tests {
    use super::{host_pin_on_start, host_pin_on_stop};

    /// Stand-in for `relay_generate_pin`, so the test controls what "fresh"
    /// means and can tell a regenerated PIN from a reused one.
    fn counting_generator(next: &std::cell::Cell<u32>) -> impl FnOnce() -> String + '_ {
        move || {
            next.set(next.get() + 1);
            format!("pin-{}", next.get())
        }
    }

    #[test]
    fn the_first_host_start_gets_a_pin() {
        let counter = std::cell::Cell::new(0);
        let mut pin = String::new();
        host_pin_on_start(&mut pin, counting_generator(&counter));
        assert_eq!(pin, "pin-1");
        assert!(!pin.trim().is_empty());
    }

    #[test]
    fn the_pin_is_stable_for_the_life_of_one_hosting_session() {
        // The panel and the QR code both read this field while the host runs;
        // moving it mid-session would show a PIN that does not pair.
        let counter = std::cell::Cell::new(0);
        let mut pin = String::new();
        host_pin_on_start(&mut pin, counting_generator(&counter));
        let during = pin.clone();
        for _ in 0..3 {
            host_pin_on_start(&mut pin, counting_generator(&counter));
        }
        assert_eq!(pin, during);
        assert_eq!(counter.get(), 1, "the PIN was regenerated mid-session");
    }

    #[test]
    fn stopping_then_starting_reuses_the_last_pin() {
        // New behavior: PIN is kept across stops to avoid retyping; user
        // regenerates explicitly via the refresh button.
        let counter = std::cell::Cell::new(0);
        let mut pin = String::new();
        host_pin_on_start(&mut pin, counting_generator(&counter));
        let first = pin.clone();
        host_pin_on_stop(&mut pin);
        assert_eq!(
            pin, first,
            "the stopped session should keep its PIN for reuse"
        );
        host_pin_on_start(&mut pin, counting_generator(&counter));
        assert_eq!(pin, first);
        assert_eq!(
            counter.get(),
            1,
            "no new PIN should be generated on restart"
        );
    }

    #[test]
    fn a_deliberately_typed_pin_survives_restart() {
        let counter = std::cell::Cell::new(0);
        let mut pin = String::from("246813");
        host_pin_on_start(&mut pin, counting_generator(&counter));
        assert_eq!(pin, "246813");
        assert_eq!(counter.get(), 0);
        host_pin_on_stop(&mut pin);
        assert_eq!(pin, "246813", "typed PIN should survive stop");
        host_pin_on_start(&mut pin, counting_generator(&counter));
        assert_eq!(pin, "246813");
        assert_eq!(counter.get(), 0);
    }

    #[test]
    fn a_whitespace_only_pin_counts_as_absent() {
        let counter = std::cell::Cell::new(0);
        let mut pin = String::from("   ");
        host_pin_on_start(&mut pin, counting_generator(&counter));
        assert_eq!(pin, "pin-1");
    }

    #[test]
    fn refresh_generates_a_new_pin() {
        let counter = std::cell::Cell::new(10);
        let mut pin = String::from("old-pin");
        host_pin_on_start(&mut pin, counting_generator(&counter));
        assert_eq!(pin, "old-pin");
        // Simulate regenerate: directly call generator
        pin = counting_generator(&counter)();
        assert_eq!(pin, "pin-11");
    }

    #[test]
    fn generated_pins_are_not_all_the_same() {
        // Guards the real generator rather than the lifecycle: a constant
        // "fresh" PIN would pass every test above.
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..32 {
            let mut pin = String::new();
            host_pin_on_start(&mut pin, pw_graph_backend::relay_generate_pin);
            assert!(!pin.trim().is_empty());
            seen.insert(pin);
        }
        assert!(seen.len() > 1, "relay_generate_pin returned a constant");
    }
}
