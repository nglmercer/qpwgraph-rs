#![cfg_attr(target_os = "android", allow(clippy::missing_const_for_thread_local))]
use std::cell::RefCell;

thread_local! {
    /// Per-JNI-thread PCM storage. It grows at most once to the realtime
    /// quantum and is filled before the engine call, so native audio
    /// methods do not allocate a Vec on every callback.
    pub static PCM_SCRATCH: RefCell<Vec<f32>> = const { RefCell::new(Vec::new()) };
}
