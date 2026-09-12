//! PipeWire input stream used by a Recorder graph destination.
//!
//! The stream callback owns a RecorderSink and performs only bounded sample
//! conversion plus the nonblocking ring write. The worker-side WAV lifecycle
//! is provided by the platform-neutral router recorder module.

use super::*;
use crate::router::AudioSink;
use crate::router::{
    AudioFormat as RouterAudioFormat, RecorderDiagnostics, RecorderResult, RecorderSink,
    RecorderWriter,
};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

pub(super) struct RecorderCallbackState {
    sink: RecorderSink,
    format: Arc<AtomicU32>,
    unsupported_format: Arc<AtomicBool>,
    stream_error: Arc<AtomicBool>,
    scratch: Vec<f32>,
}

pub(super) struct RecorderHandle {
    _stream: pw::stream::Stream,
    _listener: pw::stream::StreamListener<RecorderCallbackState>,
    pub(super) writer: RecorderWriter,
    pub(super) name: String,
    pub(super) unsupported_format: Arc<AtomicBool>,
    pub(super) stream_error: Arc<AtomicBool>,
}

impl RecorderHandle {
    pub(super) fn create(
        core: &pw::core::Core,
        name: String,
        format: RouterAudioFormat,
        recording_dir: Option<&std::path::Path>,
        capacity_frames: usize,
    ) -> BackendResult<Self> {
        let directory = recording_dir
            .map(std::path::Path::to_owned)
            .unwrap_or_else(|| std::env::temp_dir().join("qpwgraph-rs-recordings"));
        let (sink, writer) =
            RecorderWriter::start_pending_paused(&directory, format, capacity_frames)
                .map_err(|error| BackendError::Native(error.to_string()))?;
        let unsupported_format = Arc::new(AtomicBool::new(false));
        let stream_error = Arc::new(AtomicBool::new(false));
        let callback_format = Arc::new(AtomicU32::new(AudioFormat::F32LE.as_raw()));
        let stream = pw::stream::Stream::new(
            core,
            &name,
            pw::properties::properties! {
                NODE_NAME => name.as_str(),
                NODE_DESCRIPTION => "qpwgraph audio recorder",
                MEDIA_TYPE => MEDIA_TYPE_AUDIO,
                "media.category" => "Capture",
                "media.role" => "Recording",
                "media.class" => "Stream/Input/Audio",
                "node.dont-reconnect" => "true",
                "stream.dont-remix" => "true",
            },
        )
        .map_err(|error| native_error("PipeWire recorder stream creation", error))?;
        let callback_format_for_param = Arc::clone(&callback_format);
        let unsupported_for_param = Arc::clone(&unsupported_format);
        let listener = stream
            .add_local_listener_with_user_data(RecorderCallbackState {
                sink,
                format: Arc::clone(&callback_format),
                unsupported_format: Arc::clone(&unsupported_format),
                stream_error: Arc::clone(&stream_error),
                scratch: vec![0.0; format.samples(4096)],
            })
            .state_changed(|_, data, _old, new| {
                if matches!(new, pw::stream::StreamState::Error(_)) {
                    data.stream_error.store(true, Ordering::Release);
                }
            })
            .param_changed(move |_stream, _data, id, param| {
                if id != ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else {
                    return;
                };
                let mut info = AudioInfoRaw::new();
                if info.parse(param).is_ok() {
                    callback_format_for_param.store(info.format().as_raw(), Ordering::Release);
                    if info.format() != AudioFormat::F32LE {
                        unsupported_for_param.store(true, Ordering::Release);
                    }
                }
            })
            .process(process_recorder_buffer)
            .register()
            .map_err(|error| native_error("PipeWire recorder stream listener", error))?;

        let pod_bytes = recorder_audio_format_pod(format)?;
        let pod = Pod::from_bytes(&pod_bytes).ok_or_else(|| {
            BackendError::Native("could not serialize PipeWire recorder audio format".into())
        })?;
        let mut params = [pod];
        stream
            .connect(
                SpaDirection::Input,
                None,
                pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS
                    | pw::stream::StreamFlags::DONT_RECONNECT,
                &mut params,
            )
            .map_err(|error| native_error("PipeWire recorder stream connection", error))?;

        Ok(Self {
            _stream: stream,
            _listener: listener,
            writer,
            name,
            unsupported_format,
            stream_error,
        })
    }

    pub(super) fn resume(&self) -> BackendResult<()> {
        self.writer
            .resume()
            .map_err(|error| BackendError::Native(error.to_string()))
    }

    pub(super) fn request_stop(&mut self) -> BackendResult<()> {
        self.writer
            .request_stop()
            .map_err(|error| BackendError::Native(error.to_string()))
    }

    pub(super) fn status(&self) -> RecorderDiagnostics {
        self.writer.status()
    }

    pub(super) fn failure_message(&self) -> Option<&'static str> {
        if self.unsupported_format.load(Ordering::Acquire) {
            Some("PipeWire recorder negotiated a non-float32 format")
        } else if self.stream_error.load(Ordering::Acquire) {
            Some("PipeWire recorder stream failed")
        } else {
            None
        }
    }

    pub(super) fn stop(&mut self) -> BackendResult<RecorderResult> {
        let result = self
            .writer
            .finish()
            .map_err(|error| BackendError::Native(error.to_string()))?;
        if let Some(error) = self.failure_message() {
            return Err(BackendError::Native(error.into()));
        }
        Ok(RecorderResult {
            temporary_path: result.temporary_path,
            frames_written: result.frames_written,
            dropped_frames: result.dropped_frames,
            file_bytes: result.file_bytes,
        })
    }

    pub(super) fn poll(&mut self) -> BackendResult<Option<RecorderResult>> {
        let Some(result) = self
            .writer
            .poll()
            .map_err(|error| BackendError::Native(error.to_string()))?
        else {
            if let Some(error) = self.failure_message() {
                return Err(BackendError::Native(error.into()));
            }
            return Ok(None);
        };
        if let Some(error) = self.failure_message() {
            return Err(BackendError::Native(error.into()));
        }
        Ok(Some(RecorderResult {
            temporary_path: result.temporary_path,
            frames_written: result.frames_written,
            file_bytes: result.file_bytes,
            dropped_frames: result.dropped_frames,
        }))
    }
}

fn process_recorder_buffer(stream: &pw::stream::StreamRef, data: &mut RecorderCallbackState) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    if data.format.load(Ordering::Acquire) != AudioFormat::F32LE.as_raw() {
        data.unsupported_format.store(true, Ordering::Release);
        return;
    }
    let mut used = 0usize;
    for block in buffer.datas_mut() {
        let offset = block.chunk().offset() as usize;
        let size = block.chunk().size() as usize;
        let Some(bytes) = block.data() else {
            continue;
        };
        let end = offset.saturating_add(size).min(bytes.len());
        if offset >= end {
            continue;
        }
        for chunk in bytes[offset..end]
            .as_chunks::<{ std::mem::size_of::<f32>() }>()
            .0
        {
            if used == data.scratch.len() {
                let _ = data.sink.write(&data.scratch[..used]);
                used = 0;
            }
            let mut raw = [0_u8; 4];
            raw.copy_from_slice(chunk);
            let value = f32::from_le_bytes(raw);
            data.scratch[used] = if value.is_finite() { value } else { 0.0 };
            used += 1;
        }
    }
    if used != 0 {
        let _ = data.sink.write(&data.scratch[..used]);
    }
}

fn recorder_audio_format_pod(format: RouterAudioFormat) -> BackendResult<Vec<u8>> {
    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::F32LE);
    audio_info.set_rate(format.sample_rate);
    audio_info.set_channels(u32::from(format.channels));
    let object = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .map(|success| success.0.into_inner())
        .map_err(|error| native_error("PipeWire recorder format serialization", error))
}
