#pragma once

// This header is intentionally tiny. The build script supplies the versioned
// WDK include roots; ACX's public umbrella header supplies the exact
// configuration layouts, callback typedefs, and DDI declarations.
#include <acx.h>
#include <wdf.h>
#include <wdm.h>

// Bindgen sees these as ordinary NTSTATUS-returning functions while the WDK
// compiler expands the version-correct ACX *_INIT macros inside the eWDK.
static inline NTSTATUS qpwgraph_acx_driver_initialize(void *driver) {
  ACX_DRIVER_CONFIG config;
  ACX_DRIVER_CONFIG_INIT(&config);
  return AcxDriverInitialize((WDFDRIVER)driver, &config);
}

static inline NTSTATUS qpwgraph_acx_device_init_initialize(void *device_init) {
  ACX_DEVICEINIT_CONFIG config;
  ACX_DEVICEINIT_CONFIG_INIT(&config);
  return AcxDeviceInitInitialize((PWDFDEVICE_INIT)device_init, &config);
}

static inline NTSTATUS qpwgraph_acx_device_initialize(void *device) {
  ACX_DEVICE_CONFIG config;
  ACX_DEVICE_CONFIG_INIT(&config);
  return AcxDeviceInitialize((WDFDEVICE)device, &config);
}

// These wrappers deliberately stop at the documented ACX object
// creation DDIs.  They are compile-time ABI probes for the opt-in binding
// job, not an endpoint implementation: a real circuit still needs its name,
// type, pins, formats, callbacks, and device registration before it may be
// exposed to Plug and Play.
static inline NTSTATUS qpwgraph_acx_circuit_binding_probe(void *device,
                                                          void **circuit) {
  PACXCIRCUIT_INIT circuit_init = AcxCircuitInitAllocate((WDFDEVICE)device);
  if (circuit_init == NULL) {
    return STATUS_INSUFFICIENT_RESOURCES;
  }

  NTSTATUS status = AcxCircuitCreate((WDFDEVICE)device, NULL, &circuit_init,
                                     (ACXCIRCUIT *)circuit);
  if (!NT_SUCCESS(status)) {
    AcxCircuitInitFree(circuit_init);
  }
  return status;
}

static inline NTSTATUS qpwgraph_acx_pin_binding_probe(void *circuit,
                                                      void **pin) {
  ACX_PIN_CONFIG config;
  ACX_PIN_CONFIG_INIT(&config);
  return AcxPinCreate((ACXCIRCUIT)circuit, NULL, &config, (ACXPIN *)pin);
}

static inline NTSTATUS
qpwgraph_acx_data_format_binding_probe(void *device, void **data_format) {
  ACX_DATAFORMAT_CONFIG config;
  ACX_DATAFORMAT_CONFIG_INIT(&config);
  return AcxDataFormatCreate((WDFDEVICE)device, NULL, &config,
                             (ACXDATAFORMAT *)data_format);
}

static inline NTSTATUS qpwgraph_acx_rt_stream_binding_probe(void *device,
                                                            void *circuit,
                                                            void *stream_init,
                                                            void **stream) {
  PACXSTREAM_INIT init = (PACXSTREAM_INIT)stream_init;
  ACX_STREAM_CALLBACKS stream_callbacks;
  ACX_STREAM_CALLBACKS_INIT(&stream_callbacks);

  NTSTATUS status =
      AcxStreamInitAssignAcxStreamCallbacks(init, &stream_callbacks);
  if (!NT_SUCCESS(status)) {
    return status;
  }

  ACX_RT_STREAM_CALLBACKS rt_callbacks;
  ACX_RT_STREAM_CALLBACKS_INIT(&rt_callbacks);
  status = AcxStreamInitAssignAcxRtStreamCallbacks(init, &rt_callbacks);
  if (!NT_SUCCESS(status)) {
    return status;
  }

  return AcxRtStreamCreate((WDFDEVICE)device, (ACXCIRCUIT)circuit, NULL, &init,
                           (ACXSTREAM *)stream);
}

// The first production ACX path is implemented in acx_bridge.c so the WDK
// macros, callback typedefs, and opaque object layouts stay on the C side of
// the FFI. Rust only receives the status from the complete device-add
// transaction; it never recreates an ACX structure layout.
NTSTATUS qpwgraph_acx_device_add(void *driver, void *device_init);
