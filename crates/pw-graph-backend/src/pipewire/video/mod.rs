//! Linux PipeWire video streams: capture, output, and filter bridges.
//!
//! Video negotiation is stream-oriented and deliberately separate from the
//! audio `pw_filter` path: SPA video buffers (MemFd/DMABUF, stride,
//! multi-plane) negotiate through `EnumFormat` stream params, not through
//! mapped DSP ports.

pub mod bridge;
pub mod capture;
pub mod format;
pub mod output;
