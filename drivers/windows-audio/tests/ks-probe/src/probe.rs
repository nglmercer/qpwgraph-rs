use std::mem::size_of;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0};
use windows::Win32::Media::KernelStreaming::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Performance::QueryPerformanceFrequency;
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

const SUSTAINED_EOS_MARKER_BASE: i16 = 1_000;
const SUSTAINED_EOS_DEFAULT_PACKETS: u32 = 128;
const SUSTAINED_EOS_DEFAULT_TIMEOUT_MS: u64 = 20_000;
const SUSTAINED_EOS_DEFAULT_PREROLL: u32 = 2;
const SUSTAINED_EOS_DEFAULT_FINAL_BYTES: u32 = 960;
const SUSTAINED_EOS_MIN_PACKETS: u32 = 4;
const SUSTAINED_EOS_MAX_PACKETS: u32 = 20_000;
const SUSTAINED_EOS_MIN_TIMEOUT_MS: u64 = 1_000;
const SUSTAINED_EOS_MAX_TIMEOUT_MS: u64 = 600_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SustainedEosConfig {
    packets: u32,
    timeout_ms: u64,
    preroll: u32,
    final_bytes: u32,
}

impl Default for SustainedEosConfig {
    fn default() -> Self {
        Self {
            packets: SUSTAINED_EOS_DEFAULT_PACKETS,
            timeout_ms: SUSTAINED_EOS_DEFAULT_TIMEOUT_MS,
            preroll: SUSTAINED_EOS_DEFAULT_PREROLL,
            final_bytes: SUSTAINED_EOS_DEFAULT_FINAL_BYTES,
        }
    }
}

fn parse_sustained_eos_args(args: &[String]) -> Result<SustainedEosConfig> {
    let mut config = SustainedEosConfig::default();
    if args.first().map(String::as_str) != Some("--verify-sustained-eos") {
        return Err("sustained EOS mode requires --verify-sustained-eos".into());
    }
    let mut index = 1;
    while index < args.len() {
        let option = args[index].as_str();
        index += 1;
        let value = args
            .get(index)
            .ok_or_else(|| format!("missing value for {option}"))?;
        index += 1;
        match option {
            "--packets" => {
                config.packets = value
                    .parse()
                    .map_err(|_| format!("invalid sustained EOS packet count {value:?}"))?;
            }
            "--timeout-ms" => {
                config.timeout_ms = value
                    .parse()
                    .map_err(|_| format!("invalid sustained EOS timeout {value:?}"))?;
            }
            "--preroll" => {
                config.preroll = value
                    .parse()
                    .map_err(|_| format!("invalid sustained EOS preroll count {value:?}"))?;
            }
            "--eos-bytes" => {
                config.final_bytes = value
                    .parse()
                    .map_err(|_| format!("invalid sustained EOS final length {value:?}"))?;
            }
            _ => {
                return Err(format!(
                    "unknown sustained EOS option {option}; expected --packets, --timeout-ms, --preroll, or --eos-bytes"
                ));
            }
        }
    }
    if !(SUSTAINED_EOS_MIN_PACKETS..=SUSTAINED_EOS_MAX_PACKETS).contains(&config.packets) {
        return Err(format!(
            "sustained EOS packet count must be {SUSTAINED_EOS_MIN_PACKETS}..={SUSTAINED_EOS_MAX_PACKETS}"
        ));
    }
    if !(SUSTAINED_EOS_MIN_TIMEOUT_MS..=SUSTAINED_EOS_MAX_TIMEOUT_MS).contains(&config.timeout_ms) {
        return Err(format!(
            "sustained EOS timeout must be {SUSTAINED_EOS_MIN_TIMEOUT_MS}..={SUSTAINED_EOS_MAX_TIMEOUT_MS} ms"
        ));
    }
    if !matches!(config.preroll, 1..=2) {
        return Err("sustained EOS preroll must be 1 or 2 packets".into());
    }
    if config.final_bytes == 0
        || config.final_bytes > 1_920
        || !config.final_bytes.is_multiple_of(4)
    {
        return Err("sustained EOS final length must be 4..=1920 and 4-byte aligned".into());
    }
    Ok(config)
}

struct SustainedEosOracle {
    total_packets: u32,
    final_samples: u32,
    payload_packets: u32,
    leading_silence_packets: u32,
    trailing_silence_packets: u32,
    final_seen: bool,
}

impl SustainedEosOracle {
    fn new(total_packets: u32, final_bytes: u32) -> Result<Self> {
        if !(SUSTAINED_EOS_MIN_PACKETS..=SUSTAINED_EOS_MAX_PACKETS).contains(&total_packets)
            || final_bytes == 0
            || final_bytes > 1_920
            || !final_bytes.is_multiple_of(4)
        {
            return Err("invalid sustained EOS oracle geometry".into());
        }
        Ok(Self {
            total_packets,
            final_samples: final_bytes / 2,
            payload_packets: 0,
            leading_silence_packets: 0,
            trailing_silence_packets: 0,
            final_seen: false,
        })
    }

    fn marker(packet: u32) -> i16 {
        SUSTAINED_EOS_MARKER_BASE + packet as i16
    }

    fn observe(&mut self, packet: u32, values: &[i16]) -> Result<()> {
        if values.is_empty() {
            return Err("sustained EOS capture packet was empty".into());
        }
        if self.final_seen {
            if values.iter().any(|value| *value != 0) {
                return Err(format!(
                    "non-silent sustained EOS packet {packet} after final packet"
                ));
            }
            self.trailing_silence_packets = self.trailing_silence_packets.saturating_add(1);
            return Ok(());
        }
        if self.payload_packets == 0 && values.iter().all(|value| *value == 0) {
            self.leading_silence_packets = self.leading_silence_packets.saturating_add(1);
            return Ok(());
        }
        let expected_packet = self.payload_packets.saturating_add(1);
        if expected_packet > self.total_packets {
            return Err(format!(
                "sustained EOS produced payload packet {packet} after expected {} packets",
                self.total_packets
            ));
        }
        let expected_marker = Self::marker(expected_packet);
        let expected_samples = if expected_packet == self.total_packets {
            self.final_samples
        } else {
            values.len() as u32
        };
        for (index, value) in values.iter().enumerate() {
            let expected =
                if expected_packet == self.total_packets && index as u32 >= expected_samples {
                    0
                } else {
                    expected_marker
                };
            if *value != expected {
                return Err(format!(
                    "unexpected sustained EOS PCM {value} at capture packet {packet} sample {index}; expected {expected} for payload packet {expected_packet}"
                ));
            }
        }
        self.payload_packets = expected_packet;
        if expected_packet == self.total_packets {
            self.final_seen = true;
        }
        Ok(())
    }

    fn finish(&self, trailing_silence_required: u32) -> Result<()> {
        if self.payload_packets != self.total_packets || !self.final_seen {
            return Err(format!(
                "sustained EOS ended before final payload: payload packets={}, expected={}, leading silence={}",
                self.payload_packets,
                self.total_packets,
                self.leading_silence_packets
            ));
        }
        if self.trailing_silence_packets < trailing_silence_required {
            return Err(format!(
                "sustained EOS trailing silence too short: {}, expected at least {} packets",
                self.trailing_silence_packets, trailing_silence_required
            ));
        }
        Ok(())
    }
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

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct JackDescriptionResponse {
    multiple: KSMULTIPLE_ITEM,
    description: KSJACK_DESCRIPTION,
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
    fn presentation_position(&self) -> Result<KSAUDIO_PRESENTATION_POSITION> {
        get(
            self.handle.0,
            &identifier(
                KSPROPSETID_RtAudio,
                KSPROPERTY_RTAUDIO_PRESENTATION_POSITION.0 as u32,
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

fn qpc_frequency() -> Result<u64> {
    let mut frequency = 0_i64;
    unsafe { QueryPerformanceFrequency(&mut frequency) }
        .map_err(|error| format!("QueryPerformanceFrequency: {error}"))?;
    u64::try_from(frequency).map_err(|_| format!("invalid QPC frequency {frequency}"))
}

fn verify_direct_timing(paths: &[String], name: &str, capture: bool) -> Result<()> {
    use std::time::{Duration, Instant};
    const SAMPLE_RATE: u64 = 48_000;
    const MAX_ERROR_BLOCKS: u64 = SAMPLE_RATE / 20; // 50 ms
    println!("direct KS timing: {name}, capture={capture}");
    let pin = create_pin(owned_path(paths, name)?, capture, 2, 3840)?;
    let initial = pin.presentation_position()?;
    if initial.u64PositionInBlocks != 0 {
        return Err(format!(
            "new timing pin {name} started at {} blocks",
            initial.u64PositionInBlocks
        ));
    }
    let qpc_frequency = qpc_frequency()?;
    pin.state(KSSTATE_RUN)?;
    let deadline = Instant::now() + Duration::from_millis(750);
    let mut first: Option<KSAUDIO_PRESENTATION_POSITION> = None;
    let mut previous: Option<KSAUDIO_PRESENTATION_POSITION> = None;
    let mut samples = 0_u32;
    let mut max_error = 0_u64;
    let mut last_packets = 0_u32;
    while Instant::now() < deadline {
        let reading = pin.presentation_position()?;
        if let Some(previous) = previous {
            if reading.u64PositionInBlocks < previous.u64PositionInBlocks
                || reading.u64QPCPosition < previous.u64QPCPosition
            {
                return Err(format!(
                    "non-monotonic direct position on {name}: blocks {} -> {}, qpc {} -> {}",
                    previous.u64PositionInBlocks,
                    reading.u64PositionInBlocks,
                    previous.u64QPCPosition,
                    reading.u64QPCPosition
                ));
            }
        }
        if let Some(first) = first {
            let qpc_delta = reading.u64QPCPosition.saturating_sub(first.u64QPCPosition);
            let expected = (u128::from(qpc_delta) * u128::from(SAMPLE_RATE))
                .div_ceil(u128::from(qpc_frequency)) as u64;
            let actual = reading
                .u64PositionInBlocks
                .saturating_sub(first.u64PositionInBlocks);
            let error = actual.abs_diff(expected);
            max_error = max_error.max(error);
            if error > MAX_ERROR_BLOCKS {
                return Err(format!(
                    "direct timing drift on {name}: actual={actual} blocks, expected={expected}, error={error}"
                ));
            }
            samples = samples.saturating_add(1);
        } else {
            first = Some(reading);
        }
        last_packets = pin.packets()?.max(last_packets);
        previous = Some(reading);
        std::thread::sleep(Duration::from_millis(10));
    }
    if samples < 4 || last_packets < 4 {
        return Err(format!(
            "direct timing on {name} produced too little evidence: samples={samples}, packets={last_packets}"
        ));
    }
    pin.state(KSSTATE_PAUSE)?;
    let paused = pin.presentation_position()?;
    std::thread::sleep(Duration::from_millis(20));
    let paused_again = pin.presentation_position()?;
    if paused_again.u64PositionInBlocks != paused.u64PositionInBlocks {
        return Err(format!(
            "paused direct timing position advanced on {name}: blocks {} -> {}, qpc {} -> {}",
            paused.u64PositionInBlocks,
            paused_again.u64PositionInBlocks,
            paused.u64QPCPosition,
            paused_again.u64QPCPosition
        ));
    }
    pin.stop()?;
    println!(
        "  {name}: {samples} position samples, {last_packets} packets, max error {max_error} blocks; pause and STOP passed"
    );
    Ok(())
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

fn observe_sustained_capture_packet(
    capture: &Pin,
    count: u32,
    oracle: &mut SustainedEosOracle,
) -> Result<bool> {
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
            "sustained EOS capture packet index disagrees with completed count: {} vs {count}",
            read.PacketNumber
        ));
    }
    fence(Ordering::SeqCst);
    let base = ((count - 1) % capture.notification_count) * (capture.packet_bytes / 2);
    let mut values = Vec::with_capacity((capture.packet_bytes / 2) as usize);
    for index in 0..capture.packet_bytes / 2 {
        let value = unsafe {
            capture
                .buffer
                .BufferAddress
                .cast::<i16>()
                .add((base + index) as usize)
                .read_volatile()
        };
        values.push(value);
    }
    fence(Ordering::SeqCst);
    if capture.packets()? != count {
        return Err(
            "sustained EOS capture packet changed during inspection; evidence is inconclusive"
                .into(),
        );
    }
    let nonzero = values.iter().any(|value| *value != 0);
    oracle.observe(count - 1, &values)?;
    Ok(nonzero)
}

fn fill_sustained_render_packet(
    render: &Pin,
    packet: u32,
    final_packet: bool,
    final_samples: u32,
) -> Result<()> {
    use std::sync::atomic::{fence, Ordering};
    let samples = render.packet_bytes / 2;
    let base = ((packet - 1) % render.notification_count) * samples;
    let marker = SustainedEosOracle::marker(packet);
    let buffer = render.buffer.BufferAddress.cast::<i16>();
    for index in 0..samples {
        let value = if final_packet && index >= final_samples {
            // A nonzero poison tail makes an EOS implementation that copies
            // past the declared prefix observable in the capture oracle.
            3_030
        } else {
            marker
        };
        unsafe {
            buffer.add((base + index) as usize).write_volatile(value);
        }
    }
    fence(Ordering::SeqCst);
    Ok(())
}

fn verify_sustained_eos(
    paths: &[String],
    render_name: &str,
    capture_name: &str,
    config: SustainedEosConfig,
) -> Result<()> {
    use std::time::{Duration, Instant};
    const PACKET_BYTES: u32 = 1_920;
    const EOS: u32 = KSSTREAM_HEADER_OPTIONSF_ENDOFSTREAM;
    const TRAILING_SILENCE_PACKETS: u32 = 10;
    println!(
        "sustained direct EOS: {render_name} -> {capture_name}, packets={}, timeout={} ms, preroll={}, final bytes={}",
        config.packets, config.timeout_ms, config.preroll, config.final_bytes
    );
    let render = create_pin(owned_path(paths, render_name)?, false, 2, 3_840)?;
    let capture = create_pin(owned_path(paths, capture_name)?, true, 2, 3_840)?;
    if render.packet_bytes != PACKET_BYTES || capture.packet_bytes != PACKET_BYTES {
        return Err(format!(
            "sustained EOS requires 1920-byte packets: render={}, capture={}",
            render.packet_bytes, capture.packet_bytes
        ));
    }
    let mut oracle = SustainedEosOracle::new(config.packets, config.final_bytes)?;
    for packet in 1..=config.preroll {
        fill_sustained_render_packet(
            &render,
            packet,
            packet == config.packets,
            config.final_bytes / 2,
        )?;
        render
            .write_packet(
                packet,
                if packet == config.packets { EOS } else { 0 },
                if packet == config.packets {
                    config.final_bytes
                } else {
                    0
                },
            )
            .map_err(|error| format!("submit preroll packet {packet}: {error}"))?;
    }

    // Start capture first so any initial engine underflow is explicitly
    // recorded by the oracle rather than hiding a dropped packet.
    capture.state(KSSTATE_RUN)?;
    render.state(KSSTATE_RUN)?;
    let started = Instant::now();
    let deadline = started + Duration::from_millis(config.timeout_ms);
    let mut next_packet = config.preroll + 1;
    let mut previous_render_count = 0_u32;
    let mut previous_capture_count = 0_u32;
    let mut max_render_count = 0_u32;
    let mut max_capture_count = 0_u32;
    while Instant::now() < deadline {
        let render_count = render.packets()?;
        if render_count < previous_render_count {
            return Err(format!(
                "sustained EOS render packet counter regressed: {} -> {}",
                previous_render_count, render_count
            ));
        }
        previous_render_count = render_count;
        max_render_count = max_render_count.max(render_count);
        // Keep both mapped notifications occupied. At the first observation
        // after RUN, this also fills the second slot for a one-packet preroll.
        while next_packet <= config.packets
            && next_packet <= render_count.saturating_add(render.notification_count)
        {
            let final_packet = next_packet == config.packets;
            fill_sustained_render_packet(
                &render,
                next_packet,
                final_packet,
                config.final_bytes / 2,
            )?;
            render
                .write_packet(
                    next_packet,
                    if final_packet { EOS } else { 0 },
                    if final_packet { config.final_bytes } else { 0 },
                )
                .map_err(|error| format!("submit sustained packet {next_packet}: {error}"))?;
            next_packet += 1;
        }

        let capture_count = capture.packets()?;
        if capture_count < previous_capture_count {
            return Err(format!(
                "sustained EOS capture packet counter regressed: {} -> {}",
                previous_capture_count, capture_count
            ));
        }
        if capture_count > previous_capture_count + 1 {
            return Err(format!(
                "sustained EOS observation lost capture packets: {} -> {}",
                previous_capture_count, capture_count
            ));
        }
        if capture_count == previous_capture_count + 1 {
            let _ = observe_sustained_capture_packet(&capture, capture_count, &mut oracle)?;
            previous_capture_count = capture_count;
        }
        max_capture_count = max_capture_count.max(capture_count);
        if next_packet > config.packets
            && oracle.final_seen
            && oracle.trailing_silence_packets >= TRAILING_SILENCE_PACKETS
            && max_render_count >= config.packets
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let elapsed_ms = started.elapsed().as_millis();
    oracle.finish(TRAILING_SILENCE_PACKETS)?;
    if next_packet <= config.packets || max_render_count < config.packets {
        return Err(format!(
            "sustained EOS did not queue/consume all packets: next={}, render={}/{}",
            next_packet, max_render_count, config.packets
        ));
    }
    render.stop()?;
    capture.stop()?;
    println!(
        "  sustained EOS passed: elapsed={} ms, render packets={max_render_count}, capture packets={max_capture_count}, payload packets={}, leading silence={}, trailing silence={}; preroll={} and explicit STOP passed",
        elapsed_ms,
        oracle.payload_packets,
        oracle.leading_silence_packets,
        oracle.trailing_silence_packets,
        config.preroll
    );
    Ok(())
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

fn verify_jack_metadata(paths: &[String], name: &str) -> Result<()> {
    let filter = open_filter(owned_path(paths, name)?)?;
    let response: JackDescriptionResponse = get(
        filter.0,
        &KSP_PIN {
            Property: identifier(
                KSPROPSETID_Jack,
                KSPROPERTY_JACK_DESCRIPTION.0 as u32,
                KSPROPERTY_TYPE_GET,
            ),
            PinId: 1,
            ..Default::default()
        },
    )?;
    if response.multiple.Count != 1
        || response.multiple.Size < size_of::<JackDescriptionResponse>() as u32
    {
        return Err(format!(
            "unexpected jack description array on {name}: size={}, count={}",
            response.multiple.Size, response.multiple.Count
        ));
    }
    let description = response.description;
    if description.ChannelMapping != 3
        || description.ConnectionType.0 != 3
        || description.GeoLocation.0 != 2
        || description.GenLocation.0 != 0
        || description.PortConnection.0 != 1
    {
        return Err(format!(
            "unexpected jack description on {name}: channel map={}, connection={}, geo={}, gen={}, port={}, connected={}",
            description.ChannelMapping,
            description.ConnectionType.0,
            description.GeoLocation.0,
            description.GenLocation.0,
            description.PortConnection.0,
            description.IsConnected.0,
        ));
    }
    println!(
        "direct jack metadata: {name}, channel map={}, connection={}, geo={}, gen={}, port={}, connected={}",
        description.ChannelMapping,
        description.ConnectionType.0,
        description.GeoLocation.0,
        description.GenLocation.0,
        description.PortConnection.0,
        description.IsConnected.0,
    );
    Ok(())
}

fn verify_pin_lifecycle(paths: &[String], name: &str, capture: bool) -> Result<()> {
    use std::time::Duration;
    println!("direct KS lifecycle: {name}, capture={capture}");
    // The driver has a bounded 8-slot stream registry. Repeating more than
    // twice that capacity makes a leaked slot observable without requiring a
    // privileged or long-running stress harness.
    const CYCLES: u32 = 17;
    for cycle in 1..=CYCLES {
        let pin = create_pin(owned_path(paths, name)?, capture, 2, 3840)?;
        if pin.packets()? != 0 {
            return Err(format!(
                "new lifecycle pin {name} started with queued packets"
            ));
        }
        pin.state(KSSTATE_RUN)?;
        std::thread::sleep(Duration::from_millis(20));
        let running = pin.packets()?;
        if running == 0 {
            return Err(format!("lifecycle start produced no packets on {name}"));
        }
        pin.state(KSSTATE_PAUSE)?;
        let paused = pin.packets()?;
        std::thread::sleep(Duration::from_millis(15));
        if pin.packets()? != paused {
            return Err(format!("paused lifecycle pin advanced on {name}"));
        }
        pin.state(KSSTATE_RUN)?;
        std::thread::sleep(Duration::from_millis(20));
        let resumed = pin.packets()?;
        if resumed <= paused {
            return Err(format!("lifecycle resume produced no progress on {name}"));
        }
        pin.stop()?;
        if cycle == 1 || cycle == CYCLES {
            println!(
                "  cycle {cycle}/{CYCLES}: start={running}, paused={paused}, resumed={resumed}; STOP passed"
            );
        }
    }

    let reopened = create_pin(owned_path(paths, name)?, capture, 2, 3840)?;
    if reopened.packets()? != 0 {
        return Err(format!(
            "reopened lifecycle pin {name} retained packet state"
        ));
    }
    reopened.stop()?;
    println!("  reopen: packet state reset; explicit STOP passed");
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
    if args.first().map(String::as_str) == Some("--verify-sustained-eos") {
        let config = parse_sustained_eos_args(&args)?;
        let paths = interfaces()?;
        for (render, capture) in [
            ("QPWGraphVirtualOutput", "QPWGraphVirtualMonitor"),
            ("QPWGraphRelaySink", "QPWGraphRelayMicrophone"),
        ] {
            verify_sustained_eos(&paths, render, capture, config)?;
        }
        println!(
            "Bounded sustained direct KS EOS/preroll checks passed on both cables: {} packets per cable, {} ms timeout; this run does not exercise 32-bit counter wrap.",
            config.packets, config.timeout_ms
        );
        return Ok(());
    }
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
    if args == ["--verify-lifecycle"] {
        let paths = interfaces()?;
        for (name, capture) in [
            ("QPWGraphVirtualOutput", false),
            ("QPWGraphVirtualMonitor", true),
            ("QPWGraphRelaySink", false),
            ("QPWGraphRelayMicrophone", true),
        ] {
            verify_pin_lifecycle(&paths, name, capture)?;
        }
        println!("Direct KS lifecycle checks passed on all four endpoints.");
        return Ok(());
    }
    if args == ["--verify-timing"] {
        let paths = interfaces()?;
        for (name, capture) in [
            ("QPWGraphVirtualOutput", false),
            ("QPWGraphVirtualMonitor", true),
            ("QPWGraphRelaySink", false),
            ("QPWGraphRelayMicrophone", true),
        ] {
            verify_direct_timing(&paths, name, capture)?;
        }
        println!("Direct KS timing checks passed on all four endpoints.");
        return Ok(());
    }
    if args == ["--verify-jacks"] {
        let paths = interfaces()?;
        for name in [
            "QPWGraphVirtualOutput",
            "QPWGraphVirtualMonitor",
            "QPWGraphRelaySink",
            "QPWGraphRelayMicrophone",
        ] {
            verify_jack_metadata(&paths, name)?;
        }
        println!("Jack metadata checks passed on all four endpoints.");
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
            "usage: qpwgraph-audio-ks-probe [--inspect | --open-pins | --verify-eos | --verify-sustained-eos [--packets N] [--timeout-ms N] [--preroll N] [--eos-bytes N] | --verify-formats | --verify-lifecycle | --verify-timing | --verify-jacks]".into(),
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

    #[test]
    fn sustained_eos_defaults_are_bounded_and_configurable() {
        let defaults = parse_sustained_eos_args(&["--verify-sustained-eos".into()]).unwrap();
        assert_eq!(defaults, SustainedEosConfig::default());
        let configured = parse_sustained_eos_args(&[
            "--verify-sustained-eos".into(),
            "--packets".into(),
            "64".into(),
            "--timeout-ms".into(),
            "7000".into(),
            "--preroll".into(),
            "1".into(),
            "--eos-bytes".into(),
            "1920".into(),
        ])
        .unwrap();
        assert_eq!(
            configured,
            SustainedEosConfig {
                packets: 64,
                timeout_ms: 7000,
                preroll: 1,
                final_bytes: 1920,
            }
        );
        assert!(parse_sustained_eos_args(&[
            "--verify-sustained-eos".into(),
            "--packets".into(),
            "3".into(),
        ])
        .is_err());
        assert!(parse_sustained_eos_args(&[
            "--verify-sustained-eos".into(),
            "--eos-bytes".into(),
            "1922".into(),
        ])
        .is_err());
        assert!(parse_sustained_eos_args(&[
            "--verify-sustained-eos".into(),
            "--packets".into(),
            "20001".into(),
        ])
        .is_err());
        assert!(parse_sustained_eos_args(&[
            "--verify-sustained-eos".into(),
            "--timeout-ms".into(),
            "600001".into(),
        ])
        .is_err());
    }

    #[test]
    fn sustained_eos_oracle_tracks_preroll_and_final_tail() {
        let mut oracle = SustainedEosOracle::new(4, 4).unwrap();
        oracle.observe(0, &[0, 0, 0, 0]).unwrap();
        oracle.observe(1, &[1001, 1001, 1001, 1001]).unwrap();
        oracle.observe(2, &[1002, 1002, 1002, 1002]).unwrap();
        oracle.observe(3, &[1003, 1003, 1003, 1003]).unwrap();
        oracle.observe(4, &[1004, 1004, 0, 0]).unwrap();
        oracle.observe(5, &[0, 0, 0, 0]).unwrap();
        oracle.observe(6, &[0, 0, 0, 0]).unwrap();
        oracle.finish(2).unwrap();
        assert_eq!(oracle.leading_silence_packets, 1);
        assert_eq!(oracle.payload_packets, 4);
        assert_eq!(oracle.trailing_silence_packets, 2);
    }

    #[test]
    fn sustained_eos_oracle_rejects_gap_poison_and_post_final_audio() {
        let mut gap = SustainedEosOracle::new(4, 4).unwrap();
        gap.observe(0, &[1001, 1001, 1001, 1001]).unwrap();
        assert!(gap.observe(1, &[0, 0, 0, 0]).is_err());

        let mut poison = SustainedEosOracle::new(4, 4).unwrap();
        poison.observe(0, &[1001, 1001, 1001, 1001]).unwrap();
        poison.observe(1, &[1002, 1002, 1002, 1002]).unwrap();
        poison.observe(2, &[1003, 1003, 1003, 1003]).unwrap();
        assert!(poison.observe(3, &[1004, 1004, 0, 1004]).is_err());

        let mut after_final = SustainedEosOracle::new(4, 4).unwrap();
        after_final.observe(0, &[1001, 1001, 1001, 1001]).unwrap();
        after_final.observe(1, &[1002, 1002, 1002, 1002]).unwrap();
        after_final.observe(2, &[1003, 1003, 1003, 1003]).unwrap();
        after_final.observe(3, &[1004, 1004, 0, 0]).unwrap();
        assert!(after_final.observe(4, &[1001, 0, 0, 0]).is_err());
    }
}
