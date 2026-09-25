use jni::objects::{JFloatArray, JString};
use jni::sys::{jint, jlong};
use jni::JNIEnv;
use pw_graph_relay_sdk::{
    CodecKind, DeviceKind, EngineStatus, LinkKind, PeerInfo, RelayDirection, RelayEvent,
    RelayHandle, RelayMode, SessionId, TransportPreference, TrustedPeer,
    MAX_DISCOVERED_PEER_ADDRESSES, MAX_REALTIME_QUANTUM_SAMPLES, MAX_TRUSTED_PEERS,
};
use serde_json::json;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicI64, Ordering};

static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);
static NEXT_OPERATION: AtomicI64 = AtomicI64::new(1);
pub(crate) fn string(env: &mut JNIEnv<'_>, value: JString<'_>) -> Result<String, String> {
    env.get_string(&value)
        .map(|value| value.to_string_lossy().into_owned())
        .map_err(|error| error.to_string())
}
pub(crate) fn json_string(
    env: &mut JNIEnv<'_>,
    value: serde_json::Value,
) -> jni::errors::Result<jni::sys::jstring> {
    let text = env.new_string(value.to_string())?;
    Ok(text.into_raw())
}
pub(crate) fn error_json(
    env: &mut JNIEnv<'_>,
    error: impl ToString,
) -> jni::errors::Result<jni::sys::jstring> {
    let message = error.to_string();
    let code = native_error_code(&message);
    json_string(env, json!({"type":"error","code":code,"message":message}))
}
pub(crate) fn catch_native<T, F>(operation: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String>,
{
    catch_unwind(AssertUnwindSafe(operation))
        .map_err(|_| "native relay operation panicked".to_string())?
}
pub(crate) fn json_response(
    env: &mut JNIEnv<'_>,
    result: Result<serde_json::Value, String>,
) -> jni::sys::jstring {
    let response = catch_unwind(AssertUnwindSafe(|| match result {
        Ok(value) => json_string(env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(env, error).unwrap_or(std::ptr::null_mut()),
    }));
    match response {
        Ok(value) => value,
        Err(_) => catch_unwind(AssertUnwindSafe(|| {
            json_string(
                env,
                json!({
                    "type": "error",
                    "code": "internal_error",
                    "message": "native relay operation panicked",
                }),
            )
            .unwrap_or(std::ptr::null_mut())
        }))
        .unwrap_or(std::ptr::null_mut()),
    }
}
pub(crate) fn native_error_code(message: &str) -> &'static str {
    match message {
        "unknown client handle" => "unknown_client_handle",
        "unknown host handle" => "unknown_host_handle",
        "unknown discovery handle" => "unknown_discovery_handle",
        _ => "internal_error",
    }
}
pub(crate) fn parse_direction(value: &str) -> Result<RelayDirection, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "emit" => Ok(RelayDirection::MobileToDesktop),
        "receive" => Ok(RelayDirection::DesktopToMobile),
        "both" => Err("the Android relay accepts one-way audio only; both is disabled".into()),
        other => {
            RelayDirection::parse(other).ok_or_else(|| format!("unknown audio direction '{other}'"))
        }
    }
}
pub(crate) fn parse_mode(value: &str) -> Result<RelayMode, String> {
    RelayMode::parse(value).ok_or_else(|| {
        format!(
            "unknown relay mode '{}'; expected emitter or receiver",
            value.trim()
        )
    })
}
pub(crate) fn direction_generation(value: jlong) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| "direction generation must not be negative".into())
}
pub(crate) fn parse_codec(value: &str) -> Result<CodecKind, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "pcm" => Ok(CodecKind::Pcm),
        "opus" => Ok(CodecKind::Opus),
        other => Err(format!("unknown codec '{other}'")),
    }
}
pub(crate) fn parse_transport(value: &str) -> Result<TransportPreference, String> {
    value.parse()
}
#[derive(serde::Deserialize)]
struct StoredTrustedPeer {
    peer_id: String,
    secret: String,
}
#[derive(serde::Deserialize)]
struct DiscoveredPeerSnapshot {
    id: String,
    name: String,
    address: String,
    #[serde(default)]
    link: Option<String>,
}
pub(crate) fn trusted_secret(value: &str) -> Result<[u8; 32], String> {
    let bytes = pw_graph_utils::hex::hex_decode(value.trim())
        .map_err(|_| "trusted relay secret must be 64 hexadecimal characters".to_string())?;
    bytes
        .try_into()
        .map_err(|_| "trusted relay secret must be exactly 32 bytes".to_string())
}
pub(crate) fn parse_trusted_peers(value: &str) -> Result<Vec<TrustedPeer>, String> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    let peers = serde_json::from_str::<Vec<StoredTrustedPeer>>(value)
        .map_err(|error| format!("invalid trusted relay credentials: {error}"))?;
    if peers.len() > MAX_TRUSTED_PEERS {
        return Err(format!(
            "too many trusted relay credentials (maximum is {MAX_TRUSTED_PEERS})"
        ));
    }
    peers
        .into_iter()
        .map(|stored| {
            if stored.peer_id.trim().is_empty() {
                return Err("trusted relay peer id must not be empty".into());
            }
            Ok(TrustedPeer {
                peer_id: stored.peer_id,
                secret: trusted_secret(&stored.secret)?,
            })
        })
        .collect()
}
pub(crate) fn parse_discovered_peers(
    value: &str,
) -> Result<Vec<(PeerInfo, Option<LinkKind>)>, String> {
    const MAX_DISCOVERY_SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;
    if value.len() > MAX_DISCOVERY_SNAPSHOT_BYTES {
        return Err("discovered relay peer snapshot is too large".into());
    }
    let peers = serde_json::from_str::<Vec<DiscoveredPeerSnapshot>>(value)
        .map_err(|error| format!("invalid discovered relay peers: {error}"))?;
    if peers.len() > MAX_DISCOVERED_PEER_ADDRESSES {
        return Err(format!(
            "too many discovered relay peers (maximum is {MAX_DISCOVERED_PEER_ADDRESSES})"
        ));
    }
    peers
        .into_iter()
        .map(|peer| {
            let addr = peer
                .address
                .parse()
                .map_err(|error| format!("invalid discovered relay address: {error}"))?;
            let link = match peer.link.as_deref().map(str::trim) {
                Some("usb") => Some(LinkKind::Usb),
                Some("wifi") => Some(LinkKind::Wifi),
                Some("bluetooth") => Some(LinkKind::BluetoothPan),
                Some("lan") => Some(LinkKind::Lan),
                _ => None,
            };
            Ok((
                PeerInfo {
                    id: peer.id,
                    name: peer.name,
                    kind: DeviceKind::Other,
                    addr,
                },
                link,
            ))
        })
        .collect()
}
pub(crate) fn positive_u16(name: &str, value: jint) -> Result<u16, String> {
    if value <= 0 {
        return Err(format!("{name} must be positive"));
    }
    u16::try_from(value).map_err(|_| format!("{name} is out of range"))
}
pub(crate) fn positive_u32(name: &str, value: jint) -> Result<u32, String> {
    if value <= 0 {
        return Err(format!("{name} must be positive"));
    }
    u32::try_from(value).map_err(|_| format!("{name} is out of range"))
}
pub(crate) fn android_channels(value: jint) -> Result<u16, String> {
    let channels = positive_u16("channels", value)?;
    if channels != 1 && channels != 2 {
        return Err("Android relay audio supports mono or stereo (channels=1 or 2)".into());
    }
    Ok(channels)
}
pub(crate) fn port_u16(value: jint) -> Result<u16, String> {
    if value < 0 {
        return Err("port must not be negative".into());
    }
    u16::try_from(value).map_err(|_| "port is out of range".into())
}
pub(crate) fn next_operation() -> i64 {
    NEXT_OPERATION.fetch_add(1, Ordering::Relaxed)
}
pub(crate) fn requested_pcm_length(
    env: &mut JNIEnv<'_>,
    array: &JFloatArray<'_>,
    requested: jint,
) -> Result<Option<usize>, String> {
    if requested < 0 {
        return Err("PCM length must not be negative".into());
    }
    let array_length = env
        .get_array_length(array)
        .map_err(|error| error.to_string())? as usize;
    let requested = usize::try_from(requested).map_err(|_| "PCM length is invalid".to_string())?;
    if requested > array_length {
        return Err("PCM length exceeds the Java array".into());
    }
    if requested > MAX_REALTIME_QUANTUM_SAMPLES {
        return Ok(None);
    }
    Ok(Some(requested))
}
pub(crate) fn usb_link_json() -> serde_json::Value {
    match pw_graph_relay_sdk::LocalLink::find_usb() {
        Some(link) => link_json(&link),
        None => json!({"type": "none"}),
    }
}
pub(crate) fn local_links_json() -> serde_json::Value {
    let links: Vec<serde_json::Value> = pw_graph_relay_sdk::local_links()
        .iter()
        .map(link_json)
        .collect();
    json!({ "type": "links", "links": links })
}
pub(crate) fn connected_json(session: SessionId, host_name: &str) -> serde_json::Value {
    json!({
        "type": "connected",
        "session": session.0,
        "host": host_name,
    })
}
pub(crate) fn host_display_address(handle: &RelayHandle, status: &EngineStatus) -> Option<String> {
    status
        .host_addr
        .or_else(|| {
            let config = handle.config();
            pw_graph_relay_sdk::listen_bind_addr(
                &pw_graph_relay_sdk::display_links(),
                config.transport,
            )
        })
        .map(|address| address.to_string())
}
pub(crate) fn link_json(link: &pw_graph_relay_sdk::LocalLink) -> serde_json::Value {
    json!({
        "type": "usb_link",
        "name": link.name,
        "addr": link.addr.to_string(),
        "kind": link.kind.as_str(),
    })
}
pub(crate) fn event_json(event: RelayEvent) -> serde_json::Value {
    match event {
        RelayEvent::SessionEstablished { id, peer, .. } => json!({
            "type": "connected", "session": id.0, "host": peer.name,
            "id": peer.id, "address": peer.addr.to_string()
        }),
        RelayEvent::SessionLost { id, reason } => {
            json!({"type":"disconnected","session":id.0,"message":reason})
        }
        RelayEvent::DirectionResolved {
            id,
            generation,
            direction,
            winner_device_id,
        } => json!({
            "type": "direction_resolved",
            "session": id.0,
            "generation": generation,
            "direction": direction.as_str(),
            "winner_device_id": winner_device_id,
        }),
        RelayEvent::FlowResolved {
            id,
            generation,
            flow,
            mode,
        } => json!({
            "type": "mode_resolved",
            "session": id.0,
            "generation": generation,
            "mode": mode.as_str(),
            "emitter_id": flow.emitter_id,
        }),
        RelayEvent::AudioLevel { id, rms } => json!({
            "type":"level", "session":id.0, "rms":rms
        }),
        RelayEvent::Error { message } => json!({"type":"error","message":message}),
        RelayEvent::PeerDiscovered { peer } => json!({
            "type":"peer","id":peer.id,"name":peer.name,"address":peer.addr.to_string()
        }),
        RelayEvent::PeerLost { peer } => json!({
            "type":"peer_lost","id":peer.id,"name":peer.name,"address":peer.addr.to_string()
        }),
        RelayEvent::TrustedPeerAvailable { peer_id, peer, .. } => json!({
            "type": "trusted_peer_available",
            "peer_id": peer_id,
            "id": peer.id,
            "name": peer.name,
            "address": peer.addr.to_string(),
        }),
        RelayEvent::TrustedPeerEnrollmentRequested {
            transaction_id,
            peer_id,
            peer,
        } => json!({
            "type": "trusted_enrollment_requested",
            "transaction_id": transaction_id,
            "peer_id": peer_id,
            "id": peer.id,
            "name": peer.name,
            "address": peer.addr.to_string(),
        }),
        RelayEvent::HostStarted { port } => json!({"type":"host_started","port":port}),
        RelayEvent::HostStopped => json!({"type":"host_stopped"}),
    }
}
pub(crate) fn session_status_json(
    session: &pw_graph_relay_sdk::SessionStatus,
) -> serde_json::Value {
    json!({
        "id": session.id.0,
        "peer_id": session.peer.id,
        "name": session.peer.name,
        "address": session.peer.addr.to_string(),
        "sending": session.sending,
        "receiving": session.receiving,
        "transport": session.transport,
        "link": session.link,
        "local_addr": session.local_addr.map(|address| address.to_string()),
        "remote_addr": session.remote_addr.to_string(),
        "control_state": session.control_state,
        "audio_channel_state": session.audio_channel_state,
        "trusted": session.trusted,
        "mode": session.mode.map(|mode| mode.as_str()),
        "emitter_id": session.flow.as_ref().map(|flow| flow.emitter_id.clone()),
    })
}
pub(crate) fn engine_status_json(engine: &RelayHandle) -> serde_json::Value {
    let status = engine.status();
    json!({
        "host_active": status.host_active,
        "port": status.host_port,
        "address": host_display_address(engine, &status),
        "sessions": status.sessions.iter().map(session_status_json).collect::<Vec<_>>(),
    })
}

pub(crate) fn next_handle() -> i64 {
    NEXT_HANDLE.fetch_add(1, Ordering::Relaxed)
}
