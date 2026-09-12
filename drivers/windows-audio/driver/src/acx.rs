//! Rust-owned ACX endpoint runtime.
//!
//! ACX exposes a versioned function-table API and a collection of WDK
//! initialization macros. The generated bindings and the small
//! `acx_wrapper.h` translation unit contain that ABI surface; this module
//! owns the endpoint graph, stream state, packet allocation, timer scheduling,
//! power callbacks, and the real-time transport policy.

use core::ffi::c_void;

use wdk_sys::{NTSTATUS, PWDFDEVICE_INIT, STATUS_NOT_SUPPORTED, WDFDEVICE, WDFDRIVER};
#[cfg(feature = "acx")]
use wdk_sys::{STATUS_INVALID_PARAMETER, STATUS_SUCCESS};

/// The four binding milestones that must be proven before a virtual endpoint
/// is allowed to advertise itself to Windows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub enum AcxBindingMilestone {
    DeviceInitialization,
    CircuitCreation,
    PinFormatConfiguration,
    StreamCallbackConfiguration,
}

/// The opt-in feature generates bindings from the selected WDK ACX headers.
#[allow(dead_code)]
pub const fn bindings_generated() -> bool {
    cfg!(feature = "acx")
}

/// The feature now contains the complete Rust runtime. Release packaging is
/// still gated separately by the Windows/HLK/evidence workflow; compiling
/// this feature is not a substitute for those platform gates.
#[allow(dead_code)]
pub const fn binding_available() -> bool {
    cfg!(feature = "acx")
}

#[cfg(feature = "acx")]
mod runtime {
    use super::*;

    use core::mem::{size_of, size_of_val};
    use core::ptr;
    use core::sync::atomic::{
        AtomicBool, AtomicI32, AtomicI64, AtomicPtr, AtomicU32, AtomicU64, AtomicU8, Ordering,
    };

    use crate::{ffi, transport};
    use ffi::_ACX_CIRCUIT_TYPE::{AcxCircuitTypeCapture, AcxCircuitTypeRender};
    use ffi::_ACX_JACK_CONNECTION_TYPE::AcxConnTypeAtapiInternal;
    use ffi::_ACX_JACK_GEN_LOCATION::AcxGenLocPrimaryBox;
    use ffi::_ACX_JACK_GEO_LOCATION::AcxGeoLocFront;
    use ffi::_ACX_JACK_PORT_CONNECTION::AcxPortConnIntegratedDevice;
    use ffi::_ACX_PIN_COMMUNICATION::{AcxPinCommunicationNone, AcxPinCommunicationSink};
    use ffi::_ACX_PIN_TYPE::{AcxPinTypeSink, AcxPinTypeSource};
    use ffi::_WDF_EXECUTION_LEVEL::WdfExecutionLevelPassive;
    use ffi::_WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeNone;
    use wdk_sys::ntddk::{
        ExAllocatePool2, ExFreePool, IoAllocateMdl, IoFreeMdl, KeFlushQueuedDpcs,
        KeQueryPerformanceCounter, MmBuildMdlForNonPagedPool,
    };

    const MAX_STREAMS: usize = 8;
    const MAX_PACKET_COUNT: u32 = 2;
    const MAX_PACKET_BYTES: u32 = transport::MAX_PCM16_BYTES;
    const HNS_PER_SEC: u64 = 10_000_000;
    const EOS_FLAG: u32 = 0x0000_0200;
    const DRIVER_TAG: ffi::ULONG = 0x5157_5061;
    const SPEAKER_FRONT_LEFT: ffi::ULONG = 0x1;
    const SPEAKER_FRONT_RIGHT: ffi::ULONG = 0x2;
    const APP_CABLE: u8 = 0;
    const RELAY_CABLE: u8 = 1;

    const KSCATEGORY_AUDIO: ffi::GUID = ffi::GUID {
        Data1: 0x6994_AD04,
        Data2: 0x93EF,
        Data3: 0x11D0,
        Data4: [0xA3, 0xCC, 0x00, 0xA0, 0xC9, 0x22, 0x31, 0x96],
    };
    const KSNODETYPE_MICROPHONE: ffi::GUID = ffi::GUID {
        Data1: 0xDFF2_1BE1,
        Data2: 0xF70F,
        Data3: 0x11D0,
        Data4: [0xB9, 0x17, 0x00, 0xA0, 0xC9, 0x22, 0x31, 0x96],
    };
    const KSNODETYPE_SPEAKER: ffi::GUID = ffi::GUID {
        Data1: 0xDFF2_1CE1,
        Data2: 0xF70F,
        Data3: 0x11D0,
        Data4: [0xB9, 0x17, 0x00, 0xA0, 0xC9, 0x22, 0x31, 0x96],
    };
    const RENDER_COMPONENT: ffi::GUID = ffi::GUID {
        Data1: 0x9F1D_8D45,
        Data2: 0x3F0B,
        Data3: 0x4B8E,
        Data4: [0x8A, 0x92, 0x6E, 0x22, 0x4D, 0x9B, 0xA1, 0x70],
    };
    const CAPTURE_COMPONENT: ffi::GUID = ffi::GUID {
        Data1: 0x4C4E_51A2,
        Data2: 0x2E1A,
        Data3: 0x4C68,
        Data4: [0x9F, 0x71, 0x31, 0x7C, 0x8F, 0x4D, 0x2A, 0x06],
    };
    const RELAY_RENDER_COMPONENT: ffi::GUID = ffi::GUID {
        Data1: 0x2A2A_6D04,
        Data2: 0x5D8C,
        Data3: 0x49A0,
        Data4: [0x9C, 0x65, 0x70, 0x1D, 0x6E, 0x5A, 0x21, 0x83],
    };
    const RELAY_CAPTURE_COMPONENT: ffi::GUID = ffi::GUID {
        Data1: 0x7F6E_5B11,
        Data2: 0x0A46,
        Data3: 0x4E7A,
        Data4: [0x8A, 0x2F, 0x49, 0x3C, 0x2D, 0x84, 0xB7, 0x19],
    };

    // ACX copies this descriptor during circuit initialization. The backing
    // UTF-16 storage is static so the pointer remains valid for the call.
    const RENDER_NAME: &[u16] = &[
        b'Q' as u16,
        b'P' as u16,
        b'W' as u16,
        b'G' as u16,
        b'r' as u16,
        b'a' as u16,
        b'p' as u16,
        b'h' as u16,
        b'V' as u16,
        b'i' as u16,
        b'r' as u16,
        b't' as u16,
        b'u' as u16,
        b'a' as u16,
        b'l' as u16,
        b'O' as u16,
        b'u' as u16,
        b't' as u16,
        b'p' as u16,
        b'u' as u16,
        b't' as u16,
        0,
    ];
    const CAPTURE_NAME: &[u16] = &[
        b'Q' as u16,
        b'P' as u16,
        b'W' as u16,
        b'G' as u16,
        b'r' as u16,
        b'a' as u16,
        b'p' as u16,
        b'h' as u16,
        b'V' as u16,
        b'i' as u16,
        b'r' as u16,
        b't' as u16,
        b'u' as u16,
        b'a' as u16,
        b'l' as u16,
        b'M' as u16,
        b'o' as u16,
        b'n' as u16,
        b'i' as u16,
        b't' as u16,
        b'o' as u16,
        b'r' as u16,
        0,
    ];
    const RELAY_RENDER_NAME: &[u16] = &[
        b'Q' as u16,
        b'P' as u16,
        b'W' as u16,
        b'G' as u16,
        b'r' as u16,
        b'a' as u16,
        b'p' as u16,
        b'h' as u16,
        b'R' as u16,
        b'e' as u16,
        b'l' as u16,
        b'a' as u16,
        b'y' as u16,
        b'S' as u16,
        b'i' as u16,
        b'n' as u16,
        b'k' as u16,
        0,
    ];
    const RELAY_CAPTURE_NAME: &[u16] = &[
        b'Q' as u16,
        b'P' as u16,
        b'W' as u16,
        b'G' as u16,
        b'r' as u16,
        b'a' as u16,
        b'p' as u16,
        b'h' as u16,
        b'R' as u16,
        b'e' as u16,
        b'l' as u16,
        b'a' as u16,
        b'y' as u16,
        b'M' as u16,
        b'i' as u16,
        b'c' as u16,
        b'r' as u16,
        b'o' as u16,
        b'p' as u16,
        b'h' as u16,
        b'o' as u16,
        b'n' as u16,
        b'e' as u16,
        0,
    ];

    struct DeviceState {
        device: AtomicPtr<c_void>,
        circuits: [AtomicPtr<c_void>; 4],
        added: [AtomicBool; 4],
        active: [AtomicI32; 4],
    }

    impl DeviceState {
        const fn new() -> Self {
            Self {
                device: AtomicPtr::new(ptr::null_mut()),
                circuits: [const { AtomicPtr::new(ptr::null_mut()) }; 4],
                added: [const { AtomicBool::new(false) }; 4],
                active: [const { AtomicI32::new(0) }; 4],
            }
        }
    }

    struct StreamSlot {
        claimed: AtomicBool,
        stream: AtomicPtr<c_void>,
        format: AtomicPtr<c_void>,
        device: AtomicPtr<c_void>,
        timer: AtomicPtr<c_void>,
        cable: AtomicU8,
        capture: AtomicBool,
        state: AtomicI32,
        counted: AtomicBool,
        packet_count: AtomicU32,
        packet_size: AtomicU32,
        first_packet_offset: AtomicU32,
        bytes_per_second: AtomicU32,
        current_packet: AtomicU32,
        position: AtomicU64,
        start_time: AtomicU64,
        start_position: AtomicU64,
        glitch_adjust: AtomicU64,
        current_packet_start: AtomicU64,
        last_packet_start: AtomicU64,
        performance_frequency: AtomicI64,
        packet_buffers: [AtomicPtr<u8>; 2],
        eos_state: AtomicI32,
        eos_packet: AtomicU32,
        eos_bytes: AtomicU32,
    }

    impl StreamSlot {
        const fn new() -> Self {
            Self {
                claimed: AtomicBool::new(false),
                stream: AtomicPtr::new(ptr::null_mut()),
                format: AtomicPtr::new(ptr::null_mut()),
                device: AtomicPtr::new(ptr::null_mut()),
                timer: AtomicPtr::new(ptr::null_mut()),
                cable: AtomicU8::new(APP_CABLE),
                capture: AtomicBool::new(false),
                state: AtomicI32::new(0),
                counted: AtomicBool::new(false),
                packet_count: AtomicU32::new(0),
                packet_size: AtomicU32::new(0),
                first_packet_offset: AtomicU32::new(0),
                bytes_per_second: AtomicU32::new(0),
                current_packet: AtomicU32::new(0),
                position: AtomicU64::new(0),
                start_time: AtomicU64::new(0),
                start_position: AtomicU64::new(0),
                glitch_adjust: AtomicU64::new(0),
                current_packet_start: AtomicU64::new(0),
                last_packet_start: AtomicU64::new(0),
                performance_frequency: AtomicI64::new(0),
                packet_buffers: [const { AtomicPtr::new(ptr::null_mut()) }; 2],
                eos_state: AtomicI32::new(0),
                eos_packet: AtomicU32::new(0),
                eos_bytes: AtomicU32::new(0),
            }
        }

        fn reset_fields(&self) {
            self.stream.store(ptr::null_mut(), Ordering::SeqCst);
            self.format.store(ptr::null_mut(), Ordering::SeqCst);
            self.device.store(ptr::null_mut(), Ordering::SeqCst);
            self.timer.store(ptr::null_mut(), Ordering::SeqCst);
            self.cable.store(APP_CABLE, Ordering::SeqCst);
            self.capture.store(false, Ordering::SeqCst);
            self.state.store(0, Ordering::SeqCst);
            self.counted.store(false, Ordering::SeqCst);
            self.packet_count.store(0, Ordering::SeqCst);
            self.packet_size.store(0, Ordering::SeqCst);
            self.first_packet_offset.store(0, Ordering::SeqCst);
            self.bytes_per_second.store(0, Ordering::SeqCst);
            self.current_packet.store(0, Ordering::SeqCst);
            self.position.store(0, Ordering::SeqCst);
            self.start_time.store(0, Ordering::SeqCst);
            self.start_position.store(0, Ordering::SeqCst);
            self.glitch_adjust.store(0, Ordering::SeqCst);
            self.current_packet_start.store(0, Ordering::SeqCst);
            self.last_packet_start.store(0, Ordering::SeqCst);
            self.performance_frequency.store(0, Ordering::SeqCst);
            self.packet_buffers[0].store(ptr::null_mut(), Ordering::SeqCst);
            self.packet_buffers[1].store(ptr::null_mut(), Ordering::SeqCst);
            self.eos_state.store(0, Ordering::SeqCst);
            self.eos_packet.store(0, Ordering::SeqCst);
            self.eos_bytes.store(0, Ordering::SeqCst);
        }
    }

    static DEVICE: DeviceState = DeviceState::new();
    static STREAMS: [StreamSlot; MAX_STREAMS] = [const { StreamSlot::new() }; MAX_STREAMS];

    fn as_void<T>(pointer: *mut T) -> *mut c_void {
        pointer.cast()
    }

    fn nt_success(status: NTSTATUS) -> bool {
        wdk::nt_success(status)
    }

    fn unicode_string(name: &'static [u16]) -> ffi::UNICODE_STRING {
        let bytes = ((name.len() - 1) * size_of::<u16>()) as ffi::USHORT;
        ffi::UNICODE_STRING {
            Length: bytes,
            MaximumLength: size_of_val(name) as ffi::USHORT,
            Buffer: name.as_ptr() as *mut ffi::WCHAR,
        }
    }

    fn find_stream(stream: ffi::ACXSTREAM) -> Option<&'static StreamSlot> {
        let pointer = as_void(stream);
        if pointer.is_null() {
            return None;
        }
        STREAMS
            .iter()
            .find(|slot| slot.stream.load(Ordering::SeqCst) == pointer)
    }

    fn find_timer(timer: wdk_sys::WDFTIMER) -> Option<&'static StreamSlot> {
        let pointer = as_void(timer);
        if pointer.is_null() {
            return None;
        }
        STREAMS
            .iter()
            .find(|slot| slot.timer.load(Ordering::SeqCst) == pointer)
    }

    fn claim_stream() -> Option<&'static StreamSlot> {
        for slot in STREAMS.iter() {
            if slot
                .claimed
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                slot.reset_fields();
                return Some(slot);
            }
        }
        None
    }

    fn release_stream_slot(slot: &StreamSlot) {
        slot.reset_fields();
        slot.claimed.store(false, Ordering::SeqCst);
    }

    fn active_index(slot: &StreamSlot) -> usize {
        (slot.cable.load(Ordering::SeqCst) as usize * 2)
            + usize::from(slot.capture.load(Ordering::SeqCst))
    }

    fn cable_active(cable: u8) -> bool {
        let base = cable as usize * 2;
        DEVICE.active[base].load(Ordering::SeqCst) != 0
            || DEVICE.active[base + 1].load(Ordering::SeqCst) != 0
    }

    fn clear_cable(cable: u8) {
        if cable == RELAY_CABLE {
            transport::qpwgraph_audio_transport_clear_relay();
        } else {
            transport::qpwgraph_audio_transport_clear();
        }
    }

    fn clear_idle_cables() {
        if !cable_active(APP_CABLE) {
            clear_cable(APP_CABLE);
        }
        if !cable_active(RELAY_CABLE) {
            clear_cable(RELAY_CABLE);
        }
    }

    fn stop_counted_stream(slot: &StreamSlot) {
        if !slot.counted.swap(false, Ordering::SeqCst) {
            return;
        }
        let index = active_index(slot);
        let remaining = DEVICE.active[index].fetch_sub(1, Ordering::SeqCst) - 1;
        let peer_remaining = DEVICE.active[index ^ 1].load(Ordering::SeqCst);
        if remaining == 0 && peer_remaining == 0 {
            clear_cable(slot.cable.load(Ordering::SeqCst));
        }
    }

    fn now_hns(slot: &StreamSlot) -> u64 {
        let frequency = slot.performance_frequency.load(Ordering::SeqCst);
        if frequency <= 0 {
            return 0;
        }
        let counter = unsafe { KeQueryPerformanceCounter(ptr::null_mut()) };
        unsafe { ffi::qpwgraph_acx_convert_performance_time(frequency, counter.QuadPart) as u64 }
    }

    fn update_position(slot: &StreamSlot) {
        if slot.state.load(Ordering::SeqCst) != 3 {
            return;
        }
        let bytes_per_second = slot.bytes_per_second.load(Ordering::SeqCst);
        if bytes_per_second == 0 {
            return;
        }
        let now = now_hns(slot);
        let start = slot.start_time.load(Ordering::SeqCst);
        if now < start {
            return;
        }
        let elapsed = now - start;
        let glitch = slot.glitch_adjust.load(Ordering::SeqCst);
        let start_position = slot.start_position.load(Ordering::SeqCst);
        let position = start_position.saturating_add(
            elapsed
                .saturating_sub(glitch)
                .saturating_mul(bytes_per_second as u64)
                .saturating_div(HNS_PER_SEC),
        );
        let previous = slot.position.load(Ordering::SeqCst);
        slot.position
            .store(previous.max(position), Ordering::SeqCst);
    }

    fn packet_buffer(slot: &StreamSlot, packet: u32) -> *mut u8 {
        let count = slot.packet_count.load(Ordering::SeqCst);
        if count == 0 || count > MAX_PACKET_COUNT {
            return ptr::null_mut();
        }
        let index = (packet % count) as usize;
        let buffer = slot.packet_buffers[index].load(Ordering::SeqCst);
        if buffer.is_null() {
            return ptr::null_mut();
        }
        if index == 0 {
            buffer.wrapping_add(slot.first_packet_offset.load(Ordering::SeqCst) as usize)
        } else {
            buffer
        }
    }

    fn schedule_next_pass(slot: &StreamSlot) {
        if slot.state.load(Ordering::SeqCst) != 3 {
            return;
        }
        let timer = slot.timer.load(Ordering::SeqCst);
        let packet_size = slot.packet_size.load(Ordering::SeqCst);
        let bytes_per_second = slot.bytes_per_second.load(Ordering::SeqCst);
        if timer.is_null() || packet_size == 0 || bytes_per_second == 0 {
            return;
        }
        let current_packet = slot.current_packet.load(Ordering::SeqCst) as u64;
        let next_position = (current_packet + 1).saturating_mul(packet_size as u64);
        let start_position = slot.start_position.load(Ordering::SeqCst);
        let position_from_pause = next_position.saturating_sub(start_position);
        let packet_time = position_from_pause
            .saturating_mul(HNS_PER_SEC)
            .saturating_div(bytes_per_second as u64);
        let next_time = slot
            .start_time
            .load(Ordering::SeqCst)
            .saturating_add(slot.glitch_adjust.load(Ordering::SeqCst))
            .saturating_add(packet_time);
        let current_time = now_hns(slot);
        let delay = if next_time <= current_time {
            slot.glitch_adjust
                .fetch_add(current_time.saturating_sub(next_time), Ordering::SeqCst);
            -1
        } else {
            let delta = next_time - current_time;
            -(delta.min(i64::MAX as u64) as i64)
        };
        unsafe {
            let _ = ffi::qpwgraph_wdf_timer_start(timer, delay);
        }
    }

    fn stream_pass(slot: &StreamSlot) {
        if slot.state.load(Ordering::SeqCst) != 3 {
            return;
        }
        let active_packet = slot.current_packet.load(Ordering::SeqCst);
        let buffer = packet_buffer(slot, active_packet);
        let packet_size = slot.packet_size.load(Ordering::SeqCst);
        if !buffer.is_null() && packet_size != 0 {
            let cable = slot.cable.load(Ordering::SeqCst);
            if slot.capture.load(Ordering::SeqCst) {
                unsafe {
                    if cable == RELAY_CABLE {
                        let _ = transport::qpwgraph_audio_transport_pop_relay_pcm16(
                            buffer,
                            packet_size,
                        );
                    } else {
                        let _ = transport::qpwgraph_audio_transport_pop_pcm16(buffer, packet_size);
                    }
                }
            } else {
                let eos_state = slot.eos_state.load(Ordering::SeqCst);
                let payload = transport::qpwgraph_audio_render_payload(
                    active_packet,
                    packet_size,
                    eos_state,
                    if eos_state == 1 {
                        slot.eos_packet.load(Ordering::SeqCst)
                    } else {
                        0
                    },
                    if eos_state == 1 {
                        slot.eos_bytes.load(Ordering::SeqCst)
                    } else {
                        0
                    },
                );
                if payload.eos_state == 2 && eos_state == 1 {
                    slot.eos_state.store(2, Ordering::SeqCst);
                }
                unsafe {
                    if cable == RELAY_CABLE {
                        let _ = transport::qpwgraph_audio_transport_push_relay_pcm16(
                            buffer,
                            payload.bytes,
                        );
                    } else {
                        let _ =
                            transport::qpwgraph_audio_transport_push_pcm16(buffer, payload.bytes);
                    }
                }
            }
        }
        let completed_packet = slot.current_packet.fetch_add(1, Ordering::SeqCst);
        let qpc_completed = unsafe { KeQueryPerformanceCounter(ptr::null_mut()) };
        slot.last_packet_start.store(
            slot.current_packet_start.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        let qpc_value = unsafe { qpc_completed.QuadPart };
        slot.current_packet_start
            .store(qpc_value as u64, Ordering::SeqCst);
        unsafe {
            let _ = ffi::qpwgraph_acx_rt_stream_notify_packet_complete(
                slot.stream.load(Ordering::SeqCst).cast(),
                completed_packet as ffi::ULONGLONG,
                qpc_value as ffi::ULONGLONG,
            );
        }
        schedule_next_pass(slot);
    }

    unsafe fn free_rt_packet_array(
        packets: ffi::PACX_RTPACKET,
        packet_count: ffi::ULONG,
        slot: Option<&StreamSlot>,
    ) {
        if packets.is_null() {
            return;
        }
        let count = packet_count.min(MAX_PACKET_COUNT) as usize;
        for index in 0..count {
            let packet = packets.add(index);
            let mdl = (*packet).RtPacketBuffer.u.MdlType.Mdl;
            if !mdl.is_null() {
                IoFreeMdl(mdl.cast());
            }
            if let Some(slot) = slot {
                let buffer = slot.packet_buffers[index].swap(ptr::null_mut(), Ordering::SeqCst);
                if !buffer.is_null() {
                    ExFreePool(buffer.cast());
                }
            }
        }
        ExFreePool(packets.cast());
    }

    unsafe extern "C" fn allocate_rt_packets(
        stream: ffi::ACXSTREAM,
        packet_count: ffi::ULONG,
        packet_size: ffi::ULONG,
        packets: *mut ffi::PACX_RTPACKET,
    ) -> NTSTATUS {
        if packets.is_null()
            || packet_count == 0
            || packet_count > MAX_PACKET_COUNT
            || packet_size == 0
            || packet_size > MAX_PACKET_BYTES
            || !packet_size.is_multiple_of(4)
            || packet_size > u32::MAX - (wdk_sys::PAGE_SIZE - 1)
        {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        }
        unsafe { *packets = ptr::null_mut() };
        let Some(slot) = find_stream(stream) else {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        };
        if slot.packet_count.load(Ordering::SeqCst) != 0
            || slot
                .packet_buffers
                .iter()
                .any(|buffer| !buffer.load(Ordering::SeqCst).is_null())
        {
            // ACX must free the previous packet set before requesting a new
            // one. Refusing a duplicate request prevents an overwrite from
            // orphaning non-paged buffers and MDLs.
            return wdk_sys::STATUS_INVALID_DEVICE_STATE;
        }
        let page_size = wdk_sys::PAGE_SIZE;
        let allocation_size = (packet_size + page_size - 1) & !(page_size - 1);
        let first_offset = allocation_size - packet_size;
        let array_bytes = size_of::<ffi::ACX_RTPACKET>() * packet_count as usize;
        let packet_array = unsafe {
            ExAllocatePool2(
                wdk_sys::POOL_FLAG_NON_PAGED,
                array_bytes as wdk_sys::SIZE_T,
                DRIVER_TAG,
            )
        } as ffi::PACX_RTPACKET;
        if packet_array.is_null() {
            return wdk_sys::STATUS_INSUFFICIENT_RESOURCES;
        }
        unsafe { ptr::write_bytes(packet_array, 0, packet_count as usize) };
        for index in 0..packet_count as usize {
            let buffer = unsafe {
                ExAllocatePool2(
                    wdk_sys::POOL_FLAG_NON_PAGED,
                    allocation_size as wdk_sys::SIZE_T,
                    DRIVER_TAG,
                )
            };
            if buffer.is_null() {
                unsafe { free_rt_packet_array(packet_array, index as ffi::ULONG, Some(slot)) };
                return wdk_sys::STATUS_INSUFFICIENT_RESOURCES;
            }
            let mdl = unsafe { IoAllocateMdl(buffer, allocation_size, 0, 1, ptr::null_mut()) };
            if mdl.is_null() {
                unsafe { ExFreePool(buffer) };
                unsafe { free_rt_packet_array(packet_array, index as ffi::ULONG, Some(slot)) };
                return wdk_sys::STATUS_INSUFFICIENT_RESOURCES;
            }
            unsafe {
                MmBuildMdlForNonPagedPool(mdl);
                let packet = &mut *packet_array.add(index);
                ffi::qpwgraph_acx_rt_packet_init(packet);
                ffi::qpwgraph_wdf_memory_descriptor_init_mdl(
                    (&mut packet.RtPacketBuffer as *mut ffi::WDF_MEMORY_DESCRIPTOR).cast(),
                    mdl.cast(),
                    allocation_size,
                );
                packet.RtPacketSize = packet_size;
                packet.RtPacketOffset = if index == 0 { first_offset } else { 0 };
            }
            slot.packet_buffers[index].store(buffer.cast(), Ordering::SeqCst);
        }
        slot.packet_count.store(packet_count, Ordering::SeqCst);
        slot.packet_size.store(packet_size, Ordering::SeqCst);
        slot.first_packet_offset
            .store(first_offset, Ordering::SeqCst);
        let bytes_per_second = unsafe {
            ffi::qpwgraph_acx_data_format_average_bytes_per_sec(
                slot.format.load(Ordering::SeqCst).cast(),
            )
        };
        slot.bytes_per_second
            .store(bytes_per_second, Ordering::SeqCst);
        unsafe { *packets = packet_array };
        STATUS_SUCCESS
    }

    unsafe extern "C" fn free_rt_packets(
        stream: ffi::ACXSTREAM,
        packets: ffi::PACX_RTPACKET,
        packet_count: ffi::ULONG,
    ) {
        let slot = find_stream(stream);
        unsafe { free_rt_packet_array(packets, packet_count, slot) };
        if let Some(slot) = slot {
            slot.packet_count.store(0, Ordering::SeqCst);
            slot.packet_size.store(0, Ordering::SeqCst);
            slot.first_packet_offset.store(0, Ordering::SeqCst);
            slot.bytes_per_second.store(0, Ordering::SeqCst);
        }
    }

    unsafe extern "C" fn get_hw_latency(
        _stream: ffi::ACXSTREAM,
        fifo_size: *mut ffi::ULONG,
        delay: *mut ffi::ULONG,
    ) -> NTSTATUS {
        if fifo_size.is_null() || delay.is_null() {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        }
        unsafe {
            *fifo_size = 128;
            *delay = 0;
        }
        STATUS_SUCCESS
    }

    unsafe extern "C" fn prepare_hardware(stream: ffi::ACXSTREAM) -> NTSTATUS {
        let Some(slot) = find_stream(stream) else {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        };
        let state = slot.state.load(Ordering::SeqCst);
        if state == 2 {
            return STATUS_SUCCESS;
        }
        if state != 0 {
            return wdk_sys::STATUS_INVALID_DEVICE_STATE;
        }
        if slot.timer.load(Ordering::SeqCst).is_null() {
            let mut timer: *mut c_void = ptr::null_mut();
            let status = unsafe {
                ffi::qpwgraph_wdf_timer_create(
                    slot.stream.load(Ordering::SeqCst),
                    timer_pass as *const () as *mut c_void,
                    &mut timer,
                )
            };
            if !nt_success(status) {
                return status;
            }
            slot.timer.store(timer, Ordering::SeqCst);
        }
        let mut frequency = wdk_sys::LARGE_INTEGER::default();
        unsafe {
            let _ = KeQueryPerformanceCounter(&mut frequency);
        }
        let frequency_value = unsafe { frequency.QuadPart };
        slot.performance_frequency
            .store(frequency_value, Ordering::SeqCst);
        slot.state.store(2, Ordering::SeqCst);
        slot.counted.store(false, Ordering::SeqCst);
        slot.eos_state.store(0, Ordering::SeqCst);
        slot.current_packet.store(0, Ordering::SeqCst);
        slot.position.store(0, Ordering::SeqCst);
        slot.start_time.store(0, Ordering::SeqCst);
        slot.start_position.store(0, Ordering::SeqCst);
        slot.glitch_adjust.store(0, Ordering::SeqCst);
        slot.current_packet_start.store(0, Ordering::SeqCst);
        slot.last_packet_start.store(0, Ordering::SeqCst);
        STATUS_SUCCESS
    }

    unsafe extern "C" fn release_hardware(stream: ffi::ACXSTREAM) -> NTSTATUS {
        let Some(slot) = find_stream(stream) else {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        };
        let timer = slot.timer.load(Ordering::SeqCst);
        if !timer.is_null() {
            unsafe {
                let _ = ffi::qpwgraph_wdf_timer_stop(timer, 1);
            }
            slot.timer.store(ptr::null_mut(), Ordering::SeqCst);
            unsafe { ffi::qpwgraph_wdf_object_delete(timer) };
        }
        stop_counted_stream(slot);
        unsafe { KeFlushQueuedDpcs() };
        slot.state.store(0, Ordering::SeqCst);
        slot.current_packet.store(0, Ordering::SeqCst);
        slot.position.store(0, Ordering::SeqCst);
        slot.start_time.store(0, Ordering::SeqCst);
        slot.start_position.store(0, Ordering::SeqCst);
        slot.glitch_adjust.store(0, Ordering::SeqCst);
        slot.current_packet_start.store(0, Ordering::SeqCst);
        slot.last_packet_start.store(0, Ordering::SeqCst);
        slot.eos_state.store(0, Ordering::SeqCst);
        STATUS_SUCCESS
    }

    unsafe extern "C" fn run_stream(stream: ffi::ACXSTREAM) -> NTSTATUS {
        let Some(slot) = find_stream(stream) else {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        };
        if slot.device.load(Ordering::SeqCst).is_null() {
            return wdk_sys::STATUS_INVALID_DEVICE_STATE;
        }
        let state = slot.state.load(Ordering::SeqCst);
        if state == 3 {
            return STATUS_SUCCESS;
        }
        if state != 2
            || slot.timer.load(Ordering::SeqCst).is_null()
            || slot.packet_size.load(Ordering::SeqCst) == 0
            || slot.bytes_per_second.load(Ordering::SeqCst) == 0
            || slot.performance_frequency.load(Ordering::SeqCst) <= 0
        {
            return wdk_sys::STATUS_INVALID_DEVICE_STATE;
        }
        let counter = unsafe { KeQueryPerformanceCounter(ptr::null_mut()) };
        slot.current_packet_start
            .store(counter.QuadPart as u64, Ordering::SeqCst);
        let start_time = unsafe {
            ffi::qpwgraph_acx_convert_performance_time(
                slot.performance_frequency.load(Ordering::SeqCst),
                counter.QuadPart,
            )
        };
        slot.start_time.store(start_time as u64, Ordering::SeqCst);
        slot.start_position
            .store(slot.position.load(Ordering::SeqCst), Ordering::SeqCst);
        slot.glitch_adjust.store(0, Ordering::SeqCst);
        if !slot.counted.load(Ordering::SeqCst) {
            let cable = slot.cable.load(Ordering::SeqCst);
            if !cable_active(cable) {
                clear_cable(cable);
            }
            let index = active_index(slot);
            if DEVICE.active[index]
                .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return wdk_sys::STATUS_DEVICE_BUSY;
            }
            slot.counted.store(true, Ordering::SeqCst);
        }
        slot.state.store(3, Ordering::SeqCst);
        schedule_next_pass(slot);
        STATUS_SUCCESS
    }

    unsafe extern "C" fn pause_stream(stream: ffi::ACXSTREAM) -> NTSTATUS {
        let Some(slot) = find_stream(stream) else {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        };
        let state = slot.state.load(Ordering::SeqCst);
        if state == 2 {
            return STATUS_SUCCESS;
        }
        if state != 3 {
            return wdk_sys::STATUS_INVALID_DEVICE_STATE;
        }
        update_position(slot);
        let timer = slot.timer.load(Ordering::SeqCst);
        if !timer.is_null() {
            unsafe {
                let _ = ffi::qpwgraph_wdf_timer_stop(timer, 1);
            }
        }
        stop_counted_stream(slot);
        slot.state.store(2, Ordering::SeqCst);
        STATUS_SUCCESS
    }

    unsafe extern "C" fn set_render_packet(
        stream: ffi::ACXSTREAM,
        packet: ffi::ULONG,
        flags: ffi::ULONG,
        eos_packet_length: ffi::ULONG,
    ) -> NTSTATUS {
        let Some(slot) = find_stream(stream) else {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        };
        if flags & !EOS_FLAG != 0
            || (flags & EOS_FLAG != 0
                && (eos_packet_length > slot.packet_size.load(Ordering::SeqCst)
                    || !eos_packet_length.is_multiple_of(4)))
        {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        }
        if slot.eos_state.load(Ordering::SeqCst) != 0 {
            return wdk_sys::STATUS_INVALID_DEVICE_STATE;
        }
        let current_packet = slot.current_packet.load(Ordering::SeqCst);
        if packet <= current_packet {
            return wdk_sys::STATUS_DATA_LATE_ERROR;
        }
        if packet > current_packet.saturating_add(1) {
            return wdk_sys::STATUS_DATA_OVERRUN;
        }
        if flags & EOS_FLAG != 0 {
            slot.eos_packet.store(packet, Ordering::SeqCst);
            slot.eos_bytes.store(eos_packet_length, Ordering::SeqCst);
            slot.eos_state.store(1, Ordering::SeqCst);
        }
        STATUS_SUCCESS
    }

    unsafe extern "C" fn get_current_packet(
        stream: ffi::ACXSTREAM,
        current_packet: ffi::PULONG,
    ) -> NTSTATUS {
        if current_packet.is_null() {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        }
        let Some(slot) = find_stream(stream) else {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        };
        unsafe { *current_packet = slot.current_packet.load(Ordering::SeqCst) as ffi::ULONG };
        STATUS_SUCCESS
    }

    unsafe extern "C" fn get_presentation_position(
        stream: ffi::ACXSTREAM,
        position_in_blocks: ffi::PULONGLONG,
        qpc_position: ffi::PULONGLONG,
    ) -> NTSTATUS {
        if position_in_blocks.is_null() || qpc_position.is_null() {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        }
        let Some(slot) = find_stream(stream) else {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        };
        let block_align = unsafe {
            ffi::qpwgraph_acx_data_format_block_align(slot.format.load(Ordering::SeqCst).cast())
        } as u64;
        if block_align == 0 {
            return wdk_sys::STATUS_INVALID_DEVICE_STATE;
        }
        update_position(slot);
        let position = slot.position.load(Ordering::SeqCst);
        let qpc = unsafe { KeQueryPerformanceCounter(ptr::null_mut()) };
        unsafe {
            *position_in_blocks = (position / block_align) as ffi::ULONGLONG;
            *qpc_position = qpc.QuadPart as ffi::ULONGLONG;
        }
        STATUS_SUCCESS
    }

    unsafe extern "C" fn get_capture_packet(
        stream: ffi::ACXSTREAM,
        last_capture_packet: ffi::PULONG,
        qpc_packet_start: ffi::PULONGLONG,
        more_data: ffi::PBOOLEAN,
    ) -> NTSTATUS {
        if last_capture_packet.is_null() || qpc_packet_start.is_null() || more_data.is_null() {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        }
        let Some(slot) = find_stream(stream) else {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        };
        unsafe {
            *last_capture_packet =
                slot.current_packet.load(Ordering::SeqCst).wrapping_sub(1) as ffi::ULONG;
            *qpc_packet_start = slot.last_packet_start.load(Ordering::SeqCst) as ffi::ULONGLONG;
            *more_data = 0;
        }
        STATUS_SUCCESS
    }

    unsafe extern "C" fn timer_pass(timer: wdk_sys::WDFTIMER) {
        if let Some(slot) = find_timer(timer) {
            stream_pass(slot);
        }
    }

    unsafe extern "C" fn stream_cleanup(object: ffi::WDFOBJECT) {
        let stream = object.cast::<ffi::ACXSTREAM__>();
        let Some(slot) = find_stream(stream) else {
            return;
        };
        let timer = slot.timer.load(Ordering::SeqCst);
        if !timer.is_null() {
            unsafe {
                let _ = ffi::qpwgraph_wdf_timer_stop(timer, 1);
            }
            slot.timer.store(ptr::null_mut(), Ordering::SeqCst);
            unsafe { ffi::qpwgraph_wdf_object_delete(timer) };
        }
        stop_counted_stream(slot);
        release_stream_slot(slot);
    }

    fn create_stream(
        device: ffi::WDFDEVICE,
        circuit: ffi::ACXCIRCUIT,
        stream_init: ffi::PACXSTREAM_INIT,
        stream_format: ffi::ACXDATAFORMAT,
        capture: bool,
        cable: u8,
    ) -> NTSTATUS {
        if stream_init.is_null() || stream_format.is_null() {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        }
        let Some(slot) = claim_stream() else {
            return wdk_sys::STATUS_INSUFFICIENT_RESOURCES;
        };
        slot.device.store(as_void(device), Ordering::SeqCst);
        slot.format.store(as_void(stream_format), Ordering::SeqCst);
        slot.cable.store(cable, Ordering::SeqCst);
        slot.capture.store(capture, Ordering::SeqCst);

        let mut stream_callbacks = ffi::ACX_STREAM_CALLBACKS::default();
        unsafe { ffi::qpwgraph_acx_stream_callbacks_init(&mut stream_callbacks) };
        stream_callbacks.EvtAcxStreamPrepareHardware = Some(prepare_hardware);
        stream_callbacks.EvtAcxStreamReleaseHardware = Some(release_hardware);
        stream_callbacks.EvtAcxStreamRun = Some(run_stream);
        stream_callbacks.EvtAcxStreamPause = Some(pause_stream);
        let mut status = unsafe {
            ffi::qpwgraph_acx_stream_init_set_callbacks(stream_init, &mut stream_callbacks)
        };
        if !nt_success(status) {
            release_stream_slot(slot);
            return status;
        }

        let mut rt_callbacks = ffi::ACX_RT_STREAM_CALLBACKS::default();
        unsafe { ffi::qpwgraph_acx_rt_stream_callbacks_init(&mut rt_callbacks) };
        rt_callbacks.EvtAcxStreamGetHwLatency = Some(get_hw_latency);
        rt_callbacks.EvtAcxStreamAllocateRtPackets = Some(allocate_rt_packets);
        rt_callbacks.EvtAcxStreamFreeRtPackets = Some(free_rt_packets);
        rt_callbacks.EvtAcxStreamGetCurrentPacket = Some(get_current_packet);
        rt_callbacks.EvtAcxStreamGetPresentationPosition = Some(get_presentation_position);
        if capture {
            rt_callbacks.EvtAcxStreamGetCapturePacket = Some(get_capture_packet);
        } else {
            rt_callbacks.EvtAcxStreamSetRenderPacket = Some(set_render_packet);
        }
        status = unsafe {
            ffi::qpwgraph_acx_stream_init_set_rt_callbacks(stream_init, &mut rt_callbacks)
        };
        if !nt_success(status) {
            release_stream_slot(slot);
            return status;
        }
        unsafe { ffi::qpwgraph_acx_stream_init_enable_notifications(stream_init) };

        let mut stream: ffi::ACXSTREAM = ptr::null_mut();
        status = unsafe {
            ffi::qpwgraph_acx_rt_stream_create(
                as_void(device),
                circuit,
                as_void(stream_init),
                stream_cleanup as *const () as *mut c_void,
                &mut stream,
            )
        };
        if !nt_success(status) || stream.is_null() {
            release_stream_slot(slot);
            return if nt_success(status) {
                wdk_sys::STATUS_INVALID_DEVICE_STATE
            } else {
                status
            };
        }
        slot.stream.store(as_void(stream), Ordering::SeqCst);
        let mut frequency = wdk_sys::LARGE_INTEGER::default();
        unsafe {
            let _ = KeQueryPerformanceCounter(&mut frequency);
        }
        let frequency_value = unsafe { frequency.QuadPart };
        slot.performance_frequency
            .store(frequency_value, Ordering::SeqCst);
        slot.state.store(0, Ordering::SeqCst);
        STATUS_SUCCESS
    }

    unsafe extern "C" fn render_stream_create(
        device: ffi::WDFDEVICE,
        circuit: ffi::ACXCIRCUIT,
        _pin: ffi::ACXPIN,
        stream_init: ffi::PACXSTREAM_INIT,
        stream_format: ffi::ACXDATAFORMAT,
        _signal_processing_mode: *const ffi::GUID,
        _var_arguments: ffi::ACXOBJECTBAG,
    ) -> NTSTATUS {
        create_stream(
            device,
            circuit,
            stream_init,
            stream_format,
            false,
            APP_CABLE,
        )
    }

    unsafe extern "C" fn capture_stream_create(
        device: ffi::WDFDEVICE,
        circuit: ffi::ACXCIRCUIT,
        _pin: ffi::ACXPIN,
        stream_init: ffi::PACXSTREAM_INIT,
        stream_format: ffi::ACXDATAFORMAT,
        _signal_processing_mode: *const ffi::GUID,
        _var_arguments: ffi::ACXOBJECTBAG,
    ) -> NTSTATUS {
        create_stream(device, circuit, stream_init, stream_format, true, APP_CABLE)
    }

    unsafe extern "C" fn relay_render_stream_create(
        device: ffi::WDFDEVICE,
        circuit: ffi::ACXCIRCUIT,
        _pin: ffi::ACXPIN,
        stream_init: ffi::PACXSTREAM_INIT,
        stream_format: ffi::ACXDATAFORMAT,
        _signal_processing_mode: *const ffi::GUID,
        _var_arguments: ffi::ACXOBJECTBAG,
    ) -> NTSTATUS {
        create_stream(
            device,
            circuit,
            stream_init,
            stream_format,
            false,
            RELAY_CABLE,
        )
    }

    unsafe extern "C" fn relay_capture_stream_create(
        device: ffi::WDFDEVICE,
        circuit: ffi::ACXCIRCUIT,
        _pin: ffi::ACXPIN,
        stream_init: ffi::PACXSTREAM_INIT,
        stream_format: ffi::ACXDATAFORMAT,
        _signal_processing_mode: *const ffi::GUID,
        _var_arguments: ffi::ACXOBJECTBAG,
    ) -> NTSTATUS {
        create_stream(
            device,
            circuit,
            stream_init,
            stream_format,
            true,
            RELAY_CABLE,
        )
    }

    fn add_endpoint_jack(pin: ffi::ACXPIN) -> NTSTATUS {
        if pin.is_null() {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        }
        let mut config = ffi::ACX_JACK_CONFIG::default();
        unsafe { ffi::qpwgraph_acx_jack_config_init(&mut config) };
        config.Description.ChannelMapping = SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT;
        config.Description.ConnectionType = AcxConnTypeAtapiInternal;
        config.Description.GeoLocation = AcxGeoLocFront;
        config.Description.GenLocation = AcxGenLocPrimaryBox;
        config.Description.PortConnection = AcxPortConnIntegratedDevice;
        let mut jack: ffi::ACXJACK = ptr::null_mut();
        let status = unsafe { ffi::qpwgraph_acx_jack_create(pin, &mut config, &mut jack) };
        if !nt_success(status) {
            return status;
        }
        unsafe { ffi::qpwgraph_acx_pin_add_jacks(pin, &mut jack, 1) }
    }

    fn create_circuit(
        device: ffi::WDFDEVICE,
        capture: bool,
        cable: u8,
        circuit_out: *mut ffi::ACXCIRCUIT,
    ) -> NTSTATUS {
        if circuit_out.is_null() {
            return wdk_sys::STATUS_INVALID_PARAMETER;
        }
        unsafe { *circuit_out = ptr::null_mut() };
        let init = unsafe { ffi::qpwgraph_acx_circuit_init_allocate(as_void(device)) };
        if init.is_null() {
            return wdk_sys::STATUS_INSUFFICIENT_RESOURCES;
        }
        let (component_id, name, create_callback): (
            &ffi::GUID,
            &'static [u16],
            ffi::PFN_ACX_CIRCUIT_CREATE_STREAM,
        ) = match (capture, cable) {
            (false, APP_CABLE) => (&RENDER_COMPONENT, RENDER_NAME, Some(render_stream_create)),
            (true, APP_CABLE) => (
                &CAPTURE_COMPONENT,
                CAPTURE_NAME,
                Some(capture_stream_create),
            ),
            (false, RELAY_CABLE) => (
                &RELAY_RENDER_COMPONENT,
                RELAY_RENDER_NAME,
                Some(relay_render_stream_create),
            ),
            (true, RELAY_CABLE) => (
                &RELAY_CAPTURE_COMPONENT,
                RELAY_CAPTURE_NAME,
                Some(relay_capture_stream_create),
            ),
            _ => {
                unsafe { ffi::qpwgraph_acx_circuit_init_free(init) };
                return wdk_sys::STATUS_INVALID_PARAMETER;
            }
        };
        unsafe { ffi::qpwgraph_acx_circuit_init_set_component_id(init, component_id) };
        let circuit_name = unicode_string(name);
        let mut status = unsafe { ffi::qpwgraph_acx_circuit_init_assign_name(init, &circuit_name) };
        if !nt_success(status) {
            unsafe { ffi::qpwgraph_acx_circuit_init_free(init) };
            return status;
        }
        unsafe {
            ffi::qpwgraph_acx_circuit_init_set_type(
                init,
                if capture {
                    AcxCircuitTypeCapture
                } else {
                    AcxCircuitTypeRender
                },
            );
        }
        let mut power_callbacks = ffi::ACX_CIRCUIT_PNPPOWER_CALLBACKS::default();
        unsafe { ffi::qpwgraph_acx_circuit_power_callbacks_init(&mut power_callbacks) };
        power_callbacks.EvtAcxCircuitPowerUp = Some(circuit_power_up);
        power_callbacks.EvtAcxCircuitPowerDown = Some(circuit_power_down);
        unsafe { ffi::qpwgraph_acx_circuit_init_set_power_callbacks(init, &mut power_callbacks) };
        status =
            unsafe { ffi::qpwgraph_acx_circuit_init_set_stream_callback(init, create_callback) };
        if !nt_success(status) {
            unsafe { ffi::qpwgraph_acx_circuit_init_free(init) };
            return status;
        }

        let mut circuit: ffi::ACXCIRCUIT = ptr::null_mut();
        let mut circuit_init = init;
        status = unsafe {
            ffi::qpwgraph_acx_circuit_create(as_void(device), &mut circuit_init, &mut circuit)
        };
        if !nt_success(status) || circuit.is_null() {
            if !circuit_init.is_null() {
                unsafe { ffi::qpwgraph_acx_circuit_init_free(circuit_init) };
            }
            return if nt_success(status) {
                wdk_sys::STATUS_INVALID_DEVICE_STATE
            } else {
                status
            };
        }

        let mut pins = [ptr::null_mut(); 2];
        let mut pin_config = ffi::ACX_PIN_CONFIG::default();
        unsafe { ffi::qpwgraph_acx_pin_config_init(&mut pin_config) };
        pin_config.Type = if capture {
            AcxPinTypeSource
        } else {
            AcxPinTypeSink
        };
        pin_config.Communication = AcxPinCommunicationSink;
        pin_config.Category = &KSCATEGORY_AUDIO;
        status = unsafe { ffi::qpwgraph_acx_pin_create(circuit, &mut pin_config, &mut pins[0]) };
        if !nt_success(status) {
            return status;
        }

        let mut pin_config = ffi::ACX_PIN_CONFIG::default();
        unsafe { ffi::qpwgraph_acx_pin_config_init(&mut pin_config) };
        pin_config.Type = if capture {
            AcxPinTypeSink
        } else {
            AcxPinTypeSource
        };
        pin_config.Communication = AcxPinCommunicationNone;
        pin_config.Category = if capture {
            &KSNODETYPE_MICROPHONE
        } else {
            &KSNODETYPE_SPEAKER
        };
        status = unsafe { ffi::qpwgraph_acx_pin_create(circuit, &mut pin_config, &mut pins[1]) };
        if !nt_success(status) {
            return status;
        }
        status = add_endpoint_jack(pins[1]);
        if !nt_success(status) {
            return status;
        }

        let mut format_config = ffi::ACX_DATAFORMAT_CONFIG::default();
        unsafe { ffi::qpwgraph_acx_pcm_format_config_init(&mut format_config) };
        let mut format: ffi::ACXDATAFORMAT = ptr::null_mut();
        status = unsafe {
            ffi::qpwgraph_acx_data_format_create(
                as_void(device),
                as_void(circuit),
                &mut format_config,
                &mut format,
            )
        };
        if !nt_success(status) {
            return status;
        }
        let format_list = unsafe { ffi::qpwgraph_acx_pin_get_raw_format_list(pins[0]) };
        if format_list.is_null() {
            return wdk_sys::STATUS_INSUFFICIENT_RESOURCES;
        }
        status = unsafe { ffi::qpwgraph_acx_data_format_list_add(format_list, format) };
        if !nt_success(status) {
            return status;
        }
        status = unsafe { ffi::qpwgraph_acx_circuit_add_pins(circuit, pins.as_mut_ptr(), 2) };
        if nt_success(status) {
            unsafe { *circuit_out = circuit };
        }
        status
    }

    unsafe extern "C" fn circuit_power_up(
        device: ffi::WDFDEVICE,
        _circuit: ffi::ACXCIRCUIT,
        _previous_state: ffi::WDF_POWER_DEVICE_STATE,
    ) -> NTSTATUS {
        let _ = device;
        clear_idle_cables();
        STATUS_SUCCESS
    }

    unsafe extern "C" fn circuit_power_down(
        device: ffi::WDFDEVICE,
        _circuit: ffi::ACXCIRCUIT,
        _target_state: ffi::WDF_POWER_DEVICE_STATE,
    ) -> NTSTATUS {
        let _ = device;
        clear_idle_cables();
        STATUS_SUCCESS
    }

    unsafe extern "C" fn device_prepare_hardware(
        device: ffi::WDFDEVICE,
        _resources_raw: ffi::WDFCMRESLIST,
        _resources_translated: ffi::WDFCMRESLIST,
    ) -> NTSTATUS {
        if DEVICE
            .circuits
            .iter()
            .any(|circuit| circuit.load(Ordering::SeqCst).is_null())
        {
            return wdk_sys::STATUS_INVALID_DEVICE_STATE;
        }
        let device = as_void(device);
        for index in 0..4 {
            if DEVICE.added[index].load(Ordering::SeqCst) {
                continue;
            }
            let circuit = DEVICE.circuits[index].load(Ordering::SeqCst).cast();
            let status = unsafe { ffi::qpwgraph_acx_device_add_circuit(device, circuit) };
            if !nt_success(status) {
                for rollback in (0..index).rev() {
                    if DEVICE.added[rollback].load(Ordering::SeqCst) {
                        let old = DEVICE.circuits[rollback].load(Ordering::SeqCst).cast();
                        if nt_success(unsafe {
                            ffi::qpwgraph_acx_device_remove_circuit(device, old)
                        }) {
                            DEVICE.added[rollback].store(false, Ordering::SeqCst);
                        }
                    }
                }
                return status;
            }
            DEVICE.added[index].store(true, Ordering::SeqCst);
        }
        STATUS_SUCCESS
    }

    unsafe extern "C" fn device_release_hardware(
        device: ffi::WDFDEVICE,
        _resources_translated: ffi::WDFCMRESLIST,
    ) -> NTSTATUS {
        let device = as_void(device);
        let mut result = STATUS_SUCCESS;
        for index in (0..4).rev() {
            if !DEVICE.added[index].load(Ordering::SeqCst) {
                continue;
            }
            let circuit = DEVICE.circuits[index].load(Ordering::SeqCst).cast();
            let status = unsafe { ffi::qpwgraph_acx_device_remove_circuit(device, circuit) };
            if nt_success(status) {
                DEVICE.added[index].store(false, Ordering::SeqCst);
            } else if nt_success(result) {
                result = status;
            }
        }
        clear_cable(APP_CABLE);
        clear_cable(RELAY_CABLE);
        result
    }

    unsafe extern "C" fn device_d0_entry(
        device: ffi::WDFDEVICE,
        _previous_state: ffi::WDF_POWER_DEVICE_STATE,
    ) -> NTSTATUS {
        let _ = device;
        clear_idle_cables();
        STATUS_SUCCESS
    }

    unsafe extern "C" fn device_d0_exit(
        device: ffi::WDFDEVICE,
        _target_state: ffi::WDF_POWER_DEVICE_STATE,
    ) -> NTSTATUS {
        let _ = device;
        clear_idle_cables();
        STATUS_SUCCESS
    }

    pub unsafe fn initialize_driver(driver: WDFDRIVER) -> NTSTATUS {
        let mut config = ffi::ACX_DRIVER_CONFIG::default();
        unsafe { ffi::qpwgraph_acx_driver_config_init(&mut config) };
        unsafe { ffi::qpwgraph_acx_driver_initialize(driver.cast(), &mut config) }
    }

    pub unsafe fn initialize_device(device: WDFDEVICE) -> NTSTATUS {
        let mut config = ffi::ACX_DEVICE_CONFIG::default();
        unsafe { ffi::qpwgraph_acx_device_config_init(&mut config) };
        unsafe { ffi::qpwgraph_acx_device_initialize(device.cast(), &mut config) }
    }

    pub unsafe fn initialize_device_init(device_init: PWDFDEVICE_INIT) -> NTSTATUS {
        let mut config = ffi::ACX_DEVICEINIT_CONFIG::default();
        unsafe { ffi::qpwgraph_acx_device_init_config_init(&mut config) };
        config.SynchronizationScope = WdfSynchronizationScopeNone;
        config.ExecutionLevel = WdfExecutionLevelPassive;
        unsafe { ffi::qpwgraph_acx_device_init_initialize(device_init.cast(), &mut config) }
    }

    pub unsafe fn add_device(driver: WDFDRIVER, device_init: PWDFDEVICE_INIT) -> NTSTATUS {
        let _ = driver;
        if device_init.is_null() {
            return STATUS_INVALID_PARAMETER;
        }
        let status = unsafe { initialize_device_init(device_init) };
        if !nt_success(status) {
            return status;
        }
        let mut device: *mut c_void = ptr::null_mut();
        let status = unsafe {
            ffi::qpwgraph_wdf_device_create(
                device_init.cast(),
                device_prepare_hardware as *const () as *mut c_void,
                device_release_hardware as *const () as *mut c_void,
                device_d0_entry as *const () as *mut c_void,
                device_d0_exit as *const () as *mut c_void,
                &mut device,
            )
        };
        if !nt_success(status) {
            return status;
        }
        if device.is_null() {
            return wdk_sys::STATUS_INVALID_DEVICE_STATE;
        }
        let wdf_device: WDFDEVICE = device.cast();
        let status = unsafe { initialize_device(wdf_device) };
        if !nt_success(status) {
            return status;
        }
        DEVICE.device.store(device, Ordering::SeqCst);
        let circuits = [
            (false, APP_CABLE),
            (true, APP_CABLE),
            (false, RELAY_CABLE),
            (true, RELAY_CABLE),
        ];
        for (index, (capture, cable)) in circuits.into_iter().enumerate() {
            let mut circuit = ptr::null_mut();
            let status = create_circuit(wdf_device.cast(), capture, cable, &mut circuit);
            if !nt_success(status) {
                return status;
            }
            DEVICE.circuits[index].store(as_void(circuit), Ordering::SeqCst);
        }
        clear_cable(APP_CABLE);
        clear_cable(RELAY_CABLE);
        STATUS_SUCCESS
    }
}

#[cfg(feature = "acx")]
pub unsafe fn initialize_driver(driver: WDFDRIVER) -> NTSTATUS {
    unsafe { runtime::initialize_driver(driver) }
}

#[cfg(not(feature = "acx"))]
#[allow(dead_code)]
pub unsafe fn initialize_driver(_driver: WDFDRIVER) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

#[cfg(feature = "acx")]
#[allow(dead_code)]
pub unsafe fn initialize_device(device: WDFDEVICE) -> NTSTATUS {
    unsafe { runtime::initialize_device(device) }
}

#[cfg(not(feature = "acx"))]
#[allow(dead_code)]
pub unsafe fn initialize_device(_device: WDFDEVICE) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

#[cfg(feature = "acx")]
#[allow(dead_code)]
pub unsafe fn initialize_device_init(device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    unsafe { runtime::initialize_device_init(device_init) }
}

#[cfg(not(feature = "acx"))]
#[allow(dead_code)]
pub unsafe fn initialize_device_init(_device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

#[cfg(feature = "acx")]
pub unsafe fn add_device(driver: WDFDRIVER, device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    unsafe { runtime::add_device(driver, device_init) }
}

#[cfg(not(feature = "acx"))]
pub unsafe fn add_device(_driver: WDFDRIVER, _device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

// Kept as explicit fail-closed compatibility surfaces for older validation
// harnesses. Production code uses the complete runtime above, not probes.
#[allow(dead_code)]
pub unsafe fn circuit_binding_probe(_device: WDFDEVICE, _circuit: *mut *mut c_void) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

#[allow(dead_code)]
pub unsafe fn pin_binding_probe(_circuit: *mut c_void, _pin: *mut *mut c_void) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

#[allow(dead_code)]
pub unsafe fn data_format_binding_probe(
    _device: WDFDEVICE,
    _data_format: *mut *mut c_void,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}

#[allow(dead_code)]
pub unsafe fn rt_stream_binding_probe(
    _device: WDFDEVICE,
    _circuit: *mut c_void,
    _stream_init: *mut c_void,
    _stream: *mut *mut c_void,
) -> NTSTATUS {
    STATUS_NOT_SUPPORTED
}
