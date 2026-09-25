use super::convert::{
    android_channels, catch_native, direction_generation, engine_status_json, event_json,
    host_display_address, json_response, next_handle, next_operation, parse_codec, parse_direction,
    parse_mode, parse_transport, parse_trusted_peers, port_u16, positive_u16, positive_u32,
    requested_pcm_length, string,
};
use super::pcm_scratch::PCM_SCRATCH;
use jni::objects::{JClass, JFloatArray, JString};
use jni::sys::{jboolean, jint, jlong};
use jni::JNIEnv;
use pw_graph_relay_sdk::{
    DeviceKind, RelayHandle, RelayHost, RelayHostBuilder, RelayHostPrepared, RelayMode, SessionId,
    MAX_REALTIME_QUANTUM_SAMPLES,
};
use serde_json::json;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Mutex, OnceLock};

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
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
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
            let handle = next_handle();
            let mut guard = hosts()
                .lock()
                .map_err(|_| "host store poisoned".to_string())?;
            guard.insert(handle, HostSlot::Prepared(host));
            Ok(json!({"type":"created", "handle":handle}))
        })()
    });
    json_response(&mut env, result)
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
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
            let device_name = string(&mut env, device_name)?;
            let device_id = string(&mut env, device_id)?;
            let trusted_peers = parse_trusted_peers(&string(&mut env, trusted_peers)?)?;
            let pin = string(&mut env, pin)?;
            let port = port_u16(port)?;
            let codec = parse_codec(&string(&mut env, codec)?)?;
            let transport = parse_transport(&string(&mut env, transport)?)?;
            let mode = parse_mode(&string(&mut env, mode)?)?;
            if mode != RelayMode::Receiver {
                return Err(
                    "Android hosts are Receiver endpoints; use a client for Emitter".into(),
                );
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
            let handle = next_handle();
            let mut guard = hosts()
                .lock()
                .map_err(|_| "host store poisoned".to_string())?;
            guard.insert(handle, HostSlot::Prepared(host));
            Ok(json!({"type":"created", "handle":handle}))
        })()
    });
    json_response(&mut env, result)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostStart(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
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
        })()
    });
    json_response(&mut env, result)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostStop(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
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
        })()
    });
    json_response(&mut env, result)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostPollEvents(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = catch_native(|| {
        (|| -> Result<Vec<serde_json::Value>, String> {
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
        })()
    });
    json_response(&mut env, result.map(serde_json::Value::Array))
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostTrustedEnrollmentSecret(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    transaction_id: jlong,
) -> jni::sys::jstring {
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
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
        })()
    });
    json_response(&mut env, result)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostAcceptTrustedEnrollment(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    transaction_id: jlong,
) -> jboolean {
    let result = catch_native(|| {
        (|| -> Result<bool, String> {
            let engine =
                host_engine_handle(handle)?.ok_or_else(|| "host is not running".to_string())?;
            let transaction_id = u64::try_from(transaction_id)
                .map_err(|_| "trusted enrollment transaction is invalid".to_string())?;
            engine
                .accept_trusted_enrollment(transaction_id)
                .map_err(|error| error.to_string())?;
            Ok(true)
        })()
    });
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
    let result = catch_native(|| {
        (|| -> Result<bool, String> {
            let engine =
                host_engine_handle(handle)?.ok_or_else(|| "host is not running".to_string())?;
            let transaction_id = u64::try_from(transaction_id)
                .map_err(|_| "trusted enrollment transaction is invalid".to_string())?;
            engine
                .reject_trusted_enrollment(transaction_id, string(&mut env, reason)?)
                .map_err(|error| error.to_string())?;
            Ok(true)
        })()
    });
    u8::from(result.unwrap_or(false))
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostRemoveTrustedPeer(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    peer_id: JString<'_>,
) -> jboolean {
    let result = catch_native(|| {
        (|| -> Result<bool, String> {
            let peer_id = string(&mut env, peer_id)?;
            let Some(engine) = host_engine_handle(handle)? else {
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
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostStatus(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
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
        })()
    });
    json_response(&mut env, result)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostReportError(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    message: JString<'_>,
) -> jboolean {
    let result = catch_native(|| {
        (|| -> Result<bool, String> {
            let message = string(&mut env, message)?;
            let Some(engine) = host_engine_handle(handle)? else {
                return Ok(false);
            };
            engine.report_error(message);
            Ok(true)
        })()
    });
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
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
            let engine =
                host_engine_handle(handle)?.ok_or_else(|| "host is not running".to_string())?;
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
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostOfferMode(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
    mode: JString<'_>,
    generation: jlong,
) -> jni::sys::jstring {
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
            let engine =
                host_engine_handle(handle)?.ok_or_else(|| "host is not running".to_string())?;
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
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostDisconnectSession(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
    session: jlong,
) -> jni::sys::jstring {
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
            let engine =
                host_engine_handle(handle)?.ok_or_else(|| "host is not running".to_string())?;
            if session < 0 {
                return Err("session id must not be negative".into());
            }
            engine
                .disconnect(SessionId(session as u64))
                .map(|()| json!({"type": "disconnecting", "session": session}))
                .map_err(|error| error.to_string())
        })()
    });
    json_response(&mut env, result)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostPushCapture(
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
        })()
    });
    result.unwrap_or(0)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_hostPullPlayback(
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
        })()
    });
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
    let result = catch_unwind(AssertUnwindSafe(|| {
        let Some(slot) = hosts()
            .lock()
            .ok()
            .and_then(|mut guard| guard.remove(&handle))
        else {
            return false;
        };
        match slot {
            HostSlot::Prepared(_) => true,
            running @ HostSlot::Running(_) => {
                if let Ok(mut guard) = hosts().lock() {
                    guard.insert(handle, running);
                }
                false
            }
            transitioning @ (HostSlot::Starting { .. } | HostSlot::Stopping { .. }) => {
                if let Ok(mut guard) = hosts().lock() {
                    guard.insert(handle, transitioning);
                }
                false
            }
        }
    }))
    .unwrap_or(false);
    u8::from(result)
}
