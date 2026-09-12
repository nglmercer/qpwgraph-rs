//! The user-mode audio router.
//!
//! # Why this exists
//!
//! On Linux, qpwgraph asks PipeWire to make a link and PipeWire moves the
//! audio. On Windows there is no equivalent: Core Audio will tell you which
//! endpoint an application session is attached to, and will let you change
//! that endpoint's volume, but it will not let you re-point the session at a
//! different endpoint, insert an effect into a route, or expose a new capture
//! device. Every one of those is a link the user can draw on Linux, so
//! `connect`, `disconnect`, and `effects` are all reported as unsupported by
//! the Windows backend today, honestly.
//!
//! The way out is for qpwgraph to own the PCM itself. This module is that
//! ownership: a deterministic routing engine that pulls from sources, mixes,
//! converts, processes, meters, and pushes to sinks, with no knowledge of
//! which operating system produced either end.
//!
//! # What it is not
//!
//! It is not a driver, and it does not make `connect` return `Ok` on its own.
//! A router with nothing but WASAPI endpoints on both ends can already carry
//! "this device to that device" and "this application to a peer", but a
//! qpwgraph-owned *capture* device -- the virtual microphone the relay needs,
//! and the endpoint an arbitrary application could be pointed at -- requires a
//! kernel-mode component that this crate deliberately does not contain. The
//! parity roadmap gates that behind an architecture decision record and a
//! spike, and nothing here pre-empts it. What this module does is make sure
//! that when the endpoints arrive, the graph semantics above them are already
//! written, tested, and honest.
//!
//! # Layout
//!
//! | Module | Owns |
//! | --- | --- |
//! | [`format`] | what a buffer of samples means |
//! | [`buffer`] | the wait-free hand-off between a device and the router |
//! | [`resample`] | rate conversion and clock-drift correction |
//! | [`meter`] | peak and RMS levels, including the RMS Core Audio cannot give |
//! | [`diagnostics`] | the counters that explain a dropout after the fact |
//! | [`engine`] | sources, sinks, effects, routes, and the block cycle |
//! | [`endpoints`] | ring-backed and in-memory sources and sinks |
//! | [`thread`] | the paced loop that turns the crank in production |
//! | `wasapi` | real Windows endpoints as sources and sinks (Windows only) |

pub mod buffer;
pub mod diagnostics;
pub mod endpoints;
pub mod engine;
pub mod format;
pub mod meter;
pub mod recorder;
pub mod resample;
pub mod thread;
/// Real Windows endpoints as router sources and sinks.
#[cfg(target_os = "windows")]
pub mod wasapi;

pub use diagnostics::{RouteFault, RouteMetrics};
pub use endpoints::{
    ring_sink, ring_source, ring_source_fanout, BufferSource, CaptureSink, RingSink, RingSinkDrain,
    RingSource, RingSourceFanout, RingSourceFanoutControl, RingSourceFeed,
};
pub use engine::{
    AudioSink, AudioSource, Backlog, DestinationSpec, ProcessReport, ProcessorId, RouteId,
    RouteSpec, RouterConfig, RouterCore, RouterError, SinkId, SinkWrite, SourceId, SourceRead,
    StreamHealth,
};
pub use format::{AudioFormat, ChannelMap};
pub use meter::MeterReading;
pub use recorder::{
    copy_recording, copy_recording_preserving_source, default_recording_filename,
    pending_recording_dir, read_wav_header, recover_pending_recordings, render_recording_filename,
    repair_wav_header, save_recording, scan_pending_recordings, unique_recording_path,
    RecorderDiagnostics, RecorderError, RecorderResult, RecorderSink, RecorderWriter,
    RecorderWriterState, RecoveredRecording, WavHeader, WavWriter, DEFAULT_RECORDER_CAPACITY_MS,
    MAX_RIFF_DATA_BYTES,
};
pub use thread::{RouterStopped, RouterThread};

#[cfg(test)]
mod tests;
