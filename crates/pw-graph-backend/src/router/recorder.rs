//! Bounded, asynchronous recording for router audio destinations.
//!
//! RecorderSink is the realtime side: a thin wrapper around the router's
//! bounded RingSink. It never opens a file, waits for a lock, or formats an
//! error. RecorderWriter owns the matching drain on a normal worker thread
//! and writes a temporary float32 WAV file there.

use super::endpoints::{ring_sink, RingSink, RingSinkDrain};
use super::engine::{AudioSink, Backlog, SinkWrite, StreamHealth};
use super::format::AudioFormat;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const DEFAULT_RECORDER_CAPACITY_MS: u32 = 250;
pub const MAX_RIFF_DATA_BYTES: u64 = u32::MAX as u64 - 36;

const WAV_HEADER_BYTES: u64 = 44;
const WRITER_POLL: Duration = Duration::from_millis(5);
const STATE_STARTING: u8 = 0;
const STATE_RECORDING: u8 = 1;
const STATE_STOPPING: u8 = 2;
const STATE_FINISHED: u8 = 3;
const STATE_ERROR: u8 = 4;

static NEXT_RECORDING_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Error)]
pub enum RecorderError {
    #[error("recording format is invalid: {0}")]
    InvalidFormat(String),
    #[error("recording path is invalid: {0}")]
    InvalidPath(String),
    #[error("recording WAV file is too large for classic RIFF ({bytes} bytes)")]
    TooLarge { bytes: u64 },
    #[error("recording writer thread failed: {0}")]
    Writer(String),
    #[error("recording writer has already been stopped")]
    AlreadyStopped,
    #[error("recording file is not a valid float32 WAV: {0}")]
    InvalidWav(String),
    #[error("could not access recording file: {0}")]
    Io(#[from] io::Error),
}

fn validate_format(format: AudioFormat) -> Result<(), RecorderError> {
    if format.sample_rate == 0 || format.channels == 0 {
        return Err(RecorderError::InvalidFormat(format!(
            "{} Hz / {} channels",
            format.sample_rate, format.channels
        )));
    }
    Ok(())
}

fn frame_bytes(format: AudioFormat) -> u64 {
    u64::from(format.channels) * 4
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RecorderWriterState {
    #[default]
    Starting,
    Recording,
    Stopping,
    Finished,
    Error,
}

impl RecorderWriterState {
    fn as_raw(self) -> u8 {
        match self {
            Self::Starting => STATE_STARTING,
            Self::Recording => STATE_RECORDING,
            Self::Stopping => STATE_STOPPING,
            Self::Finished => STATE_FINISHED,
            Self::Error => STATE_ERROR,
        }
    }

    fn from_raw(raw: u8) -> Self {
        match raw {
            STATE_RECORDING => Self::Recording,
            STATE_STOPPING => Self::Stopping,
            STATE_FINISHED => Self::Finished,
            STATE_ERROR => Self::Error,
            _ => Self::Starting,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecorderDiagnostics {
    pub frames_written: u64,
    pub dropped_frames: u64,
    pub queue_depth: usize,
    pub queue_capacity: usize,
    pub sample_rate: u32,
    pub channels: u16,
    pub writer_state: RecorderWriterState,
    pub file_bytes: u64,
    pub temporary_path: PathBuf,
    pub last_error: Option<String>,
}

struct WriterShared {
    accepting: AtomicBool,
    started: AtomicBool,
    frames_written: AtomicU64,
    dropped_frames: AtomicU64,
    queue_depth: AtomicU64,
    queue_capacity: usize,
    file_bytes: AtomicU64,
    state: AtomicU8,
    error: Mutex<Option<String>>,
}

impl WriterShared {
    fn new(capacity_frames: usize, accepting: bool) -> Self {
        Self {
            accepting: AtomicBool::new(accepting),
            started: AtomicBool::new(accepting),
            frames_written: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
            queue_depth: AtomicU64::new(0),
            queue_capacity: capacity_frames,
            file_bytes: AtomicU64::new(0),
            state: AtomicU8::new(STATE_STARTING),
            error: Mutex::new(None),
        }
    }

    fn set_state(&self, state: RecorderWriterState) {
        self.state.store(state.as_raw(), Ordering::Release);
    }

    fn set_error(&self, error: impl Into<String>) {
        if let Ok(mut slot) = self.error.lock() {
            *slot = Some(error.into());
        }
        self.accepting.store(false, Ordering::Release);
        self.set_state(RecorderWriterState::Error);
    }

    fn snapshot(&self, format: AudioFormat, path: &Path) -> RecorderDiagnostics {
        RecorderDiagnostics {
            frames_written: self.frames_written.load(Ordering::Acquire),
            dropped_frames: self.dropped_frames.load(Ordering::Acquire),
            queue_depth: self.queue_depth.load(Ordering::Acquire) as usize,
            queue_capacity: self.queue_capacity,
            sample_rate: format.sample_rate,
            channels: format.channels,
            writer_state: RecorderWriterState::from_raw(self.state.load(Ordering::Acquire)),
            file_bytes: self.file_bytes.load(Ordering::Acquire),
            temporary_path: path.to_owned(),
            last_error: self.error.lock().ok().and_then(|error| error.clone()),
        }
    }
}

pub struct RecorderSink {
    format: AudioFormat,
    inner: RingSink,
    shared: Arc<WriterShared>,
}

impl std::fmt::Debug for RecorderSink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecorderSink")
            .field("format", &self.format)
            .field("backlog", &self.inner.backlog())
            .finish()
    }
}

impl RecorderSink {
    fn new(format: AudioFormat, inner: RingSink, shared: Arc<WriterShared>) -> Self {
        Self {
            format,
            inner,
            shared,
        }
    }

    pub fn queue_depth(&self) -> usize {
        self.inner
            .backlog()
            .map(|backlog| backlog.frames)
            .unwrap_or_default()
    }

    pub fn dropped_frames(&self) -> u64 {
        self.shared.dropped_frames.load(Ordering::Acquire)
    }
}

impl AudioSink for RecorderSink {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn write(&mut self, src: &[f32]) -> SinkWrite {
        let channels = self.format.channels as usize;
        let offered = src.len() / channels;
        if offered == 0 {
            return SinkWrite::ok(0);
        }
        if !self.shared.accepting.load(Ordering::Acquire) {
            match RecorderWriterState::from_raw(self.shared.state.load(Ordering::Acquire)) {
                // A native recorder is present in the graph before Record is
                // pressed. Ignoring that idle input is not a route failure.
                RecorderWriterState::Starting
                | RecorderWriterState::Recording
                | RecorderWriterState::Stopping
                | RecorderWriterState::Finished => return SinkWrite::ok(offered),
                RecorderWriterState::Error => {
                    self.shared
                        .dropped_frames
                        .fetch_add(offered as u64, Ordering::Relaxed);
                    // A writer failure is a recorder diagnostic, not a lost
                    // physical endpoint. Keep Windows' route recovery from
                    // reopening every source while the user chooses Discard.
                    return SinkWrite {
                        frames: 0,
                        health: StreamHealth::Starved,
                    };
                }
            }
        }
        let result = self.inner.write(&src[..offered * channels]);
        let dropped = offered.saturating_sub(result.frames);
        if dropped != 0 {
            self.shared
                .dropped_frames
                .fetch_add(dropped as u64, Ordering::Relaxed);
        }
        if let Some(backlog) = self.inner.backlog() {
            self.shared
                .queue_depth
                .store(backlog.frames as u64, Ordering::Release);
        }
        result
    }

    fn backlog(&self) -> Option<Backlog> {
        self.inner.backlog()
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.shared.queue_depth.store(0, Ordering::Release);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecorderResult {
    pub temporary_path: PathBuf,
    pub frames_written: u64,
    pub dropped_frames: u64,
    pub file_bytes: u64,
}

enum WriterCommand {
    Stop,
}

pub struct RecorderWriter {
    format: AudioFormat,
    temporary_path: PathBuf,
    shared: Arc<WriterShared>,
    command_tx: Option<Sender<WriterCommand>>,
    result_rx: Receiver<Result<RecorderResult, RecorderError>>,
    worker: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for RecorderWriter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecorderWriter")
            .field("format", &self.format)
            .field("temporary_path", &self.temporary_path)
            .field("status", &self.status())
            .finish()
    }
}

impl RecorderWriter {
    pub fn start(
        format: AudioFormat,
        temporary_path: impl Into<PathBuf>,
        capacity_frames: usize,
    ) -> Result<(RecorderSink, Self), RecorderError> {
        Self::start_internal(format, temporary_path, capacity_frames, true)
    }

    /// Start the bounded writer without accepting audio yet. This is useful
    /// for native stream-backed recorders whose graph destination exists in
    /// Idle state; call resume when the user presses Record.
    pub fn start_paused(
        format: AudioFormat,
        temporary_path: impl Into<PathBuf>,
        capacity_frames: usize,
    ) -> Result<(RecorderSink, Self), RecorderError> {
        Self::start_internal(format, temporary_path, capacity_frames, false)
    }

    fn start_internal(
        format: AudioFormat,
        temporary_path: impl Into<PathBuf>,
        capacity_frames: usize,
        accepting: bool,
    ) -> Result<(RecorderSink, Self), RecorderError> {
        validate_format(format)?;
        if capacity_frames == 0 {
            return Err(RecorderError::InvalidFormat(
                "recorder queue capacity must be non-zero".into(),
            ));
        }
        let temporary_path = temporary_path.into();
        if temporary_path.as_os_str().is_empty() {
            return Err(RecorderError::InvalidPath("path is empty".into()));
        }

        let (inner, drain) = ring_sink(format, capacity_frames);
        let shared = Arc::new(WriterShared::new(capacity_frames, accepting));
        let (command_tx, command_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let worker_shared = Arc::clone(&shared);
        let worker_path = temporary_path.clone();
        let worker = thread::Builder::new()
            .name("qpwgraph-recorder".into())
            .spawn(move || {
                run_writer(
                    format,
                    worker_path,
                    drain,
                    command_rx,
                    result_tx,
                    worker_shared,
                )
            })
            .map_err(|error| RecorderError::Writer(error.to_string()))?;

        let sink = RecorderSink::new(format, inner, Arc::clone(&shared));
        let writer = Self {
            format,
            temporary_path,
            shared,
            command_tx: Some(command_tx),
            result_rx,
            worker: Some(worker),
        };
        Ok((sink, writer))
    }

    pub fn start_pending(
        directory: impl AsRef<Path>,
        format: AudioFormat,
        capacity_frames: usize,
    ) -> Result<(RecorderSink, Self), RecorderError> {
        Self::start_pending_with_accepting(directory, format, capacity_frames, true)
    }

    /// Allocate a crash-repairable pending path without accepting audio yet.
    /// Native stream-backed recorders use this while their graph destination
    /// is visible in Idle state; the first Record action calls `resume`.
    pub fn start_pending_paused(
        directory: impl AsRef<Path>,
        format: AudioFormat,
        capacity_frames: usize,
    ) -> Result<(RecorderSink, Self), RecorderError> {
        Self::start_pending_with_accepting(directory, format, capacity_frames, false)
    }

    fn start_pending_with_accepting(
        directory: impl AsRef<Path>,
        format: AudioFormat,
        capacity_frames: usize,
        accepting: bool,
    ) -> Result<(RecorderSink, Self), RecorderError> {
        let pending = pending_recording_dir(directory);
        fs::create_dir_all(&pending)?;
        let path = unique_recording_path(
            &pending,
            &format!("recording-{}", recording_id()),
            "wav.part",
        )?;
        Self::start_internal(format, path, capacity_frames, accepting)
    }

    pub fn request_stop(&mut self) -> Result<(), RecorderError> {
        if self.worker.is_none() {
            return Err(RecorderError::AlreadyStopped);
        }
        self.shared.accepting.store(false, Ordering::Release);
        self.shared.set_state(RecorderWriterState::Stopping);
        if let Some(command_tx) = self.command_tx.take() {
            command_tx
                .send(WriterCommand::Stop)
                .map_err(|_| RecorderError::Writer("writer thread is not running".into()))?;
        }
        Ok(())
    }

    /// Begin accepting audio on a paused writer.
    pub fn resume(&self) -> Result<(), RecorderError> {
        if self.worker.is_none() {
            return Err(RecorderError::AlreadyStopped);
        }
        self.shared.started.store(true, Ordering::Release);
        self.shared.accepting.store(true, Ordering::Release);
        self.shared.set_state(RecorderWriterState::Recording);
        Ok(())
    }

    pub fn poll(&mut self) -> Result<Option<RecorderResult>, RecorderError> {
        match self.result_rx.try_recv() {
            Ok(result) => {
                self.join_worker()?;
                result.map(Some)
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                self.join_worker()?;
                Err(self
                    .status()
                    .last_error
                    .map(RecorderError::Writer)
                    .unwrap_or_else(|| RecorderError::Writer("writer thread stopped".into())))
            }
        }
    }

    pub fn finish(&mut self) -> Result<RecorderResult, RecorderError> {
        if self.worker.is_none() {
            return Err(RecorderError::AlreadyStopped);
        }
        if self.command_tx.is_some() {
            self.request_stop()?;
        }
        let result = self
            .result_rx
            .recv()
            .map_err(|_| RecorderError::Writer("writer thread stopped".into()))?;
        self.join_worker()?;
        result
    }

    pub fn status(&self) -> RecorderDiagnostics {
        self.shared.snapshot(self.format, &self.temporary_path)
    }

    pub fn temporary_path(&self) -> &Path {
        &self.temporary_path
    }

    fn join_worker(&mut self) -> Result<(), RecorderError> {
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| {
                RecorderError::Writer("writer thread panicked while finalizing".into())
            })?;
        }
        Ok(())
    }
}

impl Drop for RecorderWriter {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let never_started = !self.shared.started.load(Ordering::Acquire);
            let temporary_path = self.temporary_path.clone();
            let _ = self.request_stop();
            let _ = self.join_worker();
            if never_started
                && self.shared.frames_written.load(Ordering::Acquire) == 0
                && self.shared.state.load(Ordering::Acquire)
                    == RecorderWriterState::Finished.as_raw()
            {
                let _ = fs::remove_file(temporary_path);
            }
        }
    }
}

fn run_writer(
    format: AudioFormat,
    temporary_path: PathBuf,
    mut drain: RingSinkDrain,
    command_rx: Receiver<WriterCommand>,
    result_tx: Sender<Result<RecorderResult, RecorderError>>,
    shared: Arc<WriterShared>,
) {
    let result = run_writer_inner(format, &temporary_path, &mut drain, &command_rx, &shared);
    match result {
        Ok(result) => {
            shared.set_state(RecorderWriterState::Finished);
            let _ = result_tx.send(Ok(result));
        }
        Err(error) => {
            shared.set_error(error.to_string());
            let _ = result_tx.send(Err(error));
        }
    }
}

fn run_writer_inner(
    format: AudioFormat,
    temporary_path: &Path,
    drain: &mut RingSinkDrain,
    command_rx: &Receiver<WriterCommand>,
    shared: &WriterShared,
) -> Result<RecorderResult, RecorderError> {
    let mut writer = WavWriter::create(temporary_path, format)?;
    shared.file_bytes.store(WAV_HEADER_BYTES, Ordering::Release);
    if shared.accepting.load(Ordering::Acquire) {
        shared.set_state(RecorderWriterState::Recording);
    }
    let mut stopped = false;
    let mut samples = vec![0.0_f32; format.samples(4096)];

    loop {
        while let Ok(WriterCommand::Stop) = command_rx.try_recv() {
            stopped = true;
            shared.set_state(RecorderWriterState::Stopping);
        }

        let available_frames = drain.available() / format.channels as usize;
        if available_frames != 0 {
            let frames = available_frames.min(samples.len() / format.channels as usize);
            let sample_count = format.samples(frames);
            let pulled = drain.pull(&mut samples[..sample_count]);
            let pulled_frames = pulled / format.channels as usize;
            if pulled_frames != 0 {
                writer.write_interleaved(&samples[..format.samples(pulled_frames)])?;
                shared
                    .frames_written
                    .fetch_add(pulled_frames as u64, Ordering::Relaxed);
                shared
                    .file_bytes
                    .store(writer.file_bytes(), Ordering::Release);
            }
            shared.queue_depth.store(
                (drain.available() / format.channels as usize) as u64,
                Ordering::Release,
            );
            continue;
        }

        if stopped {
            let result = writer.finalize()?;
            shared
                .file_bytes
                .store(result.file_bytes, Ordering::Release);
            shared.queue_depth.store(0, Ordering::Release);
            return Ok(RecorderResult {
                temporary_path: temporary_path.to_owned(),
                frames_written: shared.frames_written.load(Ordering::Acquire),
                dropped_frames: shared.dropped_frames.load(Ordering::Acquire),
                file_bytes: result.file_bytes,
            });
        }

        match command_rx.recv_timeout(WRITER_POLL) {
            Ok(WriterCommand::Stop) => {
                stopped = true;
                shared.set_state(RecorderWriterState::Stopping);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                stopped = true;
                shared.set_state(RecorderWriterState::Stopping);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WavHeader {
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub data_bytes: u64,
    pub file_bytes: u64,
}

/// Small classic RIFF/WAVE writer for interleaved IEEE float samples.
pub struct WavWriter {
    file: File,
    format: AudioFormat,
    data_bytes: u64,
    scratch: Vec<u8>,
}

impl std::fmt::Debug for WavWriter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WavWriter")
            .field("format", &self.format)
            .field("data_bytes", &self.data_bytes)
            .finish()
    }
}

impl WavWriter {
    /// Open without overwriting an existing path and write a placeholder
    /// header. The parent directory is created on the writer thread.
    pub fn create(path: impl AsRef<Path>, format: AudioFormat) -> Result<Self, RecorderError> {
        validate_format(format)?;
        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        if let Some(parent) = parent {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .open(path)?;
        write_wav_header(&mut file, format, 0)?;
        Ok(Self {
            file,
            format,
            data_bytes: 0,
            scratch: Vec::new(),
        })
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    pub fn frames(&self) -> u64 {
        self.data_bytes / frame_bytes(self.format)
    }

    pub fn data_bytes(&self) -> u64 {
        self.data_bytes
    }

    pub fn file_bytes(&self) -> u64 {
        WAV_HEADER_BYTES.saturating_add(self.data_bytes)
    }

    pub fn write_interleaved(&mut self, samples: &[f32]) -> Result<(), RecorderError> {
        let channels = self.format.channels as usize;
        if !samples.len().is_multiple_of(channels) {
            return Err(RecorderError::InvalidFormat(format!(
                "{} samples are not a whole number of {}-channel frames",
                samples.len(),
                channels
            )));
        }
        let bytes = (samples.len() as u64)
            .checked_mul(4)
            .ok_or(RecorderError::TooLarge { bytes: u64::MAX })?;
        let next = self
            .data_bytes
            .checked_add(bytes)
            .ok_or(RecorderError::TooLarge { bytes: u64::MAX })?;
        if next > MAX_RIFF_DATA_BYTES {
            return Err(RecorderError::TooLarge { bytes: next });
        }
        let byte_count = usize::try_from(bytes).map_err(|_| RecorderError::TooLarge { bytes })?;
        self.scratch.clear();
        self.scratch.try_reserve(byte_count).map_err(|error| {
            RecorderError::Writer(format!("could not allocate WAV block: {error}"))
        })?;
        for sample in samples {
            self.scratch.extend_from_slice(&sample.to_le_bytes());
        }
        self.file.seek(SeekFrom::End(0))?;
        self.file.write_all(&self.scratch)?;
        self.file.flush()?;
        self.data_bytes = next;
        Ok(())
    }

    pub fn finalize(mut self) -> Result<WavHeader, RecorderError> {
        write_wav_header(&mut self.file, self.format, self.data_bytes)?;
        self.file.flush()?;
        Ok(WavHeader {
            sample_rate: self.format.sample_rate,
            channels: self.format.channels,
            frames: self.frames(),
            data_bytes: self.data_bytes,
            file_bytes: WAV_HEADER_BYTES + self.data_bytes,
        })
    }
}

fn write_wav_header(
    file: &mut File,
    format: AudioFormat,
    data_bytes: u64,
) -> Result<(), RecorderError> {
    validate_format(format)?;
    if data_bytes > MAX_RIFF_DATA_BYTES {
        return Err(RecorderError::TooLarge { bytes: data_bytes });
    }
    let data_bytes = data_bytes as u32;
    let riff_size = 36_u32
        .checked_add(data_bytes)
        .ok_or(RecorderError::TooLarge {
            bytes: u64::from(data_bytes),
        })?;
    let block_align = format
        .channels
        .checked_mul(4)
        .ok_or_else(|| RecorderError::InvalidFormat("channel block alignment overflow".into()))?;
    let byte_rate = format
        .sample_rate
        .checked_mul(u32::from(block_align))
        .ok_or_else(|| RecorderError::InvalidFormat("WAV byte rate overflow".into()))?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(b"RIFF")?;
    file.write_all(&riff_size.to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16_u32.to_le_bytes())?;
    file.write_all(&3_u16.to_le_bytes())?;
    file.write_all(&format.channels.to_le_bytes())?;
    file.write_all(&format.sample_rate.to_le_bytes())?;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&32_u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_bytes.to_le_bytes())?;
    Ok(())
}

/// Read and validate a finalized or placeholder float32 WAV header.
pub fn read_wav_header(path: impl AsRef<Path>) -> Result<WavHeader, RecorderError> {
    let path = path.as_ref();
    let mut file = File::open(path)?;
    let file_bytes = file.metadata()?.len();
    if file_bytes < WAV_HEADER_BYTES {
        return Err(RecorderError::InvalidWav(
            "file is shorter than its header".into(),
        ));
    }
    let mut header = [0_u8; 44];
    file.read_exact(&mut header)?;
    if &header[0..4] != b"RIFF"
        || &header[8..12] != b"WAVE"
        || &header[12..16] != b"fmt "
        || &header[36..40] != b"data"
    {
        return Err(RecorderError::InvalidWav(
            "missing RIFF/WAVE/fmt/data chunks".into(),
        ));
    }
    let audio_format = u16::from_le_bytes([header[20], header[21]]);
    let channels = u16::from_le_bytes([header[22], header[23]]);
    let sample_rate = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
    let block_align = u16::from_le_bytes([header[32], header[33]]);
    let bits = u16::from_le_bytes([header[34], header[35]]);
    if audio_format != 3 || bits != 32 || channels == 0 || block_align != channels.saturating_mul(4)
    {
        return Err(RecorderError::InvalidWav(
            "expected interleaved 32-bit IEEE float samples".into(),
        ));
    }
    let declared_data = u64::from(u32::from_le_bytes([
        header[40], header[41], header[42], header[43],
    ]));
    let available_data = file_bytes.saturating_sub(WAV_HEADER_BYTES);
    let data_bytes = declared_data.min(available_data);
    let aligned = data_bytes - data_bytes % u64::from(block_align);
    Ok(WavHeader {
        sample_rate,
        channels,
        frames: aligned / u64::from(block_align),
        data_bytes: aligned,
        file_bytes: WAV_HEADER_BYTES + aligned,
    })
}

/// Repair a .wav.part after a crash. The payload is preserved, truncated to a
/// complete frame, and the RIFF/data lengths are rewritten.
pub fn repair_wav_header(path: impl AsRef<Path>) -> Result<WavHeader, RecorderError> {
    let path = path.as_ref();
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let file_bytes = file.metadata()?.len();
    if file_bytes < WAV_HEADER_BYTES {
        return Err(RecorderError::InvalidWav(
            "file is shorter than its header".into(),
        ));
    }
    let mut header = [0_u8; 44];
    file.read_exact(&mut header)?;
    if &header[0..4] != b"RIFF"
        || &header[8..12] != b"WAVE"
        || &header[12..16] != b"fmt "
        || &header[36..40] != b"data"
    {
        return Err(RecorderError::InvalidWav(
            "missing RIFF/WAVE/fmt/data chunks".into(),
        ));
    }
    let channels = u16::from_le_bytes([header[22], header[23]]);
    let sample_rate = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
    let audio_format = u16::from_le_bytes([header[20], header[21]]);
    let bits = u16::from_le_bytes([header[34], header[35]]);
    if audio_format != 3 || bits != 32 || channels == 0 {
        return Err(RecorderError::InvalidWav("not a float32 WAV".into()));
    }
    let format = AudioFormat::new(sample_rate, channels);
    let alignment = frame_bytes(format);
    let data_bytes = file_bytes.saturating_sub(WAV_HEADER_BYTES);
    let aligned = data_bytes - data_bytes % alignment;
    file.set_len(WAV_HEADER_BYTES + aligned)?;
    write_wav_header(&mut file, format, aligned)?;
    file.flush()?;
    Ok(WavHeader {
        sample_rate,
        channels,
        frames: aligned / alignment,
        data_bytes: aligned,
        file_bytes: WAV_HEADER_BYTES + aligned,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredRecording {
    pub path: PathBuf,
    pub header: WavHeader,
    pub modified: Option<SystemTime>,
}

/// Scan a pending directory and repair every recoverable .wav.part file.
/// Invalid files are ignored and left in place for an explicit decision.
pub fn scan_pending_recordings(
    directory: impl AsRef<Path>,
) -> Result<Vec<RecoveredRecording>, RecorderError> {
    let directory = directory.as_ref();
    let mut recovered = Vec::new();
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(recovered),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("part") {
            continue;
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| !name.ends_with(".wav.part"))
        {
            continue;
        }
        let Ok(header) = repair_wav_header(&path) else {
            continue;
        };
        let modified = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        recovered.push(RecoveredRecording {
            path,
            header,
            modified,
        });
    }
    recovered.sort_by_key(|entry| std::cmp::Reverse(entry.modified));
    Ok(recovered)
}

pub fn recover_pending_recordings(
    directory: impl AsRef<Path>,
) -> Result<Vec<RecoveredRecording>, RecorderError> {
    scan_pending_recordings(directory)
}

pub fn pending_recording_dir(directory: impl AsRef<Path>) -> PathBuf {
    directory.as_ref().join("pending")
}

fn recording_id() -> String {
    let sequence = NEXT_RECORDING_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{nanos:016x}-{sequence:08x}")
}

/// Pick a path without overwriting an existing file. extension may contain a
/// compound suffix such as wav.part.
pub fn unique_recording_path(
    directory: impl AsRef<Path>,
    stem: &str,
    extension: &str,
) -> Result<PathBuf, RecorderError> {
    let directory = directory.as_ref();
    if stem.trim().is_empty() || extension.trim().is_empty() {
        return Err(RecorderError::InvalidPath("recording name is empty".into()));
    }
    fs::create_dir_all(directory)?;
    for suffix in 0_u32..=u32::MAX {
        let name = if suffix == 0 {
            format!("{stem}.{extension}")
        } else {
            format!("{stem} ({suffix}).{extension}")
        };
        let path = directory.join(name);
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(RecorderError::InvalidPath(
        "could not find a unique recording name".into(),
    ))
}

/// Render {date}/{time} and strip characters invalid on Windows or unsafe as
/// a path component.
pub fn render_recording_filename(template: &str, time: SystemTime) -> String {
    let date_time = time
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| chrono_like_utc(duration.as_secs()))
        .unwrap_or_else(|| ("1970-01-01".into(), "00-00-00".into()));
    let rendered = template
        .replace("{date}", &date_time.0)
        .replace("{time}", &date_time.1);
    let safe: String = rendered
        .chars()
        .map(|character| match character {
            ':' | '*' | '?' | '"' | '<' | '>' | '|' | '/' | '\\' => '-',
            character if character.is_control() => '-',
            character => character,
        })
        .collect();
    let safe = safe.trim().trim_matches('.').to_owned();
    if safe.is_empty() {
        "Recording".into()
    } else {
        safe
    }
}

pub fn default_recording_filename(time: SystemTime) -> String {
    render_recording_filename("Recording {date} {time}", time)
}

// Compact Gregorian conversion from Unix days, avoiding a date/time
// dependency for filename generation.
fn chrono_like_utc(seconds: u64) -> Option<(String, String)> {
    let days = seconds / 86_400;
    let day_seconds = seconds % 86_400;
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    Some((
        format!("{year:04}-{month:02}-{day:02}"),
        format!("{hour:02}-{minute:02}-{second:02}"),
    ))
}

/// Copy a recording without replacing an existing destination. The temporary
/// source is removed only after a successful copy and flush.
pub fn copy_recording_preserving_source(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<(), RecorderError> {
    let source = source.as_ref();
    let destination = destination.as_ref();
    if source == destination {
        return Err(RecorderError::InvalidPath(
            "recording source and destination are identical".into(),
        ));
    }
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| RecorderError::InvalidPath("destination has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let mut input = File::open(source)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    if let Err(error) = io::copy(&mut input, &mut output) {
        drop(output);
        let _ = fs::remove_file(destination);
        return Err(error.into());
    }
    if let Err(error) = output.flush() {
        drop(output);
        let _ = fs::remove_file(destination);
        return Err(error.into());
    }
    drop(output);
    Ok(())
}

pub fn copy_recording(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<(), RecorderError> {
    let source = source.as_ref();
    copy_recording_preserving_source(source, destination)?;
    fs::remove_file(source)?;
    Ok(())
}

pub fn save_recording(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<(), RecorderError> {
    copy_recording(source, destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::endpoints::BufferSource;
    use crate::router::engine::{RouteId, RouteSpec, RouterConfig, RouterCore, SinkId, SourceId};
    use std::io::Read;

    fn test_directory(label: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "qpwgraph-recorder-{label}-{}-{stamp}",
            std::process::id()
        ))
    }

    #[test]
    fn wav_writer_round_trips_interleaved_float_samples() {
        let directory = test_directory("round-trip");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("take.wav");
        let format = AudioFormat::new(48_000, 2);
        let samples = [0.0, 1.0, -0.5, 0.25, 0.75, -1.0];

        let mut writer = WavWriter::create(&path, format).unwrap();
        writer.write_interleaved(&samples).unwrap();
        let header = writer.finalize().unwrap();

        assert_eq!(header.sample_rate, 48_000);
        assert_eq!(header.channels, 2);
        assert_eq!(header.frames, 3);
        assert_eq!(header.data_bytes, samples.len() as u64 * 4);
        assert_eq!(read_wav_header(&path).unwrap(), header);

        let mut file = File::open(&path).unwrap();
        file.seek(SeekFrom::Start(WAV_HEADER_BYTES)).unwrap();
        let mut bytes = vec![0_u8; samples.len() * 4];
        file.read_exact(&mut bytes).unwrap();
        let decoded: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
            .collect();
        assert_eq!(decoded, samples);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unfinished_part_is_repaired_from_its_payload_length() {
        let directory = test_directory("repair");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("take.wav.part");
        let format = AudioFormat::new(48_000, 2);
        let samples = [0.1, 0.2, 0.3, 0.4];

        let mut writer = WavWriter::create(&path, format).unwrap();
        writer.write_interleaved(&samples).unwrap();
        drop(writer);

        assert_eq!(read_wav_header(&path).unwrap().frames, 0);
        let repaired = repair_wav_header(&path).unwrap();
        assert_eq!(repaired.frames, 2);
        assert_eq!(scan_pending_recordings(&directory).unwrap().len(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn recorder_writer_finishes_after_the_queue_drains() {
        let directory = test_directory("writer");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("take.wav.part");
        let format = AudioFormat::new(48_000, 2);
        let (mut sink, mut writer) = RecorderWriter::start(format, &path, 128).unwrap();
        let samples = vec![0.25_f32; format.samples(64)];
        assert_eq!(sink.write(&samples).frames, 64);
        let result = writer.finish().unwrap();

        assert_eq!(result.frames_written, 64);
        assert_eq!(read_wav_header(&path).unwrap().frames, 64);
        assert_eq!(writer.status().writer_state, RecorderWriterState::Finished);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn writer_failures_are_reported_without_replacing_the_existing_file() {
        let directory = test_directory("writer-error");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("already-exists.wav.part");
        fs::write(&path, b"keep this file").unwrap();
        let format = AudioFormat::new(48_000, 2);
        let (_sink, mut writer) = RecorderWriter::start(format, &path, 128).unwrap();

        let error = writer.finish().unwrap_err();
        assert!(matches!(
            &error,
            RecorderError::Io(error) if error.kind() == io::ErrorKind::AlreadyExists
        ));
        assert_eq!(writer.status().writer_state, RecorderWriterState::Error);
        assert_eq!(fs::read(&path).unwrap(), b"keep this file");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn router_mix_reaches_one_recorder_destination() {
        let directory = test_directory("router-mix");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("take.wav.part");
        let format = AudioFormat::new(48_000, 1);
        let (sink, mut writer) = RecorderWriter::start(format, &path, 128).unwrap();
        let mut core = RouterCore::new(RouterConfig {
            block_frames: 4,
            clock_rate: 48_000,
        });
        core.add_source(
            SourceId(1),
            Box::new(BufferSource::new(format, vec![0.25; 4])),
        )
        .unwrap();
        core.add_source(
            SourceId(2),
            Box::new(BufferSource::new(format, vec![0.5; 4])),
        )
        .unwrap();
        core.add_sink(SinkId(1), Box::new(sink)).unwrap();
        core.set_routes(&[
            RouteSpec::direct(RouteId(1), SourceId(1), SinkId(1)),
            RouteSpec::direct(RouteId(2), SourceId(2), SinkId(1)),
        ])
        .unwrap();
        core.process();
        let result = writer.finish().unwrap();

        assert_eq!(result.frames_written, 4);
        let mut file = File::open(&path).unwrap();
        file.seek(SeekFrom::Start(WAV_HEADER_BYTES)).unwrap();
        let mut bytes = vec![0_u8; 4 * std::mem::size_of::<f32>()];
        file.read_exact(&mut bytes).unwrap();
        let decoded: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
            .collect();
        assert_eq!(decoded, vec![0.75; 4]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn dropping_an_unstarted_pending_writer_removes_its_empty_placeholder() {
        let directory = test_directory("paused-drop");
        let (sink, writer) =
            RecorderWriter::start_pending_paused(&directory, AudioFormat::new(48_000, 2), 128)
                .unwrap();
        let path = writer.temporary_path().to_owned();
        drop(sink);
        drop(writer);

        assert!(!path.exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn recorder_queue_overflow_is_bounded_and_counted() {
        let format = AudioFormat::new(48_000, 2);
        let (inner, _drain) = ring_sink(format, 2);
        let shared = Arc::new(WriterShared::new(2, true));
        let mut sink = RecorderSink::new(format, inner, shared);
        let samples = vec![0.0_f32; format.samples(128)];
        let result = sink.write(&samples);

        assert!(result.frames < 128);
        assert_eq!(sink.dropped_frames(), 128 - result.frames as u64);
        assert!(sink.queue_depth() <= 2);
    }

    #[test]
    fn save_does_not_overwrite_and_sanitizes_filename() {
        let directory = test_directory("save");
        fs::create_dir_all(&directory).unwrap();
        let source = directory.join("source.wav.part");
        let destination = directory.join("Recording.wav");
        fs::write(&source, b"recording").unwrap();
        fs::write(&destination, b"existing").unwrap();

        let error = copy_recording(&source, &destination).unwrap_err();
        assert!(
            matches!(error, RecorderError::Io(error) if error.kind() == io::ErrorKind::AlreadyExists)
        );
        assert_eq!(fs::read(&source).unwrap(), b"recording");
        assert_eq!(
            render_recording_filename("Bad:Name {date} {time}", UNIX_EPOCH),
            "Bad-Name 1970-01-01 00-00-00"
        );
        fs::remove_dir_all(directory).unwrap();
    }
}
