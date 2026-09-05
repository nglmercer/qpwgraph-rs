//! Per-process WASAPI loopback capture.
//!
//! Activation is asynchronous even though [`ProcessLoopbackSource::open`]
//! presents a synchronous boundary. The activation parameters, blob,
//! `PROPVARIANT`, completion handler, and operation all remain owned by the
//! worker until `ActivateCompleted` has run. In particular, the blob never
//! points at a stack temporary; an earlier implementation of this feature did
//! that and could corrupt the process heap.

use std::collections::BTreeMap;
use std::mem::{size_of, ManuallyDrop};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::core::{IUnknown, Interface};
use windows::Win32::Media::Audio;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{self, BLOB, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VT_BLOB;

use crate::api::{BackendError, BackendResult};
use crate::router::{ring_source, AudioFormat, RingSource, RingSourceFeed};

const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const BUFFER_DURATION_HNS: i64 = 400_000;
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// Which process tree the virtual loopback device captures.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ProcessLoopbackMode {
    IncludeProcessTree,
    ExcludeProcessTree,
}

/// Result of operational process-loopback capability detection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessLoopbackCapability {
    Unknown,
    Available,
    Unavailable { hresult: i32, reason: String },
}

impl ProcessLoopbackCapability {
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Unavailable { reason, .. } => Some(reason),
            Self::Unknown | Self::Available => None,
        }
    }
}

/// A live process-loopback worker feeding a router source ring.
pub struct ProcessLoopbackSource {
    pid: u32,
    mode: ProcessLoopbackMode,
    generation: u64,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ProcessLoopbackSource {
    fn capability_cache(
    ) -> &'static Mutex<BTreeMap<(u32, ProcessLoopbackMode), ProcessLoopbackCapability>> {
        static CACHE: OnceLock<
            Mutex<BTreeMap<(u32, ProcessLoopbackMode), ProcessLoopbackCapability>>,
        > = OnceLock::new();
        CACHE.get_or_init(|| Mutex::new(BTreeMap::new()))
    }

    /// Probe the operating system without creating a long-lived worker.
    ///
    /// The result is deliberately based on the activation HRESULT rather
    /// than a Windows version string: enterprise builds and protected audio
    /// paths can expose different capabilities on the same OS release.
    pub fn detect_capability(pid: u32, mode: ProcessLoopbackMode) -> ProcessLoopbackCapability {
        if pid == 0 {
            return ProcessLoopbackCapability::Unavailable {
                hresult: -1,
                reason: "process-loopback PID must be non-zero".into(),
            };
        }
        if let Ok(cache) = Self::capability_cache().lock() {
            if let Some(capability) = cache.get(&(pid, mode)) {
                return capability.clone();
            }
        }
        // Keep the operational probe off the caller's thread. If Core Audio
        // never delivers its asynchronous completion callback, a bounded
        // probe can detach this worker while the worker retains the activation
        // blob until Windows finally releases it; blocking a UI/COM apartment
        // here would make capability discovery an app-wide hang.
        let (result_tx, result_rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name(format!("qpwgraph-process-loopback-probe-{pid}"))
            .spawn(move || {
                let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
                if initialized.is_err() {
                    let error = windows::core::Error::from(initialized);
                    let _ = result_tx.send(Err((error.code().0, error.to_string())));
                    return;
                }
                let result = activate_process_audio_client(pid, mode)
                    .map(|_| ())
                    .map_err(|error| (error.code().0, error.to_string()));
                unsafe { Com::CoUninitialize() };
                let _ = result_tx.send(result);
            });
        let result = match worker {
            Ok(worker) => match result_rx.recv_timeout(START_TIMEOUT) {
                Ok(result) => {
                    let _ = worker.join();
                    result
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    drop(worker);
                    Err((
                        windows::Win32::Foundation::ERROR_TIMEOUT.0 as i32,
                        "timed out waiting for process-loopback activation".into(),
                    ))
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let _ = worker.join();
                    Err((
                        windows::Win32::Foundation::E_FAIL.0,
                        "process-loopback activation probe disconnected".into(),
                    ))
                }
            },
            Err(error) => Err((
                windows::Win32::Foundation::E_FAIL.0,
                format!("could not start process-loopback capability probe: {error}"),
            )),
        };
        let capability = match result {
            Ok(_) => ProcessLoopbackCapability::Available,
            Err((hresult, reason)) => ProcessLoopbackCapability::Unavailable { hresult, reason },
        };
        if let Ok(mut cache) = Self::capability_cache().lock() {
            cache.insert((pid, mode), capability.clone());
        }
        capability
    }

    /// Drop operational probe results after a device/audio-service change.
    /// The next capability query performs a fresh activation.
    pub fn clear_capability_cache() {
        if let Ok(mut cache) = Self::capability_cache().lock() {
            cache.clear();
        }
    }

    /// Activate and start capture for `pid` and return the router-facing ring.
    ///
    /// Capability detection is deliberately operational: unsupported Windows
    /// versions return their HRESULT instead of being guessed from a version
    /// number.
    pub fn open(
        pid: u32,
        mode: ProcessLoopbackMode,
        format: AudioFormat,
        ring_frames: usize,
    ) -> BackendResult<(RingSource, Self)> {
        if pid == 0 {
            return Err(BackendError::native(
                "process-loopback PID must be non-zero",
            ));
        }
        if !format.is_valid() {
            return Err(BackendError::native(format!(
                "{} Hz with {} channels is not a usable process-loopback format",
                format.sample_rate, format.channels
            )));
        }

        static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
        let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        let (source, feed) = ring_source(format, ring_frames);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (started_tx, started_rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name(format!("qpwgraph-process-loopback-{pid}"))
            .spawn(move || run(pid, mode, format, feed, &worker_stop, started_tx))
            .map_err(|error| {
                BackendError::native(format!("could not start process-loopback worker: {error}"))
            })?;

        match started_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => Ok((
                source,
                Self {
                    pid,
                    mode,
                    generation,
                    stop,
                    worker: Some(worker),
                },
            )),
            Ok(Err(error)) => {
                stop.store(true, Ordering::Release);
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                stop.store(true, Ordering::Release);
                // Activation has no cancellation API and may still own the
                // caller-provided PROPVARIANT/blob. Detaching the worker on a
                // timeout lets that owning state live until the completion
                // callback runs, while `run` observes `stop` before starting
                // a client if activation eventually succeeds.
                drop(worker);
                Err(BackendError::native(
                    "process-loopback activation did not complete in time",
                ))
            }
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn mode(&self) -> ProcessLoopbackMode {
        self.mode
    }

    /// Distinguishes this activation from a later capture of a reused PID.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the worker is still pumping samples. A worker can finish while
    /// the owning route remains in the graph (for example after a target
    /// process exits), so callers should surface this as a degraded route and
    /// rebuild it for a newly resolved process identity.
    pub fn is_running(&self) -> bool {
        !self.stop.load(Ordering::Acquire)
            && self
                .worker
                .as_ref()
                .is_some_and(|worker| !worker.is_finished())
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for ProcessLoopbackSource {
    fn drop(&mut self) {
        self.stop();
    }
}

impl std::fmt::Debug for ProcessLoopbackSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessLoopbackSource")
            .field("pid", &self.pid)
            .field("mode", &self.mode)
            .field("generation", &self.generation)
            .field("running", &self.is_running())
            .finish()
    }
}

fn run(
    pid: u32,
    mode: ProcessLoopbackMode,
    format: AudioFormat,
    feed: RingSourceFeed,
    stop: &AtomicBool,
    started: Sender<BackendResult<()>>,
) {
    let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
    if initialized.is_err() {
        let _ = started.send(Err(native(
            "initialize COM for process loopback",
            windows::core::Error::from(initialized),
        )));
        return;
    }

    let result = activate_process_audio_client(pid, mode)
        .map_err(|error| native("activate process-loopback audio client", error))
        .and_then(|client| {
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            initialize_client(&client, format)?;
            let capture = unsafe { client.GetService::<Audio::IAudioCaptureClient>() }
                .map_err(|error| native("acquire process-loopback capture service", error))?;
            unsafe { client.Start() }
                .map_err(|error| native("start process-loopback capture", error))?;
            let _ = started.send(Ok(()));
            capture_loop(&capture, feed, format, stop);
            let _ = unsafe { client.Stop() };
            Ok(())
        });
    if let Err(error) = result {
        let _ = started.send(Err(error));
    }
    unsafe { Com::CoUninitialize() };
}

#[windows::core::implement(Audio::IActivateAudioInterfaceCompletionHandler)]
struct CompletionHandler(Sender<windows::core::Result<()>>);

impl Audio::IActivateAudioInterfaceCompletionHandler_Impl for CompletionHandler_Impl {
    fn ActivateCompleted(
        &self,
        operation: windows::core::Ref<'_, Audio::IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        // Only signal completion here.  The async operation was created on
        // the activating thread and is retained by `ProcessLoopbackActivation`;
        // retrieving the IAudioClient there keeps that interface in the same
        // COM apartment instead of sending it through a Rust channel from the
        // callback's MTA thread.
        let _ = self.0.send(operation.ok().map(|_| ()));
        Ok(())
    }
}

fn activation_result(
    operation: &Audio::IActivateAudioInterfaceAsyncOperation,
) -> windows::core::Result<Audio::IAudioClient> {
    let mut result = windows::core::HRESULT::default();
    let mut interface: Option<IUnknown> = None;
    unsafe { operation.GetActivateResult(&mut result, &mut interface)? };
    result.ok()?;
    interface
        .ok_or_else(|| windows::core::Error::from(result))?
        .cast()
}

/// Owns everything referenced by the activation call until its callback has
/// completed. Field order is not relied on; `params` is boxed so the blob's
/// pointer stays stable when this value moves.
struct ProcessLoopbackActivation {
    params: Box<Audio::AUDIOCLIENT_ACTIVATION_PARAMS>,
    propvariant: PROPVARIANT,
    operation: Option<Audio::IActivateAudioInterfaceAsyncOperation>,
    completion: Audio::IActivateAudioInterfaceCompletionHandler,
    completed: Receiver<windows::core::Result<()>>,
}

fn activate_process_audio_client(
    pid: u32,
    mode: ProcessLoopbackMode,
) -> windows::core::Result<Audio::IAudioClient> {
    let process_mode = match mode {
        ProcessLoopbackMode::IncludeProcessTree => {
            Audio::PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
        }
        ProcessLoopbackMode::ExcludeProcessTree => {
            Audio::PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
        }
    };
    let mut params = Box::new(Audio::AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: Audio::AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: Audio::AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: Audio::AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: process_mode,
            },
        },
    });
    let propvariant = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<Audio::AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&mut *params as *mut Audio::AUDIOCLIENT_ACTIVATION_PARAMS)
                            .cast(),
                    },
                },
            }),
        },
    };
    let (tx, rx) = mpsc::channel();
    let completion: Audio::IActivateAudioInterfaceCompletionHandler = CompletionHandler(tx).into();
    let mut activation = ProcessLoopbackActivation {
        params,
        propvariant,
        operation: None,
        completion,
        completed: rx,
    };
    let operation = unsafe {
        Audio::ActivateAudioInterfaceAsync(
            Audio::VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &Audio::IAudioClient::IID,
            Some(&activation.propvariant),
            &activation.completion,
        )
    }?;
    activation.operation = Some(operation);

    // Keep every owned field live through the callback. Reading the params is
    // intentional: it also makes this invariant visible to dead-code analysis.
    debug_assert_eq!(
        activation.params.ActivationType,
        Audio::AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK
    );
    let callback_result = match activation.completed.recv_timeout(START_TIMEOUT) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // There is no cancellation method on
            // IActivateAudioInterfaceAsyncOperation. Windows retains the
            // completion handler until the callback runs, so the owning
            // PROPVARIANT/blob must remain alive even after our diagnostic
            // timeout. Waiting for the callback before dropping `activation`
            // is the only safe failure path; the outer worker timeout will
            // still surface the operation as unavailable to its caller.
            let _ = activation.completed.recv();
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::ERROR_TIMEOUT.to_hresult(),
                "timed out waiting for process-loopback activation",
            ));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "process-loopback activation callback disconnected",
            ));
        }
    };
    callback_result?;
    let operation = activation.operation.as_ref().ok_or_else(|| {
        windows::core::Error::new(
            windows::Win32::Foundation::E_FAIL,
            "process-loopback activation returned without an async operation",
        )
    })?;
    activation_result(operation)
}

fn initialize_client(client: &Audio::IAudioClient, format: AudioFormat) -> BackendResult<()> {
    let bits = 32u16;
    let block_align = format.channels * bits / 8;
    let wave = Audio::WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: format.channels,
        nSamplesPerSec: format.sample_rate,
        nAvgBytesPerSec: format.sample_rate * u32::from(block_align),
        nBlockAlign: block_align,
        wBitsPerSample: bits,
        cbSize: 0,
    };
    let flags = Audio::AUDCLNT_STREAMFLAGS_LOOPBACK
        | Audio::AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | Audio::AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
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
    .map_err(|error| native("initialize process-loopback audio client", error))
}

fn capture_loop(
    capture: &Audio::IAudioCaptureClient,
    mut feed: RingSourceFeed,
    format: AudioFormat,
    stop: &AtomicBool,
) {
    let channels = usize::from(format.channels);
    let silence = vec![0.0f32; format.samples(4096)];
    while !stop.load(Ordering::Acquire) {
        let mut pending = match unsafe { capture.GetNextPacketSize() } {
            Ok(value) => value,
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
            let mut frames = 0;
            let mut flags = 0;
            if unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }.is_err()
            {
                feed.mark_lost();
                return;
            }
            let samples = frames as usize * channels;
            if frames > 0 && flags & Audio::AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                let mut remaining = samples;
                while remaining > 0 {
                    let pushed = feed.push(&silence[..remaining.min(silence.len())]);
                    if pushed == 0 {
                        break;
                    }
                    remaining -= pushed;
                }
            } else if frames > 0 && !data.is_null() {
                // WASAPI owns this buffer through ReleaseBuffer and guarantees
                // the frame geometry requested during Initialize.
                let block = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), samples) };
                feed.push(block);
            } else if frames > 0 {
                // A non-silent packet must carry a readable buffer. Treat a
                // null pointer as endpoint loss rather than quietly turning
                // that packet into zeros.
                feed.mark_lost();
                return;
            }
            if unsafe { capture.ReleaseBuffer(frames) }.is_err() {
                feed.mark_lost();
                return;
            }
            pending = match unsafe { capture.GetNextPacketSize() } {
                Ok(value) => value,
                Err(_) => {
                    feed.mark_lost();
                    return;
                }
            };
        }
    }
    // A RingSource otherwise reports a normal starvation forever after the
    // worker has exited. Marking it lost lets RouterCore record DeviceLost and
    // retire the route on the next block, which is the same behavior as a
    // physical endpoint invalidation.
    if !stop.load(Ordering::Acquire) {
        feed.mark_lost();
    }
}

fn native(context: &str, error: windows::core::Error) -> BackendError {
    BackendError::native(format!(
        "{context}: {error} (HRESULT {:#010x})",
        error.code().0 as u32
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_blob_points_at_owned_box() {
        // This compile-time/layout assertion protects the most dangerous part
        // of process activation: the blob must contain exactly the params.
        assert_eq!(size_of::<Audio::AUDIOCLIENT_ACTIVATION_PARAMS>() as u32, 12);
    }
}
