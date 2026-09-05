//! WASAPI audio endpoints for the relay on Windows.
//!
//! On Linux the relay owns two virtual PipeWire nodes, so any application can
//! be routed into or out of it through the patchbay. Windows has no user-mode
//! API for creating an audio endpoint — a device other applications can select
//! requires a kernel-mode driver — so the relay is wired to fixed endpoints
//! instead:
//!
//! Emitter mode can capture a physical input with `eCapture`, loopback-record
//! a selected playback endpoint with `eRender`, or drain a live read-only
//! process-loopback source. Receiver mode
//! renders peer audio to an `eRender` endpoint. Exactly one worker is active
//! for a relay instance; changing the mode or selector stops that worker
//! before starting its replacement.
//!
//! Direct mode does not create a virtual capture endpoint for other Windows
//! applications. That optional system-wide integration belongs to a separate
//! driver. Both loops poll rather than waiting on an event handle: loopback
//! capture does not raise the WASAPI event, so a single polling shape keeps the
//! two paths symmetrical.

use crate::api::{BackendError, BackendResult, RelayReceiveSink, RelaySendSource};
use crate::router::{AudioFormat, AudioSource, RingSource, StreamHealth};
use crate::windows::{
    find_qpwgraph_endpoint, verify_live_process_identity, ProcessLoopbackMode,
    ProcessLoopbackSource, QpwVirtualEndpointRole,
};
use pw_graph_relay::{EngineConfig, RelayEngine, RelayHandle, RelayMode};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::core::PWSTR;
use windows::Win32::Media::Audio;
use windows::Win32::System::Com::{self, CLSCTX_ALL, COINIT_MULTITHREADED};

/// The relay engine's PCM format. WASAPI is asked to convert to it, so the
/// endpoint's own mix format never leaks into the wire format.
pub(crate) const RELAY_SAMPLE_RATE: u32 = 48_000;
pub(crate) const RELAY_CHANNELS: u16 = 2;

/// `WAVEFORMATEX.wFormatTag` for 32-bit float samples.
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
/// Endpoint buffer, in 100 ns units. 40 ms is comfortably above the shared
/// mode period on every device tested and keeps the poll loop cheap.
const BUFFER_DURATION_HNS: i64 = 400_000;
/// Poll interval. Well under the buffer duration so neither side starves.
const POLL_INTERVAL: Duration = Duration::from_millis(5);
/// How long `start` waits for each endpoint to report that WASAPI accepted it.
/// Process-loopback activation is asynchronous and has its own ten-second
/// capability timeout, so the outer relay boundary leaves room for it.
const ENDPOINT_START_TIMEOUT: Duration = Duration::from_secs(15);
/// Bounded hand-off between process-loopback activation and the relay engine.
const APPLICATION_RING_FRAMES: usize = 4096;
const APPLICATION_BLOCK_FRAMES: usize = 480;

/// Legacy endpoint pair retained for compatibility with older callers.
///
/// `None` means the current default playback endpoint, which is also what a
/// removed or unplugged device falls back to. Ids come straight from the
/// driver's endpoint enumeration, so the UI can offer the same list it already
/// draws as graph nodes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelayEndpoints {
    /// Endpoint whose loopback is sent to peers.
    pub capture: Option<String>,
    /// Endpoint that peer audio is played on.
    pub playback: Option<String>,
}

/// The relay engine plus the single active WASAPI worker.
///
/// Dropping this stops the worker and the engine. The struct is deliberately
/// the only owner of the `RelayEngine`, so the endpoint cannot outlive it.
pub(crate) struct WindowsRelayDevices {
    engine: RelayEngine,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    endpoints: RelayEndpoints,
    mode: RelayMode,
    send_source: RelaySendSource,
    receive_sink: RelayReceiveSink,
    /// Live PID resolved from a stable application selector. The PID is never
    /// persisted and is refreshed whenever the worker snapshot changes.
    application: Option<RelayApplicationSource>,
    /// Generation of the process-loopback activation, mirrored into the
    /// shared capture manager for diagnostics and consumer ownership.
    process_capture_key: Option<crate::windows::ProcessCaptureKey>,
    /// The device id WASAPI actually opened. This differs from the requested
    /// id when a configured endpoint disappeared and the current default was
    /// used as the documented fallback.
    resolved_endpoint: Option<String>,
    default_generation: u64,
}

impl WindowsRelayDevices {
    /// The endpoints this instance was started with. Changing them means
    /// restarting the devices, because a WASAPI client is bound to its device.
    pub(crate) fn endpoints(&self) -> &RelayEndpoints {
        &self.endpoints
    }

    pub(crate) fn resolved_endpoint(&self) -> Option<&str> {
        self.resolved_endpoint.as_deref()
    }

    /// Whether a local WASAPI worker is currently carrying this relay mode.
    /// The authenticated engine can remain alive while an application source
    /// is temporarily absent, so checking only for the engine would make the
    /// UI report an active route that has no audio endpoint behind it.
    pub(crate) fn endpoint_active(&self) -> bool {
        !self.stop.load(Ordering::Acquire)
            && !self.threads.is_empty()
            && self.threads.iter().all(|thread| !thread.is_finished())
    }

    pub(crate) fn needs_restart(
        &self,
        mode: RelayMode,
        send_source: &RelaySendSource,
        receive_sink: &RelayReceiveSink,
        application: Option<&RelayApplicationSource>,
        default_generation: u64,
    ) -> bool {
        let follows_default = match mode {
            RelayMode::Emitter => matches!(
                send_source,
                RelaySendSource::DefaultInput | RelaySendSource::DefaultOutputMonitor
            ),
            RelayMode::Receiver => matches!(receive_sink, RelayReceiveSink::DefaultOutput),
        };
        self.threads.is_empty()
            || self.threads.iter().any(JoinHandle::is_finished)
            || self.mode != mode
            || (mode == RelayMode::Emitter && &self.send_source != send_source)
            || (mode == RelayMode::Emitter
                && matches!(send_source, RelaySendSource::Application(_))
                && self.application.as_ref() != application)
            || (mode == RelayMode::Receiver && &self.receive_sink != receive_sink)
            || (follows_default && self.default_generation != default_generation)
    }

    /// Start the direct Windows implementation for exactly one local mode.
    ///
    /// Emitter opens one source worker: a physical eCapture device, an eRender
    /// loopback monitor, or a process-loopback activation. Receiver opens one
    /// eRender destination.
    /// Keeping the inactive direction out of the process is important on
    /// Windows: there is no virtual endpoint to hide a second active path
    /// behind, and starting both would violate the one-way flow invariant.
    pub(crate) fn start_mode(
        config: EngineConfig,
        endpoints: RelayEndpoints,
        selection: RelayWorkerSelection,
    ) -> BackendResult<Self> {
        let RelayWorkerSelection {
            mode,
            send_source,
            receive_sink,
            application,
            default_generation,
            resolved_send_source,
            resolved_receive_sink,
        } = selection;
        let (direction, target) = endpoint_for_mode(
            mode,
            &resolved_send_source,
            &resolved_receive_sink,
            application.as_ref(),
        )?;

        let engine = RelayEngine::start(config)
            .map_err(|error| BackendError::native(format!("relay engine start failed: {error}")))?;
        let stop = Arc::new(AtomicBool::new(false));
        let (thread, started) = match spawn_endpoint_thread(
            match direction {
                Direction::Input => "qpwgraph-relay-input",
                Direction::Monitor => "qpwgraph-relay-monitor",
                Direction::Render => "qpwgraph-relay-render",
                Direction::Application => "qpwgraph-relay-application",
            },
            engine.handle(),
            Arc::clone(&stop),
            direction,
            target,
        ) {
            Ok(result) => result,
            Err(error) => {
                engine.shutdown();
                return Err(error);
            }
        };
        let start = match started.recv_timeout(ENDPOINT_START_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err(BackendError::native(
                "Windows relay endpoint did not start in time",
            )),
        };
        let (resolved_endpoint, capture_generation) = match start {
            Ok(start) => (start.resolved_id, start.capture_generation),
            Err(error) => {
                stop_threads(&stop, &mut vec![thread]);
                engine.shutdown();
                return Err(error);
            }
        };
        Ok(Self {
            engine,
            stop,
            threads: vec![thread],
            endpoints,
            mode,
            send_source,
            receive_sink,
            process_capture_key: application_capture_key(application.as_ref(), capture_generation),
            application,
            resolved_endpoint: Some(resolved_endpoint),
            default_generation,
        })
    }

    /// Replace only the active WASAPI worker while retaining the relay engine
    /// and its authenticated sessions. Device-default changes and mode
    /// switches therefore do not need to tear down the control connection.
    pub(crate) fn restart_endpoint(
        &mut self,
        selection: RelayWorkerSelection,
    ) -> BackendResult<()> {
        let RelayWorkerSelection {
            mode,
            send_source,
            receive_sink,
            application,
            default_generation,
            resolved_send_source,
            resolved_receive_sink,
        } = selection;
        let (direction, target) = endpoint_for_mode(
            mode,
            &resolved_send_source,
            &resolved_receive_sink,
            application.as_ref(),
        )?;
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }

        let stop = Arc::new(AtomicBool::new(false));
        let (thread, started) = spawn_endpoint_thread(
            match direction {
                Direction::Input => "qpwgraph-relay-input",
                Direction::Monitor => "qpwgraph-relay-monitor",
                Direction::Render => "qpwgraph-relay-render",
                Direction::Application => "qpwgraph-relay-application",
            },
            self.engine.handle(),
            Arc::clone(&stop),
            direction,
            target,
        )?;
        let start = match started.recv_timeout(ENDPOINT_START_TIMEOUT) {
            Ok(result) => result,
            Err(_) => Err(BackendError::native(
                "Windows relay endpoint did not start in time",
            )),
        };
        let (resolved_endpoint, capture_generation) = match start {
            Ok(start) => (start.resolved_id, start.capture_generation),
            Err(error) => {
                stop_threads(&stop, &mut vec![thread]);
                self.stop = stop;
                return Err(error);
            }
        };
        self.stop = stop;
        self.threads.push(thread);
        self.mode = mode;
        self.send_source = send_source;
        self.receive_sink = receive_sink;
        self.process_capture_key =
            application_capture_key(application.as_ref(), capture_generation);
        self.application = application;
        self.resolved_endpoint = Some(resolved_endpoint);
        self.default_generation = default_generation;
        Ok(())
    }

    pub(crate) fn handle(&self) -> RelayHandle {
        self.engine.handle()
    }

    pub(crate) fn process_capture_key(&self) -> Option<&crate::windows::ProcessCaptureKey> {
        self.process_capture_key.as_ref()
    }

    /// Stop an application capture while retaining the authenticated relay
    /// engine. When the same stable selector reappears, `ensure_relay` can
    /// resolve its new PID and restart only this worker without forcing the
    /// peer to pair again.
    pub(crate) fn deactivate_application(&mut self, message: &str) {
        if self.application.is_none() && self.threads.is_empty() {
            return;
        }
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        self.stop = Arc::new(AtomicBool::new(false));
        self.application = None;
        self.process_capture_key = None;
        self.resolved_endpoint = None;
        self.engine.handle().report_error(message);
    }
}

impl Drop for WindowsRelayDevices {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        self.engine.shutdown();
    }
}

impl std::fmt::Debug for WindowsRelayDevices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsRelayDevices")
            .field("endpoint_threads", &self.threads.len())
            .field("mode", &self.mode)
            .field("resolved_endpoint", &self.resolved_endpoint)
            .field("application", &self.application)
            .field("process_capture_key", &self.process_capture_key)
            .field("default_generation", &self.default_generation)
            .field("stopping", &self.stop.load(Ordering::Acquire))
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Direction {
    /// Record a physical eCapture input device.
    Input,
    /// Loopback-record an eRender playback endpoint.
    Monitor,
    /// Play what the engine received on the selected playback endpoint.
    Render,
    /// Capture one live process without changing its normal local output.
    Application,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RelayApplicationSource {
    pub(crate) selector_key: String,
    pub(crate) pid: u32,
}

/// The durable relay selection plus the live WASAPI-bound selectors. Keeping
/// these together prevents the restart path from accidentally comparing one
/// identity while opening another endpoint.
pub(crate) struct RelayWorkerSelection {
    pub(crate) mode: RelayMode,
    pub(crate) send_source: RelaySendSource,
    pub(crate) receive_sink: RelayReceiveSink,
    pub(crate) application: Option<RelayApplicationSource>,
    pub(crate) default_generation: u64,
    pub(crate) resolved_send_source: RelaySendSource,
    pub(crate) resolved_receive_sink: RelayReceiveSink,
}

enum EndpointTarget {
    Device(Option<String>),
    Application(RelayApplicationSource),
}

struct EndpointStart {
    resolved_id: String,
    capture_generation: Option<u64>,
}

type StartResult = Receiver<BackendResult<EndpointStart>>;

fn application_capture_key(
    application: Option<&RelayApplicationSource>,
    generation: Option<u64>,
) -> Option<crate::windows::ProcessCaptureKey> {
    let application = application?;
    Some(crate::windows::ProcessCaptureKey {
        selector: application.selector_key.clone(),
        pid: application.pid,
        generation: generation?,
        mode: ProcessLoopbackMode::IncludeProcessTree,
    })
}

fn endpoint_for_mode(
    mode: RelayMode,
    send_source: &RelaySendSource,
    receive_sink: &RelayReceiveSink,
    application: Option<&RelayApplicationSource>,
) -> BackendResult<(Direction, EndpointTarget)> {
    match mode {
        RelayMode::Emitter => match send_source {
            RelaySendSource::DefaultInput => Ok((Direction::Input, EndpointTarget::Device(None))),
            RelaySendSource::InputDevice(id) => {
                Ok((Direction::Input, EndpointTarget::Device(Some(id.clone()))))
            }
            RelaySendSource::DefaultOutputMonitor => {
                Ok((Direction::Monitor, EndpointTarget::Device(None)))
            }
            RelaySendSource::OutputMonitor(id) => {
                Ok((Direction::Monitor, EndpointTarget::Device(Some(id.clone()))))
            }
            RelaySendSource::Application(selector) => {
                let Some(application) = application else {
                    return Err(BackendError::unsupported(
                        "selected application is not currently available for process-loopback capture",
                    ));
                };
                if !application.selector_key.eq_ignore_ascii_case(selector) {
                    return Err(BackendError::unsupported(
                        "selected application is no longer available for process-loopback capture",
                    ));
                }
                Ok((
                    Direction::Application,
                    EndpointTarget::Application(application.clone()),
                ))
            }
            RelaySendSource::ManualGraph => Err(BackendError::unsupported(
                "Windows direct relay cannot use a manual graph source",
            )),
        },
        RelayMode::Receiver => match receive_sink {
            RelayReceiveSink::DefaultOutput => {
                Ok((Direction::Render, EndpointTarget::Device(None)))
            }
            RelayReceiveSink::OutputDevice(id) => {
                Ok((Direction::Render, EndpointTarget::Device(Some(id.clone()))))
            }
            RelayReceiveSink::VirtualMicrophone => Ok((
                Direction::Render,
                EndpointTarget::Device(Some("qpwgraph://relay-render".into())),
            )),
            RelayReceiveSink::ManualGraph => Err(BackendError::unsupported(
                "Windows direct relay cannot use a manual graph sink",
            )),
        },
    }
}

fn spawn_endpoint_thread(
    name: &str,
    handle: RelayHandle,
    stop: Arc<AtomicBool>,
    direction: Direction,
    target: EndpointTarget,
) -> BackendResult<(JoinHandle<()>, StartResult)> {
    let (started_tx, started_rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            // Every COM apartment is per-thread, so each endpoint initializes
            // its own and tears it down on the way out.
            let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
            if initialized.is_err() {
                let _ = started_tx.send(Err(BackendError::native(format!(
                    "could not initialize COM for the relay endpoint: {initialized:?}"
                ))));
                return;
            }
            run_endpoint(&handle, &stop, direction, target, started_tx);
            unsafe { Com::CoUninitialize() };
        })
        .map_err(|error| {
            BackendError::native(format!("could not start relay endpoint: {error}"))
        })?;
    Ok((thread, started_rx))
}

fn stop_threads(stop: &Arc<AtomicBool>, threads: &mut Vec<JoinHandle<()>>) {
    stop.store(true, Ordering::Release);
    for thread in threads.drain(..) {
        let _ = thread.join();
    }
}

/// Interleaved 48 kHz stereo float, which is what the engine speaks.
fn relay_format() -> Audio::WAVEFORMATEX {
    let channels = RELAY_CHANNELS;
    let bits = 32u16;
    let block_align = channels * bits / 8;
    Audio::WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: channels,
        nSamplesPerSec: RELAY_SAMPLE_RATE,
        nAvgBytesPerSec: RELAY_SAMPLE_RATE * u32::from(block_align),
        nBlockAlign: block_align,
        wBitsPerSample: bits,
        cbSize: 0,
    }
}

/// Open the endpoint, report whether WASAPI accepted it, then run until asked
/// to stop. The report is sent exactly once, so `start` can surface a real
/// error instead of leaving a silent host behind.
fn run_endpoint(
    handle: &RelayHandle,
    stop: &Arc<AtomicBool>,
    direction: Direction,
    target: EndpointTarget,
    started: Sender<BackendResult<EndpointStart>>,
) {
    if direction == Direction::Application {
        let EndpointTarget::Application(application) = target else {
            let _ = started.send(Err(BackendError::native(
                "Windows relay application endpoint had no process selector",
            )));
            return;
        };
        let format = AudioFormat::new(RELAY_SAMPLE_RATE, RELAY_CHANNELS);
        match verify_live_process_identity(&application.selector_key, application.pid).and_then(
            |_| {
                ProcessLoopbackSource::open(
                    application.pid,
                    ProcessLoopbackMode::IncludeProcessTree,
                    format,
                    APPLICATION_RING_FRAMES,
                )
            },
        ) {
            Ok((mut source, activation)) => {
                let _ = started.send(Ok(EndpointStart {
                    resolved_id: format!("application:{}", application.selector_key),
                    capture_generation: Some(activation.generation()),
                }));
                application_capture_loop(&mut source, handle, stop);
            }
            Err(error) => {
                let _ = started.send(Err(error));
            }
        }
        return;
    }
    let EndpointTarget::Device(device_id) = target else {
        let _ = started.send(Err(BackendError::native(
            "Windows relay physical endpoint had an application target",
        )));
        return;
    };
    // The service is acquired here, before the endpoint reports success.
    // Doing it inside the loop hid a real failure: `GetService` was being
    // called as `cast`, which returns E_NOINTERFACE, so both loops exited
    // immediately and the relay carried no audio while still looking started.
    let opened =
        open_endpoint(direction, device_id.as_deref()).and_then(|(client, resolved_id)| {
            match direction {
                Direction::Input | Direction::Monitor => {
                    unsafe { client.GetService::<Audio::IAudioCaptureClient>() }
                        .map(|service| (client, Service::Capture(service)))
                        .map(|(client, service)| (client, service, resolved_id))
                        .map_err(|error| native("get capture service", error))
                }
                Direction::Render => unsafe { client.GetService::<Audio::IAudioRenderClient>() }
                    .map(|service| (client, Service::Render(service)))
                    .map(|(client, service)| (client, service, resolved_id))
                    .map_err(|error| native("get render service", error)),
                Direction::Application => {
                    unreachable!("process-loopback bypasses WASAPI endpoint service")
                }
            }
        });
    match opened {
        Ok((client, service, resolved_id)) => {
            if let Err(error) = unsafe { client.Start() } {
                let _ = started.send(Err(native("start audio client", error)));
                return;
            }
            let _ = started.send(Ok(EndpointStart {
                resolved_id,
                capture_generation: None,
            }));
            match service {
                Service::Capture(capture) => capture_loop(&capture, handle, stop),
                Service::Render(render) => render_loop(&client, &render, handle, stop),
            }
            if let Err(error) = unsafe { client.Stop() } {
                report_endpoint_error(handle, "stop audio client", error);
            }
        }
        Err(error) => {
            let _ = started.send(Err(error));
        }
    }
}

/// Drain the bounded process-loopback ring into the relay engine. The
/// activation worker owns the Windows capture client; this thread only moves
/// already-converted PCM across the router boundary.
fn application_capture_loop(source: &mut RingSource, handle: &RelayHandle, stop: &Arc<AtomicBool>) {
    let format = source.format();
    let mut block = vec![0.0f32; format.samples(APPLICATION_BLOCK_FRAMES)];
    while !stop.load(Ordering::Acquire) {
        let read = source.read(&mut block);
        if read.health == StreamHealth::Lost {
            handle.report_error("Windows relay process-loopback source was lost");
            break;
        }
        if read.frames > 0 {
            handle.push_capture(&block[..format.samples(read.frames)]);
        } else {
            thread::sleep(POLL_INTERVAL);
        }
    }
}

enum Service {
    Capture(Audio::IAudioCaptureClient),
    Render(Audio::IAudioRenderClient),
}

fn open_endpoint(
    direction: Direction,
    device_id: Option<&str>,
) -> BackendResult<(Audio::IAudioClient, String)> {
    let enumerator: Audio::IMMDeviceEnumerator =
        unsafe { Com::CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|error| native("create MMDeviceEnumerator", error))?;
    let flow = match direction {
        Direction::Input => Audio::eCapture,
        Direction::Monitor | Direction::Render => Audio::eRender,
        Direction::Application => unreachable!("process-loopback does not use MMDeviceEnumerator"),
    };
    // A physical device id that has since been unplugged falls back to the
    // current default of the required data flow. The provider-owned relay
    // target is different: it fails closed rather than sending peer audio to
    // an unrelated physical endpoint.
    let device = match device_id {
        Some("qpwgraph://relay-render") => Some(
            find_qpwgraph_endpoint(&enumerator, flow, QpwVirtualEndpointRole::RelayRender)?
                .ok_or_else(|| {
                    BackendError::unsupported(
                        "QPWGraph Relay Sink requires the optional virtual audio driver",
                    )
                })?,
        ),
        Some(id) => {
            let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe { enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr())) }.ok()
        }
        None => None,
    };
    let device = match device {
        Some(device) => device,
        None => unsafe { enumerator.GetDefaultAudioEndpoint(flow, Audio::eConsole) }
            .map_err(|error| native("open default audio endpoint", error))?,
    };
    let resolved_id = unsafe { device.GetId() }
        .map(take_pwstr)
        .map_err(|error| native("read resolved audio endpoint id", error))?;
    let client: Audio::IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|error| native("activate audio client", error))?;

    let format = relay_format();
    // AUTOCONVERTPCM makes the audio engine resample and remix for us, so an
    // endpoint running at 44.1 kHz or 7.1 still yields the relay's format and
    // no resampler is needed here.
    let mut flags =
        Audio::AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | Audio::AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    if matches!(direction, Direction::Monitor) {
        flags |= Audio::AUDCLNT_STREAMFLAGS_LOOPBACK;
    }
    unsafe {
        client.Initialize(
            Audio::AUDCLNT_SHAREMODE_SHARED,
            flags,
            BUFFER_DURATION_HNS,
            0,
            &format,
            None,
        )
    }
    .map_err(|error| native("initialize audio client", error))?;

    Ok((client, resolved_id))
}

fn take_pwstr(value: PWSTR) -> String {
    let text = unsafe { value.to_string() }.unwrap_or_default();
    unsafe { Com::CoTaskMemFree(Some(value.0 as *mut _)) };
    text
}

/// Loopback-record the playback endpoint and hand every frame to the engine.
fn capture_loop(
    capture: &Audio::IAudioCaptureClient,
    handle: &RelayHandle,
    stop: &Arc<AtomicBool>,
) {
    let channels = usize::from(RELAY_CHANNELS);
    // WASAPI can mark a packet silent without populating its buffer. Keep a
    // reusable bounded block so a quiet playback device does not allocate on
    // every poll.
    let silence = vec![0.0f32; 4096 * channels];

    while !stop.load(Ordering::Acquire) {
        let mut pending = match unsafe { capture.GetNextPacketSize() } {
            Ok(pending) => pending,
            Err(error) => {
                report_endpoint_error(handle, "read capture packet size", error);
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
            if let Err(error) =
                unsafe { capture.GetBuffer(&mut data, &mut frames, &mut buffer_flags, None, None) }
            {
                report_endpoint_error(handle, "read capture buffer", error);
                return;
            }
            if frames > 0 {
                if buffer_flags & Audio::AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    // WASAPI is allowed to hand back a silent packet without
                    // filling the buffer, so synthesise the silence instead of
                    // reading whatever the pointer happens to hold.
                    let mut remaining = frames as usize * channels;
                    while remaining > 0 {
                        let chunk = remaining.min(silence.len());
                        handle.push_capture(&silence[..chunk]);
                        remaining -= chunk;
                    }
                } else if !data.is_null() {
                    let samples = unsafe {
                        std::slice::from_raw_parts(data.cast::<f32>(), frames as usize * channels)
                    };
                    handle.push_capture(samples);
                } else {
                    handle.report_error("Windows relay capture endpoint returned a null buffer");
                    return;
                }
            }
            if let Err(error) = unsafe { capture.ReleaseBuffer(frames) } {
                report_endpoint_error(handle, "release capture buffer", error);
                return;
            }
            pending = match unsafe { capture.GetNextPacketSize() } {
                Ok(pending) => pending,
                Err(error) => {
                    report_endpoint_error(handle, "read capture packet size", error);
                    return;
                }
            };
        }
    }
}

/// Rate-correction step applied per adaptation tick, as a fraction of the
/// nominal read rate. Two hundred parts per million per tick crosses any
/// realistic clock drift within a few ticks, while each individual nudge
/// stays far below audibility.
const DRIFT_RATE_STEP: f64 = 0.0002;
/// Hard clamp on the read rate's distance from nominal. Real crystal drift
/// sits in the tens of ppm; the clamp is orders of magnitude wider so the
/// adapter can also recover from a backlog burst, while linear interpolation
/// at this ratio remains transparent.
const DRIFT_RATE_LIMIT: f64 = 0.002;
/// Render iterations between backlog samples. The poll loop turns over
/// roughly every [`POLL_INTERVAL`], so this samples the backlog a couple of
/// times per second — fast enough to track drift, slow enough that the
/// session-table lock behind `playback_levels` is irrelevant.
const DRIFT_TICK_INTERVAL: u32 = 128;

/// Variable-rate reader between the relay engine and the render endpoint.
///
/// The engine fills its playback queues from the peer's capture clock; the
/// render endpoint drains at its own device clock. The two disagree by tens
/// of parts per million, and a fixed-rate reader turns that drift into
/// audible events every few minutes: underrun silence when the local clock
/// runs fast, drop-oldest clicks when it runs slow. The adapter reads at a
/// micro-adjusted rate, nudged by the engine backlog, so the same drift is
/// absorbed as a constant, inaudible resampling.
///
/// The interpolation is linear between neighbouring frames, with a small
/// FIFO so frames the grid has not yet reached survive across pulls —
/// stretching (rate < 1) must never discard audio, and squeezing (rate > 1)
/// must never repeat it. At exactly 1.0 the grid lands on whole frames and
/// the output is bit-identical to what the engine produced.
struct DriftAdapter {
    channels: usize,
    /// Input frames consumed per output frame; 1.0 is exact passthrough.
    rate: f64,
    /// Grid position of the next output frame, in FIFO frames. Always kept
    /// in `[0, 1)` between calls by the retirement step in [`Self::render`].
    position: f64,
    /// Valid frames currently held in `fifo`.
    filled: usize,
    /// PCM handed over by the engine but not yet consumed by the grid.
    fifo: Vec<f32>,
    ticks: u32,
}

impl DriftAdapter {
    fn new(channels: usize, max_output_frames: usize) -> Self {
        let frames = ((max_output_frames as f64 * (1.0 + DRIFT_RATE_LIMIT)) as usize) + 4;
        Self {
            channels,
            rate: 1.0,
            position: 0.0,
            filled: 0,
            fifo: vec![0.0; frames * channels],
            ticks: 0,
        }
    }

    /// Sample the engine backlog every [`DRIFT_TICK_INTERVAL`] calls and
    /// nudge the read rate back toward equilibrium. The receive queues trim
    /// to `target` on every push, so aiming at its midpoint keeps the depth
    /// clear of both the drop-oldest trim and the underrun floor: a deep
    /// backlog means the peer's clock runs fast here and the grid must
    /// advance quicker, a starved one means it runs slow and the grid must
    /// stretch.
    fn observe(&mut self, depth: usize, target: usize) {
        self.ticks += 1;
        if self.ticks < DRIFT_TICK_INTERVAL {
            return;
        }
        self.ticks = 0;
        if target == 0 {
            return;
        }
        let ideal = target / 2;
        if depth > ideal + ideal / 2 {
            self.rate += DRIFT_RATE_STEP;
        } else if depth < ideal / 2 {
            self.rate -= DRIFT_RATE_STEP;
        } else {
            return;
        }
        self.rate = self
            .rate
            .clamp(1.0 - DRIFT_RATE_LIMIT, 1.0 + DRIFT_RATE_LIMIT);
    }

    fn tick(&mut self, handle: &RelayHandle) {
        let (depth, target) = handle.playback_levels();
        self.observe(depth, target);
    }

    /// Fill `output` completely from the engine, pulling through `pull` at
    /// the adapted rate. Whatever the engine cannot supply plays as silence.
    fn render(&mut self, output: &mut [f32], mut pull: impl FnMut(&mut [f32]) -> usize) {
        let channels = self.channels;
        let out_frames = output.len() / channels;
        if out_frames == 0 {
            return;
        }
        // Keep a little more on hand than one output's worth: interpolation
        // always needs one lookahead frame, and the grid may run up to one
        // `rate` step past the last producible point before the next pull.
        let want_frames =
            (((out_frames as f64) * self.rate).ceil() as usize + 4).min(self.fifo.len() / channels);
        if self.filled < want_frames {
            let room = want_frames - self.filled;
            let got = pull(&mut self.fifo[self.filled * channels..(self.filled + room) * channels]);
            self.filled += got / channels;
        }

        let mut produced = 0usize;
        let mut position = self.position;
        while produced < out_frames {
            let base = position.floor();
            let right = base as isize + 1;
            if right < 0 || right as usize >= self.filled {
                // The grid ran past the audio on hand: play silence until
                // the engine catches up. Unread frames stay buffered.
                break;
            }
            debug_assert!(base >= 0.0, "the grid never moves backwards");
            let fraction = (position - base) as f32;
            for channel in 0..channels {
                let left = self.fifo[base as usize * channels + channel];
                let sample = self.fifo[right as usize * channels + channel];
                output[produced * channels + channel] = left + (sample - left) * fraction;
            }
            produced += 1;
            position += self.rate;
        }
        for slot in output[produced * channels..].iter_mut() {
            *slot = 0.0;
        }

        // Retire the frames the grid has fully passed, so the next call's
        // grid starts inside `[0, 1)` again and the FIFO cannot grow.
        let used = position.floor().max(0.0) as usize;
        let dropped = used.min(self.filled);
        if dropped > 0 {
            self.fifo
                .copy_within(dropped * channels..self.filled * channels, 0);
            self.filled -= dropped;
            position -= dropped as f64;
        }
        self.position = position;
    }
}

/// Drain audio received from peers onto the playback endpoint.
fn render_loop(
    client: &Audio::IAudioClient,
    render: &Audio::IAudioRenderClient,
    handle: &RelayHandle,
    stop: &Arc<AtomicBool>,
) {
    let buffer_frames = match unsafe { client.GetBufferSize() } {
        Ok(buffer_frames) => buffer_frames,
        Err(error) => {
            report_endpoint_error(handle, "read render buffer size", error);
            return;
        }
    };
    if buffer_frames == 0 {
        handle.report_error("Windows relay render endpoint returned a zero-sized buffer");
        return;
    }
    let channels = usize::from(RELAY_CHANNELS);
    let mut scratch = vec![0.0f32; buffer_frames as usize * channels];
    // The engine fills its queues from the peer's capture clock while this
    // loop drains on the endpoint's own clock; the adapter absorbs the
    // difference as a micro-adjusted read rate instead of letting it
    // accumulate into underrun silence and drop-oldest clicks.
    let mut adapter = DriftAdapter::new(channels, buffer_frames as usize);

    while !stop.load(Ordering::Acquire) {
        let padding = match unsafe { client.GetCurrentPadding() } {
            Ok(padding) => padding,
            Err(error) => {
                report_endpoint_error(handle, "read render padding", error);
                break;
            }
        };
        let available = buffer_frames.saturating_sub(padding);
        if available == 0 {
            thread::sleep(POLL_INTERVAL);
            continue;
        }
        let wanted = available as usize * channels;
        adapter.tick(handle);
        adapter.render(&mut scratch[..wanted], |out| handle.pull_playback(out));

        let data = match unsafe { render.GetBuffer(available) } {
            Ok(data) => data,
            Err(error) => {
                report_endpoint_error(handle, "acquire render buffer", error);
                break;
            }
        };
        if data.is_null() {
            handle.report_error("Windows relay render endpoint returned a null buffer");
            break;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(scratch.as_ptr(), data.cast::<f32>(), wanted);
            if let Err(error) = render.ReleaseBuffer(available, 0) {
                report_endpoint_error(handle, "release render buffer", error);
                break;
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn report_endpoint_error(handle: &RelayHandle, operation: &str, error: windows::core::Error) {
    handle.report_error(format!("Windows relay {operation} failed: {error}"));
}

fn native(context: &str, error: windows::core::Error) -> BackendError {
    BackendError::native(format!("Windows relay {context} failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_relay_format_describes_interleaved_stereo_float() {
        let format = relay_format();
        // WAVEFORMATEX is packed, so each field has to be copied out before it
        // can be compared.
        let (tag, channels) = (format.wFormatTag, format.nChannels);
        let (rate, bits) = (format.nSamplesPerSec, format.wBitsPerSample);
        let (block_align, avg_bytes) = (format.nBlockAlign, format.nAvgBytesPerSec);

        assert_eq!(tag, WAVE_FORMAT_IEEE_FLOAT);
        assert_eq!(channels, RELAY_CHANNELS);
        assert_eq!(rate, RELAY_SAMPLE_RATE);
        assert_eq!(bits, 32);
        // A frame is one sample per channel; getting this wrong makes WASAPI
        // walk the buffer at the wrong stride, so the audio comes out
        // pitch-shifted rather than failing outright.
        assert_eq!(block_align, RELAY_CHANNELS * 4);
        assert_eq!(avg_bytes, RELAY_SAMPLE_RATE * u32::from(block_align));
    }

    #[test]
    fn application_sources_require_a_live_process_loopback_target() {
        let source = RelaySendSource::Application("sha256:abc".into());
        let missing = endpoint_for_mode(
            RelayMode::Emitter,
            &source,
            &RelayReceiveSink::DefaultOutput,
            None,
        );
        assert!(missing.is_err());

        let application = RelayApplicationSource {
            selector_key: "sha256:abc".into(),
            pid: 42,
        };
        let (direction, target) = endpoint_for_mode(
            RelayMode::Emitter,
            &source,
            &RelayReceiveSink::DefaultOutput,
            Some(&application),
        )
        .expect("a matching live application should resolve");
        assert_eq!(direction, Direction::Application);
        assert!(matches!(target, EndpointTarget::Application(found) if found == application));
    }

    /// Deterministic engine stand-in: hands out consecutive frames, then runs
    /// dry, so adapter tests never need WASAPI or a live relay engine.
    struct ScriptedSource {
        pcm: Vec<f32>,
        offset: usize,
    }

    impl ScriptedSource {
        fn pull(&mut self, out: &mut [f32]) -> usize {
            let count = out.len().min(self.pcm.len() - self.offset);
            out[..count].copy_from_slice(&self.pcm[self.offset..self.offset + count]);
            self.offset += count;
            count
        }
    }

    #[test]
    fn identity_rate_is_bit_exact_across_calls() {
        let channels = 2;
        let mut adapter = DriftAdapter::new(channels, 8);
        let fifo_capacity = adapter.fifo.len();
        // More stream than the run consumes, so the final output still has
        // its interpolation lookahead; running a resampler to exact end of
        // stream legitimately decays to silence on the last frame.
        let pcm: Vec<f32> = (0..256 * channels)
            .map(|i| ((i % 7) as f32 / 7.0) - 0.5)
            .collect();
        let mut source = ScriptedSource { pcm, offset: 0 };
        let mut played = Vec::new();
        for _ in 0..12 {
            let mut out = vec![0.0f32; 8 * channels];
            adapter.render(&mut out, |buf| source.pull(buf));
            played.extend_from_slice(&out);
        }
        // At rate 1.0 the grid lands on whole frames, so nothing may be
        // resampled, dropped, or duplicated across the call boundaries.
        assert_eq!(played, source.pcm[..played.len()]);
        assert_eq!(played.len(), 96 * channels);
        assert!(played.iter().all(|sample| sample.is_finite()));
        assert_eq!(adapter.fifo.len(), fifo_capacity, "the FIFO must not grow");
    }

    #[test]
    fn backlog_nudges_the_read_rate_both_ways_and_clamps() {
        let mut adapter = DriftAdapter::new(2, 8);
        // Four frames of stereo 48 kHz — the receive queues' trim depth.
        let target = 4 * 960 * 2;
        // Dead centre of the comfort band: no correction.
        adapter.ticks = DRIFT_TICK_INTERVAL - 1;
        adapter.observe(target / 2, target);
        assert_eq!(adapter.rate, 1.0);
        // Deep backlog: the peer's clock runs fast here, the grid squeezes,
        // and repeated nudges stop at the limit.
        for _ in 0..20 {
            adapter.ticks = DRIFT_TICK_INTERVAL - 1;
            adapter.observe(target * 2, target);
        }
        assert!((adapter.rate - (1.0 + DRIFT_RATE_LIMIT)).abs() < 1e-9);
        // Starved: the peer's clock runs slow, the grid stretches, and the
        // opposite clamp applies.
        for _ in 0..20 {
            adapter.ticks = DRIFT_TICK_INTERVAL - 1;
            adapter.observe(0, target);
        }
        assert!((adapter.rate - (1.0 - DRIFT_RATE_LIMIT)).abs() < 1e-9);
    }

    #[test]
    fn an_empty_engine_plays_silence_then_resumes_at_the_stream_start() {
        let channels = 2;
        let mut adapter = DriftAdapter::new(channels, 8);
        let mut out = vec![0.25f32; 8 * channels];
        adapter.render(&mut out, |_| 0);
        assert!(out.iter().all(|sample| *sample == 0.0));

        let pcm: Vec<f32> = (0..16 * channels)
            .map(|i| (i as f32 * 0.125).fract())
            .collect();
        let mut source = ScriptedSource { pcm, offset: 0 };
        let mut out = vec![-1.0f32; 8 * channels];
        adapter.render(&mut out, |buf| source.pull(buf));
        // The first rendered frame is the first frame of the stream, not a
        // resume from stale grid state.
        assert_eq!(&out[..2], &source.pcm[..2]);
        assert!(out.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn the_rate_moves_input_consumption_in_the_right_direction() {
        let channels = 1;
        let pcm: Vec<f32> = vec![0.5; 4096];
        let run = |rate_hint: f64| {
            let mut adapter = DriftAdapter::new(channels, 64);
            adapter.rate = rate_hint;
            let mut source = ScriptedSource {
                pcm: pcm.clone(),
                offset: 0,
            };
            for _ in 0..32 {
                let mut out = vec![0.0f32; 64];
                adapter.render(&mut out, |buf| source.pull(buf));
            }
            source.offset
        };
        let squeezed = run(1.0 + DRIFT_RATE_LIMIT);
        let stretched = run(1.0 - DRIFT_RATE_LIMIT);
        assert!(squeezed > stretched, "a faster grid must read more input");
        // 2048 outputs at ±0.2% differ by about 8 input frames; the slightly
        // different FIFO leftovers at the end are worth a frame of slack.
        assert!(squeezed - stretched >= 6, "{squeezed} vs {stretched}");
    }
}
