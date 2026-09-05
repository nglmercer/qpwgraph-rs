//! Isolated policy boundary for per-application output selection.
//!
//! Windows has no documented Core Audio API for moving another process. The
//! safe default therefore always returns [`AppRoutePolicySupport::ManualOnly`]
//! and directs the user to Volume mixer. An undocumented implementation must
//! live behind this trait, an explicit configuration switch, and verified
//! interface declarations; vtable probing is never permitted here.

use crate::api::{BackendError, BackendResult};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use windows::core::PWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessIdentity {
    pub executable_path_hash: Option<String>,
    pub executable_name: Option<String>,
    pub package_family_name: Option<String>,
    pub app_user_model_id: Option<String>,
    pub display_name: Option<String>,
}

impl ProcessIdentity {
    pub fn is_stable(&self) -> bool {
        self.executable_path_hash.is_some()
            || self.package_family_name.is_some()
            || self.app_user_model_id.is_some()
    }

    /// Resolve the stable identity that is available for a live process.
    ///
    /// The PID is used only during this query and is intentionally not stored
    /// in the returned value. A path hash and executable name together prevent
    /// a persisted route from silently attaching to an unrelated PID reuse.
    pub fn from_pid(pid: u32) -> BackendResult<Self> {
        if pid == 0 {
            return Err(BackendError::native(
                "process identity PID must be non-zero",
            ));
        }
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .map_err(|error| {
                BackendError::native(format!(
                    "could not open process {pid} for identity: {error}"
                ))
            })?;
        let mut buffer = [0u16; 32_768];
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
        result.map_err(|error| {
            BackendError::native(format!("could not read process {pid} identity: {error}"))
        })?;
        let path = OsString::from_wide(&buffer[..length as usize]);
        let path = path.to_string_lossy().into_owned();
        let executable_name = Path::new(&path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty());
        let executable_path_hash = Some(hash_executable_path(&path));
        Ok(Self {
            executable_path_hash,
            executable_name: executable_name.clone(),
            package_family_name: None,
            app_user_model_id: None,
            display_name: executable_name,
        })
    }

    /// A compact selector suitable for endpoint-choice IDs and diagnostics.
    /// It contains no path or PID and is stable across process restarts.
    pub fn selector_key(&self) -> Option<String> {
        self.executable_path_hash
            .clone()
            .or_else(|| self.package_family_name.clone())
            .or_else(|| self.app_user_model_id.clone())
    }

    /// Match only fields present in the persisted selector. Stable fields are
    /// compared case-insensitively because Windows paths and package names are
    /// case-insensitive even when their spelling changes between launches.
    pub fn matches(&self, candidate: &Self) -> bool {
        fn field_matches(expected: &Option<String>, actual: &Option<String>) -> bool {
            expected.as_ref().is_none_or(|expected| {
                actual
                    .as_deref()
                    .is_some_and(|actual| expected.eq_ignore_ascii_case(actual))
            })
        }
        self.is_stable()
            && field_matches(&self.executable_path_hash, &candidate.executable_path_hash)
            && field_matches(&self.executable_name, &candidate.executable_name)
            && field_matches(&self.package_family_name, &candidate.package_family_name)
            && field_matches(&self.app_user_model_id, &candidate.app_user_model_id)
            && field_matches(&self.display_name, &candidate.display_name)
    }
}

fn hash_executable_path(path: &str) -> String {
    let normalized = path.to_ascii_lowercase();
    let digest = Sha256::digest(normalized.as_bytes());
    let mut text = String::with_capacity(7 + digest.len() * 2);
    text.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    text
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioFlow {
    Render,
    Capture,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioRole {
    Console,
    Multimedia,
    Communications,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppRoutePolicySupport {
    ManualOnly { reason: String },
    Experimental { interface_version: String },
}

pub trait AppRoutePolicy: Send + Sync {
    fn support(&self) -> AppRoutePolicySupport;

    fn get_persisted_endpoint(
        &self,
        process: &ProcessIdentity,
        flow: AudioFlow,
        role: AudioRole,
    ) -> BackendResult<Option<String>>;

    fn set_persisted_endpoint(
        &self,
        process: &ProcessIdentity,
        flow: AudioFlow,
        role: AudioRole,
        endpoint: Option<&str>,
    ) -> BackendResult<()>;
}

#[derive(Debug, Default)]
pub struct UnsupportedAppRoutePolicy;

impl UnsupportedAppRoutePolicy {
    pub const MANUAL_INSTRUCTIONS: &'static str =
        "Set this app's output to QPWGraph Virtual Output in Settings > System > Sound > Volume mixer.";
}

impl AppRoutePolicy for UnsupportedAppRoutePolicy {
    fn support(&self) -> AppRoutePolicySupport {
        AppRoutePolicySupport::ManualOnly {
            reason: Self::MANUAL_INSTRUCTIONS.into(),
        }
    }

    fn get_persisted_endpoint(
        &self,
        _process: &ProcessIdentity,
        _flow: AudioFlow,
        _role: AudioRole,
    ) -> BackendResult<Option<String>> {
        Err(BackendError::unsupported(Self::MANUAL_INSTRUCTIONS))
    }

    fn set_persisted_endpoint(
        &self,
        _process: &ProcessIdentity,
        _flow: AudioFlow,
        _role: AudioRole,
        _endpoint: Option<&str>,
    ) -> BackendResult<()> {
        Err(BackendError::unsupported(Self::MANUAL_INSTRUCTIONS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_is_not_part_of_process_identity() {
        let identity = ProcessIdentity {
            executable_path_hash: Some("sha256:abc".into()),
            executable_name: Some("player.exe".into()),
            package_family_name: None,
            app_user_model_id: None,
            display_name: Some("Player".into()),
        };
        assert!(identity.is_stable());
    }

    #[test]
    fn unsupported_policy_is_actionable_and_safe() {
        let policy = UnsupportedAppRoutePolicy;
        let support = policy.support();
        assert!(matches!(support, AppRoutePolicySupport::ManualOnly { .. }));
        assert!(policy
            .set_persisted_endpoint(
                &ProcessIdentity {
                    executable_path_hash: None,
                    executable_name: Some("player.exe".into()),
                    package_family_name: None,
                    app_user_model_id: None,
                    display_name: None,
                },
                AudioFlow::Render,
                AudioRole::Multimedia,
                Some("endpoint")
            )
            .is_err());
    }

    #[test]
    fn selectors_match_stable_identity_without_a_pid() {
        let selector = ProcessIdentity {
            executable_path_hash: Some("sha256:abc".into()),
            executable_name: Some("Player.EXE".into()),
            package_family_name: None,
            app_user_model_id: None,
            display_name: None,
        };
        let candidate = ProcessIdentity {
            executable_path_hash: Some("SHA256:ABC".into()),
            executable_name: Some("player.exe".into()),
            package_family_name: None,
            app_user_model_id: None,
            display_name: Some("A different display name".into()),
        };
        assert!(selector.matches(&candidate));
        assert_eq!(selector.selector_key().as_deref(), Some("sha256:abc"));
    }
}
