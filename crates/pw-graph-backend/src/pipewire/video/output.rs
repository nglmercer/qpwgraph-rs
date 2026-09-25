//! PipeWire video output streams.
//!
//! An output stream publishes processed frames from a filter worker into the
//! graph so downstream video sinks (preview sinks, recorders, future relay)
//! can consume them. The process callback copies the newest available frame
//! into the dequeued buffer; when no frame is ready yet it queues an empty
//! buffer so the stream stays alive without inventing content.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::super::*;
use super::format::{build_fixed_format_pod, build_output_buffers_pod, parse_format_param};
use pw_graph_video::{LatestFrame, VideoDiagnostics, VideoSpec};

/// State shared between the realtime callback and the driver thread.
pub struct VideoOutputShared {
    /// Worker output slot; the callback always takes the newest frame.
    pub source: LatestFrame,
    pub diagnostics: VideoDiagnostics,
    /// Spec we offered. A daemon-side renegotiation that disagrees sets
    /// `format_mismatch` so the driver can rebuild the stream.
    pub offered: Mutex<Option<VideoSpec>>,
    pub negotiated: Mutex<Option<VideoSpec>>,
    pub connected: AtomicBool,
    pub format_mismatch: AtomicBool,
    pub frames_written: AtomicU64,
}

pub struct VideoOutputCallback {
    pub shared: Arc<VideoOutputShared>,
}

pub struct VideoOutputHandle {
    pub _stream: pw::stream::Stream,
    pub _listener: pw::stream::StreamListener<VideoOutputCallback>,
    pub shared: Arc<VideoOutputShared>,
}

/// Create an output stream producing exactly `spec`. Must be called with the
/// thread loop lock held.
pub fn create_video_output_locked(
    core: &pw::core::Core,
    stream_name: &str,
    spec: &VideoSpec,
    source: LatestFrame,
) -> BackendResult<VideoOutputHandle> {
    let properties = pw::properties::properties! {
        NODE_NAME => stream_name,
        MEDIA_TYPE => MEDIA_TYPE_VIDEO,
        PROP_MEDIA_CATEGORY => MEDIA_CATEGORY_PLAYBACK,
        MEDIA_CLASS => MEDIA_CLASS_VIDEO_OUTPUT,
    };
    let stream = pw::stream::Stream::new(core, stream_name, properties)
        .map_err(|error| native_error("PipeWire video output stream creation", error))?;
    let shared = Arc::new(VideoOutputShared {
        source,
        diagnostics: VideoDiagnostics::new(),
        offered: Mutex::new(Some(*spec)),
        negotiated: Mutex::new(None),
        connected: AtomicBool::new(false),
        format_mismatch: AtomicBool::new(false),
        frames_written: AtomicU64::new(0),
    });
    let listener = stream
        .add_local_listener_with_user_data(VideoOutputCallback {
            shared: shared.clone(),
        })
        .state_changed(|_, data, _old, new| {
            let streaming = matches!(new, pw::stream::StreamState::Streaming);
            data.shared.connected.store(streaming, Ordering::Relaxed);
            if matches!(new, pw::stream::StreamState::Error(_)) {
                data.shared.diagnostics.note_error("output stream error");
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
                Ok(negotiated) => {
                    let mismatch = data
                        .shared
                        .offered
                        .lock()
                        .map(|offered| *offered != Some(negotiated))
                        .unwrap_or(false);
                    data.shared
                        .format_mismatch
                        .store(mismatch, Ordering::Relaxed);
                    if let Ok(mut current) = data.shared.negotiated.lock() {
                        *current = Some(negotiated);
                    }
                    if mismatch {
                        data.shared.diagnostics.note_error(
                            "output renegotiated to an unexpected format; stream rebuild required",
                        );
                    } else {
                        data.shared.diagnostics.clear_error();
                    }
                }
                Err(error) => {
                    data.shared
                        .diagnostics
                        .note_error(&format!("unsupported output format: {error}"));
                }
            }
        })
        .process(process_video_output)
        .register()
        .map_err(|error| native_error("PipeWire video output listener", error))?;

    let format_bytes = build_fixed_format_pod(spec)?;
    let format_pod = Pod::from_bytes(&format_bytes).ok_or_else(|| {
        BackendError::Native("could not serialize PipeWire video output format".into())
    })?;
    let buffers_bytes = build_output_buffers_pod(spec)?;
    let buffers_pod = Pod::from_bytes(&buffers_bytes).ok_or_else(|| {
        BackendError::Native("could not serialize PipeWire video output buffers".into())
    })?;
    let mut params = [format_pod, buffers_pod];
    stream
        .connect(
            SpaDirection::Output,
            None,
            // No AUTOCONNECT: a targetless producer must publish and wait
            // for consumers instead of erroring with "no target node
            // available" (there is no default video sink). Links to
            // consumers are created explicitly by the driver.
            pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|error| native_error("PipeWire video output stream connection", error))?;
    stream
        .set_active(true)
        .map_err(|error| native_error("PipeWire video output stream activation", error))?;

    Ok(VideoOutputHandle {
        _stream: stream,
        _listener: listener,
        shared,
    })
}

/// Realtime frame publication. Copies the newest worker output into the
/// buffer and marks the chunk; an empty chunk keeps the stream alive when no
/// frame is ready. Dropping the buffer queues it downstream.
pub fn process_video_output(stream: &pw::stream::StreamRef, data: &mut VideoOutputCallback) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    if data.shared.format_mismatch.load(Ordering::Relaxed) {
        queue_empty(&mut buffer);
        return;
    }
    let Some(frame) = data.shared.source.latest() else {
        queue_empty(&mut buffer);
        return;
    };
    let bytes = frame.bytes();
    let mut written = 0_usize;
    for block in buffer.datas_mut() {
        if written >= bytes.len() {
            break;
        }
        let Some(target) = block.data() else {
            continue;
        };
        let take = (target.len()).min(bytes.len() - written);
        target[..take].copy_from_slice(&bytes[written..written + take]);
        let chunk = block.chunk_mut();
        *chunk.offset_mut() = 0;
        *chunk.size_mut() = take as u32;
        *chunk.stride_mut() = frame_stride(&frame);
        written += take;
        // Single-block fast path covers the MemFd case; multi-block buffers
        // receive contiguous plane slices in order.
        if written >= bytes.len() {
            break;
        }
    }
    if written < bytes.len() {
        // Buffer too small for the frame: queue what fits would corrupt the
        // image, so queue empty and report instead of tearing.
        data.shared
            .diagnostics
            .note_error("output buffer smaller than frame");
        queue_empty(&mut buffer);
        return;
    }
    data.shared.frames_written.fetch_add(1, Ordering::Relaxed);
}

fn queue_empty(buffer: &mut pw::buffer::Buffer<'_>) {
    for block in buffer.datas_mut() {
        let chunk = block.chunk_mut();
        *chunk.offset_mut() = 0;
        *chunk.size_mut() = 0;
    }
}

fn frame_stride(frame: &pw_graph_video::VideoFrame) -> i32 {
    match frame.format() {
        pw_graph_video::VideoPixelFormat::Rgbx
        | pw_graph_video::VideoPixelFormat::Bgrx
        | pw_graph_video::VideoPixelFormat::Rgba => frame.width() as i32 * 4,
        pw_graph_video::VideoPixelFormat::Nv12 | pw_graph_video::VideoPixelFormat::I420 => {
            frame.width() as i32
        }
    }
}
