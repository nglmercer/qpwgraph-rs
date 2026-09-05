//! Windows Core Audio backend.
//!
//! Core Audio exposes endpoint and application-session state, but it does not
//! expose PipeWire's arbitrary patchbay graph. So the graph here has two kinds
//! of link in it, and telling them apart is the whole design:
//!
//! * **observed** — an application session and the endpoint Windows says it is
//!   playing to. Visible, selectable, and immutable, because Windows offers no
//!   supported way to move one.
//! * **carried** — a route between two endpoint ports that qpwgraph opened
//!   WASAPI streams for and is moving the PCM through itself. Mutable, because
//!   qpwgraph owns it.
//!
//! All COM interfaces stay on the worker thread; the public driver
//! communicates with that thread through owned commands and snapshots.
//!
//! | Module | Owns |
//! | --- | --- |
//! | [`driver`] | the public `GraphDriver`, which owns no COM pointer |
//! | [`worker`] | the Core Audio thread: enumeration, meters, the graph |
//! | [`callbacks`] | the COM notification sinks Core Audio calls back on |
//! | [`identity`] | stable graph ids derived from Core Audio's strings |
//! | [`routing`] | the links qpwgraph carries, and the audio behind them |

use super::api::{
    AudioMeter, BackendCapabilities, BackendError, BackendResult, GraphDriver, MeterPolicy,
    NodeAudioState, NodeCapabilities, UNITY_VOLUME,
};
use pw_graph_core::{
    encode_backend_id, BackendNamespace, Direction, Graph, GraphError, Link, LinkId, Node, NodeId,
    NodeType, Port, PortId, PortKey, PortType, LOCAL_ID_MASK,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
#[cfg(feature = "relay")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use windows::core::{Interface, GUID, PCWSTR, PWSTR};
use windows::Win32::Devices::Properties;
use windows::Win32::Foundation::{CloseHandle, PROPERTYKEY};
use windows::Win32::Media::Audio;
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioMeterInformation};
use windows::Win32::System::Com::{
    self, StructuredStorage, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows_core::BOOL;

pub mod app_route_policy;
mod app_route_reconciler;
mod callbacks;
mod driver;
mod effects;
mod identity;
mod process_capture;
pub mod process_loopback;
mod routing;
pub mod virtual_device;
mod worker;

#[cfg(test)]
mod tests;

// One 2,100-line file before; `pub(super)` keeps the reach a bare item had
// there, which is private to `windows`.
#[cfg(feature = "relay")]
pub(crate) use self::app_route_policy::verify_live_process_identity;
pub use self::app_route_policy::{
    AppRoutePolicy, AppRoutePolicySupport, AudioFlow, AudioRole, ProcessIdentity,
    UnsupportedAppRoutePolicy,
};
pub use self::app_route_reconciler::{
    ApplicationRouteActivation, ApplicationRouteCandidate, ApplicationRouteEnvironment,
    ApplicationRoutePlan, ApplicationRouteReconciler, ApplicationRouteState,
    ProcessCaptureReadiness,
};
use self::callbacks::*;
pub use self::driver::WindowsAudioDriver;
use self::driver::*;
use self::effects::*;
#[cfg(feature = "relay")]
pub(crate) use self::identity::find_qpwgraph_endpoint;
pub use self::identity::WindowsEndpointSelector;
use self::identity::*;
pub use self::process_capture::{
    ProcessCaptureConsumer, ProcessCaptureKey, ProcessCaptureManager, ProcessCaptureRequest,
    ProcessCaptureState, ProcessCaptureStatus, ProcessMeterReading, ProcessMeterTarget,
};
pub use self::process_loopback::{
    ProcessLoopbackCapability, ProcessLoopbackMode, ProcessLoopbackSource,
};
use self::routing::*;
pub use self::virtual_device::{
    classify_driver_owned_endpoint, classify_virtual_endpoint, QpwVirtualEndpointIdentity,
    QpwVirtualEndpointRole, VirtualAudioDriverHealth,
};
use self::worker::*;

/// Capabilities of a Windows application session are intentionally split by
/// operation. Process-loopback capture is read-only and does not prove that
/// qpwgraph may rerender the application locally; only an application already
/// isolated on QPWGraph Virtual Output gets the latter capabilities.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcessAudioCapabilities {
    pub capture_readonly: bool,
    pub relay_source: bool,
    pub meter_peak: bool,
    pub meter_rms: bool,
    pub mutable_route: bool,
    pub effects: bool,
}

impl ProcessAudioCapabilities {
    pub const fn capture_only() -> Self {
        Self {
            capture_readonly: true,
            relay_source: true,
            meter_peak: true,
            meter_rms: true,
            mutable_route: false,
            effects: false,
        }
    }

    pub const fn isolated() -> Self {
        Self {
            capture_readonly: true,
            relay_source: true,
            meter_peak: true,
            meter_rms: true,
            mutable_route: true,
            effects: true,
        }
    }
}
