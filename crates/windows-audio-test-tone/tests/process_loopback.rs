//! Opt-in live process-loopback smoke test.
//!
//! This test deliberately does nothing on ordinary CI machines. Set
//! `PW_GRAPH_TEST_PROCESS_LOOPBACK=1` on a Windows test host with an active
//! output endpoint to validate the complete helper → WASAPI process-loopback
//! → router source path.
//!
//! An ordinary-application relay check is opt-in as well. The generic form is
//! `PW_GRAPH_TEST_APPLICATION_RELAY=1 PW_GRAPH_TEST_APPLICATION_PID=<pid>`
//! with `PW_GRAPH_TEST_APPLICATION_NAME=<friendly name>`; the legacy browser
//! variables remain accepted (Firefox is the default name). It verifies the
//! stable selector, relay activation, and non-silent local session meter while
//! capture is running, so the same probe can cover Firefox, Chrome, or VLC.

#![cfg(target_os = "windows")]

use pw_graph_backend::router::{AudioFormat, AudioSource, StreamHealth};
use pw_graph_backend::{
    GraphDriver, MeterPolicy, ProcessIdentity, ProcessLoopbackMode, ProcessLoopbackSource,
    WindowsAudioDriver,
};
#[cfg(feature = "relay-tests")]
use pw_graph_backend::{
    RelayCodecKind, RelayDirection, RelayDriver, RelayHostRequest, RelayMode, RelaySendSource,
    RelayTransportPreference,
};
use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

fn helper_path() -> Option<std::path::PathBuf> {
    std::env::var_os("CARGO_BIN_EXE_windows-audio-test-tone").map(std::path::PathBuf::from)
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn spawn_tone(
    helper: &Path,
    duration_ms: u64,
    frequency: u32,
    amplitude: f32,
    sessions: usize,
) -> Child {
    Command::new(helper)
        .args([
            "--duration-ms".into(),
            duration_ms.to_string(),
            "--frequency".into(),
            frequency.to_string(),
            "--amplitude".into(),
            amplitude.to_string(),
            "--sessions".into(),
            sessions.to_string(),
        ])
        .spawn()
        .expect("start deterministic WASAPI test tone")
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
fn packaged_application_identity_and_process_loopback_are_visible_when_opted_in() {
    if std::env::var("PW_GRAPH_TEST_PACKAGED_APP").ok().as_deref() != Some("1") {
        return;
    }
    let pid = std::env::var("PW_GRAPH_TEST_PACKAGED_PID")
        .expect("PW_GRAPH_TEST_PACKAGED_PID is required for packaged-app smoke")
        .parse::<u32>()
        .expect("PW_GRAPH_TEST_PACKAGED_PID is numeric");
    let identity = ProcessIdentity::from_pid(pid).expect("packaged process is queryable");
    assert!(
        identity.package_family_name.is_some(),
        "packaged helper did not expose a package family: {identity:?}"
    );
    assert!(
        identity.app_user_model_id.is_some(),
        "packaged helper did not expose an AUMID: {identity:?}"
    );
    let (mut source, mut worker) = ProcessLoopbackSource::open(
        pid,
        ProcessLoopbackMode::IncludeProcessTree,
        AudioFormat::new(48_000, 2),
        4_096,
    )
    .expect("packaged helper process-loopback activation failed");
    let deadline = Instant::now() + Duration::from_secs(8);
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
    println!(
        "packaged process family={:?} aumid={:?} selector={:?} audio={observed}",
        identity.package_family_name,
        identity.app_user_model_id,
        identity.selector_key()
    );
    assert!(
        observed,
        "packaged helper produced no process-loopback audio"
    );
}

#[test]
fn packaged_application_restart_preserves_aumid_selector_when_opted_in() {
    if std::env::var("PW_GRAPH_TEST_PACKAGED_RESTART")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let pids = std::env::var("PW_GRAPH_TEST_PACKAGED_PIDS")
        .expect("PW_GRAPH_TEST_PACKAGED_PIDS is required for packaged restart smoke")
        .split(',')
        .map(|value| value.parse::<u32>().expect("packaged PID is numeric"))
        .collect::<Vec<_>>();
    assert_eq!(pids.len(), 2, "packaged restart smoke needs two PIDs");
    let first = ProcessIdentity::from_pid(pids[0]).expect("first packaged process is queryable");
    let second =
        ProcessIdentity::from_pid(pids[1]).expect("restarted packaged process is queryable");
    assert_eq!(
        first.package_family_name, second.package_family_name,
        "package family changed across the restart: first={first:?} second={second:?}"
    );
    assert_eq!(
        first.app_user_model_id, second.app_user_model_id,
        "AUMID changed across the restart: first={first:?} second={second:?}"
    );
    assert_eq!(
        first.selector_key(),
        second.selector_key(),
        "stable selector changed across the restart: first={first:?} second={second:?}"
    );
    println!(
        "packaged restart selectors: first_pid={} second_pid={} selector={:?}",
        pids[0],
        pids[1],
        second.selector_key()
    );
}

#[test]
fn process_loopback_includes_audio_from_child_processes() {
    if std::env::var("PW_GRAPH_TEST_PROCESS_CHILD_TREE")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let Some(helper) = helper_path() else {
        panic!("Cargo did not provide CARGO_BIN_EXE_windows-audio-test-tone");
    };
    let mut parent = Command::new(helper)
        .args([
            "--duration-ms",
            "30000",
            "--frequency",
            "1000",
            "--amplitude",
            "0.25",
            "--spawn-child-only",
        ])
        .spawn()
        .expect("start deterministic child-tree tone helper");
    let result = (|| -> Result<(), String> {
        let (mut source, mut worker) = ProcessLoopbackSource::open(
            parent.id(),
            ProcessLoopbackMode::IncludeProcessTree,
            AudioFormat::new(48_000, 2),
            4_096,
        )
        .map_err(|error| error.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut block = vec![0.0f32; 480 * 2];
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
                worker.stop();
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        worker.stop();
        Err("include-process-tree loopback did not expose child audio".into())
    })();
    stop_child(&mut parent);
    result.expect("child-process loopback smoke test failed");
}

#[test]
fn process_loopback_excludes_another_application_on_the_same_endpoint() {
    if std::env::var("PW_GRAPH_TEST_PROCESS_ISOLATION")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let Some(helper) = helper_path() else {
        panic!("Cargo did not provide CARGO_BIN_EXE_windows-audio-test-tone");
    };
    let mut target = spawn_tone(&helper, 30_000, 1_000, 0.25, 1);
    let mut other = spawn_tone(&helper, 30_000, 3_000, 0.8, 1);
    let result = (|| -> Result<(), String> {
        let (mut source, mut worker) = ProcessLoopbackSource::open(
            target.id(),
            ProcessLoopbackMode::IncludeProcessTree,
            AudioFormat::new(48_000, 2),
            4_096,
        )
        .map_err(|error| error.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut block = vec![0.0f32; 480 * 2];
        while Instant::now() < deadline {
            let read = source.read(&mut block);
            if read.health == StreamHealth::Lost {
                break;
            }
            if read.frames > 0 {
                let samples = &block[..read.frames * 2];
                let rms = (samples.iter().map(|sample| sample * sample).sum::<f32>()
                    / samples.len() as f32)
                    .sqrt();
                // The target's 0.25 sine is about 0.177 RMS. Including the
                // other process' 0.8 sine would raise this well above 0.3.
                if (0.05..0.3).contains(&rms) {
                    worker.stop();
                    return Ok(());
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        worker.stop();
        Err("process-loopback capture included the wrong endpoint session".into())
    })();
    stop_child(&mut target);
    stop_child(&mut other);
    result.expect("process isolation smoke test failed");
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
fn silent_process_remains_meterable_without_fabricating_audio() {
    if std::env::var("PW_GRAPH_TEST_PROCESS_SILENT")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let Some(helper) = helper_path() else {
        panic!("Cargo did not provide CARGO_BIN_EXE_windows-audio-test-tone");
    };
    let mut child = spawn_tone(&helper, 30_000, 1_000, 0.0, 1);
    let result = (|| -> Result<(), String> {
        let mut driver = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
        driver
            .set_meter_policy(MeterPolicy::Always)
            .map_err(|error| error.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            driver.refresh().map_err(|error| error.to_string())?;
            let rms_nodes: Vec<_> = driver
                .graph()
                .nodes
                .values()
                .filter(|node| {
                    node.name
                        .to_ascii_lowercase()
                        .contains("windows-audio-test-tone")
                })
                .filter(|node| driver.node_capabilities(node.id).meter_rms)
                .map(|node| node.id)
                .collect();
            if !rms_nodes.is_empty() {
                let meters = driver.audio_meters().map_err(|error| error.to_string())?;
                if meters.iter().any(|meter| {
                    rms_nodes.contains(&meter.node_id)
                        && meter.available
                        && meter.rms <= 0.001
                        && meter.peak <= 0.001
                }) {
                    return Ok(());
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err("silent helper did not produce an available zero-level process meter".into())
    })();
    stop_child(&mut child);
    result.expect("silent process smoke test failed");
}

#[test]
fn multiple_audio_sessions_in_one_process_remain_meterable() {
    if std::env::var("PW_GRAPH_TEST_PROCESS_MULTI_SESSION")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let Some(helper) = helper_path() else {
        panic!("Cargo did not provide CARGO_BIN_EXE_windows-audio-test-tone");
    };
    let mut child = spawn_tone(&helper, 30_000, 1_000, 0.25, 2);
    let result = (|| -> Result<(), String> {
        let mut driver = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
        driver
            .set_meter_policy(MeterPolicy::Always)
            .map_err(|error| error.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            driver.refresh().map_err(|error| error.to_string())?;
            let helper_nodes: Vec<_> = driver
                .graph()
                .nodes
                .values()
                .filter(|node| {
                    node.name
                        .to_ascii_lowercase()
                        .contains("windows-audio-test-tone")
                        && driver.node_capabilities(node.id).meter_rms
                })
                .map(|node| node.id)
                .collect();
            if helper_nodes.len() >= 2 {
                let meters = driver.audio_meters().map_err(|error| error.to_string())?;
                if helper_nodes.iter().all(|node_id| {
                    meters.iter().any(|meter| {
                        meter.node_id == *node_id && meter.available && meter.rms > 0.05
                    })
                }) {
                    return Ok(());
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err("the helper did not expose two independent active audio sessions".into())
    })();
    stop_child(&mut child);
    result.expect("multiple-session process smoke test failed");
}

#[test]
fn process_started_after_driver_is_discovered_and_metered() {
    if std::env::var("PW_GRAPH_TEST_PROCESS_START_AFTER_DRIVER")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let Some(helper) = helper_path() else {
        panic!("Cargo did not provide CARGO_BIN_EXE_windows-audio-test-tone");
    };
    let result = (|| -> Result<(), String> {
        let mut driver = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
        driver
            .set_meter_policy(MeterPolicy::Always)
            .map_err(|error| error.to_string())?;
        driver.refresh().map_err(|error| error.to_string())?;
        let baseline_nodes: BTreeSet<_> = driver.graph().nodes.keys().copied().collect();
        let mut child = spawn_tone(&helper, 30_000, 1_000, 0.25, 1);
        let result = (|| -> Result<(), String> {
            let deadline = Instant::now() + Duration::from_secs(8);
            while Instant::now() < deadline {
                driver.refresh().map_err(|error| error.to_string())?;
                if driver.graph().nodes.values().any(|node| {
                    !baseline_nodes.contains(&node.id)
                        && node
                            .name
                            .to_ascii_lowercase()
                            .contains("windows-audio-test-tone")
                        && driver.node_capabilities(node.id).meter_rms
                }) {
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err("process started after qpwgraph but never became an RMS-capable session".into())
        })();
        stop_child(&mut child);
        result
    })();
    result.expect("process-start-after-driver smoke test failed");
}

#[test]
fn meter_policy_controls_process_worker_lifetime() {
    if std::env::var("PW_GRAPH_TEST_PROCESS_POLICY")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let Some(helper) = helper_path() else {
        panic!("Cargo did not provide CARGO_BIN_EXE_windows-audio-test-tone");
    };
    let mut child = Command::new(&helper)
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
        let mut driver = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(8);
        let helper_nodes = loop {
            driver.refresh().map_err(|error| error.to_string())?;
            let nodes: Vec<_> = driver
                .graph()
                .nodes
                .values()
                .filter(|node| driver.node_capabilities(node.id).meter_rms)
                .map(|node| node.id)
                .collect();
            if !nodes.is_empty() {
                break nodes;
            }
            if Instant::now() >= deadline {
                return Err(
                    "the live helper session did not advertise process RMS capability".into(),
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let report_count = |driver: &mut WindowsAudioDriver| -> Result<usize, String> {
            driver.audio_meters().map_err(|error| error.to_string())?;
            driver
                .windows_audio_report()
                .lines()
                .find_map(|line| {
                    line.strip_prefix("process_loopback_captures=")
                        .and_then(|value| value.parse().ok())
                })
                .ok_or_else(|| "Windows audio report omitted process capture count".into())
        };

        driver
            .set_meter_policy(MeterPolicy::Always)
            .map_err(|error| error.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline && report_count(&mut driver)? == 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        if report_count(&mut driver)? == 0 {
            return Err("always meter policy did not start a process worker".into());
        }

        driver
            .set_meter_policy(MeterPolicy::OnDemand)
            .map_err(|error| error.to_string())?;
        driver
            .request_meters(&BTreeSet::new())
            .map_err(|error| error.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline && report_count(&mut driver)? != 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        if report_count(&mut driver)? != 0 {
            return Err("on-demand policy kept an unrequested process worker alive".into());
        }

        driver
            .request_meters(&BTreeSet::from([helper_nodes[0]]))
            .map_err(|error| error.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline && report_count(&mut driver)? == 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        if report_count(&mut driver)? == 0 {
            return Err("on-demand request did not start a process worker".into());
        }

        driver
            .set_meter_policy(MeterPolicy::Disabled)
            .map_err(|error| error.to_string())?;
        if report_count(&mut driver)? != 0 {
            return Err("disabled meter policy kept a process worker alive".into());
        }
        Ok(())
    })();
    stop_child(&mut child);
    result.expect("meter policy smoke test failed");
}

#[test]
fn process_loopback_failure_preserves_native_peak() {
    if std::env::var("PW_GRAPH_TEST_PROCESS_LOOPBACK_FAILURE")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let Some(helper) = helper_path() else {
        panic!("Cargo did not provide CARGO_BIN_EXE_windows-audio-test-tone");
    };
    let mut child = spawn_tone(&helper, 30_000, 1_000, 0.25, 1);
    let result = (|| -> Result<(), String> {
        let mut driver = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
        driver
            .set_meter_policy(MeterPolicy::Always)
            .map_err(|error| error.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            driver.refresh().map_err(|error| error.to_string())?;
            let helper_node = driver
                .graph()
                .nodes
                .values()
                .find(|node| {
                    node.name
                        .to_ascii_lowercase()
                        .contains("windows-audio-test-tone")
                        && driver.node_capabilities(node.id).meter_peak
                })
                .map(|node| node.id);
            let Some(helper_node) = helper_node else {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            if let Some(meter) = driver
                .audio_meters()
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|meter| meter.node_id == helper_node)
            {
                if meter.available && meter.peak > 0.05 {
                    if meter.rms != 0.0 {
                        return Err(format!(
                            "fault-injected process capture fabricated RMS {:.3}",
                            meter.rms
                        ));
                    }
                    if !driver.windows_audio_report().contains("state=Unavailable") {
                        return Err(
                            "Windows audio report did not retain the process-loopback failure"
                                .into(),
                        );
                    }
                    return Ok(());
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err("native peak fallback was not observed for the live helper session".into())
    })();
    stop_child(&mut child);
    result.expect("process-loopback failure fallback smoke test failed");
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

#[test]
fn process_loopback_survives_one_thousand_start_stop_cycles() {
    if std::env::var("PW_GRAPH_TEST_PROCESS_CYCLES")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let Some(helper) = helper_path() else {
        panic!("Cargo did not provide CARGO_BIN_EXE_windows-audio-test-tone");
    };
    let mut child = spawn_tone(&helper, 120_000, 1_000, 0.25, 1);
    let result = (|| -> Result<(), String> {
        for cycle in 0..1_000 {
            let (_, mut worker) = ProcessLoopbackSource::open(
                child.id(),
                ProcessLoopbackMode::IncludeProcessTree,
                AudioFormat::new(48_000, 2),
                1_024,
            )
            .map_err(|error| format!("activation cycle {cycle} failed: {error}"))?;
            worker.stop();
        }
        Ok(())
    })();
    stop_child(&mut child);
    result.expect("process-loopback cycle smoke test failed");
}

#[cfg(feature = "relay-tests")]
#[test]
fn application_relay_rebinds_after_target_restart_without_virtual_driver() {
    if std::env::var("PW_GRAPH_TEST_RELAY_APPLICATION")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let Some(helper) = helper_path() else {
        panic!("Cargo did not provide CARGO_BIN_EXE_windows-audio-test-tone");
    };
    let mut child = Command::new(&helper)
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
        let mut driver = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
        let selector = {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                driver.refresh().map_err(|error| error.to_string())?;
                if let Some(source) = driver
                    .relay_send_sources()
                    .into_iter()
                    .find(|source| source.name.eq_ignore_ascii_case("windows-audio-test-tone"))
                {
                    break source
                        .id
                        .strip_prefix("application:")
                        .ok_or_else(|| "application relay source had an invalid ID".to_owned())?
                        .to_owned();
                }
                if Instant::now() >= deadline {
                    return Err(
                        "the live helper session did not appear as an application relay source"
                            .into(),
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        };

        driver
            .relay_set_send_source(RelaySendSource::Application(selector.clone()))
            .map_err(|error| error.to_string())?;
        driver
            .relay_connect_mode(
                "127.0.0.1:9"
                    .parse()
                    .expect("discard port is a valid target"),
                "123456",
                RelayMode::Emitter,
                1,
            )
            .map_err(|error| error.to_string())?;
        if !driver.relay_devices_active() {
            return Err("the application relay worker did not start".into());
        }

        stop_child(&mut child);
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            driver.refresh().map_err(|error| error.to_string())?;
            if !driver.relay_devices_active() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if driver.relay_devices_active() {
            return Err("the application relay worker stayed active after target exit".into());
        }

        child = Command::new(&helper)
            .args([
                "--duration-ms",
                "30000",
                "--frequency",
                "1000",
                "--amplitude",
                "0.25",
            ])
            .spawn()
            .map_err(|error| format!("restart deterministic WASAPI test tone: {error}"))?;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            driver.refresh().map_err(|error| error.to_string())?;
            if driver.relay_devices_active() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(format!(
            "the application relay did not rebind selector {selector} after target restart"
        ))
    })();
    stop_child(&mut child);
    result.expect("application relay restart smoke test failed");
}

#[cfg(feature = "relay-tests")]
#[test]
fn application_relay_keeps_the_authenticated_control_session_when_target_exits() {
    if std::env::var("PW_GRAPH_TEST_RELAY_APPLICATION_SESSION")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let Some(helper) = helper_path() else {
        panic!("Cargo did not provide CARGO_BIN_EXE_windows-audio-test-tone");
    };
    let mut child = Command::new(&helper)
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
        let mut host = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
        let port = host
            .relay_start_host(RelayHostRequest {
                device_id: "qpwgraph-test-host".into(),
                trusted_peers: Vec::new(),
                trust_new_peers: true,
                device_name: "qpwgraph-rs test host".into(),
                pin: "123456".into(),
                port: 0,
                codec: RelayCodecKind::Opus,
                frame_ms: 10,
                transport: RelayTransportPreference::Auto,
                direction: RelayDirection::MobileToDesktop,
                direction_generation: 1,
                mode: RelayMode::Receiver,
                mode_generation: 1,
            })
            .map_err(|error| format!("start local relay host: {error}"))?;
        let mut client = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
        let selector = {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                client.refresh().map_err(|error| error.to_string())?;
                if let Some(source) = client
                    .relay_send_sources()
                    .into_iter()
                    .find(|source| source.name.eq_ignore_ascii_case("windows-audio-test-tone"))
                {
                    break source
                        .id
                        .strip_prefix("application:")
                        .ok_or_else(|| "application relay source had an invalid ID".to_owned())?
                        .to_owned();
                }
                if Instant::now() >= deadline {
                    return Err(
                        "the live helper session did not appear as an application relay source"
                            .into(),
                    );
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        client
            .relay_set_send_source(RelaySendSource::Application(selector))
            .map_err(|error| error.to_string())?;
        let session = client
            .relay_connect_mode(
                format!("127.0.0.1:{port}")
                    .parse()
                    .expect("local relay host address is valid"),
                "123456",
                RelayMode::Emitter,
                2,
            )
            .map_err(|error| format!("connect local relay client: {error}"))?;

        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            let _ = client.relay_events();
            let _ = host.relay_events();
            if client
                .relay_status()
                .sessions
                .iter()
                .any(|status| status.id == session && status.control_state == "active")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !client
            .relay_status()
            .sessions
            .iter()
            .any(|status| status.id == session && status.control_state == "active")
        {
            return Err("the local relay control session did not become active".into());
        }

        stop_child(&mut child);
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            client.refresh().map_err(|error| error.to_string())?;
            let _ = client.relay_events();
            if !client.relay_devices_active()
                && client
                    .relay_status()
                    .sessions
                    .iter()
                    .any(|status| status.id == session && status.control_state == "active")
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err("target exit stopped the relay control session with its capture worker".into())
    })();
    stop_child(&mut child);
    result.expect("application relay control-session smoke test failed");
}

#[cfg(feature = "relay-tests")]
#[test]
fn ordinary_application_relay_keeps_local_session_audio_alive() {
    let generic_enabled = std::env::var("PW_GRAPH_TEST_APPLICATION_RELAY")
        .ok()
        .as_deref()
        == Some("1");
    let legacy_enabled = std::env::var("PW_GRAPH_TEST_BROWSER_RELAY").ok().as_deref() == Some("1");
    if !generic_enabled && !legacy_enabled {
        return;
    }
    let pid_text = std::env::var("PW_GRAPH_TEST_APPLICATION_PID")
        .or_else(|_| std::env::var("PW_GRAPH_TEST_BROWSER_PID"))
        .expect("PW_GRAPH_TEST_APPLICATION_PID is required for application relay smoke");
    let pid = pid_text
        .parse::<u32>()
        .expect("PW_GRAPH_TEST_APPLICATION_PID is numeric");
    let application_name = std::env::var("PW_GRAPH_TEST_APPLICATION_NAME")
        .or_else(|_| std::env::var("PW_GRAPH_TEST_BROWSER_NAME"))
        .unwrap_or_else(|_| "Mozilla Firefox".into());
    let result = (|| -> Result<(), String> {
        let identity = ProcessIdentity::from_pid(pid).map_err(|error| error.to_string())?;
        let expected_selector = identity
            .selector_key()
            .ok_or_else(|| "application process has no stable application selector".to_owned())?;
        let mut driver = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
        driver
            .set_meter_policy(MeterPolicy::OnDemand)
            .map_err(|error| error.to_string())?;
        let node = {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                driver.refresh().map_err(|error| error.to_string())?;
                if let Some(node) = driver
                    .graph()
                    .nodes
                    .values()
                    .find(|node| node.name.eq_ignore_ascii_case(&application_name))
                {
                    break node.id;
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "{application_name} audio session node did not appear"
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        let observe_native_peak = |driver: &mut WindowsAudioDriver| -> Result<f32, String> {
            let deadline = Instant::now() + Duration::from_secs(8);
            let mut peak = 0.0f32;
            while Instant::now() < deadline {
                driver.refresh().map_err(|error| error.to_string())?;
                if let Some(reading) = driver
                    .audio_meters()
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .find(|reading| reading.node_id == node && reading.available)
                {
                    peak = peak.max(reading.peak);
                    if peak > 0.01 {
                        return Ok(peak);
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(format!(
                "{application_name} native session meter stayed silent"
            ))
        };
        let selector = {
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                driver.refresh().map_err(|error| error.to_string())?;
                if let Some(source) = driver
                    .relay_send_sources()
                    .into_iter()
                    .find(|source| source.name.eq_ignore_ascii_case(&application_name))
                {
                    let selector = source
                        .id
                        .strip_prefix("application:")
                        .ok_or_else(|| {
                            "application source had an invalid application ID".to_owned()
                        })?
                        .to_owned();
                    if !selector.eq_ignore_ascii_case(&expected_selector) {
                        return Err(format!(
                            "application source selector {selector:?} did not match PID {pid} identity"
                        ));
                    }
                    break selector;
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "{application_name} was not listed as an application relay source"
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        driver
            .relay_set_send_source(RelaySendSource::Application(selector.clone()))
            .map_err(|error| error.to_string())?;
        let session = driver
            .relay_connect_mode(
                "127.0.0.1:9".parse().expect("discard target is valid"),
                "123456",
                RelayMode::Emitter,
                1,
            )
            .map_err(|error| error.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            driver.refresh().map_err(|error| error.to_string())?;
            if driver.relay_devices_active() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !driver.relay_devices_active() {
            return Err("ordinary application relay worker did not start".into());
        }
        driver
            .request_meters(&BTreeSet::from([node]))
            .map_err(|error| error.to_string())?;
        let during = observe_native_peak(&mut driver)?;
        println!(
            "ordinary application relay name={application_name} selector={selector} local_peak_during={during:.4}"
        );
        driver
            .relay_disconnect(session)
            .map_err(|error| error.to_string())?;
        Ok(())
    })();
    result.expect("ordinary application relay local-output smoke test failed");
}
