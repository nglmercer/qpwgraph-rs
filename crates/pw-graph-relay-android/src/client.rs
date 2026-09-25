use super::convert::{
    android_channels, catch_native, connected_json, direction_generation, engine_status_json,
    event_json, json_response, next_handle, next_operation, parse_codec, parse_direction,
    parse_discovered_peers, parse_mode, parse_transport, parse_trusted_peers, positive_u16,
    positive_u32, requested_pcm_length, string, trusted_secret,
};
use super::pcm_scratch::PCM_SCRATCH;
use jni::objects::{JClass, JFloatArray, JString};
use jni::sys::{jboolean, jint, jlong};
use jni::JNIEnv;
use pw_graph_relay_sdk::{
    DeviceKind, RelayClient, RelayClientBuilder, RelayHandle, SessionId,
    MAX_REALTIME_QUANTUM_SAMPLES,
};
use serde_json::json;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Mutex, OnceLock};

static CLIENTS: OnceLock<Mutex<HashMap<i64, ClientSlot>>> = OnceLock::new();
fn clients() -> &'static Mutex<HashMap<i64, ClientSlot>> {
    CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
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
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
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
            let handle = next_handle();
            let mut guard = clients()
                .lock()
                .map_err(|_| "client store poisoned".to_string())?;
            guard.insert(handle, ClientSlot::Prepared(client));
            Ok(json!({"type":"created", "handle":handle}))
        })()
    });
    json_response(&mut env, result)
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
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
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
            let handle = next_handle();
            let mut guard = clients()
                .lock()
                .map_err(|_| "client store poisoned".to_string())?;
            guard.insert(handle, ClientSlot::Prepared(client));
            Ok(json!({"type":"created", "handle":handle}))
        })()
    });
    json_response(&mut env, result)
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
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
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
                    Some(ClientSlot::Connected(_)) => {
                        return Err("client is already connected".into())
                    }
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
        })()
    });
    json_response(&mut env, result)
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
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
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
                    Some(ClientSlot::Connected(_)) => {
                        return Err("client is already connected".into())
                    }
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
        })()
    });
    json_response(&mut env, result)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_clientTrustedPeer(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
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
        })()
    });
    json_response(&mut env, result)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_removeTrustedPeer(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    peer_id: JString<'_>,
) -> jboolean {
    let result = catch_native(|| {
        (|| -> Result<bool, String> {
            let peer_id = string(&mut env, peer_id)?;
            let Some(engine) = client_engine_handle(handle)? else {
                return Ok(false);
            };
            engine
                .remove_trusted_peer(&peer_id)
                .map_err(|error| error.to_string())?;
            Ok(true)
        })()
    });
    u8::from(result.unwrap_or(false))
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_reportError(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    message: JString<'_>,
) -> jboolean {
    let result = catch_native(|| {
        (|| -> Result<bool, String> {
            let message = string(&mut env, message)?;
            let Some(engine) = client_engine_handle(handle)? else {
                return Ok(false);
            };
            engine.report_error(message);
            Ok(true)
        })()
    });
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
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
            let engine = client_engine_handle(handle)?
                .ok_or_else(|| "client is not connected".to_string())?;
            let session =
                u64::try_from(session).map_err(|_| "session id is invalid".to_string())?;
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
        })()
    });
    json_response(&mut env, result)
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
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
            let engine = client_engine_handle(handle)?
                .ok_or_else(|| "client is not connected".to_string())?;
            let session =
                u64::try_from(session).map_err(|_| "session id is invalid".to_string())?;
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
        })()
    });
    json_response(&mut env, result)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_disconnect(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    let result = catch_native(|| {
        let slot = clients()
            .lock()
            .map_err(|_| "client store poisoned".to_string())?
            .remove(&handle);
        match slot {
            Some(ClientSlot::Connected(client)) => {
                client.disconnect().map_err(|error| error.to_string())
            }
            Some(ClientSlot::Prepared(_) | ClientSlot::Connecting { .. }) => Ok(()),
            None => Err("unknown client handle".into()),
        }
    });
    u8::from(result.is_ok())
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_pollEvents(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = catch_native(|| {
        (|| -> Result<Vec<serde_json::Value>, String> {
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
        })()
    });
    json_response(&mut env, result.map(serde_json::Value::Array))
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_clientStatus(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
            let engine =
                client_engine_handle(handle)?.ok_or_else(|| "unknown client handle".to_string())?;
            Ok(engine_status_json(&engine))
        })()
    });
    json_response(&mut env, result)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_updateClientPeers(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    peers: JString<'_>,
) -> jboolean {
    let result = catch_native(|| {
        (|| -> Result<bool, String> {
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
        })()
    });
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
    let result = catch_native(|| {
        (|| -> Result<jint, String> {
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
        })()
    });
    result.unwrap_or(0)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_pullPlayback(
    env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    output: JFloatArray<'_>,
) -> jint {
    let result = catch_native(|| {
        (|| -> Result<jint, String> {
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
        })()
    });
    result.unwrap_or(0)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_release(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let slot = clients()
            .lock()
            .ok()
            .and_then(|mut guard| guard.remove(&handle));
        if let Some(ClientSlot::Connected(client)) = slot {
            let _ = client.disconnect();
        }
    }));
}
