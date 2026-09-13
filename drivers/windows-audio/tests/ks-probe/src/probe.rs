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

#[derive(Default)]
struct Samples {
    first: u32,
    final_prefix: u32,
}
impl Samples {
    fn observe(&mut self, value: i16) -> Result<()> {
        match value {
            0 => Ok(()),
            1010 if self.first < 960 && self.final_prefix == 0 => {
                self.first += 1;
                Ok(())
            }
            2020 if self.first == 960 && self.final_prefix < 960 => {
                self.final_prefix += 1;
                Ok(())
            }
            _ => Err(format!(
                "unexpected, replayed, or reordered PCM16 sample {value}"
            )),
        }
    }
    fn finish(&self, bytes: u32) -> Result<()> {
        if bytes > 1920
            || !bytes.is_multiple_of(4)
            || self.first != 960
            || self.final_prefix != bytes / 2
        {
            return Err(format!(
                "EOS sample mismatch: first={}, final={}, expected final bytes={bytes}",
                self.first, self.final_prefix
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

struct Pin {
    handle: OwnedHandle,
    buffer: KSRTAUDIO_BUFFER,
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

fn create_pin(path: &str, capture: bool) -> Result<Pin> {
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
    let request = PinRequest {
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
                    SampleSize: 4,
                    MajorFormat: KSDATAFORMAT_TYPE_AUDIO,
                    SubFormat: KSDATAFORMAT_SUBTYPE_PCM,
                    Specifier: KSDATAFORMAT_SPECIFIER_WAVEFORMATEX,
                    ..Default::default()
                },
            },
            wave: WAVEFORMATEXTENSIBLE {
                Format: WAVEFORMATEX {
                    wFormatTag: 0xfffe,
                    nChannels: 2,
                    nSamplesPerSec: 48_000,
                    nAvgBytesPerSec: 192_000,
                    nBlockAlign: 4,
                    wBitsPerSample: 16,
                    cbSize: 22,
                },
                Samples: WAVEFORMATEXTENSIBLE_0 {
                    wValidBitsPerSample: 16,
                },
                dwChannelMask: 3,
                SubFormat: KSDATAFORMAT_SUBTYPE_PCM,
            },
        },
    };
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
    };
    pin.buffer = get(
        pin.handle.0,
        &KSRTAUDIO_BUFFER_PROPERTY_WITH_NOTIFICATION {
            Property: identifier(
                KSPROPSETID_RtAudio,
                KSPROPERTY_RTAUDIO_BUFFER_WITH_NOTIFICATION.0 as u32,
                KSPROPERTY_TYPE_GET,
            ),
            RequestedBufferSize: 3840,
            NotificationCount: 2,
            ..Default::default()
        },
    )?;
    if pin.buffer.BufferAddress.is_null() || pin.buffer.ActualBufferSize != 3840 {
        return Err(format!("unexpected buffer mapping: {:?}", pin.buffer));
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
    let render = create_pin(owned_path(paths, render_name)?, false)?;
    let capture = create_pin(owned_path(paths, capture_name)?, true)?;
    // The stream is PAUSE and owns its mapping. Volatile accesses avoid Rust
    // references to memory also accessed asynchronously by the kernel.
    let buffer = render.buffer.BufferAddress.cast::<i16>();
    for index in 0..1920 {
        let sample = if index < 960 {
            1010
        } else if index < 960 + eos_bytes as usize / 2 {
            2020
        } else {
            3030
        };
        unsafe {
            buffer.add(index).write_volatile(sample);
        }
    }
    fence(Ordering::SeqCst);
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
    capture.state(KSSTATE_RUN)?;
    render.state(KSSTATE_RUN)?;
    let deadline = Instant::now() + Duration::from_millis(300);
    let mut previous_count = 0;
    let mut samples = Samples::default();
    let mut last_nonzero_packet = 0;
    while Instant::now() < deadline {
        let count = capture.packets()?;
        if count != previous_count {
            if count != previous_count + 1 {
                return Err(format!("capture observation lost packets: {previous_count} -> {count}; cannot certify EOS data"));
            }
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
            let base = ((count - 1) % 2) as usize * 960;
            for index in 0..960 {
                let value = unsafe {
                    capture
                        .buffer
                        .BufferAddress
                        .cast::<i16>()
                        .add(base + index)
                        .read_volatile()
                };
                samples.observe(value)?;
                if value != 0 {
                    last_nonzero_packet = count;
                }
            }
            fence(Ordering::SeqCst);
            if capture.packets()? != count {
                return Err(
                    "capture packet changed during inspection; timing evidence is inconclusive"
                        .into(),
                );
            }
            previous_count = count;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let render_count = render.packets()?;
    samples.finish(eos_bytes)?;
    if previous_count < last_nonzero_packet + 10 || render_count < 20 {
        return Err(format!("EOS progress failure: capture packets={previous_count}, last nonzero={last_nonzero_packet}, render packets={render_count}"));
    }
    render.stop()?;
    capture.stop()?;
    println!("  EOS passed: first={} samples, final={} samples, poisoned tail=0, render/capture packets={render_count}/{previous_count}; at least 10 later capture packets silent; explicit STOP passed", samples.first, samples.final_prefix);
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
            for bytes in [0, 4, 16, 960, 1920] {
                verify_eos(&paths, render, capture, bytes)?;
            }
        }
        println!("Direct two-packet KS EOS checks passed on both cables.");
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
            let pin = create_pin(owned_path(&paths, name)?, capture)?;
            pin.stop()?;
        }
        return Ok(());
    }
    if args.iter().any(|arg| arg != "--inspect") {
        return Err(
            "usage: qpwgraph-audio-ks-probe [--inspect | --open-pins | --verify-eos]".into(),
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
            let mut samples = Samples::default();
            samples.observe(0).unwrap();
            for _ in 0..960 {
                samples.observe(1010).unwrap();
            }
            for _ in 0..bytes / 2 {
                samples.observe(2020).unwrap();
            }
            samples.observe(0).unwrap();
            samples.finish(bytes).unwrap();
        }
    }

    #[test]
    fn missing_reordered_poisoned_and_replayed_samples_fail() {
        let mut samples = Samples::default();
        assert!(samples.finish(0).is_err());
        assert!(samples.observe(2020).is_err());
        assert!(samples.observe(3030).is_err());
        assert!(samples.observe(-1).is_err());
        for _ in 0..960 {
            samples.observe(1010).unwrap();
        }
        assert!(samples.observe(1010).is_err());
        samples.observe(2020).unwrap();
        assert!(samples.observe(1010).is_err());
        assert!(samples.finish(4).is_err());
        samples.observe(2020).unwrap();
        samples.finish(4).unwrap();
        assert!(samples.finish(3).is_err());
        assert!(samples.finish(1924).is_err());
        assert!(samples.finish(0).is_err());
    }
}
