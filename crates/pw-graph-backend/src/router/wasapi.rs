//! WASAPI endpoints as router sources and sinks.
//!
//! This is where the platform-neutral engine meets Windows. Each endpoint gets
//! its own thread, which owns the COM apartment it initialized and every
//! interface created in it -- the same invariant the Core Audio observation
//! backend keeps, and for the same reason: a COM pointer that escapes its
//! apartment is a crash waiting for a race.
//!
//! The thread and the router never share anything but the bounded ring in
//! [`super::buffer`]. So a device that stalls cannot stall the router, a
//! router that stalls cannot stall the device, and neither has to know the
//! other's period.
//!
//! Three kinds of endpoint are opened here, and they are exactly the three the
//! parity work needs from user mode:
//!
//! * a **render** endpoint, so a route can end at real speakers;
//! * a **capture** endpoint, so a route can begin at a real microphone;
//! * a render endpoint's **loopback** capture, so a route can begin at
//!   whatever a playback device is currently playing.
//!
//! What is deliberately *not* here is a qpwgraph-owned endpoint that other
//! applications can select -- the virtual microphone the relay needs, and the
//! destination an arbitrary application could be pointed at. User-mode code
//! cannot create one; that requires a driver, and the parity roadmap gates it
//! behind an architecture decision record. Until then this module is honest
//! about covering device-to-device routing and nothing more.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Media::Audio;
use windows::Win32::System::Com::{self, CLSCTX_ALL, COINIT_MULTITHREADED};

use crate::api::{BackendError, BackendResult};

use super::endpoints::{
    ring_sink, ring_source, RingSink, RingSinkDrain, RingSource, RingSourceFeed,
};
use super::format::AudioFormat;

/// `WAVEFORMATEX.wFormatTag` for 32-bit float samples, which is what the
/// router speaks everywhere.
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;

/// Endpoint buffer, in 100 ns units.
///
/// 40 ms sits comfortably above the shared-mode period of every device tested
/// and keeps the poll loop cheap. It is the device's own buffer, not the
/// router's latency budget: the ring between them is sized separately.
const BUFFER_DURATION_HNS: i64 = 400_000;

/// Poll interval, well under the buffer duration so neither side starves.
///
/// Polling rather than waiting on the WASAPI event is deliberate: loopback
/// capture does not raise the event, so one shape keeps every endpoint thread
/// identical instead of splitting them into two subtly different loops.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// How long opening waits for the thread to report that WASAPI accepted the
/// endpoint.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// Which endpoint to open, and how.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointKind {
    /// A playback device: the route's audio comes out of it.
    Render,
    /// A recording device: the route's audio starts there.
    Capture,
    /// A playback device's loopback: the route starts with whatever that
    /// device is currently playing, including other applications' audio.
    RenderLoopback,
}

impl EndpointKind {
    /// Which side of the enumerator the device lives on. Loopback opens a
    /// *render* device and records from it, which is why this is not simply a
    /// capture flag.
    fn data_flow(self) -> Audio::EDataFlow {
        match self {
            Self::Render | Self::RenderLoopback => Audio::eRender,
            Self::Capture => Audio::eCapture,
        }
    }
}

/// A running WASAPI endpoint thread.
///
/// Dropping it stops the thread and releases the endpoint. The router's half
/// of the ring keeps working until it is dropped too; it simply runs dry, or
/// backs up, which the route's diagnostics already account for.
pub struct WasapiEndpoint {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    kind: EndpointKind,
    format: AudioFormat,
}

impl WasapiEndpoint {
    pub fn kind(&self) -> EndpointKind {
        self.kind
    }

    /// The geometry WASAPI was asked to deliver. The audio engine converts to
    /// it, so an endpoint running at 44.1 kHz or 7.1 still meets the router in
    /// the format the route was built for.
    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Stop the endpoint and wait for its thread.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for WasapiEndpoint {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for WasapiEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasapiEndpoint")
            .field("kind", &self.kind)
            .field("format", &self.format)
            .field("running", &self.worker.is_some())
            .finish()
    }
}

/// Open a playback endpoint as a router destination.
///
/// `device_id` is a Core Audio device id, the same string the endpoint nodes
/// are built from, so the UI can offer the cards it already draws. `None`
/// follows the default playback device.
///
/// Returns once WASAPI has accepted the endpoint, so a failure is reported
/// here rather than as a route that looks connected and carries nothing.
pub fn open_render_sink(
    device_id: Option<&str>,
    format: AudioFormat,
    ring_frames: usize,
) -> BackendResult<(RingSink, WasapiEndpoint)> {
    let (sink, drain) = ring_sink(format, ring_frames);
    let endpoint = spawn(
        EndpointKind::Render,
        device_id,
        format,
        Carrier::Render(drain),
    )?;
    Ok((sink, endpoint))
}

/// Open a recording endpoint as a router source.
pub fn open_capture_source(
    device_id: Option<&str>,
    format: AudioFormat,
    ring_frames: usize,
) -> BackendResult<(RingSource, WasapiEndpoint)> {
    let (source, feed) = ring_source(format, ring_frames);
    let endpoint = spawn(
        EndpointKind::Capture,
        device_id,
        format,
        Carrier::Capture(feed),
    )?;
    Ok((source, endpoint))
}

/// Open a playback endpoint's loopback as a router source, so a route can
/// start from whatever that device is playing.
pub fn open_loopback_source(
    device_id: Option<&str>,
    format: AudioFormat,
    ring_frames: usize,
) -> BackendResult<(RingSource, WasapiEndpoint)> {
    let (source, feed) = ring_source(format, ring_frames);
    let endpoint = spawn(
        EndpointKind::RenderLoopback,
        device_id,
        format,
        Carrier::Capture(feed),
    )?;
    Ok((source, endpoint))
}

/// The router's half of the ring, moved onto the endpoint thread.
enum Carrier {
    Capture(RingSourceFeed),
    Render(RingSinkDrain),
}

fn spawn(
    kind: EndpointKind,
    device_id: Option<&str>,
    format: AudioFormat,
    carrier: Carrier,
) -> BackendResult<WasapiEndpoint> {
    if !format.is_valid() {
        return Err(BackendError::native(format!(
            "{} Hz with {} channels is not a usable endpoint format",
            format.sample_rate, format.channels
        )));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let device_id = device_id.map(str::to_owned);
    let (started_tx, started_rx) = mpsc::channel();
    let worker = thread::Builder::new()
        .name(format!("qpwgraph-router-{}", thread_name(kind)))
        .spawn(move || {
            // Every COM apartment is per-thread, so this thread initializes
            // its own and every interface it creates stays inside it.
            let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
            if initialized.is_err() {
                let _ = started_tx.send(Err(BackendError::native(format!(
                    "could not initialize COM for a router endpoint: {initialized:?}"
                ))));
                return;
            }
            run(
                kind,
                device_id.as_deref(),
                format,
                carrier,
                &thread_stop,
                started_tx,
            );
            unsafe { Com::CoUninitialize() };
        })
        .map_err(|error| {
            BackendError::native(format!("could not start a router endpoint thread: {error}"))
        })?;

    match wait_for_start(started_rx) {
        Ok(()) => Ok(WasapiEndpoint {
            stop,
            worker: Some(worker),
            kind,
            format,
        }),
        Err(error) => {
            stop.store(true, Ordering::Release);
            let _ = worker.join();
            Err(error)
        }
    }
}

fn wait_for_start(started: Receiver<BackendResult<()>>) -> BackendResult<()> {
    match started.recv_timeout(START_TIMEOUT) {
        Ok(result) => result,
        Err(_) => Err(BackendError::native(
            "a router audio endpoint did not start in time",
        )),
    }
}

fn thread_name(kind: EndpointKind) -> &'static str {
    match kind {
        EndpointKind::Render => "render",
        EndpointKind::Capture => "capture",
        EndpointKind::RenderLoopback => "loopback",
    }
}

/// Open the endpoint, report whether WASAPI accepted it, then pump until
/// asked to stop.
///
/// The report is sent exactly once. Without it a failure inside the thread
/// would leave a route that looks connected and silently carries nothing.
fn run(
    kind: EndpointKind,
    device_id: Option<&str>,
    format: AudioFormat,
    carrier: Carrier,
    stop: &AtomicBool,
    started: Sender<BackendResult<()>>,
) {
    let client = match open_client(kind, device_id, format) {
        Ok(client) => client,
        Err(error) => {
            let _ = started.send(Err(error));
            return;
        }
    };

    // The service is acquired before success is reported, not inside the
    // loop: an endpoint that cannot give up its service is a failure to open,
    // not a route that quietly carries no audio.
    let pump = match carrier {
        Carrier::Capture(feed) => unsafe { client.GetService::<Audio::IAudioCaptureClient>() }
            .map(|service| Pump::Capture(service, feed))
            .map_err(|error| native("acquire the capture service", error)),
        Carrier::Render(drain) => unsafe { client.GetService::<Audio::IAudioRenderClient>() }
            .map(|service| Pump::Render(service, drain))
            .map_err(|error| native("acquire the render service", error)),
    };
    let pump = match pump {
        Ok(pump) => pump,
        Err(error) => {
            let _ = started.send(Err(error));
            return;
        }
    };

    if let Err(error) = unsafe { client.Start() } {
        let _ = started.send(Err(native("start the audio client", error)));
        return;
    }
    let _ = started.send(Ok(()));

    match pump {
        Pump::Capture(service, feed) => capture_loop(&service, feed, format, stop),
        Pump::Render(service, drain) => render_loop(&client, &service, drain, format, stop),
    }

    let _ = unsafe { client.Stop() };
}

enum Pump {
    Capture(Audio::IAudioCaptureClient, RingSourceFeed),
    Render(Audio::IAudioRenderClient, RingSinkDrain),
}

fn open_client(
    kind: EndpointKind,
    device_id: Option<&str>,
    format: AudioFormat,
) -> BackendResult<Audio::IAudioClient> {
    let enumerator: Audio::IMMDeviceEnumerator =
        unsafe { Com::CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|error| native("create the device enumerator", error))?;

    // A named device that has since been unplugged falls back to the default
    // rather than failing the route outright: the user asked for "speakers",
    // and the machine still has some.
    let device = match device_id {
        Some(id) => {
            let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe { enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr())) }.ok()
        }
        None => None,
    };
    let device = match device {
        Some(device) => device,
        None => unsafe { enumerator.GetDefaultAudioEndpoint(kind.data_flow(), Audio::eConsole) }
            .map_err(|error| native("open the default endpoint", error))?,
    };

    let client: Audio::IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|error| native("activate the audio client", error))?;

    let wave = wave_format(format);
    // AUTOCONVERTPCM makes the Windows audio engine resample and remix, so an
    // endpoint running at 44.1 kHz or 7.1 still meets the router in the
    // format the route was built for. The router's own resampler then only
    // has to handle drift and genuinely differing route rates.
    let mut flags =
        Audio::AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | Audio::AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    if kind == EndpointKind::RenderLoopback {
        flags |= Audio::AUDCLNT_STREAMFLAGS_LOOPBACK;
    }
    unsafe {
        client.Initialize(
            Audio::AUDCLNT_SHAREMODE_SHARED,
            flags,
            BUFFER_DURATION_HNS,
            0,
            &wave,
            None,
        )
    }
    .map_err(|error| native("initialize the audio client", error))?;

    Ok(client)
}

/// Interleaved float in the router's geometry.
fn wave_format(format: AudioFormat) -> Audio::WAVEFORMATEX {
    let bits = 32u16;
    let block_align = format.channels * bits / 8;
    Audio::WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: format.channels,
        nSamplesPerSec: format.sample_rate,
        nAvgBytesPerSec: format.sample_rate * u32::from(block_align),
        nBlockAlign: block_align,
        wBitsPerSample: bits,
        cbSize: 0,
    }
}

/// Move captured audio into the router's ring.
fn capture_loop(
    capture: &Audio::IAudioCaptureClient,
    mut feed: RingSourceFeed,
    format: AudioFormat,
    stop: &AtomicBool,
) {
    let channels = usize::from(format.channels);
    // Preallocated once: the silent-packet path needs a buffer, and
    // allocating one per packet on the capture thread is the allocation §8.1
    // forbids.
    let silence = vec![0.0f32; format.samples(4096)];

    while !stop.load(Ordering::Acquire) {
        let mut pending = match unsafe { capture.GetNextPacketSize() } {
            Ok(pending) => pending,
            Err(_) => {
                feed.mark_lost();
                return;
            }
        };
        if pending == 0 {
            thread::sleep(POLL_INTERVAL);
            continue;
        }
        while pending > 0 && !stop.load(Ordering::Acquire) {
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut buffer_flags = 0u32;
            if unsafe { capture.GetBuffer(&mut data, &mut frames, &mut buffer_flags, None, None) }
                .is_err()
            {
                feed.mark_lost();
                return;
            }
            if frames > 0 {
                let samples = frames as usize * channels;
                if buffer_flags & Audio::AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    // WASAPI may hand back a silent packet without filling the
                    // buffer at all, so the silence is synthesised rather than
                    // read from whatever the pointer happens to hold.
                    let mut left = samples;
                    while left > 0 {
                        let pushed = feed.push(&silence[..left.min(silence.len())]);
                        if pushed == 0 {
                            // The ring is full; the rest of the silence is
                            // dropped rather than spun on.
                            break;
                        }
                        left -= pushed;
                    }
                } else if !data.is_null() {
                    // Safety: WASAPI guarantees `data` points at `frames`
                    // frames in the format the client was initialized with,
                    // which is the float geometry `wave_format` requested.
                    let block = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), samples) };
                    // A short push is the router falling behind; the ring
                    // counts it, and dropping the tail is better than growing
                    // latency without bound.
                    feed.push(block);
                }
            }
            if unsafe { capture.ReleaseBuffer(frames) }.is_err() {
                feed.mark_lost();
                return;
            }
            pending = match unsafe { capture.GetNextPacketSize() } {
                Ok(pending) => pending,
                Err(_) => {
                    feed.mark_lost();
                    return;
                }
            };
        }
    }
}

/// Drain the router's ring onto the endpoint.
fn render_loop(
    client: &Audio::IAudioClient,
    render: &Audio::IAudioRenderClient,
    mut drain: RingSinkDrain,
    format: AudioFormat,
    stop: &AtomicBool,
) {
    let buffer_frames = match unsafe { client.GetBufferSize() } {
        Ok(frames) if frames > 0 => frames,
        _ => {
            drain.mark_lost();
            return;
        }
    };
    let channels = usize::from(format.channels);
    let mut scratch = vec![0.0f32; buffer_frames as usize * channels];

    while !stop.load(Ordering::Acquire) {
        let padding = match unsafe { client.GetCurrentPadding() } {
            Ok(padding) => padding,
            Err(_) => {
                drain.mark_lost();
                return;
            }
        };
        let available = buffer_frames.saturating_sub(padding);
        if available == 0 {
            thread::sleep(POLL_INTERVAL);
            continue;
        }
        let wanted = available as usize * channels;
        let filled = drain.pull(&mut scratch[..wanted]);
        // Whatever the router did not supply is silence. Repeating the
        // previous buffer would turn a dropout into a stutter, which sounds
        // like a fault in the audio rather than a gap in it.
        if filled < wanted {
            scratch[filled..wanted].fill(0.0);
        }

        let data = match unsafe { render.GetBuffer(available) } {
            Ok(data) if !data.is_null() => data,
            _ => {
                drain.mark_lost();
                return;
            }
        };
        // Safety: WASAPI just handed back a buffer for exactly `available`
        // frames in the client's float format, and `wanted` is that many
        // samples.
        unsafe {
            std::ptr::copy_nonoverlapping(scratch.as_ptr(), data.cast::<f32>(), wanted);
            if render.ReleaseBuffer(available, 0).is_err() {
                drain.mark_lost();
                return;
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn native(context: &str, error: windows::core::Error) -> BackendError {
    BackendError::native(format!("router endpoint could not {context}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wave_format_describes_interleaved_float_in_the_routers_geometry() {
        let wave = wave_format(AudioFormat::new(48_000, 2));
        // WAVEFORMATEX is packed, so each field is copied out before it can
        // be compared.
        let (tag, channels) = (wave.wFormatTag, wave.nChannels);
        let (rate, bits) = (wave.nSamplesPerSec, wave.wBitsPerSample);
        let (block_align, avg_bytes) = (wave.nBlockAlign, wave.nAvgBytesPerSec);

        assert_eq!(tag, WAVE_FORMAT_IEEE_FLOAT);
        assert_eq!(channels, 2);
        assert_eq!(rate, 48_000);
        assert_eq!(bits, 32);
        // Getting the stride wrong makes WASAPI walk the buffer at the wrong
        // step, so the audio comes out pitch-shifted rather than failing.
        assert_eq!(block_align, 8);
        assert_eq!(avg_bytes, 48_000 * 8);
    }

    #[test]
    fn a_multichannel_format_keeps_its_stride() {
        let wave = wave_format(AudioFormat::new(44_100, 6));
        let (block_align, avg_bytes) = (wave.nBlockAlign, wave.nAvgBytesPerSec);
        assert_eq!(block_align, 24);
        assert_eq!(avg_bytes, 44_100 * 24);
    }

    #[test]
    fn loopback_opens_a_render_device_because_that_is_what_it_records() {
        // The distinction matters: asking the enumerator for a capture device
        // here would open the microphone instead of the speakers' output.
        assert_eq!(EndpointKind::RenderLoopback.data_flow(), Audio::eRender);
        assert_eq!(EndpointKind::Capture.data_flow(), Audio::eCapture);
        assert_eq!(EndpointKind::Render.data_flow(), Audio::eRender);
    }

    #[test]
    fn an_unusable_format_is_refused_before_a_thread_is_started() {
        let (_sink, drain) = ring_sink(AudioFormat::new(48_000, 2), 64);
        let error = spawn(
            EndpointKind::Render,
            None,
            AudioFormat::new(48_000, 0),
            Carrier::Render(drain),
        )
        .expect_err("a zero-channel endpoint is not openable");
        assert!(matches!(error, BackendError::Native(_)));
    }
}
