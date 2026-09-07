#pragma once

// This header is the generated-binding boundary, not the driver runtime. It
// contains only WDK macro expansions and ACX function-table calls that
// bindgen cannot express directly. All state, callbacks, packet handling, and
// endpoint policy live in Rust.
#include <wdm.h>
#include <windef.h>
#define NOBITMAP
#include <ks.h>
#include <mmreg.h>
#include <ksmedia.h>
#include <ntddk.h>
#include <ntstrsafe.h>
#include <ntintsafe.h>
#include <wdf.h>
#include <acx.h>

static inline NTSTATUS qpwgraph_acx_driver_initialize(
    void *driver, PACX_DRIVER_CONFIG config) {
  return AcxDriverInitialize((WDFDRIVER)driver, config);
}

static inline NTSTATUS qpwgraph_acx_device_init_initialize(
    void *device_init, PACX_DEVICEINIT_CONFIG config) {
  return AcxDeviceInitInitialize((PWDFDEVICE_INIT)device_init, config);
}

static inline NTSTATUS qpwgraph_acx_device_initialize(
    void *device, PACX_DEVICE_CONFIG config) {
  return AcxDeviceInitialize((WDFDEVICE)device, config);
}

// Keep every versioned *_INIT macro at the generated-binding boundary. Rust
// owns the values assigned after initialization, but it must not reproduce a
// WDK default, reserved field, or structure-size convention by hand.
static inline void qpwgraph_acx_driver_config_init(PACX_DRIVER_CONFIG config) {
  ACX_DRIVER_CONFIG_INIT(config);
}

static inline void qpwgraph_acx_device_init_config_init(
    PACX_DEVICEINIT_CONFIG config) {
  ACX_DEVICEINIT_CONFIG_INIT(config);
}

static inline void qpwgraph_acx_device_config_init(PACX_DEVICE_CONFIG config) {
  ACX_DEVICE_CONFIG_INIT(config);
}

static inline void qpwgraph_acx_circuit_power_callbacks_init(
    PACX_CIRCUIT_PNPPOWER_CALLBACKS callbacks) {
  ACX_CIRCUIT_PNPPOWER_CALLBACKS_INIT(callbacks);
}

static inline void qpwgraph_acx_pin_config_init(PACX_PIN_CONFIG config) {
  ACX_PIN_CONFIG_INIT(config);
}

static inline void qpwgraph_acx_jack_config_init(PACX_JACK_CONFIG config) {
  ACX_JACK_CONFIG_INIT(config);
}

static inline void qpwgraph_acx_stream_callbacks_init(
    PACX_STREAM_CALLBACKS callbacks) {
  ACX_STREAM_CALLBACKS_INIT(callbacks);
}

static inline void qpwgraph_acx_rt_stream_callbacks_init(
    PACX_RT_STREAM_CALLBACKS callbacks) {
  ACX_RT_STREAM_CALLBACKS_INIT(callbacks);
}

static inline void qpwgraph_acx_rt_packet_init(PACX_RTPACKET packet) {
  ACX_RTPACKET_INIT(packet);
}

static inline PACXCIRCUIT_INIT qpwgraph_acx_circuit_init_allocate(
    void *device) {
  return AcxCircuitInitAllocate((WDFDEVICE)device);
}

static inline void qpwgraph_acx_circuit_init_free(PACXCIRCUIT_INIT init) {
  AcxCircuitInitFree(init);
}

static inline void qpwgraph_acx_circuit_init_set_component_id(
    PACXCIRCUIT_INIT init, const GUID *component_id) {
  AcxCircuitInitSetComponentId(init, component_id);
}

static inline NTSTATUS qpwgraph_acx_circuit_init_assign_name(
    PACXCIRCUIT_INIT init, const UNICODE_STRING *name) {
  return AcxCircuitInitAssignName(init, name);
}

static inline void qpwgraph_acx_circuit_init_set_type(
    PACXCIRCUIT_INIT init, ACX_CIRCUIT_TYPE type) {
  AcxCircuitInitSetCircuitType(init, type);
}

static inline void qpwgraph_acx_circuit_init_set_power_callbacks(
    PACXCIRCUIT_INIT init, PACX_CIRCUIT_PNPPOWER_CALLBACKS callbacks) {
  AcxCircuitInitSetAcxCircuitPnpPowerCallbacks(init, callbacks);
}

static inline NTSTATUS qpwgraph_acx_circuit_init_set_stream_callback(
    PACXCIRCUIT_INIT init, PFN_ACX_CIRCUIT_CREATE_STREAM callback) {
  return AcxCircuitInitAssignAcxCreateStreamCallback(init, callback);
}

static inline NTSTATUS qpwgraph_acx_circuit_create(
    void *device, PACXCIRCUIT_INIT *init, ACXCIRCUIT *circuit) {
  WDF_OBJECT_ATTRIBUTES attributes;
  WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
  return AcxCircuitCreate((WDFDEVICE)device, &attributes, init, circuit);
}

static inline NTSTATUS qpwgraph_acx_device_add_circuit(
    void *device, ACXCIRCUIT circuit) {
  return AcxDeviceAddCircuit((WDFDEVICE)device, circuit);
}

static inline NTSTATUS qpwgraph_acx_device_remove_circuit(
    void *device, ACXCIRCUIT circuit) {
  return AcxDeviceRemoveCircuit((WDFDEVICE)device, circuit);
}

static inline NTSTATUS qpwgraph_acx_circuit_add_pins(
    ACXCIRCUIT circuit, ACXPIN *pins, ULONG count) {
  return AcxCircuitAddPins(circuit, pins, count);
}

static inline NTSTATUS qpwgraph_acx_pin_create(
    ACXCIRCUIT circuit, PACX_PIN_CONFIG config, ACXPIN *pin) {
  WDF_OBJECT_ATTRIBUTES attributes;
  WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
  attributes.ParentObject = (WDFOBJECT)circuit;
  return AcxPinCreate(circuit, &attributes, config, pin);
}

static inline NTSTATUS qpwgraph_acx_pin_add_jacks(
    ACXPIN pin, ACXJACK *jacks, ULONG count) {
  return AcxPinAddJacks(pin, jacks, count);
}

static inline ACXDATAFORMATLIST qpwgraph_acx_pin_get_raw_format_list(
    ACXPIN pin) {
  return AcxPinGetRawDataFormatList(pin);
}

static inline NTSTATUS qpwgraph_acx_jack_create(
    ACXPIN pin, PACX_JACK_CONFIG config, ACXJACK *jack) {
  WDF_OBJECT_ATTRIBUTES attributes;
  WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
  attributes.ParentObject = (WDFOBJECT)pin;
  return AcxJackCreate(pin, &attributes, config, jack);
}

static inline NTSTATUS qpwgraph_acx_data_format_create(
    void *device, void *parent, PACX_DATAFORMAT_CONFIG config,
    ACXDATAFORMAT *format) {
  WDF_OBJECT_ATTRIBUTES attributes;
  WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
  attributes.ParentObject = (WDFOBJECT)parent;
  return AcxDataFormatCreate((WDFDEVICE)device, &attributes, config, format);
}

static inline NTSTATUS qpwgraph_acx_data_format_list_add(
    ACXDATAFORMATLIST list, ACXDATAFORMAT format) {
  return AcxDataFormatListAddDataFormat(list, format);
}

// The KS format is an exact WDK layout and is deliberately initialized by
// the WDK macro. Rust owns the endpoint policy and all runtime state; this
// helper keeps only the compiler-specific KS structure initialization at the
// generated-binding boundary.
static inline void qpwgraph_acx_pcm_format_config_init(
    PACX_DATAFORMAT_CONFIG config) {
  static const KSDATAFORMAT_WAVEFORMATEXTENSIBLE pcm_format = {
      {sizeof(KSDATAFORMAT_WAVEFORMATEXTENSIBLE), 0, 0, 0,
       STATICGUIDOF(KSDATAFORMAT_TYPE_AUDIO),
       STATICGUIDOF(KSDATAFORMAT_SUBTYPE_PCM),
       STATICGUIDOF(KSDATAFORMAT_SPECIFIER_WAVEFORMATEX)},
      {{WAVE_FORMAT_EXTENSIBLE, 2, 48000, 192000, 4, 16,
        sizeof(WAVEFORMATEXTENSIBLE) - sizeof(WAVEFORMATEX)},
       16,
       KSAUDIO_SPEAKER_STEREO,
       STATICGUIDOF(KSDATAFORMAT_SUBTYPE_PCM)}};
  ACX_DATAFORMAT_CONFIG_INIT_KS(config, (PVOID)&pcm_format);
}

static inline ULONG qpwgraph_acx_data_format_average_bytes_per_sec(
    ACXDATAFORMAT format) {
  return AcxDataFormatGetAverageBytesPerSec(format);
}

static inline ULONG qpwgraph_acx_data_format_block_align(
    ACXDATAFORMAT format) {
  return AcxDataFormatGetBlockAlign(format);
}

static inline NTSTATUS qpwgraph_acx_stream_init_set_callbacks(
    PACXSTREAM_INIT init, PACX_STREAM_CALLBACKS callbacks) {
  return AcxStreamInitAssignAcxStreamCallbacks(init, callbacks);
}

static inline NTSTATUS qpwgraph_acx_stream_init_set_rt_callbacks(
    PACXSTREAM_INIT init, PACX_RT_STREAM_CALLBACKS callbacks) {
  return AcxStreamInitAssignAcxRtStreamCallbacks(init, callbacks);
}

static inline void qpwgraph_acx_stream_init_enable_notifications(
    PACXSTREAM_INIT init) {
  AcxStreamInitSetAcxRtStreamSupportsNotifications(init);
}

static inline NTSTATUS qpwgraph_acx_rt_stream_create(
    void *device, ACXCIRCUIT circuit, void *stream_init,
    void *destroy_callback, ACXSTREAM *stream) {
  WDF_OBJECT_ATTRIBUTES attributes;
  PACXSTREAM_INIT init = (PACXSTREAM_INIT)stream_init;
  WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
  attributes.EvtDestroyCallback =
      (PFN_WDF_OBJECT_CONTEXT_DESTROY)destroy_callback;
  return AcxRtStreamCreate((WDFDEVICE)device, circuit, &attributes,
                           &init, stream);
}

static inline NTSTATUS qpwgraph_acx_rt_stream_notify_packet_complete(
    ACXSTREAM stream, ULONGLONG packet, ULONGLONG qpc_position) {
  return AcxRtStreamNotifyPacketComplete(stream, packet, qpc_position);
}

// KMDF macro glue. The Rust side passes opaque callback pointers so the
// generated binding does not need to recreate WDF object-attribute layouts.
static inline NTSTATUS qpwgraph_wdf_device_create(
    void *device_init, void *prepare_hardware, void *release_hardware,
    void *d0_entry, void *d0_exit, void **device) {
  WDF_PNPPOWER_EVENT_CALLBACKS callbacks;
  WDF_OBJECT_ATTRIBUTES attributes;
  WDFDEVICE wdf_device = NULL;

  WDF_PNPPOWER_EVENT_CALLBACKS_INIT(&callbacks);
  callbacks.EvtDevicePrepareHardware =
      (PFN_WDF_DEVICE_PREPARE_HARDWARE)prepare_hardware;
  callbacks.EvtDeviceReleaseHardware =
      (PFN_WDF_DEVICE_RELEASE_HARDWARE)release_hardware;
  callbacks.EvtDeviceD0Entry = (PFN_WDF_DEVICE_D0_ENTRY)d0_entry;
  callbacks.EvtDeviceD0Exit = (PFN_WDF_DEVICE_D0_EXIT)d0_exit;
  WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
  PWDFDEVICE_INIT init = (PWDFDEVICE_INIT)device_init;
  WdfDeviceInitSetPnpPowerEventCallbacks(init, &callbacks);
  NTSTATUS status = WdfDeviceCreate(&init, &attributes, &wdf_device);
  if (NT_SUCCESS(status) && device != NULL) {
    *device = (void *)wdf_device;
  }
  return status;
}

static inline NTSTATUS qpwgraph_wdf_timer_create(
    void *parent, void *timer_callback, void **timer) {
  WDF_TIMER_CONFIG config;
  WDF_OBJECT_ATTRIBUTES attributes;
  WDFTIMER wdf_timer = NULL;

  WDF_TIMER_CONFIG_INIT(&config, (PFN_WDF_TIMER)timer_callback);
  config.AutomaticSerialization = TRUE;
  config.UseHighResolutionTimer = WdfTrue;
  config.Period = 0;
  WDF_OBJECT_ATTRIBUTES_INIT(&attributes);
  attributes.ParentObject = (WDFOBJECT)parent;
  NTSTATUS status = WdfTimerCreate(&config, &attributes, &wdf_timer);
  if (NT_SUCCESS(status) && timer != NULL) {
    *timer = (void *)wdf_timer;
  }
  return status;
}

static inline BOOLEAN qpwgraph_wdf_timer_start(void *timer, LONGLONG due_time) {
  return WdfTimerStart((WDFTIMER)timer, due_time);
}

static inline BOOLEAN qpwgraph_wdf_timer_stop(void *timer, BOOLEAN wait) {
  return WdfTimerStop((WDFTIMER)timer, wait);
}

static inline void qpwgraph_wdf_object_delete(void *object) {
  WdfObjectDelete((WDFOBJECT)object);
}

static inline void qpwgraph_wdf_memory_descriptor_init_mdl(
    void *descriptor, void *mdl, ULONG length) {
  WDF_MEMORY_DESCRIPTOR_INIT_MDL((PWDF_MEMORY_DESCRIPTOR)descriptor,
                                 (PMDL)mdl, length);
}

static inline ULONGLONG qpwgraph_acx_convert_performance_time(
    LONGLONG frequency, LONGLONG performance_time) {
  LARGE_INTEGER counter;
  counter.QuadPart = performance_time;
  return KSCONVERT_PERFORMANCE_TIME(frequency, counter);
}
