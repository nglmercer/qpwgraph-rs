//! PipeWire video capture streams.
//!
//! A capture stream reads frames from a video source node (screen-cast
//! stream, camera, filter output) and pushes them into a bounded
//! [`VideoQueue`]. The process callback only copies validated bytes and
//! pushes; it never blocks, allocates unboundedly, or touches anything but
//! the queue and atomic diagnostics.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use super::super::*;
use super::format::{build_capture_buffers_pod, build_enum_format_pod, parse_format_param};
use pw_graph_video::{
    LatestFrame, VideoDiagnostics, VideoFrame, VideoPixelFormat, VideoQueue, VideoSpec,
};

/// State shared between the realtime callback and the driver thread.
pub struct VideoCaptureShared {
    pub queue: Arc<VideoQueue>,
    pub diagnostics: VideoDiagnostics,
    /// Latest frame for direct preview taps (capture bypassing filters).
    pub latest: LatestFrame,
    pub spec: Mutex<Option<VideoSpec>>,
    pub connected: AtomicBool,
    pub sequence: AtomicU64,
}

impl VideoCaptureShared {
    /// Share a bridge's queue and diagnostics so capture ingestion, worker
    /// consumption, and counters observe the same pipeline.
    pub fn with_queue(queue: Arc<VideoQueue>, diagnostics: VideoDiagnostics) -> Arc<Self> {
        Arc::new(Self {
            queue,
            diagnostics,
            latest: LatestFrame::new(),
            spec: Mutex::new(None),
            connected: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
        })
    }
}

/// `Stream::connect` listener state. Lives on the PipeWire thread loop.
pub struct VideoCaptureCallback {
    pub shared: Arc<VideoCaptureShared>,
}

pub struct VideoCaptureHandle {
    pub _stream: pw::stream::Stream,
    pub _listener: pw::stream::StreamListener<VideoCaptureCallback>,
    pub shared: Arc<VideoCaptureShared>,
}

/// Create a capture stream aimed at `target` (serial preferred, name
/// fallback — the same rule as audio meters). Frames land in the shared
/// queue. Must be called with the thread loop lock held.
pub fn create_video_capture_locked(
    core: &pw::core::Core,
    stream_name: &str,
    target: &str,
    formats: &[VideoPixelFormat],
    shared: &Arc<VideoCaptureShared>,
) -> BackendResult<VideoCaptureHandle> {
    let properties = pw::properties::properties! {
        NODE_NAME => stream_name,
        MEDIA_TYPE => MEDIA_TYPE_VIDEO,
        PROP_MEDIA_CATEGORY => MEDIA_CATEGORY_CAPTURE,
        PROP_MEDIA_ROLE => MEDIA_ROLE_VIDEO,
        MEDIA_CLASS => MEDIA_CLASS_VIDEO_CAPTURE,
        "node.passive" => "true",
        "node.dont-reconnect" => "true",
        "target.object" => target,
    };
    let stream = pw::stream::Stream::new(core, stream_name, properties)
        .map_err(|error| native_error("PipeWire video capture stream creation", error))?;
    let shared = shared.clone();
    let listener = stream
        .add_local_listener_with_user_data(VideoCaptureCallback {
            shared: shared.clone(),
        })
        .state_changed(|_, data, _old, new| {
            let streaming = matches!(new, pw::stream::StreamState::Streaming);
            data.shared.connected.store(streaming, Ordering::Relaxed);
            if !streaming {
                // A stall is diagnosable, not fatal: the driver observes
                // `connected` and reports the node state accordingly.
                if matches!(new, pw::stream::StreamState::Error(_)) {
                    data.shared.diagnostics.note_error("capture stream error");
                }
            }
        })
        .param_changed(|_stream, data, id, param| {
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else {
                return;
            };
            match parse_format_param(param) {
                Ok(spec) => {
                    // Renegotiation replaces the spec; queued frames from
                    // the old spec are dropped so downstream never mixes
                    // geometries.
                    let changed = data
                        .shared
                        .spec
                        .lock()
                        .map(|s| *s != Some(spec))
                        .unwrap_or(true);
                    if changed {
                        data.shared.queue.clear();
                        if let Ok(mut current) = data.shared.spec.lock() {
                            *current = Some(spec);
                        }
                        data.shared.diagnostics.note_spec(&spec);
                        data.shared.diagnostics.clear_error();
                    }
                }
                Err(error) => {
                    data.shared
                        .diagnostics
                        .note_error(&format!("unsupported capture format: {error}"));
                }
            }
        })
        .process(process_video_capture)
        .register()
        .map_err(|error| native_error("PipeWire video capture listener", error))?;

    let format_bytes = build_enum_format_pod(formats)?;
    let format_pod = Pod::from_bytes(&format_bytes).ok_or_else(|| {
        BackendError::Native("could not serialize PipeWire video capture format".into())
    })?;
    let buffers_bytes = build_capture_buffers_pod()?;
    let buffers_pod = Pod::from_bytes(&buffers_bytes).ok_or_else(|| {
        BackendError::Native("could not serialize PipeWire video capture buffers".into())
    })?;
    let mut params = [format_pod, buffers_pod];
    stream
        .connect(
            SpaDirection::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::RT_PROCESS
                | pw::stream::StreamFlags::DONT_RECONNECT,
            &mut params,
        )
        .map_err(|error| native_error("PipeWire video capture stream connection", error))?;
    stream
        .set_active(true)
        .map_err(|error| native_error("PipeWire video capture stream activation", error))?;

    Ok(VideoCaptureHandle {
        _stream: stream,
        _listener: listener,
        shared,
    })
}

/// Realtime frame ingestion. Copies one frame into an exactly-sized,
/// validated buffer and pushes it into the bounded queue, dropping on
/// overload. No locks are held across the copy beyond the queue's brief
/// `try_lock`.
pub fn process_video_capture(stream: &pw::stream::StreamRef, data: &mut VideoCaptureCallback) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let spec = data.shared.spec.lock().ok().and_then(|spec| *spec);
    let Some(spec) = spec else {
        return;
    };
    let expected = match spec.frame_bytes() {
        Ok(len) => len,
        Err(_) => return,
    };
    // Gather chunk bytes from one or more data blocks, bounded by the
    // validated frame size. Multi-plane buffers arrive as several blocks;
    // single-block MemFd buffers are the common fast path.
    let mut bytes = Vec::new();
    for block in buffer.datas_mut() {
        if bytes.len() >= expected {
            break;
        }
        let offset = block.chunk().offset() as usize;
        let size = block.chunk().size() as usize;
        let Some(data_bytes) = block.data() else {
            continue;
        };
        let end = offset.saturating_add(size).min(data_bytes.len());
        if offset >= end {
            continue;
        }
        let remaining = expected - bytes.len();
        let take = (end - offset).min(remaining);
        bytes.extend_from_slice(&data_bytes[offset..offset + take]);
    }
    if bytes.len() != expected {
        // Short/ragged buffer: reject the frame but stay streaming. This is
        // a negotiation/peer problem, not overload, so it gets its own
        // counter rather than the queue-drop counter.
        data.shared.diagnostics.note_rejected();
        return;
    }
    let sequence = data.shared.sequence.fetch_add(1, Ordering::Relaxed);
    match VideoFrame::new(spec, bytes) {
        Ok(mut frame) => {
            frame.set_sequence(sequence);
            data.shared.diagnostics.note_received();
            data.shared.latest.publish(frame.clone());
            data.shared.queue.push_from_callback(frame);
        }
        Err(error) => {
            data.shared.diagnostics.note_error(&error.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::format::SUPPORTED_PIXEL_FORMATS;
    use super::*;

    #[test]
    fn supported_formats_cover_task_set() {
        assert!(SUPPORTED_PIXEL_FORMATS.contains(&VideoPixelFormat::Rgbx));
        assert!(SUPPORTED_PIXEL_FORMATS.contains(&VideoPixelFormat::Bgrx));
        assert!(SUPPORTED_PIXEL_FORMATS.contains(&VideoPixelFormat::Rgba));
        assert!(SUPPORTED_PIXEL_FORMATS.contains(&VideoPixelFormat::Nv12));
        assert!(SUPPORTED_PIXEL_FORMATS.contains(&VideoPixelFormat::I420));
    }
}
