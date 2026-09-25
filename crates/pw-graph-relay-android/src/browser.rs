use super::convert::{
    catch_native, event_json, json_response, local_links_json, next_handle, string, usb_link_json,
};
use jni::objects::{JClass, JString};
use jni::sys::{jboolean, jlong};
use jni::JNIEnv;
use pw_graph_relay_sdk::{LinkKind, RelayBrowser};
use serde_json::json;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Mutex, OnceLock};

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
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
            let device_name = string(&mut env, device_name)?;
            let browser = RelayBrowser::start(device_name).map_err(|error| error.to_string())?;
            let handle = next_handle();
            let mut guard = browsers()
                .lock()
                .map_err(|_| "browser store poisoned".to_string())?;
            guard.insert(handle, browser);
            Ok(json!({"type":"created", "handle":handle}))
        })()
    });
    json_response(&mut env, result)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_discoveryStart(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
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
        })()
    });
    json_response(&mut env, result)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_discoveryStop(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
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
        })()
    });
    json_response(&mut env, result)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_discoveryUsbLinkLost(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jboolean {
    let result = catch_unwind(AssertUnwindSafe(|| {
        browsers()
            .lock()
            .ok()
            .and_then(|guard| guard.get(&handle).map(|browser| browser.handle()))
            .map(|engine| {
                engine.discovery_usb_link_lost();
                true
            })
            .unwrap_or(false)
    }))
    .unwrap_or(false);
    u8::from(result)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_discoveryPeers(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
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
        })()
    });
    json_response(&mut env, result)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_discoveryPollEvents(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) -> jni::sys::jstring {
    let result = catch_native(|| {
        (|| -> Result<serde_json::Value, String> {
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
        })()
    });
    json_response(&mut env, result)
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_discoveryRelease(
    _env: JNIEnv<'_>,
    _class: JClass<'_>,
    handle: jlong,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let browser = browsers()
            .lock()
            .ok()
            .and_then(|mut guard| guard.remove(&handle));
        if let Some(browser) = browser {
            browser.shutdown();
        }
    }));
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_usbLink(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jni::sys::jstring {
    json_response(&mut env, catch_native(|| Ok(usb_link_json())))
}
#[no_mangle]
pub extern "system" fn Java_io_qpwgraph_relay_NativeBridge_localLinks(
    mut env: JNIEnv<'_>,
    _class: JClass<'_>,
) -> jni::sys::jstring {
    json_response(&mut env, catch_native(|| Ok(local_links_json())))
}
