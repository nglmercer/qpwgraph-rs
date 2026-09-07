//! The one isolated boundary for Windows' private per-application endpoint API.
//!
//! `Windows.Media.Internal.AudioPolicyConfig` is not a documented public Core
//! Audio interface. This module consequently uses only an explicitly
//! declared interface shape and only on builds for which the declaration has
//! been checked. There is no slot probing, fallback IID guessing, or
//! "try-until-one-works" behavior here. Every caller outside this file sees
//! the [`AppRoutePolicy`] trait and can safely fall back to Volume Mixer.

use super::app_route_policy::{
    AppRoutePolicy, AppRoutePolicySupport, AudioFlow, AudioRole, ProcessIdentity,
};
use super::identity::WindowsEndpointSelector;
use crate::api::{BackendError, BackendResult};
use pw_graph_config::WindowsApplicationSelector;
use std::ffi::c_void;
use std::mem::{transmute_copy, MaybeUninit};
use std::ptr::null_mut;
use std::sync::Mutex;
use windows::core::{IUnknown, Interface, GUID, HRESULT, HSTRING};
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::Audio::{self, EDataFlow, ERole};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::WinRT::{RoGetActivationFactory, WindowsCreateString};

const AUDIO_POLICY_CONFIG_CLASS: &str = "Windows.Media.Internal.AudioPolicyConfig";
const POLICY_MMDEVICE_PREFIX: &str = r"\\?\SWD#MMDEVAPI#";
const POLICY_RENDER_SUFFIX: &str = "#{e6327cad-dcec-4949-ae8a-991e976a79d2}";
const POLICY_CAPTURE_SUFFIX: &str = "#{2eef81be-33fa-4800-9670-1cd474972c3f}";

/// Windows 10 downlevel IID used by the interface declaration below.
pub const AUDIO_POLICY_CONFIG_FACTORY_IID_WIN10: GUID =
    GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258);

/// Windows 11/21H2 IID is recorded here for diagnostics and future table
/// expansion. It is not enabled by the current build-gated table because this
/// repository has no live Windows 11 evidence for the same method layout.
pub const AUDIO_POLICY_CONFIG_FACTORY_IID_WIN11: GUID =
    GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324);

/// A single build-matched private interface declaration.
///
/// The Windows 10 range is deliberately narrow: the exact interface was
/// activated and queried on build 19045, and the table must not turn that one
/// observation into an unbounded version guess.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifiedAudioPolicyAbi {
    pub interface_version: &'static str,
    pub iid: GUID,
    pub minimum_build: u32,
    pub maximum_build: Option<u32>,
}

pub const VERIFIED_AUDIO_POLICY_ABIS: &[VerifiedAudioPolicyAbi] = &[VerifiedAudioPolicyAbi {
    interface_version: "audio-policy-config-win10-v1",
    iid: AUDIO_POLICY_CONFIG_FACTORY_IID_WIN10,
    minimum_build: 19_041,
    maximum_build: Some(19_045),
}];

fn current_os_build() -> u32 {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..OSVERSIONINFOW::default()
    };
    let status = unsafe { RtlGetVersion(&mut version) };
    if status.0 >= 0 {
        version.dwBuildNumber
    } else {
        0
    }
}

fn verified_abi_for_build(build: u32) -> Option<VerifiedAudioPolicyAbi> {
    VERIFIED_AUDIO_POLICY_ABIS.iter().copied().find(|abi| {
        build >= abi.minimum_build && abi.maximum_build.is_none_or(|maximum| build <= maximum)
    })
}

fn verified_abi_for_build_and_iid(build: u32, iid: GUID) -> Option<VerifiedAudioPolicyAbi> {
    verified_abi_for_build(build).filter(|abi| abi.iid == iid)
}

/// The exact IInspectable vtable declared by the known Windows 10 interface.
///
/// The 19 opaque entries are intentionally named in declaration order. They
/// are not callable through this module; their only purpose is to preserve the
/// ABI offset before the three endpoint-policy methods. Keeping them explicit
/// makes a layout change visible in review instead of hiding it behind a
/// guessed slot index.
#[repr(C)]
struct AudioPolicyConfigVtable {
    query_interface: unsafe extern "system" fn(
        this: *mut c_void,
        iid: *const GUID,
        interface: *mut *mut c_void,
    ) -> HRESULT,
    add_ref: unsafe extern "system" fn(this: *mut c_void) -> u32,
    release: unsafe extern "system" fn(this: *mut c_void) -> u32,
    get_iids: unsafe extern "system" fn(
        this: *mut c_void,
        count: *mut u32,
        iids: *mut *mut GUID,
    ) -> HRESULT,
    get_runtime_class_name:
        unsafe extern "system" fn(this: *mut c_void, name: *mut HSTRING) -> HRESULT,
    get_trust_level: unsafe extern "system" fn(this: *mut c_void, level: *mut i32) -> HRESULT,
    add_ctx_volume_change: *const c_void,
    remove_ctx_volume_changed: *const c_void,
    add_ringer_vibrate_state_changed: *const c_void,
    remove_ringer_vibrate_state_change: *const c_void,
    set_volume_group_gain_for_id: *const c_void,
    get_volume_group_gain_for_id: *const c_void,
    get_active_volume_group_for_endpoint_id: *const c_void,
    get_volume_groups_for_endpoint: *const c_void,
    get_current_volume_context: *const c_void,
    set_volume_group_mute_for_id: *const c_void,
    get_volume_group_mute_for_id: *const c_void,
    set_ringer_vibrate_state: *const c_void,
    get_ringer_vibrate_state: *const c_void,
    set_preferred_chat_application: *const c_void,
    reset_preferred_chat_application: *const c_void,
    get_preferred_chat_application: *const c_void,
    get_current_chat_applications: *const c_void,
    add_chat_context_changed: *const c_void,
    remove_chat_context_changed: *const c_void,
    set_persisted_default_audio_endpoint: unsafe extern "system" fn(
        this: *mut c_void,
        process_id: u32,
        flow: EDataFlow,
        role: ERole,
        device_id: *mut c_void,
    ) -> HRESULT,
    get_persisted_default_audio_endpoint: unsafe extern "system" fn(
        this: *mut c_void,
        process_id: u32,
        flow: EDataFlow,
        role: ERole,
        device_id: *mut HSTRING,
    ) -> HRESULT,
    clear_all_persisted_application_default_endpoints:
        unsafe extern "system" fn(this: *mut c_void) -> HRESULT,
}

/// Owned COM wrapper for the exact interface above. The factory is returned by
/// `RoGetActivationFactory`, so its lifetime is reference-counted by
/// `IUnknown`; no raw pointer escapes this module.
#[repr(transparent)]
#[derive(Clone)]
struct AudioPolicyConfigFactory(IUnknown);

unsafe impl Interface for AudioPolicyConfigFactory {
    type Vtable = AudioPolicyConfigVtable;

    const IID: GUID = AUDIO_POLICY_CONFIG_FACTORY_IID_WIN10;
}

struct ComApartment {
    uninitialize: bool,
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.uninitialize {
            unsafe { CoUninitialize() };
        }
    }
}

fn initialize_com() -> Result<ComApartment, HRESULT> {
    let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if result.0 >= 0 {
        Ok(ComApartment { uninitialize: true })
    } else if result.0 == RPC_E_CHANGED_MODE.0 {
        // The caller already owns an STA. It is still legal to use the WinRT
        // activation factory; only balance the CoInitializeEx calls made here.
        Ok(ComApartment {
            uninitialize: false,
        })
    } else {
        Err(result)
    }
}

fn activate_factory() -> Result<(ComApartment, AudioPolicyConfigFactory), PolicyCallError> {
    let apartment = initialize_com().map_err(|error| PolicyCallError {
        hresult: Some(error.0),
        message: format!(
            "COM initialization failed: HRESULT {:#010x}",
            error.0 as u32
        ),
    })?;
    let class_name = HSTRING::from(AUDIO_POLICY_CONFIG_CLASS);
    let factory = unsafe { RoGetActivationFactory::<AudioPolicyConfigFactory>(&class_name) }
        .map_err(|error| PolicyCallError {
            hresult: Some(error.code().0),
            message: format!(
                "AudioPolicyConfig activation failed: HRESULT {:#010x}",
                error.code().0 as u32
            ),
        })?;
    Ok((apartment, factory))
}

fn hstring_handle(value: &HSTRING) -> *mut c_void {
    // HSTRING is a transparent handle in windows-core/windows-strings. The
    // private ABI takes that handle by value (C# exposes it as IntPtr).
    unsafe { transmute_copy(value) }
}

fn flow_value(flow: AudioFlow) -> EDataFlow {
    match flow {
        AudioFlow::Render => Audio::eRender,
        AudioFlow::Capture => Audio::eCapture,
    }
}

fn role_value(role: AudioRole) -> ERole {
    match role {
        AudioRole::Console => Audio::eConsole,
        AudioRole::Multimedia => Audio::eMultimedia,
        AudioRole::Communications => Audio::eCommunications,
    }
}

fn policy_endpoint_suffix(flow: AudioFlow) -> &'static str {
    match flow {
        AudioFlow::Render => POLICY_RENDER_SUFFIX,
        AudioFlow::Capture => POLICY_CAPTURE_SUFFIX,
    }
}

fn pack_policy_endpoint_id(endpoint: &str, flow: AudioFlow) -> String {
    if endpoint
        .get(..POLICY_MMDEVICE_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(POLICY_MMDEVICE_PREFIX))
    {
        return endpoint.to_owned();
    }
    format!(
        "{POLICY_MMDEVICE_PREFIX}{endpoint}{}",
        policy_endpoint_suffix(flow)
    )
}

fn unpack_policy_endpoint_id(endpoint: &str, flow: AudioFlow) -> String {
    let endpoint = endpoint
        .get(POLICY_MMDEVICE_PREFIX.len()..)
        .filter(|_| {
            endpoint
                .get(..POLICY_MMDEVICE_PREFIX.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(POLICY_MMDEVICE_PREFIX))
        })
        .unwrap_or(endpoint);
    let suffix = policy_endpoint_suffix(flow);
    endpoint
        .get(..endpoint.len().saturating_sub(suffix.len()))
        .filter(|prefix| {
            endpoint.len() >= suffix.len()
                && endpoint[endpoint.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
                && !prefix.is_empty()
        })
        .unwrap_or(endpoint)
        .to_owned()
}

#[derive(Debug)]
struct PolicyCallError {
    hresult: Option<i32>,
    message: String,
}

fn failed_hresult(operation: &str, result: HRESULT) -> PolicyCallError {
    PolicyCallError {
        hresult: Some(result.0),
        message: format!("{operation} failed: HRESULT {:#010x}", result.0 as u32),
    }
}

fn get_endpoint(
    process_id: u32,
    flow: AudioFlow,
    role: AudioRole,
) -> Result<Option<String>, PolicyCallError> {
    let (_apartment, factory) = activate_factory()?;
    let mut device_id = MaybeUninit::<HSTRING>::zeroed();
    let result = unsafe {
        (factory.vtable().get_persisted_default_audio_endpoint)(
            factory.as_raw(),
            process_id,
            flow_value(flow),
            role_value(role),
            device_id.as_mut_ptr(),
        )
    };
    if result.0 < 0 {
        return Err(failed_hresult("GetPersistedDefaultAudioEndpoint", result));
    }
    let device_id = unsafe { device_id.assume_init() };
    let value = device_id.to_string_lossy();
    Ok((!value.is_empty()).then(|| unpack_policy_endpoint_id(&value, flow)))
}

fn set_endpoint(
    process_id: u32,
    flow: AudioFlow,
    role: AudioRole,
    endpoint: Option<&str>,
) -> Result<(), PolicyCallError> {
    let (_apartment, factory) = activate_factory()?;
    let endpoint = match endpoint {
        Some(endpoint) => {
            let endpoint = pack_policy_endpoint_id(endpoint, flow);
            let wide: Vec<u16> = endpoint.encode_utf16().collect();
            unsafe { WindowsCreateString(Some(&wide)) }.map_err(|error| PolicyCallError {
                hresult: Some(error.code().0),
                message: format!(
                    "WindowsCreateString failed: HRESULT {:#010x}",
                    error.code().0 as u32
                ),
            })?
        }
        None => HSTRING::new(),
    };
    let device_id = if endpoint.is_empty() {
        null_mut()
    } else {
        hstring_handle(&endpoint)
    };
    let result = unsafe {
        (factory.vtable().set_persisted_default_audio_endpoint)(
            factory.as_raw(),
            process_id,
            flow_value(flow),
            role_value(role),
            device_id,
        )
    };
    if result.0 < 0 {
        return Err(failed_hresult("SetPersistedDefaultAudioEndpoint", result));
    }
    Ok(())
}

/// Non-sensitive support/operation information exported to diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioPolicyDiagnostics {
    pub enabled: bool,
    pub os_build: u32,
    pub interface_version: Option<String>,
    pub last_hresult: Option<i32>,
    pub last_operation: Option<String>,
    pub fallback_reason: String,
}

impl AudioPolicyDiagnostics {
    fn manual(enabled: bool, os_build: u32, reason: impl Into<String>) -> Self {
        Self {
            enabled,
            os_build,
            interface_version: None,
            last_hresult: None,
            last_operation: None,
            fallback_reason: reason.into(),
        }
    }
}

/// The only implementation allowed to call the private policy interface.
#[derive(Debug)]
pub struct VerifiedAudioPolicyConfig {
    diagnostics: Mutex<AudioPolicyDiagnostics>,
}

impl VerifiedAudioPolicyConfig {
    pub fn new(enabled: bool) -> Self {
        let build = current_os_build();
        let diagnostics = match (
            enabled,
            verified_abi_for_build_and_iid(build, AUDIO_POLICY_CONFIG_FACTORY_IID_WIN10),
        ) {
            (false, _) => AudioPolicyDiagnostics::manual(
                false,
                build,
                "experimental automatic application routing is disabled",
            ),
            (true, Some(abi)) => match activate_factory() {
                Ok((_apartment, _factory)) => AudioPolicyDiagnostics {
                    enabled: true,
                    os_build: build,
                    interface_version: Some(abi.interface_version.into()),
                    last_hresult: Some(0),
                    last_operation: Some("activate".into()),
                    fallback_reason:
                        "verified ABI activation succeeded; automatic routing remains opt-in".into(),
                },
                Err(error) => AudioPolicyDiagnostics {
                    enabled: true,
                    os_build: build,
                    interface_version: None,
                    last_hresult: error.hresult,
                    last_operation: Some("activate".into()),
                    fallback_reason: error.message,
                },
            },
            (true, None) => AudioPolicyDiagnostics::manual(
                true,
                build,
                "no AudioPolicyConfig ABI is verified for this Windows build",
            ),
        };
        Self {
            diagnostics: Mutex::new(diagnostics),
        }
    }

    pub fn disabled() -> Self {
        Self::new(false)
    }

    pub fn diagnostics(&self) -> AudioPolicyDiagnostics {
        self.diagnostics
            .lock()
            .map(|diagnostics| diagnostics.clone())
            .unwrap_or_else(|_| {
                AudioPolicyDiagnostics::manual(false, 0, "diagnostics lock poisoned")
            })
    }

    fn record_rejection(&self, operation: &str, hresult: Option<i32>) -> BackendError {
        if let Ok(mut diagnostics) = self.diagnostics.lock() {
            diagnostics.last_operation = Some(operation.into());
            diagnostics.last_hresult = hresult;
        }
        BackendError::unsupported(self.diagnostics().fallback_reason)
    }

    fn record_failure(&self, operation: &str, error: &PolicyCallError) -> BackendError {
        if let Ok(mut diagnostics) = self.diagnostics.lock() {
            diagnostics.last_operation = Some(operation.into());
            diagnostics.last_hresult = error.hresult;
            // A method failure means that the declaration was not safe for
            // this runtime observation. Stop all subsequent automatic calls
            // until the user explicitly recreates the backend capability.
            diagnostics.interface_version = None;
            diagnostics.fallback_reason = error.message.clone();
        }
        BackendError::unsupported(error.message.clone())
    }

    fn record_success(&self, operation: &str) {
        if let Ok(mut diagnostics) = self.diagnostics.lock() {
            diagnostics.last_operation = Some(operation.into());
            diagnostics.last_hresult = Some(0);
        }
    }

    fn require_stable_identity(&self, process: &ProcessIdentity) -> BackendResult<u32> {
        if !process.is_stable() {
            return Err(self.record_rejection("reject_unstable_process_identity", None));
        }
        let Some(pid) = process.runtime_pid() else {
            return Err(self.record_rejection("reject_missing_process_id", None));
        };
        let live = ProcessIdentity::from_pid(pid).inspect_err(|error| {
            if let Ok(mut diagnostics) = self.diagnostics.lock() {
                diagnostics.last_operation = Some("reject_stale_process_identity".into());
                diagnostics.last_hresult = None;
                diagnostics.fallback_reason = error.to_string();
            }
        })?;
        if !process.matches(&live) {
            return Err(self.record_rejection("reject_stale_process_identity", None));
        }
        Ok(pid)
    }

    /// Clear one process/flow/role entry by passing a null HSTRING handle.
    /// The global `ClearAllPersistedApplicationDefaultEndpoints` method is
    /// intentionally never used: it would erase routes qpwgraph does not own.
    pub fn clear_persisted_endpoint(
        &self,
        process: &ProcessIdentity,
        flow: AudioFlow,
        role: AudioRole,
    ) -> BackendResult<()> {
        self.set_persisted_endpoint(process, flow, role, None)
    }

    /// Undo one member of a partially applied transaction.
    ///
    /// A failed private call demotes the public capability to `ManualOnly`,
    /// which is the correct steady-state safety behavior. A transaction that
    /// already changed an earlier role nevertheless needs one last, identity-
    /// checked attempt to restore that role. This method is deliberately
    /// crate-private and is only used by the automatic transaction rollback;
    /// it never re-enables the public capability after a failure.
    pub(crate) fn rollback_persisted_endpoint(
        &self,
        process: &ProcessIdentity,
        flow: AudioFlow,
        role: AudioRole,
        endpoint: Option<&str>,
    ) -> BackendResult<()> {
        let pid = self.require_stable_identity(process)?;
        match set_endpoint(pid, flow, role, endpoint) {
            Ok(()) => {
                self.record_success("rollback_persisted_endpoint");
                Ok(())
            }
            Err(error) => Err(self.record_failure("rollback_persisted_endpoint", &error)),
        }
    }
}

impl Default for VerifiedAudioPolicyConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

impl AppRoutePolicy for VerifiedAudioPolicyConfig {
    fn support(&self) -> AppRoutePolicySupport {
        let diagnostics = self.diagnostics();
        if let Some(interface_version) = diagnostics.interface_version {
            AppRoutePolicySupport::Experimental {
                interface_version,
                os_build: diagnostics.os_build,
            }
        } else {
            AppRoutePolicySupport::ManualOnly {
                reason: diagnostics.fallback_reason,
            }
        }
    }

    fn get_persisted_endpoint(
        &self,
        process: &ProcessIdentity,
        flow: AudioFlow,
        role: AudioRole,
    ) -> BackendResult<Option<String>> {
        let pid = self.require_stable_identity(process)?;
        if self.diagnostics().interface_version.is_none() {
            return Err(self.record_rejection("get_persisted_endpoint", None));
        }
        match get_endpoint(pid, flow, role) {
            Ok(endpoint) => {
                self.record_success("get_persisted_endpoint");
                Ok(endpoint)
            }
            Err(error) => Err(self.record_failure("get_persisted_endpoint", &error)),
        }
    }

    fn set_persisted_endpoint(
        &self,
        process: &ProcessIdentity,
        flow: AudioFlow,
        role: AudioRole,
        endpoint: Option<&str>,
    ) -> BackendResult<()> {
        let pid = self.require_stable_identity(process)?;
        if self.diagnostics().interface_version.is_none() {
            return Err(self.record_rejection("set_persisted_endpoint", None));
        }
        match set_endpoint(pid, flow, role, endpoint) {
            Ok(()) => {
                self.record_success("set_persisted_endpoint");
                Ok(())
            }
            Err(error) => Err(self.record_failure("set_persisted_endpoint", &error)),
        }
    }
}

/// Ownership record for an automatic endpoint change. The route can only be
/// restored while the endpoint currently observed is the endpoint qpwgraph
/// applied. A user change therefore prevents an unsafe restore.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AutomaticAppRouteLease {
    pub selector: WindowsApplicationSelector,
    /// Runtime-only PID to which `applied_endpoint` was written. This is
    /// deliberately not part of the persisted selector; the private Windows
    /// policy is PID-scoped on the verified Windows 10 ABI, so a selector
    /// restart must reapply the route to the replacement PID.
    pub(crate) process_id: u32,
    pub original_endpoint: Option<WindowsEndpointSelector>,
    pub applied_endpoint: WindowsEndpointSelector,
    pub generation: u64,
    pub owned: bool,
}

impl AutomaticAppRouteLease {
    pub fn new(
        selector: WindowsApplicationSelector,
        process_id: u32,
        original_endpoint: Option<WindowsEndpointSelector>,
        applied_endpoint: WindowsEndpointSelector,
        generation: u64,
    ) -> Self {
        Self {
            selector,
            process_id,
            original_endpoint,
            applied_endpoint,
            generation,
            owned: true,
        }
    }

    pub fn can_restore(&self, current: Option<&WindowsEndpointSelector>) -> bool {
        self.owned && current == Some(&self.applied_endpoint)
    }

    pub fn mark_user_override(&mut self) {
        self.owned = false;
    }

    pub fn restore_if_owned(
        &mut self,
        current: Option<&WindowsEndpointSelector>,
    ) -> Option<WindowsEndpointSelector> {
        if !self.can_restore(current) {
            return None;
        }
        self.owned = false;
        self.original_endpoint.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selector(key: &str) -> WindowsApplicationSelector {
        WindowsApplicationSelector {
            executable_path_hash: Some(key.into()),
            executable_name: Some("player.exe".into()),
            ..WindowsApplicationSelector::default()
        }
    }

    fn endpoint(id: &str) -> WindowsEndpointSelector {
        WindowsEndpointSelector {
            stable_id: Some(id.into()),
            current_mmdevice_id: Some(format!("mm:{id}")),
            friendly_name: Some(id.into()),
            data_flow: AudioFlow::Render,
        }
    }

    #[test]
    fn unsupported_build_is_manual_only() {
        assert!(verified_abi_for_build(22_000).is_none());
    }

    #[test]
    fn unknown_iid_is_manual_only_even_on_a_supported_build() {
        let unknown = GUID::from_u128(0x6c3f4d5e_71a2_4f28_9b50_07b9b5a1a0d4);
        assert!(verified_abi_for_build_and_iid(19_045, unknown).is_none());
    }

    #[test]
    fn supported_build_requires_activation_before_experimental_support() {
        let policy = VerifiedAudioPolicyConfig::new(true);
        if verified_abi_for_build(current_os_build()).is_some() {
            assert!(matches!(
                policy.support(),
                AppRoutePolicySupport::Experimental { .. }
            ));
        } else {
            assert!(matches!(
                policy.support(),
                AppRoutePolicySupport::ManualOnly { .. }
            ));
        }
    }

    #[test]
    fn private_interface_layout_has_the_three_known_policy_methods_after_nineteen_entries() {
        let pointer = std::mem::size_of::<*const c_void>();
        let header = 6 * pointer;
        let placeholders = 19 * pointer;
        let vtable = std::mem::size_of::<AudioPolicyConfigVtable>();
        assert_eq!(vtable, header + placeholders + 3 * pointer);
    }

    #[test]
    fn policy_endpoint_ids_round_trip_between_mmdevice_and_interface_forms() {
        let mmdevice = r#"{0.0.0.00000000}.{device}"#;
        let packed = pack_policy_endpoint_id(mmdevice, AudioFlow::Render);
        assert_eq!(
            packed,
            r#"\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{device}#{e6327cad-dcec-4949-ae8a-991e976a79d2}"#
        );
        assert_eq!(
            unpack_policy_endpoint_id(&packed, AudioFlow::Render),
            mmdevice
        );
        let mixed_case = packed.to_ascii_uppercase();
        assert_eq!(
            unpack_policy_endpoint_id(&mixed_case, AudioFlow::Render),
            mmdevice.to_ascii_uppercase()
        );
        assert_eq!(pack_policy_endpoint_id(&packed, AudioFlow::Render), packed);
    }

    #[test]
    fn lease_restores_only_when_qpwgraph_still_owns_the_current_route() {
        let original = endpoint("physical");
        let applied = endpoint("virtual");
        let mut lease = AutomaticAppRouteLease::new(
            selector("sha256:player"),
            4242,
            Some(original.clone()),
            applied.clone(),
            7,
        );
        assert!(!lease.can_restore(Some(&original)));
        assert_eq!(lease.restore_if_owned(Some(&original)), None);
        assert_eq!(lease.restore_if_owned(Some(&applied)), Some(original));
        assert!(!lease.owned);
    }

    #[test]
    fn user_override_revokes_restore_ownership() {
        let applied = endpoint("virtual");
        let mut lease =
            AutomaticAppRouteLease::new(selector("sha256:player"), 4242, None, applied.clone(), 1);
        lease.mark_user_override();
        assert!(!lease.can_restore(Some(&applied)));
        assert_eq!(lease.restore_if_owned(Some(&applied)), None);
    }

    #[test]
    fn lease_pid_is_runtime_only_and_can_identify_a_restart() {
        let lease = AutomaticAppRouteLease::new(
            selector("sha256:player"),
            4242,
            None,
            endpoint("virtual"),
            1,
        );
        assert_eq!(lease.process_id, 4242);
        assert!(lease.selector.is_stable());
    }

    #[test]
    fn display_name_only_identity_is_rejected_and_recorded() {
        let policy = VerifiedAudioPolicyConfig::new(true);
        let process = ProcessIdentity {
            executable_path_hash: None,
            executable_name: None,
            package_family_name: None,
            app_user_model_id: None,
            display_name: Some("Player".into()),
            process_id: None,
        };
        assert!(policy
            .set_persisted_endpoint(
                &process,
                AudioFlow::Render,
                AudioRole::Multimedia,
                Some("virtual")
            )
            .is_err());
        assert_eq!(
            policy.diagnostics().last_operation.as_deref(),
            Some("reject_unstable_process_identity")
        );
    }

    #[test]
    fn private_call_failure_demotes_the_policy_to_manual_only() {
        let policy = VerifiedAudioPolicyConfig {
            diagnostics: Mutex::new(AudioPolicyDiagnostics {
                enabled: true,
                os_build: 19_045,
                interface_version: Some("audio-policy-config-win10-v1".into()),
                last_hresult: Some(0),
                last_operation: Some("activate".into()),
                fallback_reason: "activation succeeded".into(),
            }),
        };
        let error = PolicyCallError {
            hresult: Some(-2_147_024_863),
            message: "SetPersistedDefaultAudioEndpoint failed".into(),
        };
        let _ = policy.record_failure("set_persisted_endpoint", &error);
        assert!(matches!(
            policy.support(),
            AppRoutePolicySupport::ManualOnly { .. }
        ));
        assert_eq!(policy.diagnostics().last_hresult, error.hresult);
    }

    #[test]
    #[ignore = "requires QPWGRAPH_TEST_AUDIO_POLICY=1 and changes only this test process while restoring it"]
    fn live_policy_roundtrip_restores_the_original_endpoint() {
        if std::env::var("QPWGRAPH_TEST_AUDIO_POLICY").as_deref() != Ok("1") {
            return;
        }
        let policy = VerifiedAudioPolicyConfig::new(true);
        assert!(matches!(
            policy.support(),
            AppRoutePolicySupport::Experimental { .. }
        ));
        let com = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
        };
        assert!(com.0 >= 0);
        let pid = std::env::var("QPWGRAPH_TEST_AUDIO_POLICY_PID")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(std::process::id);
        let process = ProcessIdentity::from_pid(pid).unwrap();
        let roles = [
            AudioRole::Console,
            AudioRole::Multimedia,
            AudioRole::Communications,
        ];
        let before: Vec<_> = roles
            .iter()
            .map(|role| {
                policy
                    .get_persisted_endpoint(&process, AudioFlow::Render, *role)
                    .unwrap()
            })
            .collect();
        let enumerator: Audio::IMMDeviceEnumerator = unsafe {
            windows::Win32::System::Com::CoCreateInstance(
                &Audio::MMDeviceEnumerator,
                None,
                windows::Win32::System::Com::CLSCTX_ALL,
            )
        }
        .unwrap();
        let endpoints =
            unsafe { enumerator.EnumAudioEndpoints(Audio::eRender, Audio::DEVICE_STATE_ACTIVE) }
                .unwrap();
        let device = unsafe { endpoints.Item(0) }.unwrap();
        let endpoint = super::super::identity::take_pwstr(unsafe { device.GetId() }.unwrap());

        for role in roles {
            policy
                .set_persisted_endpoint(&process, AudioFlow::Render, role, Some(&endpoint))
                .unwrap();
        }
        for role in roles {
            let after = policy
                .get_persisted_endpoint(&process, AudioFlow::Render, role)
                .unwrap();
            assert_eq!(after.as_deref(), Some(endpoint.as_str()));
        }

        for (role, original) in roles.iter().zip(&before) {
            policy
                .set_persisted_endpoint(&process, AudioFlow::Render, *role, original.as_deref())
                .unwrap();
        }
        for (role, original) in roles.iter().zip(&before) {
            let restored = policy
                .get_persisted_endpoint(&process, AudioFlow::Render, *role)
                .unwrap();
            assert_eq!(restored, *original);
        }
        unsafe { windows::Win32::System::Com::CoUninitialize() };
    }
}
