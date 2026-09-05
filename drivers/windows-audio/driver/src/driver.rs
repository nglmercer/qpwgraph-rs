//! KMDF bootstrap for the ACX virtual audio adapter.
//!
//! Device/circuit creation is intentionally a separate milestone. Until the
//! ACX circuit is implemented, `EvtDeviceAdd` rejects the device so the
//! development package cannot claim to expose an audio endpoint it does not
//! yet own.

use wdk::{nt_success, paged_code};
use wdk_sys::{
    call_unsafe_wdf_function_binding, DRIVER_OBJECT, NTSTATUS, PCUNICODE_STRING,
    PDRIVER_OBJECT, PWDFDEVICE_INIT, STATUS_NOT_SUPPORTED, STATUS_SUCCESS, WDFDRIVER,
    WDF_DRIVER_CONFIG,
};

const WDF_DRIVER_CONFIG_SIZE: u32 = core::mem::size_of::<WDF_DRIVER_CONFIG>() as u32;

/// Required kernel entry point. WDF owns the driver object after this returns.
#[link_section = "INIT"]
#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    driver: &mut DRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    let mut config = WDF_DRIVER_CONFIG {
        Size: WDF_DRIVER_CONFIG_SIZE,
        EvtDriverDeviceAdd: Some(evt_device_add),
        ..WDF_DRIVER_CONFIG::default()
    };
    let mut handle = core::ptr::null_mut::<WDFDRIVER>();
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            driver as PDRIVER_OBJECT,
            registry_path,
            core::ptr::null_mut(),
            &raw mut config,
            &raw mut handle,
        )
    };
    if !nt_success(status) {
        return status;
    }
    STATUS_SUCCESS
}

/// ACX adapter creation will replace this callback in the next driver stage.
#[link_section = "PAGE"]
extern "C" fn evt_device_add(
    _driver: WDFDRIVER,
    _device_init: PWDFDEVICE_INIT,
) -> NTSTATUS {
    paged_code!();
    STATUS_NOT_SUPPORTED
}
