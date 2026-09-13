use std::mem::size_of;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0};
use windows::Win32::Media::KernelStreaming::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::IO::DeviceIoControl;

type Result<T> = std::result::Result<T, String>;
const ROOT: &str = r"ROOT\DEVGEN\QPWGRAPH_AUDIO";

struct Samples {
    first_expected: u32,
    final_expected: u32,
    first: u32,
    final_prefix: u32,
    first_packet: Option<u32>,
    final_packet: Option<u32>,
}
impl Samples {
    fn new(first_expected: u32, final_expected: u32) -> Self {
        Self {
            first_expected,
            final_expected,
            first: 0,
            final_prefix: 0,
            first_packet: None,
            final_packet: None,
        }
    }
    fn observe(&mut self, packet: u32, index: u32, value: i16) -> Result<()> {
        let expected = if let Some(first_packet) = self.first_packet {
            if packet == first_packet {
                1010
            } else if let Some(final_packet) = self.final_packet {
                if packet == final_packet && index < self.final_expected {
                    2020
                } else {
                    0
                }
            } else if value == 2020 && index == 0 && self.first == self.first_expected {
                self.final_packet = Some(packet);
                2020
            } else {
                0
            }
        } else if value == 1010 && index == 0 {
            self.first_packet = Some(packet);
            1010
        } else {
            0
        };
        if value != expected {
            return Err(format!(
                "unexpected, replayed, or reordered PCM16 sample {value} at packet {packet} sample {index}; expected {expected}"
            ));
        }
        if self.first_packet == Some(packet) {
            if self.first >= self.first_expected {
                return Err("replayed first-packet PCM samples".into());
            }
            self.first += 1;
        } else if self.final_packet == Some(packet) && index < self.final_expected {
            self.final_prefix += 1;
        }
        Ok(())
    }
    fn finish(&self) -> Result<()> {
        if self.first != self.first_expected
            || self.final_prefix != self.final_expected
            || self.first_packet.is_none()
            || (self.final_expected != 0 && self.final_packet.is_none())
        {
            return Err(format!(
                "EOS sample mismatch: first={}, expected {}, final={}, expected {}",
                self.first, self.first_expected, self.final_prefix, self.final_expected
            ));
        }
        Ok(())
    }
}

struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn identifier(set: GUID, id: u32, flags: u32) -> KSIDENTIFIER {
    KSIDENTIFIER {
        Anonymous: KSIDENTIFIER_0 {
            Anonymous: KSIDENTIFIER_0_0 {
                Set: set,
                Id: id,
                Flags: flags,
            },
        },
    }
}

fn get<Q, R: Default + Copy>(handle: HANDLE, query: &Q) -> Result<R> {
    let mut output = R::default();
    let mut returned = 0;
    unsafe {
        DeviceIoControl(
            handle,
            IOCTL_KS_PROPERTY,
            Some(std::ptr::from_ref(query).cast()),
            size_of::<Q>() as u32,
            Some(std::ptr::from_mut(&mut output).cast()),
            size_of::<R>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|e| format!("KS property: {e}"))?;
    if returned != size_of::<R>() as u32 {
        return Err(format!(
            "short KS property: {returned} bytes, expected {}",
            size_of::<R>()
        ));
    }
    Ok(output)
}

fn interfaces() -> Result<Vec<String>> {
    let root = wide(ROOT);
    for _ in 0..3 {
        let mut count = 0;
        let status = unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &mut count,
                &KSCATEGORY_AUDIO,
                PCWSTR(root.as_ptr()),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if status != CR_SUCCESS {
            return Err(format!("interface list size: {status:?}"));
        }
        if count == 0 || count > 65_536 {
            return Err(format!("invalid interface list length {count}"));
        }
        let mut buffer = vec![0_u16; count as usize];
        let status = unsafe {
            CM_Get_Device_Interface_ListW(
                &KSCATEGORY_AUDIO,
                PCWSTR(root.as_ptr()),
                &mut buffer,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if status == CR_BUFFER_SMALL {
            continue;
        }
        if status != CR_SUCCESS {
            return Err(format!("interface list: {status:?}"));
        }
        let paths: Vec<_> = buffer
            .split(|c| *c == 0)
            .take_while(|s| !s.is_empty())
            .map(String::from_utf16)
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| format!("interface UTF-16: {e}"))?;
        if paths.is_empty() {
            return Err(format!("no active KSCATEGORY_AUDIO interfaces for {ROOT}"));
        }
        return Ok(paths);
    }
    Err("device interfaces kept changing".into())
}

fn open_filter(path: &str) -> Result<OwnedHandle> {
    let path = wide(path);
    unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map(OwnedHandle)
    .map_err(|e| format!("open KS filter: {e}"))
}

fn set<R: Copy>(handle: HANDLE, query: &KSIDENTIFIER, mut value: R) -> Result<()> {
    let mut returned = 0;
    unsafe {
        DeviceIoControl(
            handle,
            IOCTL_KS_PROPERTY,
            Some(std::ptr::from_ref(query).cast()),
            size_of::<KSIDENTIFIER>() as u32,
            Some(std::ptr::from_mut(&mut value).cast()),
            size_of::<R>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|e| format!("set KS property: {e}"))
}

#[repr(C, packed)]
struct PcmFormat {
    data: KSDATAFORMAT,
    wave: WAVEFORMATEXTENSIBLE,
}
#[repr(C)]
struct PinRequest {
    connect: KSPIN_CONNECT,
    format: PcmFormat,
}

fn pin_request(sample_rate: u32, channels: u16, bits: u16) -> PinRequest {
    let bytes_per_sample = bits.div_ceil(8);
    let block_align = channels.saturating_mul(bytes_per_sample);
    PinRequest {
        connect: KSPIN_CONNECT {
            Interface: identifier(
                KSINTERFACESETID_Standard,
                KSINTERFACE_STANDARD_LOOPED_STREAMING.0 as u32,
                0,
            ),
            Medium: identifier(KSMEDIUMSETID_Standard, KSMEDIUM_TYPE_ANYINSTANCE, 0),
            PinId: 0,
            Priority: KSPRIORITY {
                PriorityClass: KSPRIORITY_NORMAL,
                PrioritySubClass: 1,
            },
            ..Default::default()
        },
        format: PcmFormat {
            data: KSDATAFORMAT {
                Anonymous: KSDATAFORMAT_0 {
                    FormatSize: size_of::<PcmFormat>() as u32,
                    SampleSize: u32::from(block_align),
                    MajorFormat: KSDATAFORMAT_TYPE_AUDIO,
                    SubFormat: KSDATAFORMAT_SUBTYPE_PCM,
                    Specifier: KSDATAFORMAT_SPECIFIER_WAVEFORMATEX,
                    ..Default::default()
                },
            },
            wave: WAVEFORMATEXTENSIBLE {
                Format: WAVEFORMATEX {
                    wFormatTag: 0xfffe,
                    nChannels: channels,
                    nSamplesPerSec: sample_rate,
                    nAvgBytesPerSec: sample_rate.saturating_mul(u32::from(block_align)),
                    nBlockAlign: block_align,
                    wBitsPerSample: bits,
                    cbSize: 22,
                },
                Samples: WAVEFORMATEXTENSIBLE_0 {
                    wValidBitsPerSample: bits,
                },
                dwChannelMask: if channels == 2 { 3 } else { 0 },
                SubFormat: KSDATAFORMAT_SUBTYPE_PCM,
            },
        },
    }
}

struct Pin {
    handle: OwnedHandle,
    buffer: KSRTAUDIO_BUFFER,
    notification_count: u32,
    packet_bytes: u32,
}
impl Pin {
    fn stop(&self) -> Result<()> {
        self.state(KSSTATE_PAUSE)?;
        self.state(KSSTATE_ACQUIRE)?;
        self.state(KSSTATE_STOP)
    }
    fn state(&self, state: KSSTATE) -> Result<()> {
        set(
            self.handle.0,
            &identifier(
                KSPROPSETID_Connection,
                KSPROPERTY_CONNECTION_STATE.0 as u32,
                KSPROPERTY_TYPE_SET,
            ),
            state,
        )
        .map_err(|e| format!("state {}: {e}", state.0))
    }
    fn packets(&self) -> Result<u32> {
        get(
            self.handle.0,
            &identifier(
                KSPROPSETID_RtAudio,
                KSPROPERTY_RTAUDIO_PACKETCOUNT.0 as u32,
                KSPROPERTY_TYPE_GET,
            ),
        )
    }
    fn write_packet(&self, packet: u32, flags: u32, bytes: u32) -> Result<()> {
        set(
            self.handle.0,
            &identifier(
                KSPROPSETID_RtAudio,
                KSPROPERTY_RTAUDIO_SETWRITEPACKET.0 as u32,
                KSPROPERTY_TYPE_SET,
            ),
            KSRTAUDIO_SETWRITEPACKET_INFO {
                PacketNumber: packet,
                Flags: flags,
                EosPacketLength: bytes,
            },
        )
    }
}
impl Drop for Pin {
    fn drop(&mut self) {
        let _ = self.state(KSSTATE_PAUSE);
        let _ = self.state(KSSTATE_ACQUIRE);
        let _ = self.state(KSSTATE_STOP);
    }
}

fn create_pin(
    path: &str,
    capture: bool,
    notification_count: u32,
    requested_buffer_size: u32,
) -> Result<Pin> {
    if !matches!(notification_count, 1 | 2) {
        return Err(format!(
            "unsupported notification count {notification_count}"
        ));
    }
    let filter = open_filter(path)?;
    // The owned circuit contract fixes pin 0 as the host pin; verify it before
    // creation rather than opening a pin based solely on its index.
    let query = |id| KSP_PIN {
        Property: identifier(KSPROPSETID_Pin, id, KSPROPERTY_TYPE_GET),
        PinId: 0,
        ..Default::default()
    };
    let flow: KSPIN_DATAFLOW = get(filter.0, &query(KSPROPERTY_PIN_DATAFLOW.0 as u32))?;
    let communication: KSPIN_COMMUNICATION =
        get(filter.0, &query(KSPROPERTY_PIN_COMMUNICATION.0 as u32))?;
    if flow
        != if capture {
            KSPIN_DATAFLOW_OUT
        } else {
            KSPIN_DATAFLOW_IN
        }
        || communication != KSPIN_COMMUNICATION_SINK
    {
        return Err("unexpected host pin contract".into());
    }
    let request = pin_request(48_000, 2, 16);
    let mut handle = HANDLE::default();
    let code = unsafe {
        KsCreatePin(
            filter.0,
            &request.connect,
            if capture {
                GENERIC_READ.0
            } else {
                GENERIC_WRITE.0
            },
            &mut handle,
        )
    };
    if code != 0 {
        return Err(format!("KsCreatePin: Win32 {code}"));
    }
    let mut pin = Pin {
        handle: OwnedHandle(handle),
        buffer: KSRTAUDIO_BUFFER::default(),
        notification_count,
        packet_bytes: 0,
    };
    pin.buffer = get(
        pin.handle.0,
        &KSRTAUDIO_BUFFER_PROPERTY_WITH_NOTIFICATION {
            Property: identifier(
                KSPROPSETID_RtAudio,
                KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION.0 as u32,
                KSPROPERTY_TYPE_GET,
            ),
            RequestedBufferSize: requested_buffer_size,
            NotificationCount: notification_count,
            ..Default::default()
        },
    )?;
    let expected_buffer_size = match notification_count {
        1 => 4096,
        2 => 3840,
        _ => unreachable!(),
    };
    if pin.buffer.BufferAddress.is_null() || pin.buffer.ActualBufferSize != expected_buffer_size {
        return Err(format!("unexpected buffer mapping: {:?}", pin.buffer));
    }
    pin.packet_bytes = pin.buffer.ActualBufferSize / notification_count;
    if pin.packet_bytes == 0 || !pin.packet_bytes.is_multiple_of(4) {
        return Err(format!(
            "unexpected packet mapping size {}",
            pin.packet_bytes
        ));
    }
    pin.state(KSSTATE_ACQUIRE)?;
    pin.state(KSSTATE_PAUSE)?;
    println!(
        "  mapped {} bytes; capture={capture}, packet count={}",
        pin.buffer.ActualBufferSize,
        pin.packets()?
    );
    Ok(pin)
}

fn observe_capture_packet(capture: &Pin, count: u32, samples: &mut Samples) -> Result<bool> {
    use std::sync::atomic::{fence, Ordering};
    let read: KSRTAUDIO_GETREADPACKET_INFO = get(
        capture.handle.0,
        &identifier(
            KSPROPSETID_RtAudio,
            KSPROPERTY_RTAUDIO_GETREADPACKET.0 as u32,
            KSPROPERTY_TYPE_GET,
        ),
    )?;
    if read.PacketNumber != count - 1 {
        return Err(format!(
            "capture packet index disagrees with completed count: {} vs {count}",
            read.PacketNumber
        ));
    }
    fence(Ordering::SeqCst);
    let base = ((count - 1) % capture.notification_count) * (capture.packet_bytes / 2);
    let mut nonzero = false;
    for index in 0..capture.packet_bytes / 2 {
        let value = unsafe {
            capture
                .buffer
                .BufferAddress
                .cast::<i16>()
                .add((base + index) as usize)
                .read_volatile()
        };
        nonzero |= value != 0;
        samples.observe(count - 1, index, value)?;
    }
    fence(Ordering::SeqCst);
    if capture.packets()? != count {
        return Err(
            "capture packet changed during inspection; timing evidence is inconclusive".into(),
        );
    }
    Ok(nonzero)
}

fn owned_path<'a>(paths: &'a [String], name: &str) -> Result<&'a str> {
    let suffix = format!("\\{name}");
    let matches: Vec<_> = paths
        .iter()
        .filter(|p| {
            p.to_ascii_lowercase()
                .ends_with(&suffix.to_ascii_lowercase())
        })
        .collect();
    if matches.len() != 1 {
        return Err(format!(
            "expected one owned circuit {name}, found {}",
            matches.len()
        ));
    }
    Ok(matches[0])
}

fn verify_non_eos_length_is_ignored(paths: &[String], render_name: &str) -> Result<()> {
    const IGNORED_LENGTH: u32 = u32::MAX;
    println!("direct non-EOS length: {render_name}, ignored bytes={IGNORED_LENGTH}");
    let render = create_pin(owned_path(paths, render_name)?, false, 2, 3840)?;
    render
        .write_packet(1, 0, IGNORED_LENGTH)
        .map_err(|e| format!("submit non-EOS packet with ignored length: {e}"))?;
    render.stop()?;
    println!("  non-EOS ignored length accepted; explicit STOP passed");
    Ok(())
}

fn verify_unsupported_format_is_rejected(
    paths: &[String],
    name: &str,
    capture: bool,
) -> Result<()> {
    const UNSUPPORTED_SAMPLE_RATE: u32 = 44_100;
    println!(
        "direct unsupported format: {name}, capture={capture}, sample rate={UNSUPPORTED_SAMPLE_RATE}"
    );
    let filter = open_filter(owned_path(paths, name)?)?;
    let request = pin_request(UNSUPPORTED_SAMPLE_RATE, 2, 16);
    let mut handle = HANDLE::default();
    let code = unsafe {
        KsCreatePin(
            filter.0,
            &request.connect,
            if capture {
                GENERIC_READ.0
            } else {
                GENERIC_WRITE.0
            },
            &mut handle,
        )
    };
    if code == 0 {
        let _handle = OwnedHandle(handle);
        return Err(format!(
            "driver accepted unsupported {UNSUPPORTED_SAMPLE_RATE} Hz format on {name}"
        ));
    }
    println!("  unsupported format rejected with Win32 code {code}");
    Ok(())
}

fn verify_eos(
    paths: &[String],
    render_name: &str,
    capture_name: &str,
    eos_bytes: u32,
) -> Result<()> {
    use std::sync::atomic::{fence, Ordering};
    use std::time::{Duration, Instant};
    const PACKET_BYTES: u32 = 1920;
    const EOS: u32 = KSSTREAM_HEADER_OPTIONSF_ENDOFSTREAM;
    println!("direct EOS: {render_name} -> {capture_name}, final bytes={eos_bytes}");
    let render = create_pin(owned_path(paths, render_name)?, false, 2, 3840)?;
    let capture = create_pin(owned_path(paths, capture_name)?, true, 2, 3840)?;
    // The stream is PAUSE and owns its mapping. Volatile accesses avoid Rust
    // references to memory also accessed asynchronously by the kernel.
    let buffer = render.buffer.BufferAddress.cast::<i16>();
    for index in 0..render.buffer.ActualBufferSize / 2 {
        let sample = if index < 960 {
            1010
        } else if index < 960 + eos_bytes / 2 {
            2020
        } else {
            3030
        };
        unsafe {
            buffer.add(index as usize).write_volatile(sample);
        }
    }
    fence(Ordering::SeqCst);
    if render.write_packet(3, EOS, 0).is_ok() {
        return Err("driver accepted a skipped EOS packet".into());
    }
    if render.write_packet(0, EOS, 0).is_ok() {
        return Err("driver accepted a late EOS packet".into());
    }
    if render.write_packet(1, EOS, PACKET_BYTES + 4).is_ok() {
        return Err("driver accepted an oversized EOS packet".into());
    }
    if render.write_packet(1, EOS, 3).is_ok() {
        return Err("driver accepted a non-frame-aligned EOS packet".into());
    }
    if render.write_packet(1, 0x400, 0).is_ok() {
        return Err("driver accepted undefined render packet flags".into());
    }
    render
        .write_packet(1, EOS, eos_bytes)
        .map_err(|e| format!("submit explicit EOS: {e}"))?;
    if render.write_packet(2, 0, 0).is_ok() {
        return Err("driver accepted a render packet after EOS was submitted".into());
    }
    render.state(KSSTATE_RUN)?;
    capture.state(KSSTATE_RUN)?;
    let deadline = Instant::now() + Duration::from_millis(300);
    let mut previous_count = 0;
    let mut samples = Samples::new(960, eos_bytes / 2);
    let mut last_nonzero_packet = 0;
    while Instant::now() < deadline {
        let count = capture.packets()?;
        if count != previous_count {
            if count != previous_count + 1 {
                return Err(format!("capture observation lost packets: {previous_count} -> {count}; cannot certify EOS data"));
            }
            let nonzero = observe_capture_packet(&capture, count, &mut samples)?;
            if nonzero {
                last_nonzero_packet = count;
            }
            previous_count = count;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let render_count = render.packets()?;
    samples.finish()?;
    if previous_count < last_nonzero_packet + 10 || render_count < 20 {
        return Err(format!("EOS progress failure: capture packets={previous_count}, last nonzero={last_nonzero_packet}, render packets={render_count}"));
    }
    render.stop()?;
    capture.stop()?;
    println!("  two-packet EOS passed: first={} samples, final={} samples, poisoned tail=0, render/capture packets={render_count}/{previous_count}; at least 10 later capture packets silent; explicit STOP passed", samples.first, samples.final_prefix);
    Ok(())
}

fn verify_single_packet_eos(
    paths: &[String],
    render_name: &str,
    capture_name: &str,
    eos_bytes: u32,
) -> Result<()> {
    use std::sync::atomic::{fence, Ordering};
    use std::time::{Duration, Instant};
    const REQUESTED_BYTES: u32 = 1920;
    const PACKET_BYTES: u32 = 4096;
    const EOS: u32 = KSSTREAM_HEADER_OPTIONSF_ENDOFSTREAM;
    println!("direct single-packet EOS: {render_name} -> {capture_name}, final bytes={eos_bytes}");
    if eos_bytes > PACKET_BYTES || !eos_bytes.is_multiple_of(4) {
        return Err(format!("invalid single-packet EOS test length {eos_bytes}"));
    }
    let render = create_pin(owned_path(paths, render_name)?, false, 1, REQUESTED_BYTES)?;
    let capture = create_pin(owned_path(paths, capture_name)?, true, 1, REQUESTED_BYTES)?;
    if render.packet_bytes != PACKET_BYTES || capture.packet_bytes != PACKET_BYTES {
        return Err(format!(
            "single-packet page mapping is not 4096 bytes: render={}, capture={}",
            render.packet_bytes, capture.packet_bytes
        ));
    }
    let render_samples = render.packet_bytes / 2;
    let buffer = render.buffer.BufferAddress.cast::<i16>();
    for index in 0..render_samples {
        unsafe {
            buffer.add(index as usize).write_volatile(1010);
        }
    }
    fence(Ordering::SeqCst);

    // With one notification, packet 1 is the final packet and the same mapped
    // page is reused after packet 0 completes. The client replaces the mapped
    // contents at that boundary; the driver applies EOS to packet 1 and must
    // never copy the poisoned tail.
    if render.write_packet(3, EOS, 0).is_ok() {
        return Err("driver accepted a skipped single-packet submission".into());
    }
    if render.write_packet(0, 0, 0).is_ok() {
        return Err("driver accepted a late single-packet submission".into());
    }
    if render.write_packet(1, EOS, PACKET_BYTES + 4).is_ok() {
        return Err("driver accepted an oversized single-packet EOS".into());
    }
    if render.write_packet(1, EOS, 3).is_ok() {
        return Err("driver accepted an unaligned single-packet EOS".into());
    }
    if render.write_packet(1, 0x400, 0).is_ok() {
        return Err("driver accepted undefined single-packet flags".into());
    }
    render
        .write_packet(1, EOS, eos_bytes)
        .map_err(|e| format!("submit single-packet EOS: {e}"))?;
    if render.write_packet(2, 0, 0).is_ok() {
        return Err("driver accepted a packet after single-packet EOS".into());
    }

    render.state(KSSTATE_RUN)?;
    capture.state(KSSTATE_RUN)?;
    let deadline = Instant::now() + Duration::from_millis(300);
    let mut samples = Samples::new(render_samples, eos_bytes / 2);
    let mut previous_count = 0;
    let mut last_nonzero_packet = 0;
    let mut final_buffer_written = false;
    while Instant::now() < deadline {
        let render_count = render.packets()?;
        let capture_count = capture.packets()?;
        if render_count > capture_count.saturating_add(1) {
            return Err(format!(
                "single-packet render advanced too far before capture: render/capture={render_count}/{capture_count}"
            ));
        }
        if render_count >= 1 && !final_buffer_written {
            for index in 0..render_samples {
                let sample = if index < eos_bytes / 2 { 2020 } else { 3030 };
                unsafe {
                    buffer.add(index as usize).write_volatile(sample);
                }
            }
            fence(Ordering::SeqCst);
            final_buffer_written = true;
        }
        let count = capture_count;
        if count != previous_count {
            if count != previous_count + 1 {
                return Err(format!(
                    "single-packet capture observation lost packets: {previous_count} -> {count}"
                ));
            }
            let nonzero = observe_capture_packet(&capture, count, &mut samples)?;
            if nonzero {
                last_nonzero_packet = count;
            }
            previous_count = count;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let render_count = render.packets()?;
    if !final_buffer_written {
        return Err("single-packet render boundary was not observed before timeout".into());
    }
    samples.finish()?;
    if previous_count < last_nonzero_packet + 10 || render_count < 10 {
        return Err(format!(
            "single-packet EOS progress failure: capture packets={previous_count}, last nonzero={last_nonzero_packet}, render packets={render_count}"
        ));
    }
    render.stop()?;
    capture.stop()?;
    println!(
        "  single-packet EOS passed: first={} samples, final={} samples, poisoned tail=0, render/capture packets={render_count}/{previous_count}; at least 10 later capture packets silent; explicit STOP passed",
        samples.first, samples.final_prefix
    );
    Ok(())
}

pub fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--verify-eos"] {
        let paths = interfaces()?;
        for (render, capture) in [
            ("QPWGraphVirtualOutput", "QPWGraphVirtualMonitor"),
            ("QPWGraphRelaySink", "QPWGraphRelayMicrophone"),
        ] {
            verify_non_eos_length_is_ignored(&paths, render)?;
            for bytes in [0, 4, 16, 960, 1920] {
                verify_eos(&paths, render, capture, bytes)?;
            }
            for bytes in [0, 4, 16, 2048, 4096] {
                verify_single_packet_eos(&paths, render, capture, bytes)?;
            }
        }
        println!("Direct single- and two-packet KS EOS checks passed on both cables.");
        return Ok(());
    }
    if args == ["--verify-formats"] {
        let paths = interfaces()?;
        for (name, capture) in [
            ("QPWGraphVirtualOutput", false),
            ("QPWGraphVirtualMonitor", true),
            ("QPWGraphRelaySink", false),
            ("QPWGraphRelayMicrophone", true),
        ] {
            verify_unsupported_format_is_rejected(&paths, name, capture)?;
        }
        println!("Unsupported 44.1 kHz format rejected on all four endpoints.");
        return Ok(());
    }
    if args == ["--open-pins"] {
        let paths = interfaces()?;
        for (name, capture) in [
            ("QPWGraphVirtualOutput", false),
            ("QPWGraphVirtualMonitor", true),
            ("QPWGraphRelaySink", false),
            ("QPWGraphRelayMicrophone", true),
        ] {
            println!("opening owned circuit {name}");
            let pin = create_pin(owned_path(&paths, name)?, capture, 2, 3840)?;
            pin.stop()?;
        }
        return Ok(());
    }
    if args.iter().any(|arg| arg != "--inspect") {
        return Err(
            "usage: qpwgraph-audio-ks-probe [--inspect | --open-pins | --verify-eos | --verify-formats]".into(),
        );
    }
    for path in interfaces()? {
        println!("owned filter: {path}");
        let filter = open_filter(&path)?;
        let count: u32 = get(
            filter.0,
            &identifier(
                KSPROPSETID_Pin,
                KSPROPERTY_PIN_CTYPES.0 as u32,
                KSPROPERTY_TYPE_GET,
            ),
        )?;
        if count > 32 {
            return Err(format!("unexpected pin count: {count}"));
        }
        for pin in 0..count {
            let query = |id| KSP_PIN {
                Property: identifier(KSPROPSETID_Pin, id, KSPROPERTY_TYPE_GET),
                PinId: pin,
                ..Default::default()
            };
            let flow: KSPIN_DATAFLOW = get(filter.0, &query(KSPROPERTY_PIN_DATAFLOW.0 as u32))?;
            let communication: KSPIN_COMMUNICATION =
                get(filter.0, &query(KSPROPERTY_PIN_COMMUNICATION.0 as u32))?;
            println!(
                "  pin {pin}: flow={} communication={}",
                flow.0, communication.0
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_pin_format_layout() {
        assert_eq!(size_of::<PcmFormat>(), 104);
        assert_eq!(
            std::mem::offset_of!(PinRequest, format),
            size_of::<KSPIN_CONNECT>()
        );
    }

    #[test]
    fn circuit_identity_requires_unique_exact_suffix() {
        let paths = vec!["owned\\QPWGraphVirtualOutput".into()];
        assert!(owned_path(&paths, "qpwgraphvirtualoutput").is_ok());
        assert!(owned_path(&paths, "VirtualOutput").is_err());
        assert!(owned_path(
            &[paths[0].clone(), paths[0].clone()],
            "QPWGraphVirtualOutput"
        )
        .is_err());
    }

    #[test]
    fn all_boundary_lengths_accept_exact_ordered_samples() {
        for bytes in [0, 4, 16, 960, 1920] {
            let mut samples = Samples::new(960, bytes / 2);
            for index in 0..960 {
                samples.observe(0, index, 1010).unwrap();
            }
            for index in 0..bytes / 2 {
                samples.observe(1, index, 2020).unwrap();
            }
            for index in bytes / 2..960 {
                samples.observe(1, index, 0).unwrap();
            }
            samples.finish().unwrap();
        }
    }

    #[test]
    fn missing_reordered_poisoned_and_replayed_samples_fail() {
        let mut samples = Samples::new(960, 2);
        assert!(samples.finish().is_err());
        assert!(samples.observe(0, 0, 2020).is_err());
        assert!(samples.observe(0, 0, 3030).is_err());
        assert!(samples.observe(0, 0, -1).is_err());
        for index in 0..960 {
            samples.observe(0, index, 1010).unwrap();
        }
        assert!(samples.observe(0, 960, 1010).is_err());
        samples.observe(1, 0, 2020).unwrap();
        assert!(samples.observe(1, 1, 1010).is_err());
        assert!(samples.finish().is_err());
        samples.observe(1, 1, 2020).unwrap();
        samples.finish().unwrap();
    }

    #[test]
    fn single_packet_oracle_requires_full_first_packet_and_zero_tail() {
        let mut samples = Samples::new(2048, 2);
        for index in 0..2048 {
            samples.observe(0, index, 1010).unwrap();
        }
        samples.observe(1, 0, 2020).unwrap();
        samples.observe(1, 1, 2020).unwrap();
        for index in 2..2048 {
            samples.observe(1, index, 0).unwrap();
        }
        samples.finish().unwrap();
    }

    #[test]
    fn oracle_allows_only_leading_underflow_silence() {
        let mut samples = Samples::new(2, 2);
        samples.observe(0, 0, 0).unwrap();
        samples.observe(0, 1, 0).unwrap();
        samples.observe(1, 0, 1010).unwrap();
        samples.observe(1, 1, 1010).unwrap();
        samples.observe(2, 0, 0).unwrap();
        samples.observe(2, 1, 0).unwrap();
        samples.observe(3, 0, 2020).unwrap();
        samples.observe(3, 1, 2020).unwrap();
        samples.observe(3, 2, 0).unwrap();
        assert!(samples.observe(3, 3, 2020).is_err());
        samples.finish().unwrap();
    }
}
