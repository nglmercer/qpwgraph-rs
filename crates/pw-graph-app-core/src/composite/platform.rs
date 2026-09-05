//! Accessors that exist on only one platform: the Windows relay endpoint
//! selection, and the MIDI child Windows polls for device arrival.

use super::*;

impl CompositeDriver {
    /// Return the Windows-only audio diagnostics report without exposing the
    /// native driver object to the UI layer.  The report is text-only and
    /// bounded by the backend, so callers can safely place it on a clipboard
    /// or attach it to a support ticket.
    #[cfg(target_os = "windows")]
    pub fn windows_audio_report(&self) -> String {
        self.windows_audio
            .as_ref()
            .map(WindowsAudioDriver::windows_audio_report)
            .unwrap_or_else(|| "qpwgraph Windows audio backend unavailable\n".into())
    }

    /// Safe per-application routing capability.  The current backend only
    /// exposes the documented manual Volume Mixer fallback; keeping the
    /// capability on the composite lets the UI explain that state without
    /// probing an undocumented ABI.
    #[cfg(target_os = "windows")]
    pub fn windows_app_route_policy_support(&self) -> pw_graph_backend::AppRoutePolicySupport {
        self.windows_audio
            .as_ref()
            .map(WindowsAudioDriver::app_route_policy_support)
            .unwrap_or_else(|| pw_graph_backend::AppRoutePolicySupport::ManualOnly {
                reason: "Windows audio backend is unavailable".into(),
            })
    }

    #[cfg(target_os = "windows")]
    pub fn has_windows_midi(&self) -> bool {
        self.windows_midi.is_some()
    }

    #[cfg(all(target_os = "windows", feature = "relay"))]
    pub fn windows_relay_endpoint_choices(&self) -> Vec<(String, String)> {
        self.windows_audio
            .as_ref()
            .map(|driver| driver.relay_endpoint_choices())
            .unwrap_or_default()
    }

    #[cfg(all(target_os = "windows", feature = "relay"))]
    pub fn windows_relay_endpoints(&self) -> pw_graph_backend::RelayEndpoints {
        self.windows_audio
            .as_ref()
            .map(|driver| driver.relay_endpoints().clone())
            .unwrap_or_default()
    }

    #[cfg(all(target_os = "windows", feature = "relay"))]
    pub fn set_windows_relay_endpoints(
        &mut self,
        endpoints: pw_graph_backend::RelayEndpoints,
    ) -> BackendResult<()> {
        self.windows_audio
            .as_mut()
            .ok_or_else(|| Self::unsupported("Windows audio backend is unavailable"))?
            .set_relay_endpoints(endpoints)
    }
}
