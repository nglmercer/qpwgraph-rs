//! Narrow ACX bootstrap boundary.
//!
//! This module is deliberately conservative: ACX is a WDK class extension,
//! not a stable `wdk-sys` surface. The default build remains fail-closed, and
//! the opt-in `acx` feature is the only place where generated eWDK bindings
//! may be connected. The opaque object setup and realtime callbacks remain in
//! the C bridge; Rust owns the bounded PCM transport.

#[cfg(feature = "acx")]
use crate::ffi;
#[cfg(feature = "acx")]
use core::ffi::c_void;
use wdk_sys::{NTSTATUS, PWDFDEVICE_INIT, STATUS_NOT_SUPPORTED, WDFDEVICE, WDFDRIVER};

/// The four binding milestones that must be proven before a virtual endpoint
/// is allowed to advertise itself to Windows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcxBindingMilestone {
    DeviceInitialization,
    CircuitCreation,
    PinFormatConfiguration,
    StreamCallbackConfiguration,
}

/// Current binding gate. The opt-in feature generates the isolated ACX header
/// bindings and macro wrappers. It is still an opt-in validation build: the
/// package remains fail-closed until a test-signed Windows pass proves it.
pub const fn bindings_generated() -> bool {
    cfg!(feature = "acx")
}

/// The generated header bindings are not the same thing as a validated
/// endpoint implementation. Keep this false until device, circuit, pin, and
/// stream wrappers have passed on a test-signed Windows image.
pub const fn binding_available() -> bool {
    false
}

/// Run the ACX driver initializer through the eWDK-generated wrapper.
///
/// This stays separate from the production endpoint path, but it makes the
/// binding milestone exercise the real ACX initializer macros instead of only
/// declaring unused symbols.
pub unsafe fn initialize_driver(driver: WDFDRIVER) -> NTSTATUS {
    #[cfg(feature = "acx")]
    {
        return unsafe { ffi::qpwgraph_acx_driver_initialize(driver.cast()) };
    }
    #[cfg(not(feature = "acx"))]
    {
        let _ = driver;
        STATUS_NOT_SUPPORTED
    }
}

/// Initialize the ACX device after `WdfDeviceCreate` has succeeded.
///
/// The default path is intentionally `STATUS_NOT_SUPPORTED`. With the
/// feature enabled this function is the single place where the generated
/// `AcxDeviceInitialize` binding is called; the feature remains an eWDK
/// validation target until the ABI is proven on Windows.
pub unsafe fn initialize_device(device: WDFDEVICE) -> NTSTATUS {
    #[cfg(feature = "acx")]
    {
        return unsafe { ffi::qpwgraph_acx_device_initialize(device.cast()) };
    }
    #[cfg(not(feature = "acx"))]
    {
        let _ = device;
        STATUS_NOT_SUPPORTED
    }
}

/// Initialize ACX requirements on a child/circuit device-init object once the
/// generated `ACX_DEVICEINIT_CONFIG` layout is available.
pub unsafe fn initialize_device_init(device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    #[cfg(feature = "acx")]
    {
        return unsafe { ffi::qpwgraph_acx_device_init_initialize(device_init.cast()) };
    }
    #[cfg(not(feature = "acx"))]
    {
        let _ = device_init;
        STATUS_NOT_SUPPORTED
    }
}

/// Create the ACX app and relay endpoint pairs behind the opt-in feature.
///
/// The C bridge owns the WDK/ACX callback structs and opaque handles. It
/// initializes the device, creates both circuits and pins, registers the
/// shared-mode format, and connects the RT stream callbacks before returning
/// to KMDF. The default build still returns `STATUS_NOT_SUPPORTED`.
pub unsafe fn add_device(driver: WDFDRIVER, device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    #[cfg(feature = "acx")]
    {
        return unsafe { ffi::qpwgraph_acx_device_add(driver.cast(), device_init.cast()) };
    }
    #[cfg(not(feature = "acx"))]
    {
        let _ = (driver, device_init);
        STATUS_NOT_SUPPORTED
    }
}

/// Compile-only ABI probe for the documented circuit creation surface.
///
/// The probe is intentionally not called by `EvtDeviceAdd`: it creates an
/// otherwise incomplete circuit and therefore must not be used as an
/// endpoint. The production path still needs circuit metadata, pins, and
/// registration with the ACX device.
#[cfg(feature = "acx")]
pub unsafe fn circuit_binding_probe(device: WDFDEVICE, circuit: *mut *mut c_void) -> NTSTATUS {
    unsafe { ffi::qpwgraph_acx_circuit_binding_probe(device.cast(), circuit) }
}

/// Compile-only ABI probe for `AcxPinCreate` and `ACX_PIN_CONFIG_INIT`.
#[cfg(feature = "acx")]
pub unsafe fn pin_binding_probe(circuit: *mut c_void, pin: *mut *mut c_void) -> NTSTATUS {
    unsafe { ffi::qpwgraph_acx_pin_binding_probe(circuit, pin) }
}

/// Compile-only ABI probe for the ACX data-format configuration surface.
#[cfg(feature = "acx")]
pub unsafe fn data_format_binding_probe(
    device: WDFDEVICE,
    data_format: *mut *mut c_void,
) -> NTSTATUS {
    unsafe { ffi::qpwgraph_acx_data_format_binding_probe(device.cast(), data_format) }
}

/// Compile-only ABI probe for the basic and RT stream callback assignment
/// surface plus `AcxRtStreamCreate`.
#[cfg(feature = "acx")]
pub unsafe fn rt_stream_binding_probe(
    device: WDFDEVICE,
    circuit: *mut c_void,
    stream_init: *mut c_void,
    stream: *mut *mut c_void,
) -> NTSTATUS {
    unsafe {
        ffi::qpwgraph_acx_rt_stream_binding_probe(device.cast(), circuit, stream_init, stream)
    }
}
