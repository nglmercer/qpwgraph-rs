//! Relay presentation, configuration conversion, and endpoint helpers.
//!
//! The bridge keeps protocol/event orchestration in relay.rs while this
//! module owns the small pure mappings and UI-facing relay projections.

use super::*;

#[cfg(all(feature = "relay", test))]
pub(crate) fn desktop_roles(direction: AudioDirection) -> pw_graph_backend::RelayRoles {
    match direction {
        AudioDirection::MobileToDesktop => pw_graph_backend::RelayRoles::receive_only(),
        AudioDirection::DesktopToMobile => pw_graph_backend::RelayRoles::emit_only(),
    }
}

#[cfg(feature = "relay")]
pub(crate) fn backend_direction(direction: AudioDirection) -> RelayDirection {
    match direction {
        AudioDirection::MobileToDesktop => RelayDirection::MobileToDesktop,
        AudioDirection::DesktopToMobile => RelayDirection::DesktopToMobile,
    }
}

#[cfg(feature = "relay")]
pub(crate) fn config_direction(direction: RelayDirection) -> AudioDirection {
    match direction {
        RelayDirection::MobileToDesktop => AudioDirection::MobileToDesktop,
        RelayDirection::DesktopToMobile => AudioDirection::DesktopToMobile,
    }
}

#[cfg(feature = "relay")]
pub(crate) fn relay_codec(value: &str) -> RelayCodecKind {
    if value.eq_ignore_ascii_case("pcm") {
        RelayCodecKind::Pcm
    } else {
        RelayCodecKind::Opus
    }
}

#[cfg(feature = "relay")]
pub(crate) fn relay_transport(value: &str) -> RelayTransportPreference {
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
pub(crate) fn host_link_addr(application: &Application) -> Option<std::net::Ipv4Addr> {
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

#[cfg(test)]
pub(crate) fn relay_direction_tab(direction: AudioDirection) -> i32 {
    match direction {
        AudioDirection::MobileToDesktop => 0,
        AudioDirection::DesktopToMobile => 1,
    }
}

#[cfg(test)]
pub(crate) fn relay_direction_from_tab(index: i32, fallback: AudioDirection) -> AudioDirection {
    match index {
        0 => AudioDirection::MobileToDesktop,
        1 => AudioDirection::DesktopToMobile,
        _ => fallback,
    }
}

/// Generic tab mapping used by the public panel. Index zero is always
/// Emitter, index one Receiver, and the third tab remains Advanced.
pub(crate) fn relay_mode_tab(mode: pw_graph_config::RelayMode) -> i32 {
    match mode {
        pw_graph_config::RelayMode::Emitter => 0,
        pw_graph_config::RelayMode::Receiver => 1,
    }
}

pub(crate) fn relay_mode_from_tab(
    index: i32,
    fallback: pw_graph_config::RelayMode,
) -> pw_graph_config::RelayMode {
    match index {
        0 => pw_graph_config::RelayMode::Emitter,
        1 => pw_graph_config::RelayMode::Receiver,
        _ => fallback,
    }
}

#[cfg(feature = "relay")]
pub(crate) fn backend_mode(mode: pw_graph_config::RelayMode) -> pw_graph_backend::RelayMode {
    match mode {
        pw_graph_config::RelayMode::Emitter => pw_graph_backend::RelayMode::Emitter,
        pw_graph_config::RelayMode::Receiver => pw_graph_backend::RelayMode::Receiver,
    }
}

#[cfg(feature = "relay")]
pub(crate) fn config_mode(mode: pw_graph_backend::RelayMode) -> pw_graph_config::RelayMode {
    match mode {
        pw_graph_backend::RelayMode::Emitter => pw_graph_config::RelayMode::Emitter,
        pw_graph_backend::RelayMode::Receiver => pw_graph_config::RelayMode::Receiver,
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
pub(crate) const FRAME_DURATIONS_MS: [u16; 5] = [5, 10, 20, 40, 60];

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
