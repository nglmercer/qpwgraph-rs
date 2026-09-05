//! Minimal raw ACX declarations.
//!
//! `wdk-sys` exposes KMDF but not the Audio Class Extensions headers.  Keep
//! the ABI boundary opaque until it is generated and verified inside the eWDK
//! environment; no guessed structure layout is allowed to cross this module.

#[cfg(not(feature = "acx"))]
use core::ffi::c_void;
#[cfg(not(feature = "acx"))]
use wdk_sys::{NTSTATUS, PWDFDEVICE_INIT, WDFDEVICE};

#[cfg(feature = "acx")]
include!(env!("QPWGRAPH_ACX_BINDINGS"));

/// Opaque ACX handles. Their concrete definitions belong to `acx.h` and are
/// intentionally not recreated in Rust.
#[cfg(not(feature = "acx"))]
pub type ACXDEVICE = *mut c_void;
#[cfg(not(feature = "acx"))]
pub type ACXCIRCUIT = *mut c_void;
#[cfg(not(feature = "acx"))]
pub type ACXPIN = *mut c_void;
#[cfg(not(feature = "acx"))]
pub type ACXSTREAM = *mut c_void;

/// The ACX configuration structures are opaque here for the same reason. The
/// eWDK binding generator must provide their exact size/alignment before these
/// declarations are enabled for a production endpoint.
#[repr(C)]
#[cfg(not(feature = "acx"))]
pub struct ACX_DEVICE_CONFIG_BINDING {
    _private: [u8; 0],
}

#[cfg(not(feature = "acx"))]
const _: Option<
    unsafe extern "system" fn(WDFDEVICE, *const ACX_DEVICE_CONFIG_BINDING) -> NTSTATUS,
> = None;

#[cfg(not(feature = "acx"))]
const _: Option<unsafe extern "system" fn(PWDFDEVICE_INIT, *const c_void) -> NTSTATUS> = None;
