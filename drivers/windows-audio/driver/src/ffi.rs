//! Generated ACX declarations and fail-closed non-ACX placeholders.
//!
//! The feature build includes the exact types generated from the selected WDK
//! headers. The only handwritten C is the macro/function-table glue in
//! `acx_wrapper.h`; no ACX layout is recreated here.

#[cfg(not(feature = "acx"))]
use core::ffi::c_void;
#[cfg(not(feature = "acx"))]
use wdk_sys::{NTSTATUS, PWDFDEVICE_INIT, WDFDEVICE};

#[cfg(feature = "acx")]
#[allow(
    clippy::all,
    dead_code,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    unnecessary_transmutes,
    unused_imports,
    unused_unsafe
)]
mod generated {
    include!(env!("QPWGRAPH_ACX_BINDINGS"));
}

#[cfg(feature = "acx")]
pub use generated::*;

/// Opaque ACX handles. Their concrete definitions belong to `acx.h` and are
/// intentionally not recreated in Rust.
#[cfg(not(feature = "acx"))]
#[allow(clippy::upper_case_acronyms, dead_code)]
pub type ACXDEVICE = *mut c_void;
#[cfg(not(feature = "acx"))]
#[allow(clippy::upper_case_acronyms, dead_code)]
pub type ACXCIRCUIT = *mut c_void;
#[cfg(not(feature = "acx"))]
#[allow(clippy::upper_case_acronyms, dead_code)]
pub type ACXPIN = *mut c_void;
#[cfg(not(feature = "acx"))]
#[allow(clippy::upper_case_acronyms, dead_code)]
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
