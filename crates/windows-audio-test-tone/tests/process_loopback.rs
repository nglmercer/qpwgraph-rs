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
#[cfg(feature = "relay-tests")]
use pw_graph_backend::{
    RelayCodecKind, RelayDirection, RelayDriver, RelayHostRequest, RelayMode, RelaySendSource,
    RelayTransportPreference,
};
use std::collections::BTreeSet;
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
