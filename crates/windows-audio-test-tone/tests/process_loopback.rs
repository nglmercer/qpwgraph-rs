//! Opt-in live process-loopback smoke test.
//!
//! This test deliberately does nothing on ordinary CI machines. Set
//! `PW_GRAPH_TEST_PROCESS_LOOPBACK=1` on a Windows test host with an active
//! output endpoint to validate the complete helper → WASAPI process-loopback
//! → router source path.

#![cfg(target_os = "windows")]

use pw_graph_backend::router::{AudioFormat, AudioSource, StreamHealth};
use pw_graph_backend::{
    GraphDriver, MeterPolicy, ProcessLoopbackMode, ProcessLoopbackSource, WindowsAudioDriver,
};
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

#[test]
fn helper_audio_provides_true_process_rms_without_virtual_driver() {
    if std::env::var("PW_GRAPH_TEST_PROCESS_RMS").ok().as_deref() != Some("1") {
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
    let result = (|| -> Result<(), String> {
        let mut driver = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
        driver
            .set_meter_policy(MeterPolicy::Always)
            .map_err(|error| error.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut rms_nodes = Vec::new();
        while Instant::now() < deadline {
            driver.refresh().map_err(|error| error.to_string())?;
            rms_nodes = driver
                .graph()
                .nodes
                .values()
                .filter(|node| driver.node_capabilities(node.id).meter_rms)
                .map(|node| node.id)
                .collect();
            if !rms_nodes.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if rms_nodes.is_empty() {
            return Err("the live helper session did not advertise process RMS capability".into());
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let meters = driver.audio_meters().map_err(|error| error.to_string())?;
            if meters.iter().any(|meter| {
                rms_nodes.contains(&meter.node_id) && meter.available && meter.rms > 0.05
            }) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err("the live process RMS meter never observed the helper tone".into())
    })();
    stop_child(&mut child);
    result.expect("process RMS smoke test failed");
}

#[test]
fn process_loopback_reports_loss_when_target_exits() {
    if std::env::var("PW_GRAPH_TEST_PROCESS_EXIT").ok().as_deref() != Some("1") {
        return;
    }
    let Some(helper) = helper_path() else {
        panic!("Cargo did not provide CARGO_BIN_EXE_windows-audio-test-tone");
    };
    let mut child = Command::new(helper)
        .args([
            "--duration-ms",
            "30000",
            "--frequency",
            "1000",
            "--amplitude",
            "0.25",
        ])
        .spawn()
        .expect("start deterministic WASAPI test tone");
    let result = (|| -> Result<(), String> {
        let (mut source, mut worker) = ProcessLoopbackSource::open(
            child.id(),
            ProcessLoopbackMode::IncludeProcessTree,
            AudioFormat::new(48_000, 2),
            4_096,
        )
        .map_err(|error| error.to_string())?;
        let mut block = vec![0.0f32; 480 * 2];
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut observed = false;
        while Instant::now() < deadline {
            let read = source.read(&mut block);
            if read.health == StreamHealth::Lost {
                return Err(
                    "process-loopback target was lost before its audio was observed".into(),
                );
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
        if !observed {
            worker.stop();
            return Err("process-loopback did not expose the helper before exit".into());
        }

        stop_child(&mut child);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let read = source.read(&mut block);
            if read.health == StreamHealth::Lost || !worker.is_running() {
                worker.stop();
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        worker.stop();
        Err("process-loopback worker did not report target exit".into())
    })();
    stop_child(&mut child);
    result.expect("process exit smoke test failed");
}
