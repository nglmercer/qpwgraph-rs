use super::QpwgraphApp;
use pw_graph_backend::{
    relay_build_qr_payload, relay_parse_qr_payload, BackendError, BackendResult, RelayCodecKind,
    RelayEvent, RelayHostRequest, RelayLinkKind, RelayLocalLink, RelayPeerInfo, RelayRoles,
    RelaySessionId, RelayTransportPreference,
};
use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use std::time::{Duration, Instant};

/// What a device row in the Connections list is currently doing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RelayDeviceState {
    /// Discovered and idle: offer a connect action.
    Available,
    /// A connect attempt is in flight; no session exists yet.
    Connecting,
    /// A live session, identified so the row can disconnect it.
    Connected(RelaySessionId),
}

/// One row of the unified device list, built by joining discovered peers
/// with live sessions so a device never appears twice.
#[derive(Clone, Debug)]
pub(crate) struct RelayDeviceRow {
    pub(crate) name: String,
    pub(crate) addr: SocketAddr,
    pub(crate) state: RelayDeviceState,
}

/// How long a connect attempt shows as "Connecting…" before the row falls
/// back to `Available`. The engine reports failures through `RelayEvent`,
/// but a silently dropped attempt must not pin the row forever.
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Default)]
pub(crate) struct RelayUiState {
    pub(crate) discovery_active: bool,
    pub(crate) peers: Vec<RelayPeerInfo>,
    pub(crate) message: String,
    /// Manual address typed under "Add a device manually"; accepts
    /// `host:port` or a pasted `qpw-relay://` QR payload. Kept out of
    /// `AppConfig` because it is a one-shot convenience, not a setting.
    pub(crate) quick_target: String,
    /// Set when the most recent discovery start failed, so the panel does
    /// not retry automatically on every frame.
    pub(crate) discovery_failed: bool,
    /// Address of an in-flight connect attempt and when it started.
    pub(crate) connecting: Option<(SocketAddr, Instant)>,
    /// Row the user expanded for connection details, keyed by session id.
    pub(crate) expanded: Option<u64>,
    /// Latest RMS reported per session, for the connected rows' level bars.
    pub(crate) levels: HashMap<u64, f32>,
    /// Local IPv4 links, ranked best-first; refreshed on an interval.
    pub(crate) links: Vec<RelayLocalLink>,
    /// Active USB tether link, when one is detected. USB is never a select
    /// option: `Auto` prefers it and the panel simply reports the link.
    pub(crate) usb_link: Option<RelayLocalLink>,
    /// Whether a pairing payload has been requested and the cached payload
    /// shown by the Slint relay overlay.
    pub(crate) show_qr: bool,
    pub(crate) qr_text: String,
    last_link_check: Option<Instant>,
}

/// How often the panel rescans network interfaces for a USB tether.
const LINK_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

impl RelayUiState {
    fn role(value: &str) -> RelayRoles {
        match value {
            "emit" => RelayRoles::emit_only(),
            "receive" => RelayRoles::receive_only(),
            _ => RelayRoles::both(),
        }
    }

    fn codec(value: &str) -> RelayCodecKind {
        if value.eq_ignore_ascii_case("pcm") {
            RelayCodecKind::Pcm
        } else {
            RelayCodecKind::Opus
        }
    }

    fn transport(value: &str) -> RelayTransportPreference {
        RelayTransportPreference::from_str(value).unwrap_or_default()
    }

    fn resolve_target(target: &str) -> BackendResult<SocketAddr> {
        target
            .trim()
            .to_socket_addrs()
            .map_err(|error| BackendError::Native(format!("invalid relay target: {error}")))?
            .next()
            .ok_or_else(|| BackendError::Native("relay target did not resolve".into()))
    }

    pub(crate) fn poll(&mut self, app: &mut QpwgraphApp) {
        self.peers = app.driver.relay_peers();
        // Stable, readable order: the engine reports peers from a map keyed
        // by socket address, which jumps around as addresses change.
        self.peers
            .sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.addr.cmp(&b.addr)));
        // Legacy configs may still name USB explicitly; it is now detected
        // automatically under `Auto` and no longer offered as a choice.
        if app.config.relay_transport == "usb" {
            app.config.relay_transport = "auto".to_owned();
        }
        let refresh_due = self
            .last_link_check
            .is_none_or(|at| at.elapsed() >= LINK_CHECK_INTERVAL);
        if refresh_due {
            self.last_link_check = Some(Instant::now());
            self.refresh_links(app);
        }
        let events = app.driver.relay_events();
        for event in events {
            match event {
                RelayEvent::HostStarted { port } => {
                    self.message = app.tf("relay.host_started", &[("port", port.to_string())]);
                }
                RelayEvent::HostStopped => {
                    self.message = app.t("relay.host_stopped");
                }
                RelayEvent::PeerDiscovered { peer } => {
                    self.message = app.tf("relay.peer_discovered", &[("name", peer.name)]);
                }
                RelayEvent::PeerLost { peer } => {
                    self.message = app.tf("relay.peer_lost", &[("name", peer.name)]);
                }
                RelayEvent::SessionEstablished { peer, .. } => {
                    if self.connecting.is_some_and(|(addr, _)| addr == peer.addr) {
                        self.connecting = None;
                    }
                    self.message = app.tf("relay.session_connected", &[("name", peer.name)]);
                }
                RelayEvent::SessionLost { reason, id } => {
                    self.levels.remove(&id.0);
                    if self.expanded == Some(id.0) {
                        self.expanded = None;
                    }
                    self.message = app.tf("relay.session_lost", &[("reason", reason)]);
                }
                RelayEvent::Error { message } => {
                    // A failed attempt must release the row, otherwise it
                    // stays stuck on "Connecting…" with no way back.
                    self.connecting = None;
                    self.message = app.tf("relay.error", &[("error", message)]);
                }
                RelayEvent::AudioLevel { id, rms } => {
                    self.levels.insert(id.0, rms);
                }
            }
        }
        if self
            .connecting
            .is_some_and(|(_, since)| since.elapsed() >= CONNECT_ATTEMPT_TIMEOUT)
        {
            self.connecting = None;
        }
    }

    /// The unified device list: live sessions first, then discovered hosts
    /// that are not already connected, then an in-flight attempt to an
    /// address discovery has not reported (a manually typed or scanned one).
    pub(crate) fn device_rows(&self, app: &QpwgraphApp) -> Vec<RelayDeviceRow> {
        let sessions = app.driver.relay_status().sessions;
        let mut rows: Vec<RelayDeviceRow> = sessions
            .iter()
            .map(|session| RelayDeviceRow {
                name: session.peer.name.clone(),
                addr: session.peer.addr,
                state: RelayDeviceState::Connected(session.id),
            })
            .collect();
        rows.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.addr.cmp(&b.addr)));
        let connected: Vec<SocketAddr> = rows.iter().map(|row| row.addr).collect();
        let connecting = self.connecting.map(|(addr, _)| addr);
        for peer in &self.peers {
            if connected.contains(&peer.addr) {
                continue;
            }
            rows.push(RelayDeviceRow {
                name: peer.name.clone(),
                addr: peer.addr,
                state: if connecting == Some(peer.addr) {
                    RelayDeviceState::Connecting
                } else {
                    RelayDeviceState::Available
                },
            });
        }
        if let Some(addr) = connecting {
            if !rows.iter().any(|row| row.addr == addr) {
                rows.push(RelayDeviceRow {
                    name: addr.to_string(),
                    addr,
                    state: RelayDeviceState::Connecting,
                });
            }
        }
        rows
    }

    pub(crate) fn host_request(&self, app: &QpwgraphApp) -> RelayHostRequest {
        RelayHostRequest {
            device_name: app.config.relay_device_name.clone(),
            pin: app.config.relay_host_pin.clone(),
            port: app.config.relay_host_port,
            codec: Self::codec(&app.config.relay_codec),
            frame_ms: app.config.relay_frame_ms,
            transport: Self::transport(&app.config.relay_transport),
        }
    }

    /// Payload encoded in the "scan to connect" QR: the best-ranked local
    /// address with the live control port and the pairing PIN. Links come
    /// back USB-first, so a tethered phone scans the tether address.
    pub(crate) fn qr_payload(app: &QpwgraphApp) -> Option<String> {
        let port = app.driver.relay_status().host_port?;
        let link = app
            .relay
            .links
            .first()
            .cloned()
            .or_else(|| app.driver.relay_local_links().into_iter().next())?;
        Some(relay_build_qr_payload(
            link.addr,
            port,
            &app.config.relay_host_pin,
        ))
    }

    fn refresh_links(&mut self, app: &QpwgraphApp) {
        self.links = app.driver.relay_local_links();
        self.usb_link = self
            .links
            .iter()
            .find(|link| link.kind == RelayLinkKind::Usb)
            .cloned();
    }

    pub(crate) fn start_host(&mut self, app: &mut QpwgraphApp) {
        let request = self.host_request(app);
        match app.driver.relay_start_host(request) {
            Ok(port) => {
                self.message = app.tf("relay.host_started", &[("port", port.to_string())]);
                self.refresh_links(app);
                app.refresh_graph();
            }
            Err(error) => self.message = app.tf("relay.error", &[("error", error.to_string())]),
        }
    }

    pub(crate) fn stop_host(&mut self, app: &mut QpwgraphApp) {
        match app.driver.relay_stop_host() {
            Ok(()) => self.message = app.t("relay.host_stopped"),
            Err(error) => self.message = app.tf("relay.error", &[("error", error.to_string())]),
        }
    }

    pub(crate) fn connect(&mut self, app: &mut QpwgraphApp) {
        // Accept a pasted QR payload (`qpw-relay://host:port?pin=...`) in
        // the address field: normalize it back to a plain `host:port` and
        // fill in the PIN it carries, so the saved config stays clean.
        if let Some(payload) = relay_parse_qr_payload(&app.config.relay_client_target) {
            app.config.relay_client_target = payload.target;
            if let Some(pin) = payload.pin {
                app.config.relay_client_pin = pin;
            }
        }
        let target_text = app.config.relay_client_target.clone();
        self.connect_target(app, &target_text);
    }

    /// Connect to an explicit target string (Discover-tab quick connect).
    /// The string may be a plain `host:port` or a full QR payload.
    pub(crate) fn connect_target(&mut self, app: &mut QpwgraphApp, raw_target: &str) {
        let target_text = match relay_parse_qr_payload(raw_target) {
            Some(payload) => {
                if let Some(pin) = payload.pin {
                    app.config.relay_client_pin = pin;
                }
                payload.target
            }
            None => raw_target.trim().to_owned(),
        };
        let target = match Self::resolve_target(&target_text) {
            Ok(target) => target,
            Err(error) => {
                self.message = app.tf("relay.error", &[("error", error.to_string())]);
                return;
            }
        };
        match app.driver.relay_connect(
            target,
            &app.config.relay_client_pin,
            Self::role(&app.config.relay_role),
        ) {
            Ok(()) => {
                self.connecting = Some((target, Instant::now()));
                self.message = app.t("relay.connecting");
            }
            Err(error) => {
                self.connecting = None;
                self.message = app.tf("relay.error", &[("error", error.to_string())]);
            }
        }
    }

    /// Abandon an in-flight connect attempt. The engine has no cancel; the
    /// row simply stops claiming to be connecting, and a session that does
    /// arrive later still shows up as connected.
    pub(crate) fn cancel_connect(&mut self) {
        self.connecting = None;
    }

    pub(crate) fn disconnect(&mut self, app: &mut QpwgraphApp, session: RelaySessionId) {
        match app.driver.relay_disconnect(session) {
            Ok(()) => self.message = app.t("relay.disconnecting"),
            Err(error) => self.message = app.tf("relay.error", &[("error", error.to_string())]),
        }
    }

    /// Restart browsing from scratch, the way a Wi-Fi or Bluetooth panel's
    /// refresh does: drop the stale peer list, re-prime the mDNS browse and
    /// the USB probe, and rescan local links.
    pub(crate) fn refresh(&mut self, app: &mut QpwgraphApp) {
        self.stop_discovery(app);
        self.peers.clear();
        self.discovery_failed = false;
        self.refresh_links(app);
        self.start_discovery(app);
    }

    /// Begin browsing for relay hosts. Idempotent; failures are remembered
    /// in `discovery_failed` so the Discover tab does not retry every frame.
    pub(crate) fn start_discovery(&mut self, app: &mut QpwgraphApp) {
        if self.discovery_active {
            return;
        }
        match app.driver.relay_discovery_start() {
            Ok(()) => {
                self.discovery_active = true;
                self.discovery_failed = false;
                self.message = app.t("relay.discovery_started");
            }
            Err(error) => {
                self.discovery_failed = true;
                self.message = app.tf("relay.error", &[("error", error.to_string())]);
            }
        }
    }

    pub(crate) fn stop_discovery(&mut self, app: &mut QpwgraphApp) {
        if !self.discovery_active {
            return;
        }
        app.driver.relay_discovery_stop();
        self.discovery_active = false;
        self.discovery_failed = false;
        self.message = app.t("relay.discovery_stopped");
    }
}
