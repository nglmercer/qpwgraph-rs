//! Turning Core Audio's names and GUIDs into stable graph identities.
//!
//! Windows has no persistent numeric ids for endpoints or sessions, so every
//! graph id here is derived from a stable string. The same endpoint has to
//! keep its id across a rebuild or the UI would lose selection and layout on
//! every refresh.

use super::*;

/// `PKEY_AudioEndpoint_StableId` was added to the Windows 11 24H2 SDK after
/// the `windows` crate version currently used by this project. The property is
/// the next value in the documented `PKEY_AudioEndpoint_*` key family; keep
/// the raw key isolated here until generated bindings expose it.
const PKEY_AUDIO_ENDPOINT_STABLE_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x1da5_d803_d492_4edd_8c23_e0c0_ffee_7f0e),
    pid: 12,
};

/// Project-owned endpoint role property.  The ACX endpoint provider must
/// publish this value; it is the semantic part of the ownership proof and is
/// deliberately not inferred from a user-editable friendly name.  Custom
/// device-property identifiers start at 2; PID 1 is reserved by the platform.
const PKEY_QPWGRAPH_ENDPOINT_ROLE: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x3c8e_8ef9_1f7f_4fcb_9c36_4a7e_19f3_6d12),
    pid: 2,
};

const QPWGRAPH_DRIVER_SERVICE: &str = "qpwgraph_audio";

/// The durable selector used when Windows exposes a stable endpoint id.
/// `current_mmdevice_id` and `friendly_name` are fallbacks and diagnostics,
/// never a replacement for a matching stable id.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowsEndpointSelector {
    pub stable_id: Option<String>,
    pub current_mmdevice_id: Option<String>,
    pub friendly_name: Option<String>,
    pub data_flow: AudioFlow,
}

impl WindowsEndpointSelector {
    pub fn from_device(device: &Audio::IMMDevice, data_flow: AudioFlow) -> Option<Self> {
        let current_mmdevice_id = unsafe { device.GetId() }.ok().map(take_pwstr);
        Some(Self {
            stable_id: endpoint_stable_id(device),
            current_mmdevice_id,
            friendly_name: endpoint_name(device),
            data_flow,
        })
    }

    /// Resolve in durability order. Friendly-name fallback is accepted only
    /// when exactly one active endpoint has the requested name and flow.
    pub fn resolve(
        &self,
        enumerator: &Audio::IMMDeviceEnumerator,
    ) -> BackendResult<Option<Audio::IMMDevice>> {
        let flow = match self.data_flow {
            AudioFlow::Render => Audio::eRender,
            AudioFlow::Capture => Audio::eCapture,
        };
        // Windows documents this value as opaque and case-sensitive. Do not
        // normalize it before falling back to the less durable selectors.
        if let Some(stable_id) = self.stable_id.as_deref() {
            let collection =
                unsafe { enumerator.EnumAudioEndpoints(flow, Audio::DEVICE_STATE_ACTIVE) }
                    .map_err(|error| native_error("enumerate stable endpoint selector", error))?;
            let count = unsafe { collection.GetCount() }
                .map_err(|error| native_error("read stable endpoint selector count", error))?;
            let mut matched = None;
            for index in 0..count {
                let Ok(device) = (unsafe { collection.Item(index) }) else {
                    continue;
                };
                if endpoint_stable_id(&device)
                    .as_deref()
                    .is_some_and(|candidate| candidate == stable_id)
                {
                    if matched.is_some() {
                        return Err(BackendError::unsupported(
                            "stable endpoint selector matched multiple active endpoints",
                        ));
                    }
                    matched = Some(device);
                }
            }
            if matched.is_some() {
                return Ok(matched);
            }
        }
        if let Some(mmdevice_id) = self.current_mmdevice_id.as_deref() {
            // `IMMDeviceEnumerator::GetDevice` does not carry the selector's
            // data-flow contract. Resolve through the active collection so a
            // stale/corrupt capture ID can never be returned for a render
            // selector (or vice versa).
            if let Some(device) = active_device_by_id(enumerator, flow, mmdevice_id)? {
                return Ok(Some(device));
            }
        }
        let Some(name) = self.friendly_name.as_deref() else {
            return Ok(None);
        };
        let collection = unsafe { enumerator.EnumAudioEndpoints(flow, Audio::DEVICE_STATE_ACTIVE) }
            .map_err(|error| native_error("enumerate endpoint selector fallback", error))?;
        let count = unsafe { collection.GetCount() }
            .map_err(|error| native_error("read endpoint selector fallback count", error))?;
        let mut matches = Vec::new();
        for index in 0..count {
            let Ok(device) = (unsafe { collection.Item(index) }) else {
                continue;
            };
            if endpoint_name(&device)
                .as_deref()
                .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
            {
                matches.push(device);
            }
        }
        Ok((matches.len() == 1).then(|| matches.remove(0)))
    }
}

fn active_device_by_id(
    enumerator: &Audio::IMMDeviceEnumerator,
    flow: Audio::EDataFlow,
    wanted_id: &str,
) -> BackendResult<Option<Audio::IMMDevice>> {
    let collection = unsafe { enumerator.EnumAudioEndpoints(flow, Audio::DEVICE_STATE_ACTIVE) }
        .map_err(|error| native_error("enumerate current endpoint selector", error))?;
    let count = unsafe { collection.GetCount() }
        .map_err(|error| native_error("read current endpoint selector count", error))?;
    for index in 0..count {
        let device = unsafe { collection.Item(index) }
            .map_err(|error| native_error("read current endpoint selector", error))?;
        let id = unsafe { device.GetId() }
            .map(take_pwstr)
            .map_err(|error| native_error("read current endpoint selector ID", error))?;
        if id == wanted_id {
            return Ok(Some(device));
        }
    }
    Ok(None)
}

pub(super) fn endpoint_stable_id(device: &Audio::IMMDevice) -> Option<String> {
    unsafe { property_string(device, &PKEY_AUDIO_ENDPOINT_STABLE_ID) }
}

/// Resolve an endpoint identity only from provider-owned properties.  The
/// service name proves which package owns the endpoint; the project role
/// property survives a friendly-name rename and avoids conflating the four
/// semantic endpoints.  Until the ACX provider publishes both properties,
/// this returns `None` and the worker keeps the virtual-driver state degraded.
pub(super) fn qpwgraph_virtual_endpoint_identity(
    device: &Audio::IMMDevice,
    mmdevice_id: &str,
) -> Option<QpwVirtualEndpointIdentity> {
    let service = unsafe {
        property_string(
            device,
            &Properties::DEVPKEY_Device_Service as *const _ as *const _,
        )
    }?;
    if !service.eq_ignore_ascii_case(QPWGRAPH_DRIVER_SERVICE) {
        return None;
    }
    let role = unsafe { property_string(device, &PKEY_QPWGRAPH_ENDPOINT_ROLE) }
        .and_then(|value| qpwgraph_endpoint_role(&value))?;
    classify_driver_owned_endpoint(
        role,
        endpoint_stable_id(device),
        mmdevice_id.to_owned(),
        unsafe {
            property_string(
                device,
                &Properties::DEVPKEY_Device_DriverVersion as *const _ as *const _,
            )
        },
        true,
    )
}

pub(super) fn qpwgraph_endpoint_role_matches_flow(
    flow: Audio::EDataFlow,
    role: QpwVirtualEndpointRole,
) -> bool {
    matches!(
        (flow, role),
        (Audio::eRender, QpwVirtualEndpointRole::AppRender)
            | (Audio::eRender, QpwVirtualEndpointRole::RelayRender)
            | (Audio::eCapture, QpwVirtualEndpointRole::AppMonitor)
            | (Audio::eCapture, QpwVirtualEndpointRole::RelayCapture)
    )
}

/// Find one active endpoint by the provider-owned service and semantic role.
/// Friendly names are intentionally not consulted: relay output must never be
/// redirected to an unrelated endpoint that happens to use the same label.
#[cfg(feature = "relay")]
pub(crate) fn find_qpwgraph_endpoint(
    enumerator: &Audio::IMMDeviceEnumerator,
    flow: Audio::EDataFlow,
    role: QpwVirtualEndpointRole,
) -> BackendResult<Option<Audio::IMMDevice>> {
    if !qpwgraph_endpoint_role_matches_flow(flow, role) {
        return Ok(None);
    }
    let collection = unsafe { enumerator.EnumAudioEndpoints(flow, Audio::DEVICE_STATE_ACTIVE) }
        .map_err(|error| native_error("enumerate qpwgraph virtual endpoints", error))?;
    let count = unsafe { collection.GetCount() }
        .map_err(|error| native_error("read qpwgraph virtual endpoint count", error))?;
    let mut matched = None;
    for index in 0..count {
        let device = unsafe { collection.Item(index) }
            .map_err(|error| native_error("read qpwgraph virtual endpoint", error))?;
        let mmdevice_id = unsafe { device.GetId() }
            .map(take_pwstr)
            .map_err(|error| native_error("read qpwgraph virtual endpoint ID", error))?;
        if qpwgraph_virtual_endpoint_identity(&device, &mmdevice_id)
            .is_some_and(|identity| identity.role == role)
        {
            if matched.is_some() {
                return Err(BackendError::unsupported(format!(
                    "multiple provider-owned endpoints advertise the {:?} role",
                    role
                )));
            }
            matched = Some(device);
        }
    }
    Ok(matched)
}

fn qpwgraph_endpoint_role(value: &str) -> Option<QpwVirtualEndpointRole> {
    [
        QpwVirtualEndpointRole::AppRender,
        QpwVirtualEndpointRole::AppMonitor,
        QpwVirtualEndpointRole::RelayRender,
        QpwVirtualEndpointRole::RelayCapture,
    ]
    .into_iter()
    .find(|role| {
        value.eq_ignore_ascii_case(role.config_key())
            || value.eq_ignore_ascii_case(role.stable_name())
    })
}

pub(super) fn native_error(operation: &str, error: impl std::fmt::Display) -> BackendError {
    BackendError::Native(format!("{operation} failed: {error}"))
}

pub(super) fn graph_id(local_id: u64) -> u64 {
    encode_backend_id(BackendNamespace::WindowsAudio, local_id)
}

pub(super) fn endpoint_direction(flow: Audio::EDataFlow) -> Direction {
    if flow == Audio::eRender {
        Direction::Sink
    } else {
        Direction::Source
    }
}

pub(super) fn session_direction(flow: Audio::EDataFlow) -> Direction {
    if flow == Audio::eRender {
        Direction::Source
    } else {
        Direction::Sink
    }
}

pub(super) fn session_link_ports(
    flow: Audio::EDataFlow,
    session_port: PortId,
    endpoint_port: PortId,
) -> (PortId, PortId) {
    if flow == Audio::eRender {
        (session_port, endpoint_port)
    } else {
        (endpoint_port, session_port)
    }
}

pub(super) fn stable_local_id(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    let local = hash & LOCAL_ID_MASK;
    if local == 0 {
        1
    } else {
        local
    }
}

pub(super) fn endpoint_node_local_id(endpoint_id: &str) -> u64 {
    stable_local_id(&format!("endpoint-node:{endpoint_id}"))
}

pub(super) fn endpoint_port_local_id(endpoint_id: &str) -> u64 {
    stable_local_id(&format!("endpoint-port:{endpoint_id}"))
}

/// A playback endpoint's monitor port: what that endpoint is playing, read
/// back through WASAPI loopback.
///
/// PipeWire gives every sink a monitor, and it is what makes "send the
/// speakers somewhere else" a link the user can draw rather than a hidden
/// setting. Windows has the same capability through loopback capture, so the
/// port exists here for the same reason.
pub(super) fn endpoint_monitor_port_local_id(endpoint_id: &str) -> u64 {
    stable_local_id(&format!("endpoint-monitor-port:{endpoint_id}"))
}

/// A link qpwgraph itself owns, as opposed to a relationship Core Audio
/// merely reports. Derived from the pair so the same route keeps its identity
/// across a rebuild.
pub(super) fn managed_link_local_id(output: PortId, input: PortId) -> u64 {
    stable_local_id(&format!("route-link:{}:{}", output.0, input.0))
}

pub(super) fn session_node_local_id(endpoint_id: &str, session_id: &str) -> u64 {
    stable_local_id(&format!("session-node:{endpoint_id}:{session_id}"))
}

pub(super) fn session_port_local_id(endpoint_id: &str, session_id: &str) -> u64 {
    stable_local_id(&format!("session-port:{endpoint_id}:{session_id}"))
}

pub(super) fn session_link_local_id(endpoint_id: &str, session_id: &str) -> u64 {
    stable_local_id(&format!("session-link:{endpoint_id}:{session_id}"))
}

pub(super) fn take_pwstr(value: PWSTR) -> String {
    let text = unsafe { value.to_string() }.unwrap_or_default();
    unsafe { Com::CoTaskMemFree(Some(value.0 as *mut _)) };
    text
}

pub(super) fn endpoint_name(device: &Audio::IMMDevice) -> Option<String> {
    unsafe {
        property_string(
            device,
            &Properties::DEVPKEY_Device_FriendlyName as *const _ as *const _,
        )
    }
}

pub(super) unsafe fn property_string(
    device: &Audio::IMMDevice,
    key: *const PROPERTYKEY,
) -> Option<String> {
    let store: IPropertyStore = device.OpenPropertyStore(STGM_READ).ok()?;
    let mut value = store.GetValue(key).ok()?;
    let prop_variant = &value.Anonymous.Anonymous;
    if prop_variant.vt != VT_LPWSTR {
        let _ = StructuredStorage::PropVariantClear(&mut value);
        return None;
    }
    let ptr = *(&prop_variant.Anonymous as *const _ as *const *const u16);
    if ptr.is_null() {
        let _ = StructuredStorage::PropVariantClear(&mut value);
        return None;
    }
    let mut length = 0usize;
    while length < 32_768 && *ptr.add(length) != 0 {
        length += 1;
    }
    let text = if length == 32_768 {
        None
    } else {
        Some(
            OsString::from_wide(std::slice::from_raw_parts(ptr, length))
                .to_string_lossy()
                .into_owned(),
        )
    };
    let _ = StructuredStorage::PropVariantClear(&mut value);
    text
}

pub(super) fn process_name(process_id: u32) -> Option<String> {
    if process_id == 0 {
        return None;
    }
    let process =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;
    let mut buffer = [0u16; 512];
    let mut length = buffer.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    let _ = unsafe { CloseHandle(process) };
    result.ok()?;
    let path = OsString::from_wide(&buffer[..length as usize]);
    let name = std::path::Path::new(&path).file_stem()?.to_string_lossy();
    (!name.is_empty()).then(|| name.into_owned())
}

/// An effect instance's graph identities.
///
/// Derived from the caller's instance id rather than from anything Windows
/// supplies, because Windows knows nothing about effects: the instance id is
/// what a patchbay file stores and what has to still mean the same node
/// tomorrow.
pub(super) fn effect_node_local_id(instance_id: &str) -> u64 {
    stable_local_id(&format!("effect-node:{instance_id}"))
}

pub(super) fn effect_input_port_local_id(instance_id: &str) -> u64 {
    stable_local_id(&format!("effect-in:{instance_id}"))
}

pub(super) fn effect_output_port_local_id(instance_id: &str) -> u64 {
    stable_local_id(&format!("effect-out:{instance_id}"))
}
