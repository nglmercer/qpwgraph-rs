//! Presentation icon references for Windows graph nodes.
//!
//! This mirrors the Linux `icon_name` contract: the backend supplies a
//! string, the UI resolves it to an image. On Windows the string is either a
//! full executable path (application sessions, icon extracted from the
//! binary by the UI), a device icon resource reference (`path,-index`, from
//! the MMDevice property store), or a `windows:...` sentinel for stock
//! fallbacks (see `pw_graph_core`). Icon data is presentation metadata only
//! and never participates in routing identity.

use super::*;
use pw_graph_core::{WINDOWS_ENDPOINT_CAPTURE_ICON, WINDOWS_ENDPOINT_RENDER_ICON};

/// Icon reference for an endpoint node: the device class icon path when the
/// MMDevice property store exposes one, otherwise a flow-specific sentinel
/// the UI maps to a stock system icon.
pub(super) fn endpoint_icon_name(device: &Audio::IMMDevice, flow: Audio::EDataFlow) -> String {
    endpoint_device_icon_path(device).unwrap_or_else(|| endpoint_fallback_icon(flow).to_owned())
}

/// Stock fallback for an endpoint whose device exposes no icon path.
pub(super) fn endpoint_fallback_icon(flow: Audio::EDataFlow) -> &'static str {
    if flow == Audio::eRender {
        WINDOWS_ENDPOINT_RENDER_ICON
    } else {
        WINDOWS_ENDPOINT_CAPTURE_ICON
    }
}

/// Icon reference for an application session node: the owning process's full
/// executable path, from which the UI extracts the binary's icon. `None`
/// when the process (or its image path) cannot be resolved; the node then
/// renders without an image, the same as a Linux node with no icon metadata.
pub(super) fn session_icon_name(process_id: u32) -> Option<String> {
    process_executable_path(process_id)
}

fn endpoint_device_icon_path(device: &Audio::IMMDevice) -> Option<String> {
    let path = unsafe {
        property_string(
            device,
            &Properties::DEVPKEY_DeviceClass_IconPath as *const _ as *const _,
        )
    };
    path.map(|path| path.trim().to_owned())
        .filter(|path| !path.is_empty())
}

/// Full image path of a live process, for icon extraction by the UI.
///
/// This deliberately returns the path rather than reusing
/// [`ProcessIdentity`]: identity is stable, hash-based, and persisted, while
/// an icon reference is runtime-only presentation metadata resolved straight
/// from the file on disk.
pub(super) fn process_executable_path(process_id: u32) -> Option<String> {
    if process_id == 0 {
        return None;
    }
    let process =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;
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
    result.ok()?;
    let path = OsString::from_wide(&buffer[..length as usize])
        .to_string_lossy()
        .into_owned();
    (!path.trim().is_empty()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_fallback_icons_follow_the_audio_flow() {
        assert_eq!(
            endpoint_fallback_icon(Audio::eRender),
            WINDOWS_ENDPOINT_RENDER_ICON
        );
        assert_eq!(
            endpoint_fallback_icon(Audio::eCapture),
            WINDOWS_ENDPOINT_CAPTURE_ICON
        );
    }

    #[test]
    fn endpoint_fallback_sentinels_are_distinct_windows_references() {
        assert_ne!(WINDOWS_ENDPOINT_RENDER_ICON, WINDOWS_ENDPOINT_CAPTURE_ICON);
        for sentinel in [WINDOWS_ENDPOINT_RENDER_ICON, WINDOWS_ENDPOINT_CAPTURE_ICON] {
            assert!(sentinel.starts_with("windows:"), "{sentinel} must stay in the Windows namespace so the UI never confuses it with an XDG icon name");
        }
    }

    #[test]
    fn no_icon_for_the_system_idle_process_id() {
        assert_eq!(process_executable_path(0), None);
        assert_eq!(session_icon_name(0), None);
    }

    #[test]
    fn no_icon_for_a_process_id_that_cannot_exist() {
        assert_eq!(process_executable_path(u32::MAX), None);
    }

    #[test]
    fn current_process_exposes_its_own_executable_for_icons() {
        let path = process_executable_path(std::process::id())
            .expect("the test's own process must resolve an image path");
        assert!(
            std::path::Path::new(&path).is_file(),
            "session icon reference must be an extractable file, got {path:?}"
        );
    }
}
