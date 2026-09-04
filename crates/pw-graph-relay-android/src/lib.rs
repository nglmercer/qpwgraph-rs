use jni::objects::{JClass, JFloatArray, JString};
use jni::sys::{jboolean, jint, jlong};
use jni::JNIEnv;
use pw_graph_relay_sdk::{
    CodecKind, DeviceKind, EngineStatus, LinkKind, PeerInfo, RelayBrowser, RelayClient,
    RelayClientBuilder, RelayDirection, RelayEvent, RelayHandle, RelayHost, RelayHostBuilder,
    RelayHostPrepared, RelayMode, SessionId, TransportPreference, TrustedPeer,
    MAX_DISCOVERED_PEER_ADDRESSES, MAX_REALTIME_QUANTUM_SAMPLES, MAX_TRUSTED_PEERS,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};

static NEXT_HANDLE: AtomicI64 = AtomicI64::new(1);
static NEXT_OPERATION: AtomicI64 = AtomicI64::new(1);
static CLIENTS: OnceLock<Mutex<HashMap<i64, ClientSlot>>> = OnceLock::new();

/// Wrapper module carrying a target-scoped lint suppression.
///
/// `clippy::missing_const_for_thread_local` misreads the `thread_local!`
/// expansion on targets without native TLS and asks for the `const`
/// initialiser that is already written below. Verified with clippy 1.97.1:
/// the identical item is clean for `x86_64-unknown-linux-gnu` and warns for
/// `aarch64-linux-android`. The allow is scoped to Android so a genuine
/// regression still fails the host lint, and an attribute has to sit on this
/// module because one placed on the macro invocation itself is ignored.
#[cfg_attr(target_os = "android", allow(clippy::missing_const_for_thread_local))]
mod pcm_scratch {
    use std::cell::RefCell;

    thread_local! {
        /// Per-JNI-thread PCM storage. It grows at most once to the realtime
        /// quantum and is filled before the engine call, so native audio
        /// methods do not allocate a Vec on every callback.
        pub static PCM_SCRATCH: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
    }
}

use pcm_scratch::PCM_SCRATCH;

fn clients() -> &'static Mutex<HashMap<i64, ClientSlot>> {
    CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn string(env: &mut JNIEnv<'_>, value: JString<'_>) -> Result<String, String> {
    env.get_string(&value)
        .map(|value| value.to_string_lossy().into_owned())
        .map_err(|error| error.to_string())
}

fn json_string(
    env: &mut JNIEnv<'_>,
    value: serde_json::Value,
) -> jni::errors::Result<jni::sys::jstring> {
    let text = env.new_string(value.to_string())?;
    Ok(text.into_raw())
}

fn error_json(
    env: &mut JNIEnv<'_>,
    error: impl ToString,
) -> jni::errors::Result<jni::sys::jstring> {
    let message = error.to_string();
    let code = native_error_code(&message);
    json_string(env, json!({"type":"error","code":code,"message":message}))
}

fn native_error_code(message: &str) -> &'static str {
    match message {
        "unknown client handle" => "unknown_client_handle",
        "unknown host handle" => "unknown_host_handle",
        _ => "internal_error",
    }
}

fn parse_direction(value: &str) -> Result<RelayDirection, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "emit" => Ok(RelayDirection::MobileToDesktop),
        "receive" => Ok(RelayDirection::DesktopToMobile),
        "both" => Err("the Android relay accepts one-way audio only; both is disabled".into()),
        other => {
            RelayDirection::parse(other).ok_or_else(|| format!("unknown audio direction '{other}'"))
        }
    }
}

fn parse_mode(value: &str) -> Result<RelayMode, String> {
    RelayMode::parse(value).ok_or_else(|| {
        format!(
            "unknown relay mode '{}'; expected emitter or receiver",
            value.trim()
        )
    })
}

fn direction_generation(value: jlong) -> Result<u64, String> {
    u64::try_from(value).map_err(|_| "direction generation must not be negative".into())
}

fn parse_codec(value: &str) -> Result<CodecKind, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "pcm" => Ok(CodecKind::Pcm),
        "opus" => Ok(CodecKind::Opus),
        other => Err(format!("unknown codec '{other}'")),
    }
}

fn parse_transport(value: &str) -> Result<TransportPreference, String> {
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

fn trusted_secret(value: &str) -> Result<[u8; 32], String> {
    let bytes = pw_graph_utils::hex::hex_decode(value.trim())
        .map_err(|_| "trusted relay secret must be 64 hexadecimal characters".to_string())?;
    bytes
        .try_into()
        .map_err(|_| "trusted relay secret must be exactly 32 bytes".to_string())
}

fn parse_trusted_peers(value: &str) -> Result<Vec<TrustedPeer>, String> {
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

fn parse_discovered_peers(value: &str) -> Result<Vec<(PeerInfo, Option<LinkKind>)>, String> {
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

fn positive_u16(name: &str, value: jint) -> Result<u16, String> {
    if value <= 0 {
        return Err(format!("{name} must be positive"));
    }
    u16::try_from(value).map_err(|_| format!("{name} is out of range"))
}

fn positive_u32(name: &str, value: jint) -> Result<u32, String> {
    if value <= 0 {
        return Err(format!("{name} must be positive"));
    }
    u32::try_from(value).map_err(|_| format!("{name} is out of range"))
}

/// The Android audio pump speaks mono and stereo: AudioRecord/AudioTrack use
/// the matching channel masks and the PCM arrays hold one sample per channel.
/// The negotiable wire set is exactly 1 or 2, so anything else is rejected at
/// the native boundary before the Java worker can be asked for it.
fn android_channels(value: jint) -> Result<u16, String> {
    let channels = positive_u16("channels", value)?;
    if channels != 1 && channels != 2 {
        return Err("Android relay audio supports mono or stereo (channels=1 or 2)".into());
    }
    Ok(channels)
}

fn port_u16(value: jint) -> Result<u16, String> {
    if value < 0 {
        return Err("port must not be negative".into());
    }
    u16::try_from(value).map_err(|_| "port is out of range".into())
}

fn next_operation() -> i64 {
    NEXT_OPERATION.fetch_add(1, Ordering::Relaxed)
}

fn requested_pcm_length(
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

fn client_engine_handle(handle: jlong) -> Result<Option<RelayHandle>, String> {
    let guard = clients()
        .lock()
        .map_err(|_| "client store poisoned".to_string())?;
    Ok(match guard.get(&handle) {
        Some(ClientSlot::Connected(client)) => Some(client.handle()),
        Some(ClientSlot::Prepared(_) | ClientSlot::Connecting { .. }) => None,
        None => return Err("unknown client handle".into()),
    })
}

fn host_engine_handle(handle: jlong) -> Result<Option<RelayHandle>, String> {
    let guard = hosts()
        .lock()
        .map_err(|_| "host store poisoned".to_string())?;
    Ok(match guard.get(&handle) {
        Some(HostSlot::Running(running)) => Some(running.host.handle()),
        Some(HostSlot::Prepared(_) | HostSlot::Starting { .. } | HostSlot::Stopping { .. }) => None,
        None => return Err("unknown host handle".into()),
    })
}

/// JSON snapshot of the first USB tether link, or `{"type":"none"}` when no
/// USB link is up. USB is auto-detected rather than user-selected, matching
/// the desktop panel.
fn usb_link_json() -> serde_json::Value {
    match pw_graph_relay_sdk::LocalLink::find_usb() {
        Some(link) => link_json(&link),
        None => json!({"type": "none"}),
    }
}

/// JSON snapshot of every usable local link, ranked best-first. Lets the UI
/// render the addresses a peer should dial (with the host port appended).
fn local_links_json() -> serde_json::Value {
    let links: Vec<serde_json::Value> = pw_graph_relay_sdk::local_links()
        .iter()
        .map(link_json)
        .collect();
    json!({ "type": "links", "links": links })
}

fn connected_json(session: SessionId, host_name: &str) -> serde_json::Value {
    json!({
        "type": "connected",
        "session": session.0,
        "host": host_name,
    })
}

fn host_display_address(handle: &RelayHandle, status: &EngineStatus) -> Option<String> {
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

fn link_json(link: &pw_graph_relay_sdk::LocalLink) -> serde_json::Value {
    json!({
        "type": "usb_link",
        "name": link.name,
        "addr": link.addr.to_string(),
        "kind": link.kind.as_str(),
    })
}

fn event_json(event: RelayEvent) -> serde_json::Value {
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

fn session_status_json(session: &pw_graph_relay_sdk::SessionStatus) -> serde_json::Value {
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

fn engine_status_json(engine: &RelayHandle) -> serde_json::Value {
    let status = engine.status();
    json!({
        "host_active": status.host_active,
        "port": status.host_port,
        "address": host_display_address(engine, &status),
        "sessions": status.sessions.iter().map(session_status_json).collect::<Vec<_>>(),
    })
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_create(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    device_name: JString<'_>,
    device_id: JString<'_>,
    trusted_peers: JString<'_>,
    direction: JString<'_>,
    generation: jlong,
    codec: JString<'_>,
    transport: JString<'_>,
    sample_rate: jint,
    channels: jint,
    frame_ms: jint,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let device_name = string(&mut env, device_name)?;
        let device_id = string(&mut env, device_id)?;
        let trusted_peers = parse_trusted_peers(&string(&mut env, trusted_peers)?)?;
        let direction = parse_direction(&string(&mut env, direction)?)?;
        let generation = direction_generation(generation)?;
        let codec = parse_codec(&string(&mut env, codec)?)?;
        let transport = parse_transport(&string(&mut env, transport)?)?;
        let sample_rate = positive_u32("sample rate", sample_rate)?;
        let channels = android_channels(channels)?;
        let frame_ms = positive_u16("frame duration", frame_ms)?;
        let mut builder = RelayClientBuilder::new()
            .device_name(device_name)
            .device_kind(DeviceKind::Android)
            .direction(direction)
            .direction_generation(generation)
            .codec(codec)
            .transport(transport)
            .audio(sample_rate, channels, frame_ms)
            .trusted_peers(trusted_peers);
        if !device_id.trim().is_empty() {
            builder = builder.device_id(device_id);
        }
        let client = builder
            .trust_new_peers(true)
            .build()
            .map_err(|error| error.to_string())?;
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        let mut guard = clients()
            .lock()
            .map_err(|_| "client store poisoned".to_string())?;
        guard.insert(handle, ClientSlot::Prepared(client));
        Ok(json!({"type":"created", "handle":handle}))
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_createMode(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    device_name: JString<'_>,
    device_id: JString<'_>,
    trusted_peers: JString<'_>,
    mode: JString<'_>,
    generation: jlong,
    codec: JString<'_>,
    transport: JString<'_>,
    sample_rate: jint,
    channels: jint,
    frame_ms: jint,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let device_name = string(&mut env, device_name)?;
        let device_id = string(&mut env, device_id)?;
        let trusted_peers = parse_trusted_peers(&string(&mut env, trusted_peers)?)?;
        let mode = parse_mode(&string(&mut env, mode)?)?;
        let generation = direction_generation(generation)?;
        let codec = parse_codec(&string(&mut env, codec)?)?;
        let transport = parse_transport(&string(&mut env, transport)?)?;
        let sample_rate = positive_u32("sample rate", sample_rate)?;
        let channels = android_channels(channels)?;
        let frame_ms = positive_u16("frame duration", frame_ms)?;
        let mut builder = RelayClientBuilder::new()
            .device_name(device_name)
            .device_kind(DeviceKind::Android)
            .mode(mode)
            .direction_generation(generation)
            .codec(codec)
            .transport(transport)
            .audio(sample_rate, channels, frame_ms)
            .trusted_peers(trusted_peers);
        if !device_id.trim().is_empty() {
            builder = builder.device_id(device_id);
        }
        let client = builder
            .trust_new_peers(true)
            .build()
            .map_err(|error| error.to_string())?;
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        let mut guard = clients()
            .lock()
            .map_err(|_| "client store poisoned".to_string())?;
        guard.insert(handle, ClientSlot::Prepared(client));
        Ok(json!({"type":"created", "handle":handle}))
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

enum ClientSlot {
    Prepared(pw_graph_relay_sdk::RelayClientPrepared),
    Connecting { token: i64 },
    Connected(RelayClient),
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_connect(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    target: JString<'_>,
    pin: JString<'_>,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let target = string(&mut env, target)?;
        let pin = string(&mut env, pin)?;
        let prepared = {
            let mut guard = clients()
                .lock()
                .map_err(|_| "client store poisoned".to_string())?;
            let prepared = match guard.get(&handle) {
                Some(ClientSlot::Prepared(client)) => client.clone(),
                Some(ClientSlot::Connecting { .. }) => {
                    return Err("client connection is already in progress".into())
                }
                Some(ClientSlot::Connected(_)) => return Err("client is already connected".into()),
                None => return Err("unknown client handle".into()),
            };
            let token = next_operation();
            guard.insert(handle, ClientSlot::Connecting { token });
            (token, prepared)
        };

        let (token, prepared) = prepared;
        // The potentially multi-second resolve/TCP/PAKE/negotiation operation
        // happens with no process-wide registry mutex held.
        let connected = prepared.clone().connect(&target, &pin);
        match connected {
            Ok(client) => {
                let mut guard = clients()
                    .lock()
                    .map_err(|_| "client store poisoned".to_string())?;
                let same_attempt = matches!(
                    guard.get(&handle),
                    Some(ClientSlot::Connecting { token: current, .. }) if *current == token
                );
                if same_attempt {
                    // Read the metadata while the client is still borrowed.
                    // The SDK consumed the initial SessionEstablished event
                    // while connecting, so JNI returns this snapshot directly
                    // rather than waiting for a duplicate event.
                    let metadata = connected_json(client.session(), client.host_name());
                    guard.insert(handle, ClientSlot::Connected(client));
                    Ok(metadata)
                } else {
                    drop(guard);
                    let _ = client.disconnect();
                    Err("client handle changed while connecting".into())
                }
            }
            Err(error) => {
                let mut guard = clients()
                    .lock()
                    .map_err(|_| "client store poisoned".to_string())?;
                if matches!(
                    guard.get(&handle),
                    Some(ClientSlot::Connecting { token: current, .. }) if *current == token
                ) {
                    // Keep the validated configuration reusable after a
                    // refused, timed-out, or otherwise failed connection.
                    guard.insert(handle, ClientSlot::Prepared(prepared));
                }
                Err(error.to_string())
            }
        }
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_connectTrusted(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    target: JString<'_>,
    peer_id: JString<'_>,
    secret: JString<'_>,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let target = string(&mut env, target)?;
        let peer_id = string(&mut env, peer_id)?;
        let secret = trusted_secret(&string(&mut env, secret)?)?;
        if peer_id.trim().is_empty() {
            return Err("trusted relay peer id must not be empty".into());
        }
        let prepared = {
            let mut guard = clients()
                .lock()
                .map_err(|_| "client store poisoned".to_string())?;
            let prepared = match guard.get(&handle) {
                Some(ClientSlot::Prepared(client)) => client.clone(),
                Some(ClientSlot::Connecting { .. }) => {
                    return Err("client connection is already in progress".into())
                }
                Some(ClientSlot::Connected(_)) => return Err("client is already connected".into()),
                None => return Err("unknown client handle".into()),
            };
            let token = next_operation();
            guard.insert(handle, ClientSlot::Connecting { token });
            (token, prepared)
        };

        let (token, prepared) = prepared;
        match prepared.clone().connect_trusted(&target, &peer_id, secret) {
            Ok(client) => {
                let mut guard = clients()
                    .lock()
                    .map_err(|_| "client store poisoned".to_string())?;
                let same_attempt = matches!(
                    guard.get(&handle),
                    Some(ClientSlot::Connecting { token: current, .. }) if *current == token
                );
                if same_attempt {
                    let metadata = connected_json(client.session(), client.host_name());
                    guard.insert(handle, ClientSlot::Connected(client));
                    Ok(metadata)
                } else {
                    drop(guard);
                    let _ = client.disconnect();
                    Err("client handle changed while connecting".into())
                }
            }
            Err(error) => {
                let mut guard = clients()
                    .lock()
                    .map_err(|_| "client store poisoned".to_string())?;
                if matches!(
                    guard.get(&handle),
                    Some(ClientSlot::Connecting { token: current, .. }) if *current == token
                ) {
                    guard.insert(handle, ClientSlot::Prepared(prepared));
                }
                Err(error.to_string())
            }
        }
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

/// Credential-only accessor used by Android immediately after a successful
/// PIN connection. Keeping it out of connected/status JSON prevents a normal
/// diagnostic/UI snapshot from carrying a bearer secret.
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_clientTrustedPeer(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let peer = {
            let guard = clients()
                .lock()
                .map_err(|_| "client store poisoned".to_string())?;
            match guard.get(&handle) {
                Some(ClientSlot::Connected(client)) => client.trusted_peer(),
                Some(ClientSlot::Prepared(_) | ClientSlot::Connecting { .. }) => None,
                None => return Err("unknown client handle".into()),
            }
        };
        Ok(match peer {
            Some(peer) => json!({
                "type": "trusted_peer",
                "peer_id": peer.peer_id,
                "secret": pw_graph_utils::hex::hex_encode(&peer.secret),
            }),
            None => json!({"type": "none"}),
        })
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_removeTrustedPeer(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    peer_id: JString<'_>,
) -> jboolean {
    let result = (|| -> Result<bool, String> {
        let peer_id = string(&mut env, peer_id)?;
        let Some(engine) = client_engine_handle(handle)? else {
            return Ok(false);
        };
        engine
            .remove_trusted_peer(&peer_id)
            .map_err(|error| error.to_string())?;
        Ok(true)
    })();
    u8::from(result.unwrap_or(false))
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_reportError(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    message: JString<'_>,
) -> jboolean {
    let result = (|| -> Result<bool, String> {
        let message = string(&mut env, message)?;
        let Some(engine) = client_engine_handle(handle)? else {
            return Ok(false);
        };
        engine.report_error(message);
        Ok(true)
    })();
    u8::from(result.unwrap_or(false))
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_offerDirection(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    direction: JString<'_>,
    generation: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let engine =
            client_engine_handle(handle)?.ok_or_else(|| "client is not connected".to_string())?;
        let session = u64::try_from(session).map_err(|_| "session id is invalid".to_string())?;
        let direction = parse_direction(&string(&mut env, direction)?)?;
        let generation = direction_generation(generation)?;
        engine
            .offer_direction(SessionId(session), direction, generation)
            .map(|()| {
                json!({
                    "type": "direction_offered",
                    "session": session,
                    "direction": direction.as_str(),
                    "generation": generation,
                })
            })
            .map_err(|error| error.to_string())
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_offerMode(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    mode: JString<'_>,
    generation: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let engine =
            client_engine_handle(handle)?.ok_or_else(|| "client is not connected".to_string())?;
        let session = u64::try_from(session).map_err(|_| "session id is invalid".to_string())?;
        let mode = parse_mode(&string(&mut env, mode)?)?;
        let generation = direction_generation(generation)?;
        engine
            .offer_mode(SessionId(session), mode, generation)
            .map(|()| {
                json!({
                    "type": "mode_offered",
                    "session": session,
                    "mode": mode.as_str(),
                    "generation": generation,
                })
            })
            .map_err(|error| error.to_string())
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_disconnect(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    let result = clients()
        .lock()
        .ok()
        .and_then(|mut guard| guard.remove(&handle));
    let result = match result {
        Some(ClientSlot::Connected(client)) => {
            client.disconnect().map_err(|error| error.to_string())
        }
        Some(ClientSlot::Prepared(_) | ClientSlot::Connecting { .. }) => Ok(()),
        None => Err("unknown client handle".into()),
    };
    u8::from(result.is_ok())
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_pollEvents(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<Vec<serde_json::Value>, String> {
        let engine = client_engine_handle(handle)?;
        Ok(engine
            .map(|engine| {
                engine
                    .events()
                    .into_iter()
                    .map(event_json)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default())
    })();
    match result {
        Ok(events) => json_string(&mut env, json!(events)).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_clientStatus(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let engine =
            client_engine_handle(handle)?.ok_or_else(|| "unknown client handle".to_string())?;
        Ok(engine_status_json(&engine))
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

/// Copy the browser engine's current identity-tagged peer snapshot into the
/// connected client engine. Android deliberately owns discovery through a
/// separate long-lived handle, so without this handoff the client's resume
/// worker would know only the original Wi-Fi destination and could not try a
/// newly discovered USB address for the same authenticated host.
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_updateClientPeers(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    peers: JString<'_>,
) -> jboolean {
    let result = (|| -> Result<bool, String> {
        let peers = parse_discovered_peers(&string(&mut env, peers)?)?;
        let engine = {
            let guard = clients()
                .lock()
                .map_err(|_| "client store poisoned".to_string())?;
            match guard.get(&handle) {
                Some(ClientSlot::Connected(client)) => Some(client.handle()),
                // A stale service-owned handle, or a client that has not yet
                // completed its handshake, simply has nothing to update.
                Some(ClientSlot::Prepared(_) | ClientSlot::Connecting { .. }) | None => None,
            }
        };
        if let Some(engine) = engine {
            engine.update_discovered_peer_candidates(peers);
            Ok(true)
        } else {
            Ok(false)
        }
    })();
    u8::from(result.unwrap_or(false))
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_pushCapture(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    samples: JFloatArray<'_>,
    requested: jint,
) -> jint {
    let result = (|| -> Result<jint, String> {
        let Some(length) = requested_pcm_length(&mut env, &samples, requested)? else {
            return Ok(0);
        };
        let Some(engine) = client_engine_handle(handle)? else {
            return Ok(0);
        };
        PCM_SCRATCH.with(|scratch| {
            let mut values = scratch.borrow_mut();
            values.resize(length, 0.0);
            env.get_float_array_region(&samples, 0, &mut values[..])
                .map_err(|error| error.to_string())?;
            if values.iter().any(|value| !value.is_finite()) {
                return Err("PCM contains a non-finite sample".into());
            }
            Ok(if engine.try_push_capture(&values[..]) {
                length as jint
            } else {
                0
            })
        })
    })();
    result.unwrap_or(0)
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_pullPlayback(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    output: JFloatArray<'_>,
) -> jint {
    let result = (|| -> Result<jint, String> {
        let length = env
            .get_array_length(&output)
            .map_err(|error| error.to_string())?;
        let length = (length as usize).min(MAX_REALTIME_QUANTUM_SAMPLES);
        let Some(engine) = client_engine_handle(handle)? else {
            return Ok(0);
        };
        PCM_SCRATCH.with(|scratch| {
            let mut values = scratch.borrow_mut();
            values.resize(length, 0.0);
            let count = engine.try_pull_playback(&mut values[..]);
            env.set_float_array_region(&output, 0, &values[..count])
                .map_err(|error| error.to_string())?;
            Ok(count as jint)
        })
    })();
    result.unwrap_or(0)
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_release(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let slot = clients()
        .lock()
        .ok()
        .and_then(|mut guard| guard.remove(&handle));
    if let Some(ClientSlot::Connected(client)) = slot {
        let _ = client.disconnect();
    }
}

// ---------------------------------------------------------------------------
// Receiver-host support: an Android device can host a Receiver endpoint that
// Emitter peers connect to.
// ---------------------------------------------------------------------------

static HOSTS: OnceLock<Mutex<HashMap<i64, HostSlot>>> = OnceLock::new();

fn hosts() -> &'static Mutex<HashMap<i64, HostSlot>> {
    HOSTS.get_or_init(|| Mutex::new(HashMap::new()))
}

enum HostSlot {
    Prepared(RelayHostPrepared),
    Starting { token: i64 },
    Running(RunningHost),
    Stopping { token: i64 },
}

struct RunningHost {
    host: RelayHost,
    /// Preserve the validated configuration so stop returns this slot to a
    /// restartable prepared state.
    prepared: RelayHostPrepared,
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostCreate(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    device_name: JString<'_>,
    device_id: JString<'_>,
    trusted_peers: JString<'_>,
    pin: JString<'_>,
    port: jint,
    codec: JString<'_>,
    transport: JString<'_>,
    direction: JString<'_>,
    generation: jlong,
    sample_rate: jint,
    channels: jint,
    frame_ms: jint,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let device_name = string(&mut env, device_name)?;
        let device_id = string(&mut env, device_id)?;
        let trusted_peers = parse_trusted_peers(&string(&mut env, trusted_peers)?)?;
        let pin = string(&mut env, pin)?;
        let codec = parse_codec(&string(&mut env, codec)?)?;
        let transport = parse_transport(&string(&mut env, transport)?)?;
        let direction = parse_direction(&string(&mut env, direction)?)?;
        let generation = direction_generation(generation)?;
        let port = port_u16(port)?;
        let sample_rate = positive_u32("sample rate", sample_rate)?;
        let channels = android_channels(channels)?;
        let frame_ms = positive_u16("frame duration", frame_ms)?;
        let mut builder = RelayHostBuilder::new()
            .device_name(device_name)
            .device_kind(DeviceKind::Android)
            .pin(pin)
            .port(port)
            .codec(codec)
            .transport(transport)
            .direction(direction)
            .direction_generation(generation)
            .audio(sample_rate, channels, frame_ms)
            .trusted_peers(trusted_peers);
        if !device_id.trim().is_empty() {
            builder = builder.device_id(device_id);
        }
        let host = builder
            .trust_new_peers(true)
            .build()
            .map_err(|error| error.to_string())?;
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        let mut guard = hosts()
            .lock()
            .map_err(|_| "host store poisoned".to_string())?;
        guard.insert(handle, HostSlot::Prepared(host));
        Ok(json!({"type":"created", "handle":handle}))
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostCreateMode(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    device_name: JString<'_>,
    device_id: JString<'_>,
    trusted_peers: JString<'_>,
    pin: JString<'_>,
    port: jint,
    codec: JString<'_>,
    transport: JString<'_>,
    mode: JString<'_>,
    generation: jlong,
    sample_rate: jint,
    channels: jint,
    frame_ms: jint,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let device_name = string(&mut env, device_name)?;
        let device_id = string(&mut env, device_id)?;
        let trusted_peers = parse_trusted_peers(&string(&mut env, trusted_peers)?)?;
        let pin = string(&mut env, pin)?;
        let port = port_u16(port)?;
        let codec = parse_codec(&string(&mut env, codec)?)?;
        let transport = parse_transport(&string(&mut env, transport)?)?;
        let mode = parse_mode(&string(&mut env, mode)?)?;
        if mode != RelayMode::Receiver {
            return Err("Android hosts are Receiver endpoints; use a client for Emitter".into());
        }
        let generation = direction_generation(generation)?;
        let sample_rate = positive_u32("sample rate", sample_rate)?;
        let channels = android_channels(channels)?;
        let frame_ms = positive_u16("frame duration", frame_ms)?;
        let mut builder = RelayHostBuilder::new()
            .device_name(device_name)
            .device_kind(DeviceKind::Android)
            .pin(pin)
            .port(port)
            .codec(codec)
            .transport(transport)
            .mode(mode)
            .direction_generation(generation)
            .audio(sample_rate, channels, frame_ms)
            .trusted_peers(trusted_peers);
        if !device_id.trim().is_empty() {
            builder = builder.device_id(device_id);
        }
        let host = builder
            .trust_new_peers(true)
            .build()
            .map_err(|error| error.to_string())?;
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        let mut guard = hosts()
            .lock()
            .map_err(|_| "host store poisoned".to_string())?;
        guard.insert(handle, HostSlot::Prepared(host));
        Ok(json!({"type":"created", "handle":handle}))
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostStart(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let (token, prepared) = {
            let mut guard = hosts()
                .lock()
                .map_err(|_| "host store poisoned".to_string())?;
            let prepared = match guard.get(&handle) {
                Some(HostSlot::Prepared(prepared)) => prepared.clone(),
                Some(HostSlot::Starting { .. } | HostSlot::Stopping { .. }) => {
                    return Err("host state transition is already in progress".into())
                }
                Some(HostSlot::Running(_)) => return Err("host is already running".into()),
                None => return Err("unknown host handle".into()),
            };
            let token = next_operation();
            guard.insert(handle, HostSlot::Starting { token });
            (token, prepared)
        };

        // Binding and starting the host's accept thread do not run under the
        // process-wide host registry mutex.
        match prepared.clone().start() {
            Ok(host) => {
                let port = host.port();
                let host_handle = host.handle();
                let address = host_display_address(&host_handle, &host.status());
                let mut guard = hosts()
                    .lock()
                    .map_err(|_| "host store poisoned".to_string())?;
                let same_attempt = matches!(
                    guard.get(&handle),
                    Some(HostSlot::Starting { token: current, .. }) if *current == token
                );
                if same_attempt {
                    guard.insert(handle, HostSlot::Running(RunningHost { host, prepared }));
                    Ok(json!({"type": "host_started", "port": port, "address": address}))
                } else {
                    drop(guard);
                    let _ = host.handle().host_stop();
                    Err("host handle changed while starting".into())
                }
            }
            Err(error) => {
                let mut guard = hosts()
                    .lock()
                    .map_err(|_| "host store poisoned".to_string())?;
                if matches!(
                    guard.get(&handle),
                    Some(HostSlot::Starting { token: current, .. }) if *current == token
                ) {
                    guard.insert(handle, HostSlot::Prepared(prepared));
                }
                Err(error.to_string())
            }
        }
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostStop(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let (token, prepared, host) = {
            let mut guard = hosts()
                .lock()
                .map_err(|_| "host store poisoned".to_string())?;
            match guard.remove(&handle) {
                Some(HostSlot::Running(running)) => {
                    let token = next_operation();
                    let prepared = running.prepared.clone();
                    guard.insert(handle, HostSlot::Stopping { token });
                    (token, prepared, running.host)
                }
                Some(HostSlot::Prepared(prepared)) => {
                    guard.insert(handle, HostSlot::Prepared(prepared));
                    return Ok(json!({"type": "host_stopped"}));
                }
                Some(other @ (HostSlot::Starting { .. } | HostSlot::Stopping { .. })) => {
                    guard.insert(handle, other);
                    return Err("host state transition is already in progress".into());
                }
                None => return Err("unknown host handle".into()),
            }
        };

        // Stop the engine outside the global registry lock. The prepared
        // configuration remains available for a later start.
        let stop_result = host.handle().host_stop().map_err(|error| error.to_string());
        let mut guard = hosts()
            .lock()
            .map_err(|_| "host store poisoned".to_string())?;
        let same_attempt = matches!(
            guard.get(&handle),
            Some(HostSlot::Stopping { token: current, .. }) if *current == token
        );
        if !same_attempt {
            drop(guard);
            return stop_result.map(|()| json!({"type": "host_stopped"}));
        }
        match stop_result {
            Ok(()) => {
                guard.insert(handle, HostSlot::Prepared(prepared));
                Ok(json!({"type": "host_stopped"}))
            }
            Err(error) => {
                guard.insert(handle, HostSlot::Running(RunningHost { host, prepared }));
                Err(error)
            }
        }
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostPollEvents(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<Vec<serde_json::Value>, String> {
        let engine = host_engine_handle(handle)?;
        Ok(engine
            .map(|engine| {
                engine
                    .events()
                    .into_iter()
                    .map(event_json)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default())
    })();
    match result {
        Ok(events) => json_string(&mut env, json!(events)).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostTrustedEnrollmentSecret(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    transaction_id: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let engine =
            host_engine_handle(handle)?.ok_or_else(|| "host is not running".to_string())?;
        let transaction_id = u64::try_from(transaction_id)
            .map_err(|_| "trusted enrollment transaction is invalid".to_string())?;
        let Some(secret) = engine.trusted_enrollment_secret(transaction_id) else {
            return Ok(json!({"type": "none"}));
        };
        Ok(json!({
            "type": "trusted_enrollment_secret",
            "secret": pw_graph_utils::hex::hex_encode(&secret),
        }))
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostAcceptTrustedEnrollment(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    transaction_id: jlong,
) -> jboolean {
    let result = (|| -> Result<bool, String> {
        let engine =
            host_engine_handle(handle)?.ok_or_else(|| "host is not running".to_string())?;
        let transaction_id = u64::try_from(transaction_id)
            .map_err(|_| "trusted enrollment transaction is invalid".to_string())?;
        engine
            .accept_trusted_enrollment(transaction_id)
            .map_err(|error| error.to_string())?;
        Ok(true)
    })();
    u8::from(result.unwrap_or(false))
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostRejectTrustedEnrollment(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    transaction_id: jlong,
    reason: JString<'_>,
) -> jboolean {
    let result = (|| -> Result<bool, String> {
        let engine =
            host_engine_handle(handle)?.ok_or_else(|| "host is not running".to_string())?;
        let transaction_id = u64::try_from(transaction_id)
            .map_err(|_| "trusted enrollment transaction is invalid".to_string())?;
        engine
            .reject_trusted_enrollment(transaction_id, string(&mut env, reason)?)
            .map_err(|error| error.to_string())?;
        Ok(true)
    })();
    u8::from(result.unwrap_or(false))
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostRemoveTrustedPeer(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    peer_id: JString<'_>,
) -> jboolean {
    let result = (|| -> Result<bool, String> {
        let peer_id = string(&mut env, peer_id)?;
        let Some(engine) = host_engine_handle(handle)? else {
            return Ok(false);
        };
        engine
            .remove_trusted_peer(&peer_id)
            .map_err(|error| error.to_string())?;
        Ok(true)
    })();
    u8::from(result.unwrap_or(false))
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostStatus(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let engine = host_engine_handle(handle)?;
        let Some(engine) = engine else {
            return Ok(json!({
                "type": "status",
                "host_active": false,
                "port": null,
                "address": null,
                "sessions": [],
            }));
        };
        let mut value = engine_status_json(&engine);
        value["type"] = json!("status");
        Ok(value)
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostReportError(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    message: JString<'_>,
) -> jboolean {
    let result = (|| -> Result<bool, String> {
        let message = string(&mut env, message)?;
        let Some(engine) = host_engine_handle(handle)? else {
            return Ok(false);
        };
        engine.report_error(message);
        Ok(true)
    })();
    u8::from(result.unwrap_or(false))
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostOfferDirection(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    direction: JString<'_>,
    generation: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let engine =
            host_engine_handle(handle)?.ok_or_else(|| "host is not running".to_string())?;
        let session = u64::try_from(session).map_err(|_| "session id is invalid".to_string())?;
        let direction = parse_direction(&string(&mut env, direction)?)?;
        let generation = direction_generation(generation)?;
        engine
            .offer_direction(SessionId(session), direction, generation)
            .map(|()| {
                json!({
                    "type": "direction_offered",
                    "session": session,
                    "direction": direction.as_str(),
                    "generation": generation,
                })
            })
            .map_err(|error| error.to_string())
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostOfferMode(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    mode: JString<'_>,
    generation: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let engine =
            host_engine_handle(handle)?.ok_or_else(|| "host is not running".to_string())?;
        let session = u64::try_from(session).map_err(|_| "session id is invalid".to_string())?;
        let mode = parse_mode(&string(&mut env, mode)?)?;
        let generation = direction_generation(generation)?;
        engine
            .offer_mode(SessionId(session), mode, generation)
            .map(|()| {
                json!({
                    "type": "mode_offered",
                    "session": session,
                    "mode": mode.as_str(),
                    "generation": generation,
                })
            })
            .map_err(|error| error.to_string())
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostDisconnectSession(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let engine =
            host_engine_handle(handle)?.ok_or_else(|| "host is not running".to_string())?;
        if session < 0 {
            return Err("session id must not be negative".into());
        }
        engine
            .disconnect(SessionId(session as u64))
            .map(|()| json!({"type": "disconnecting", "session": session}))
            .map_err(|error| error.to_string())
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostPushCapture(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    samples: JFloatArray<'_>,
    requested: jint,
) -> jint {
    let result = (|| -> Result<jint, String> {
        let Some(length) = requested_pcm_length(&mut env, &samples, requested)? else {
            return Ok(0);
        };
        let Some(engine) = host_engine_handle(handle)? else {
            return Ok(0);
        };
        PCM_SCRATCH.with(|scratch| {
            let mut values = scratch.borrow_mut();
            values.resize(length, 0.0);
            env.get_float_array_region(&samples, 0, &mut values[..])
                .map_err(|error| error.to_string())?;
            if values.iter().any(|value| !value.is_finite()) {
                return Err("PCM contains a non-finite sample".into());
            }
            Ok(if engine.try_push_capture(&values[..]) {
                length as jint
            } else {
                0
            })
        })
    })();
    result.unwrap_or(0)
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostPullPlayback(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    output: JFloatArray<'_>,
) -> jint {
    let result = (|| -> Result<jint, String> {
        let length = env
            .get_array_length(&output)
            .map_err(|error| error.to_string())?;
        let length = (length as usize).min(MAX_REALTIME_QUANTUM_SAMPLES);
        let Some(engine) = host_engine_handle(handle)? else {
            return Ok(0);
        };
        PCM_SCRATCH.with(|scratch| {
            let mut values = scratch.borrow_mut();
            values.resize(length, 0.0);
            let count = engine.try_pull_playback(&mut values[..]);
            env.set_float_array_region(&output, 0, &values[..count])
                .map_err(|error| error.to_string())?;
            Ok(count as jint)
        })
    })();
    result.unwrap_or(0)
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostRelease(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    // Release is deliberately not a second stop operation. A caller must
    // first complete hostStop, which returns a Running slot to Prepared. If
    // a stale caller reaches this function while workers still own a running
    // host, retain the slot so it cannot lose the only handle to live state.
    let Some(slot) = hosts()
        .lock()
        .ok()
        .and_then(|mut guard| guard.remove(&handle))
    else {
        return 0;
    };
    match slot {
        HostSlot::Prepared(_) => 1,
        running @ HostSlot::Running(_) => {
            if let Ok(mut guard) = hosts().lock() {
                guard.insert(handle, running);
            }
            0
        }
        transitioning @ (HostSlot::Starting { .. } | HostSlot::Stopping { .. }) => {
            if let Ok(mut guard) = hosts().lock() {
                guard.insert(handle, transitioning);
            }
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery support: browse the LAN for relay hosts from Android.
// ---------------------------------------------------------------------------

static BROWSERS: OnceLock<Mutex<HashMap<i64, RelayBrowser>>> = OnceLock::new();

fn browsers() -> &'static Mutex<HashMap<i64, RelayBrowser>> {
    BROWSERS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_discoveryCreate(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    device_name: JString<'_>,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let device_name = string(&mut env, device_name)?;
        let browser = RelayBrowser::start(device_name).map_err(|error| error.to_string())?;
        let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
        let mut guard = browsers()
            .lock()
            .map_err(|_| "browser store poisoned".to_string())?;
        guard.insert(handle, browser);
        Ok(json!({"type":"created", "handle":handle}))
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_discoveryStart(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let engine = {
            let guard = browsers()
                .lock()
                .map_err(|_| "browser store poisoned".to_string())?;
            guard
                .get(&handle)
                .ok_or_else(|| "unknown discovery handle".to_string())?
                .handle()
        };
        engine
            .discovery_start()
            .map(|()| json!({"type": "discovery_started"}))
            .map_err(|error| error.to_string())
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_discoveryStop(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let engine = {
            let guard = browsers()
                .lock()
                .map_err(|_| "browser store poisoned".to_string())?;
            guard
                .get(&handle)
                .ok_or_else(|| "unknown discovery handle".to_string())?
                .handle()
        };
        engine.discovery_stop();
        Ok(json!({"type": "discovery_stopped"}))
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_discoveryUsbLinkLost(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    let result = browsers()
        .lock()
        .ok()
        .and_then(|guard| guard.get(&handle).map(|browser| browser.handle()))
        .map(|engine| {
            engine.discovery_usb_link_lost();
            true
        })
        .unwrap_or(false);
    u8::from(result)
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_discoveryPeers(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let engine = {
            let guard = browsers()
                .lock()
                .map_err(|_| "browser store poisoned".to_string())?;
            guard
                .get(&handle)
                .ok_or_else(|| "unknown discovery handle".to_string())?
                .handle()
        };
        let peers = engine
            .discovered_peer_candidates()
            .into_iter()
            .map(|(peer, link)| {
                json!({
                    "id": peer.id,
                    "name": peer.name,
                    "address": peer.addr.to_string(),
                    "link": link.map(LinkKind::as_str),
                })
            })
            .collect::<Vec<_>>();
        Ok(json!(peers))
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_discoveryPollEvents(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = (|| -> Result<serde_json::Value, String> {
        let engine = {
            let guard = browsers()
                .lock()
                .map_err(|_| "browser store poisoned".to_string())?;
            guard
                .get(&handle)
                .ok_or_else(|| "unknown discovery handle".to_string())?
                .handle()
        };
        let events = engine
            .events()
            .into_iter()
            .map(event_json)
            .collect::<Vec<_>>();
        Ok(json!(events))
    })();
    match result {
        Ok(value) => json_string(&mut env, value).unwrap_or(std::ptr::null_mut()),
        Err(error) => error_json(&mut env, error).unwrap_or(std::ptr::null_mut()),
    }
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_discoveryRelease(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let browser = browsers()
        .lock()
        .ok()
        .and_then(|mut guard| guard.remove(&handle));
    if let Some(browser) = browser {
        browser.shutdown();
    }
}

// ---------------------------------------------------------------------------
// Link detection: report an active USB tether so the UI can show it without
// exposing USB as a manual transport choice.
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_usbLink(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jni::sys::jstring {
    json_string(&mut env, usb_link_json()).unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_localLinks(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jni::sys::jstring {
    json_string(&mut env, local_links_json()).unwrap_or(std::ptr::null_mut())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_android_client_options() {
        assert_eq!(
            parse_direction("emit").unwrap(),
            RelayDirection::MobileToDesktop
        );
        assert_eq!(
            parse_direction("receive").unwrap(),
            RelayDirection::DesktopToMobile
        );
        assert!(parse_direction("both").is_err());
        assert_eq!(parse_codec("pcm").unwrap(), CodecKind::Pcm);
        assert_eq!(parse_codec("opus").unwrap(), CodecKind::Opus);
        assert_eq!(parse_transport("wifi").unwrap(), TransportPreference::Wifi);
    }

    #[test]
    fn invalid_android_enum_options_are_errors_instead_of_defaults() {
        assert!(parse_direction("not-a-role").is_err());
        assert!(parse_codec("not-a-codec").is_err());
        assert!(parse_transport("not-a-transport").is_err());
    }

    #[test]
    fn lifecycle_error_codes_are_stable_and_not_message_parsed_by_clients() {
        assert_eq!(
            native_error_code("unknown client handle"),
            "unknown_client_handle"
        );
        assert_eq!(
            native_error_code("unknown host handle"),
            "unknown_host_handle"
        );
        assert_eq!(native_error_code("something else"), "internal_error");
    }

    #[test]
    fn embedded_discovery_snapshot_has_a_hard_size_and_count_bound() {
        let peers = (0..=MAX_DISCOVERED_PEER_ADDRESSES)
            .map(|index| {
                json!({
                    "id": format!("peer-{index}"),
                    "name": "relay",
                    "address": "192.168.1.20:48123"
                })
            })
            .collect::<Vec<_>>();
        let snapshot = serde_json::to_string(&peers).unwrap();
        assert!(parse_discovered_peers(&snapshot).is_err());
        assert!(parse_discovered_peers(&"x".repeat(2 * 1024 * 1024 + 1)).is_err());
    }

    #[test]
    fn jint_audio_values_are_checked_before_narrowing() {
        for value in [0, -1, -65536] {
            assert!(positive_u16("channels", value).is_err());
            assert!(positive_u32("sample rate", value).is_err());
        }
        for value in [65_536, 65_538] {
            assert!(positive_u16("channels", value).is_err());
        }
        assert_eq!(positive_u16("channels", 1).unwrap(), 1);
        assert_eq!(positive_u16("channels", 2).unwrap(), 2);
        assert_eq!(positive_u32("sample rate", 48_000).unwrap(), 48_000);
        assert_eq!(port_u16(0).unwrap(), 0);
        assert!(port_u16(-1).is_err());
        assert!(port_u16(65_536).is_err());
    }

    #[test]
    fn android_audio_accepts_the_negotiable_channel_set() {
        assert_eq!(android_channels(1).unwrap(), 1);
        assert_eq!(android_channels(2).unwrap(), 2);
        assert!(android_channels(0).is_err());
        assert!(android_channels(3).is_err());
    }

    #[test]
    fn connected_response_contains_the_consumed_session_metadata() {
        let value = connected_json(SessionId(42), "studio-pc");
        assert_eq!(
            value.get("type").and_then(|value| value.as_str()),
            Some("connected")
        );
        assert_eq!(
            value.get("session").and_then(|value| value.as_u64()),
            Some(42)
        );
        assert_eq!(
            value.get("host").and_then(|value| value.as_str()),
            Some("studio-pc")
        );
    }

    #[test]
    fn checked_frame_values_reach_builder_validation_unchanged() {
        for frame_ms in [5, 10, 20, 40, 60] {
            let frame_ms = positive_u16("frame duration", frame_ms).unwrap();
            assert!(RelayClientBuilder::new()
                .audio(48_000, 1, frame_ms)
                .build()
                .is_ok());
        }
        for frame_ms in [1, 7, 61] {
            let frame_ms = positive_u16("frame duration", frame_ms).unwrap();
            assert!(RelayClientBuilder::new()
                .audio(48_000, 1, frame_ms)
                .build()
                .is_err());
        }
    }

    #[test]
    fn pcm_scratch_capacity_is_reused_after_the_first_quantum() {
        PCM_SCRATCH.with(|scratch| {
            let mut scratch = scratch.borrow_mut();
            scratch.clear();
            scratch.resize(MAX_REALTIME_QUANTUM_SAMPLES, 0.0);
            let capacity = scratch.capacity();
            for _ in 0..16 {
                scratch.resize(MAX_REALTIME_QUANTUM_SAMPLES, 0.0);
            }
            assert_eq!(scratch.capacity(), capacity);
        });
    }

    #[test]
    fn usb_link_json_is_well_formed_without_a_tether() {
        // A desktop test box normally has no USB tether up; whatever the
        // result, it must be a JSON object with a `type` field.
        let value = usb_link_json();
        let kind = value.get("type").and_then(|field| field.as_str());
        assert!(matches!(kind, Some("usb_link") | Some("none")));
        if kind == Some("usb_link") {
            assert!(value.get("name").is_some());
            assert!(value.get("addr").is_some());
        }
    }

    #[test]
    fn local_links_json_lists_every_link_with_kind() {
        let value = local_links_json();
        assert_eq!(
            value.get("type").and_then(|field| field.as_str()),
            Some("links")
        );
        let links = value
            .get("links")
            .and_then(|field| field.as_array())
            .unwrap();
        for link in links {
            assert!(matches!(
                link.get("kind").and_then(|field| field.as_str()),
                Some("usb") | Some("wifi") | Some("bluetooth") | Some("lan")
            ));
            assert!(link.get("name").is_some());
            assert!(link.get("addr").is_some());
        }
    }
}
