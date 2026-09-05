//! Opt-in live process-loopback smoke test.
//!
//! This test deliberately does nothing on ordinary CI machines. Set
//! `PW_GRAPH_TEST_PROCESS_LOOPBACK=1` on a Windows test host with an active
//! output endpoint to validate the complete helper → WASAPI process-loopback
//! → router source path.

#![cfg(target_os = "windows")]

use pw_graph_backend::router::{AudioFormat, AudioSource, StreamHealth};
use pw_graph_backend::{ProcessLoopbackMode, ProcessLoopbackSource};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn helper_path() -> Option<std::path::PathBuf> {
    std::env::var_os("CARGO_BIN_EXE_windows-audio-test-tone").map(std::path::PathBuf::from)
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn helper_audio_is_visible_to_process_loopback_when_opted_in() {
    if std::env::var("PW_GRAPH_TEST_PROCESS_LOOPBACK")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let Some(helper) = helper_path() else {
        panic!("Cargo did not provide CARGO_BIN_EXE_windows-audio-test-tone");
    };
    let mut child = Command::new(helper)
        .args([
            "--duration-ms",
            "10000",
            "--frequency",
            "1000",
            "--amplitude",
            "0.25",
        ])
        .spawn()
        .expect("start deterministic WASAPI test tone");
    let result = (|| {
        let (mut source, mut worker) = ProcessLoopbackSource::open(
            child.id(),
            ProcessLoopbackMode::IncludeProcessTree,
            AudioFormat::new(48_000, 2),
            4_096,
        )?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut block = vec![0.0f32; 480 * 2];
        let mut observed = false;
        while Instant::now() < deadline {
            let read = source.read(&mut block);
            if read.health == StreamHealth::Lost {
                break;
            }
            if read.frames > 0
                && block[..read.frames * 2]
                    .iter()
                    .any(|sample| sample.abs() > 0.01)
            {
                observed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        worker.stop();
        assert!(observed, "process loopback did not expose the helper tone");
        Ok::<(), pw_graph_backend::BackendError>(())
    })();
    stop_child(&mut child);
    result.expect("process-loopback smoke test failed");
}
