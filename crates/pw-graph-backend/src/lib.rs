//! Backend abstraction.
//!
//! The crate root is intentionally a small compatibility façade. Shared
//! contracts live in [`api`], the deterministic implementation lives in
//! [`demo`], and the native implementation is isolated in [`pipewire`].

mod api;
mod demo;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
mod pipewire;
#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
mod pipewire_stub;
/// The user-mode audio router.
///
/// Platform-neutral on purpose: it is the PCM ownership Windows needs before
/// arbitrary routing, routed effects, and RMS metering can be anything but
/// unsupported, and it is exercised by in-memory tests on every host.
pub mod router;

pub use api::*;
pub use demo::{DemoDriver, InMemoryDriver};

#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub use pipewire::PipewireDriver;
#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
pub use pipewire_stub::PipewireDriver;

#[cfg(target_os = "windows")]
mod windows;
/// WinMM MIDI: the one Windows backend with real routing.
#[cfg(target_os = "windows")]
mod windows_midi;
/// WASAPI endpoints that drive the relay engine on Windows.
#[cfg(all(target_os = "windows", feature = "relay"))]
mod windows_relay;

#[cfg(target_os = "windows")]
pub use windows::{
    classify_virtual_endpoint, AppRoutePolicy, AppRoutePolicySupport, AudioFlow, AudioRole,
    ProcessIdentity, ProcessLoopbackCapability, ProcessLoopbackMode, ProcessLoopbackSource,
    QpwVirtualEndpointRole, UnsupportedAppRoutePolicy, VirtualAudioDriverHealth,
    WindowsAudioDriver,
};
#[cfg(target_os = "windows")]
pub use windows_midi::WindowsMidiDriver;
#[cfg(all(target_os = "windows", feature = "relay"))]
pub use windows_relay::RelayEndpoints;

// The native driver and its focused submodules use these graph types in their
// internal implementation. Keep the imports at the façade boundary so those
// modules do not need to depend on the public API module's implementation
// details.
#[allow(unused_imports)]
pub(crate) use pw_graph_core::{
    decode_backend_local_id, encode_backend_id, BackendNamespace, Direction, Graph, GraphError,
    Link, LinkId, Node, NodeId, NodeType, Port, PortId, PortKey, PortType,
};

use std::collections::BTreeSet;

/// Used by patchbay activation to avoid reconnecting identical links.
pub fn existing_connections(driver: &dyn GraphDriver) -> BTreeSet<(PortId, PortId)> {
    driver
        .graph()
        .links
        .values()
        .map(|link| (link.output_port, link.input_port))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    use pw_graph_core::PortType;
    use pw_graph_core::{NodeType, PortId};
    use std::collections::BTreeMap;
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    use std::collections::BTreeSet;

    #[test]
    fn meter_policy_round_trips_and_defaults_safely() {
        for policy in MeterPolicy::ALL {
            assert_eq!(MeterPolicy::parse(policy.as_str()), policy);
        }
        assert_eq!(MeterPolicy::parse("OFF"), MeterPolicy::Disabled);
        assert_eq!(MeterPolicy::parse("all"), MeterPolicy::Always);
        // An unreadable or older config must not silently start metering
        // everything, so anything unrecognized lands on the default.
        assert_eq!(MeterPolicy::parse("nonsense"), MeterPolicy::default());
        assert_eq!(MeterPolicy::default(), MeterPolicy::OnDemand);
    }

    #[cfg(feature = "relay")]
    #[test]
    fn desktop_relay_roles_are_derived_from_direction() {
        assert_eq!(
            desktop_relay_client_roles(RelayDirection::MobileToDesktop),
            RelayRoles::receive_only()
        );
        assert_eq!(
            desktop_relay_client_roles(RelayDirection::DesktopToMobile),
            RelayRoles::emit_only()
        );
    }

    /// Regression: metering eligibility used to require an audio *source*
    /// port, so a playback sink -- speakers, headphones, any output device --
    /// was never measurable and silently showed no meter, even though the
    /// meter stream already knew how to read a sink through its monitor.
    #[test]
    fn playback_sinks_are_measurable_through_their_monitor() {
        // Speakers: input ports only, no source port anywhere.
        assert!(is_measurable_audio_node("Audio/Sink", false, true));
        // A capture device keeps working exactly as before.
        assert!(is_measurable_audio_node("Audio/Source", true, false));
        // So does an application playing audio.
        assert!(is_measurable_audio_node("Stream/Output/Audio", true, false));
    }

    #[test]
    fn nodes_without_measurable_audio_are_left_alone() {
        // A recording application is not a sink and has no source port, so
        // there is nothing to capture and no stream should be opened for it.
        assert!(!is_measurable_audio_node("Stream/Input/Audio", false, true));
        // Video and MIDI nodes report neither audio direction.
        assert!(!is_measurable_audio_node("Video/Sink", false, false));
        assert!(!is_measurable_audio_node("Midi/Bridge", false, false));
        // A sink with no audio ports at all is not measurable either.
        assert!(!is_measurable_audio_node("Audio/Sink", false, false));
    }

    #[test]
    fn playback_sink_detection_matches_the_meter_stream_rule() {
        // `create_meter_locked` sets `stream.capture.sink` on the same test,
        // so the two must agree or a meter would capture the wrong side.
        assert!(media_class_is_playback_sink("Audio/Sink"));
        assert!(media_class_is_playback_sink("audio/sink"));
        assert!(!media_class_is_playback_sink("Audio/Source"));
        assert!(!media_class_is_playback_sink("Stream/Output/Audio"));
        assert!(!media_class_is_playback_sink(""));
    }

    #[test]
    fn demo_backend_connects_and_disconnects() {
        let mut driver = DemoDriver::demo();
        let link = driver.connect(PortId(1), PortId(3)).unwrap();
        assert_eq!(driver.graph().links.len(), 1);
        driver.disconnect(link.id).unwrap();
        assert!(driver.graph().links.is_empty());
    }

    #[test]
    fn demo_backend_has_a_stable_graph_for_demo_runs() {
        let driver = DemoDriver::demo();
        assert_eq!(driver.graph().nodes.len(), 4);
        assert_eq!(driver.graph().ports.len(), 6);
        assert!(driver
            .graph()
            .nodes
            .values()
            .all(|node| node.node_type == NodeType::PipeWire));
    }

    #[test]
    fn demo_backend_inserts_and_removes_an_effect_transactionally() {
        let mut driver = DemoDriver::demo();
        driver.connect(PortId(1), PortId(3)).unwrap();
        let source = driver.graph().port_key(PortId(1)).unwrap();
        let destination = driver.graph().port_key(PortId(3)).unwrap();
        let instance = driver
            .insert_effect(EffectInsertRequest {
                instance_id: "test-effect".into(),
                effect_id: pw_graph_effects::NOISE_GATE_ID.into(),
                module_path: None,
                source,
                destination,
                enabled: true,
                parameters: BTreeMap::new(),
                position: [250.0, 180.0],
            })
            .unwrap();
        assert_eq!(driver.effect_instances().len(), 1);
        assert_eq!(
            driver.graph().nodes[&instance.node_id].node_type,
            NodeType::Effect
        );
        assert_eq!(driver.graph().links.len(), 2);
        driver.remove_effect("test-effect").unwrap();
        assert!(driver.effect_instances().is_empty());
        assert_eq!(driver.graph().links.len(), 1);
        assert_eq!(driver.graph().nodes.len(), 4);
    }

    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    #[test]
    fn native_backend_refreshes_running_pipewire_registry() {
        let Ok(mut driver) = PipewireDriver::new() else {
            // CI and development containers may not have a user PipeWire
            // daemon. The live test is exercised automatically when one is
            // available, but should not make offline builds fail.
            return;
        };
        let nodes = driver
            .refresh()
            .expect("PipeWire registry snapshot should succeed");
        assert!(!nodes.is_empty());
        assert!(!driver.graph().ports.is_empty());
    }

    /// Regression guard for the startup behaviour users actually noticed: the
    /// driver used to open a capture stream against every audio node as soon
    /// as the graph was first read, which resumed suspended devices and made
    /// the daemon renegotiate their format.
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    #[test]
    fn native_backend_meters_nothing_until_it_is_asked_to() {
        let Ok(mut driver) = PipewireDriver::new() else {
            return;
        };
        driver.refresh().expect("registry snapshot should succeed");
        assert_eq!(driver.active_meter_count(), 0);
        assert!(driver.audio_meters().unwrap().is_empty());
    }

    /// Opt-in: this one attaches a real (passive, monitor-flagged) stream to a
    /// node in the user's live session, so it is not part of a default run.
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    #[test]
    fn native_backend_attaches_and_releases_a_requested_meter() {
        if std::env::var_os("PW_GRAPH_TEST_METERS").is_none() {
            return;
        }
        let mut driver = PipewireDriver::new().expect("PipeWire daemon should be available");
        driver.refresh().expect("registry snapshot should succeed");
        let target = driver.graph().nodes.values().find(|node| {
            node.ports.iter().any(|port_id| {
                driver.graph().port(*port_id).is_some_and(|port| {
                    port.direction.is_source() && port.port_type == PortType::Audio
                })
            })
        });
        let Some(target) = target.map(|node| node.id) else {
            return;
        };

        driver
            .request_meters(&BTreeSet::from([target]))
            .expect("requesting a meter should succeed");
        assert_eq!(driver.active_meter_count(), 1);

        // Regression guard: `process` runs on PipeWire's realtime data thread,
        // which the thread-loop lock does not exclude. Reading meters from
        // this thread while that thread publishes used to hit `RefCell already
        // borrowed` inside a callback that cannot unwind, aborting the
        // process. Polling hard for a second reliably reproduced it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let mut polls = 0_u32;
        while std::time::Instant::now() < deadline {
            for meter in driver
                .audio_meters()
                .expect("reading meters should succeed")
            {
                assert!(meter.rms.is_finite() && (0.0..=1.0).contains(&meter.rms));
                assert!(meter.peak.is_finite() && (0.0..=1.0).contains(&meter.peak));
            }
            polls += 1;
        }
        assert!(polls > 0);

        driver
            .reset_audio_config()
            .expect("releasing meters should succeed");
        assert_eq!(driver.active_meter_count(), 0);
        assert!(driver.audio_meters().unwrap().is_empty());
    }

    /// Opt-in: opens real WASAPI endpoints on the default playback device and
    /// binds a TCP control port, so it is not part of a default run.
    ///
    /// `relay_start_host` only returns once both endpoints have reported that
    /// WASAPI accepted them, so a successful call proves the loopback capture
    /// client and the render client both opened -- which is the part that
    /// cannot be checked without real hardware.
    #[cfg(all(target_os = "windows", feature = "relay"))]
    #[test]
    fn windows_relay_hosts_through_wasapi_endpoints() {
        if std::env::var_os("PW_GRAPH_TEST_RELAY").is_none() {
            return;
        }
        let Ok(mut driver) = WindowsAudioDriver::new() else {
            return;
        };
        driver.refresh().expect("registry snapshot should succeed");
        assert!(driver.relay_available());
        assert!(!driver.relay_devices_active());

        let port = driver
            .relay_start_host(RelayHostRequest {
                device_id: "backend-test-id".into(),
                trusted_peers: Vec::new(),
                trust_new_peers: true,
                device_name: "qpwgraph-rs-test".into(),
                pin: "123456".into(),
                port: 0,
                codec: RelayCodecKind::Opus,
                frame_ms: 10,
                transport: Default::default(),
                direction: RelayDirection::MobileToDesktop,
                direction_generation: 0,
                mode: RelayMode::Receiver,
                mode_generation: 0,
            })
            .expect("relay host should start");
        assert!(port > 0, "an ephemeral control port should be bound");
        assert!(driver.relay_devices_active());

        let status = driver.relay_status();
        assert!(status.host_active);
        assert_eq!(status.host_port, Some(port));

        // Give the endpoint threads a moment to run so a COM or format failure
        // after start-up still shows up here.
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(driver.relay_status().host_active);

        driver.relay_stop_host().expect("relay host should stop");
        assert!(!driver.relay_status().host_active);
    }

    /// Regression: both public directions must prepare the Windows endpoint
    /// pair. The backend receives a direction, never an arbitrary role set,
    /// and derives the exact one-way role internally.
    #[cfg(all(target_os = "windows", feature = "relay"))]
    #[test]
    fn windows_relay_accepts_each_direction() {
        if std::env::var_os("PW_GRAPH_TEST_RELAY").is_none() {
            return;
        }
        let Ok(mut driver) = WindowsAudioDriver::new() else {
            return;
        };
        // Port 9 (discard) never completes a pairing, which is fine: the
        // assertion is that the role is accepted and the endpoints come up, not
        // that a peer answers.
        let target = "127.0.0.1:9".parse().expect("a valid address");

        for direction in [
            RelayDirection::MobileToDesktop,
            RelayDirection::DesktopToMobile,
        ] {
            driver
                .relay_connect(target, "123456", direction, 0)
                .expect("every relay direction is carried by the WASAPI endpoints");
            assert!(driver.relay_devices_active());
        }
    }

    /// The relay can be pointed at a chosen playback endpoint instead of
    /// always following the default, and switching it while hosting must not
    /// silently drop the host -- which it did before the restart was handled.
    #[cfg(all(target_os = "windows", feature = "relay"))]
    #[test]
    fn windows_relay_endpoints_are_selectable_and_survive_a_switch() {
        if std::env::var_os("PW_GRAPH_TEST_RELAY").is_none() {
            return;
        }
        let Ok(mut driver) = WindowsAudioDriver::new() else {
            return;
        };
        if driver.refresh().is_err() {
            return;
        }
        let choices = driver.relay_endpoint_choices();
        if choices.len() < 2 {
            // Selection needs somewhere to switch to.
            return;
        }
        assert_eq!(
            driver.relay_endpoints(),
            &RelayEndpoints::default(),
            "the relay follows the default endpoint until told otherwise"
        );

        let pick = |index: usize| RelayEndpoints {
            capture: Some(choices[index].0.clone()),
            playback: Some(choices[index].0.clone()),
        };
        driver
            .set_relay_endpoints(pick(1))
            .expect("a listed endpoint is selectable");
        let port = driver
            .relay_start_host(RelayHostRequest {
                device_id: "backend-test-id".into(),
                trusted_peers: Vec::new(),
                trust_new_peers: true,
                device_name: "qpwgraph-rs-test".into(),
                pin: "123456".into(),
                port: 0,
                codec: RelayCodecKind::Opus,
                frame_ms: 10,
                transport: Default::default(),
                direction: RelayDirection::MobileToDesktop,
                direction_generation: 0,
                mode: RelayMode::Receiver,
                mode_generation: 0,
            })
            .expect("hosting works on a non-default endpoint");
        assert!(port > 0);

        driver
            .set_relay_endpoints(pick(0))
            .expect("switching endpoints while hosting");
        let status = driver.relay_status();
        assert!(
            status.host_active,
            "the host must survive the endpoint switch"
        );
        assert!(status.host_port.is_some());

        driver.relay_stop_host().expect("relay host should stop");
    }

    /// Volume changed *outside* this process must reach the cached state
    /// through the Core Audio change callback, without a graph refresh.
    ///
    /// Opt-in: it moves the real system volume (and puts it back). The write
    /// goes through a separate `IAudioEndpointVolume` instance so the driver's
    /// own optimistic update cannot mask a broken callback.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_follows_external_volume_changes_without_a_refresh() {
        use ::windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
        use ::windows::Win32::Media::Audio::{
            eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator,
        };
        use ::windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
        };

        if std::env::var_os("PW_GRAPH_TEST_VOLUME").is_none() {
            return;
        }
        let Ok(mut driver) = WindowsAudioDriver::new() else {
            return;
        };
        if driver.refresh().is_err() {
            return;
        }
        // The default playback endpoint is the one the raw control below moves.
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let control = (|| -> Option<IAudioEndpointVolume> {
            let enumerator: IMMDeviceEnumerator =
                unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }.ok()?;
            let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }.ok()?;
            unsafe { device.Activate(CLSCTX_ALL, None) }.ok()
        })();
        let Some(control) = control else {
            unsafe { CoUninitialize() };
            return;
        };

        let original = unsafe { control.GetMasterVolumeLevelScalar() }.unwrap_or(1.0);
        let target = if original > 0.5 { 0.25 } else { 0.75 };
        let node = driver.graph().nodes.keys().copied().find(|node| {
            driver
                .node_audio_state(*node)
                .is_ok_and(|state| (state.volume.unwrap_or(-1.0) - original).abs() < 0.01)
                && driver.node_capabilities(*node).volume_write
        });
        let Some(node) = node else {
            unsafe { CoUninitialize() };
            return;
        };

        // Clear the dirty flag immediately before the write, so the check
        // below is about this volume change and not about whatever the system
        // did earlier. Other audio activity on the machine can still race it,
        // which is why the hardware tests want `--test-threads=1`.
        let _ = driver.refresh();
        unsafe { control.SetMasterVolumeLevelScalar(target, std::ptr::null()) }
            .expect("the external write should succeed");
        // The callback lands on a Core Audio notification thread.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut followed = false;
        while std::time::Instant::now() < deadline {
            let state = driver.node_audio_state(node).expect("state stays readable");
            if (state.volume.unwrap_or(-1.0) - target).abs() < 0.01 {
                followed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }

        let _ = unsafe { control.SetMasterVolumeLevelScalar(original, std::ptr::null()) };
        let dirty = driver.graph_dirty();
        unsafe { CoUninitialize() };

        assert!(followed, "an external volume change must reach the cache");
        assert!(
            !dirty,
            "a volume change must not mark the topology dirty: the graph did not change"
        );
    }

    /// Per-application metering, without process loopback.
    ///
    /// `IAudioMeterInformation` is documented as an endpoint facility, but a
    /// session control implements it as well -- which matters because process
    /// loopback capture, the other route to a per-app level, needs Windows
    /// build 20348. This works on any build that has sessions at all.
    ///
    /// Opt-in: it plays a tone through the default playback device, which
    /// creates the session it then measures.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_meters_a_single_application_through_its_session() {
        use ::windows::Win32::Media::Audio;
        use ::windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
        };
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        if std::env::var_os("PW_GRAPH_TEST_METERS").is_none() {
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let playing = Arc::clone(&stop);
        // A full-scale-ish sine, so the expected peak is unambiguous.
        const AMPLITUDE: f32 = 0.4;
        let tone = std::thread::spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let run = || -> ::windows::core::Result<()> {
                let enumerator: Audio::IMMDeviceEnumerator =
                    CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL)?;
                let device = enumerator.GetDefaultAudioEndpoint(Audio::eRender, Audio::eConsole)?;
                let client: Audio::IAudioClient = device.Activate(CLSCTX_ALL, None)?;
                let format = client.GetMixFormat()?;
                client.Initialize(
                    Audio::AUDCLNT_SHAREMODE_SHARED,
                    Default::default(),
                    2_000_000,
                    0,
                    format,
                    None,
                )?;
                let render: Audio::IAudioRenderClient = client.GetService()?;
                let buffer_frames = client.GetBufferSize()?;
                let channels = (*format).nChannels as usize;
                let rate = (*format).nSamplesPerSec as f32;
                client.Start()?;
                let mut phase = 0.0f32;
                while !playing.load(Ordering::Acquire) {
                    let available =
                        buffer_frames.saturating_sub(client.GetCurrentPadding().unwrap_or(0));
                    if available > 0 {
                        let data = render.GetBuffer(available)?;
                        let out = std::slice::from_raw_parts_mut(
                            data.cast::<f32>(),
                            available as usize * channels,
                        );
                        for frame in 0..available as usize {
                            phase += 440.0 * std::f32::consts::TAU / rate;
                            let value = phase.sin() * AMPLITUDE;
                            for channel in 0..channels {
                                out[frame * channels + channel] = value;
                            }
                        }
                        let _ = render.ReleaseBuffer(available, 0);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                let _ = client.Stop();
                Ok(())
            };
            let _ = run();
            CoUninitialize();
        });
        std::thread::sleep(std::time::Duration::from_millis(700));

        let measured = (|| {
            let mut driver = WindowsAudioDriver::new().ok()?;
            driver.refresh().ok()?;
            let sessions: BTreeSet<NodeId> = driver
                .graph()
                .nodes
                .values()
                .filter(|node| node.node_type == NodeType::WindowsAudioSession)
                .map(|node| node.id)
                .collect();
            if sessions.is_empty() {
                return None;
            }
            // Every session reports meter capability, and at least one of them
            // is the tone.
            assert!(sessions
                .iter()
                .all(|node| driver.node_capabilities(*node).meter_peak));
            driver.set_meter_policy(MeterPolicy::Always).ok()?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                let peak = driver
                    .audio_meters()
                    .ok()?
                    .into_iter()
                    .filter(|meter| sessions.contains(&meter.node_id))
                    .map(|meter| meter.peak)
                    .fold(0.0f32, f32::max);
                if peak > 0.05 {
                    return Some(peak);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Some(0.0)
        })();

        stop.store(true, Ordering::Release);
        let _ = tone.join();

        let Some(peak) = measured else {
            return;
        };
        assert!(
            (peak - AMPLITUDE).abs() < 0.05,
            "the playing application's session should meter about {AMPLITUDE}, got {peak}"
        );
    }

    /// `IAudioMeterInformation` is an endpoint peak meter with no RMS reading,
    /// and `audio_meters` reports `rms: 0.0`. Claiming RMS capability would
    /// make the UI draw a permanently silent RMS bar beside a working peak one.
    #[cfg(target_os = "windows")]
    #[test]
    fn windows_endpoints_report_peak_metering_but_not_rms() {
        let Ok(mut driver) = WindowsAudioDriver::new() else {
            return;
        };
        if driver.refresh().is_err() {
            return;
        }
        let metered = driver
            .graph()
            .nodes
            .keys()
            .map(|node_id| driver.node_capabilities(*node_id))
            .filter(|capabilities| capabilities.has_any_meter())
            .collect::<Vec<_>>();
        if metered.is_empty() {
            return;
        }
        assert!(metered.iter().all(|capabilities| capabilities.meter_peak));
        assert!(
            metered.iter().all(|capabilities| !capabilities.meter_rms),
            "Core Audio exposes no endpoint RMS"
        );
    }

    #[cfg(all(target_os = "linux", feature = "pipewire", feature = "relay"))]
    #[test]
    fn native_backend_creates_and_removes_relay_devices_when_enabled() {
        if std::env::var_os("PW_GRAPH_TEST_RELAY").is_none() {
            return;
        }
        let mut driver = PipewireDriver::new().expect("PipeWire daemon should be available");
        driver.refresh().expect("registry snapshot should succeed");

        let port = driver
            .relay_start_host(RelayHostRequest {
                device_id: "backend-test-id".into(),
                trusted_peers: Vec::new(),
                trust_new_peers: true,
                device_name: "qpwgraph-rs-test".into(),
                pin: "123456".into(),
                port: 0,
                codec: RelayCodecKind::Opus,
                frame_ms: 20,
                transport: Default::default(),
                direction: RelayDirection::MobileToDesktop,
                direction_generation: 0,
                mode: RelayMode::Receiver,
                mode_generation: 0,
            })
            .expect("relay host should start");
        assert!(port > 0);
        assert!(driver.relay_devices_active());
        assert!(driver.relay_status().host_active);

        // The two virtual nodes must be visible in the graph with ports.
        driver.refresh().expect("registry snapshot should succeed");
        let names: Vec<String> = driver
            .graph()
            .nodes
            .values()
            .map(|node| node.name.clone())
            .collect();
        assert!(
            names.iter().any(|name| name.contains("relay.source")),
            "relay microphone node should appear in the graph, got: {names:?}"
        );
        assert!(
            names.iter().any(|name| name.contains("relay.sink")),
            "relay speaker node should appear in the graph, got: {names:?}"
        );

        // `<role>_<channel>`: the canvas groups a stereo pair into one pin by
        // splitting the channel suffix off a base name, so bare "FL"/"FR"
        // would leave these cards ungrouped in Easy mode.
        let port_names: Vec<String> = driver
            .graph()
            .ports
            .values()
            .map(|port| port.name.clone())
            .collect();
        for expected in ["capture_FL", "capture_FR", "playback_FL", "playback_FR"] {
            assert!(
                port_names.iter().any(|name| name == expected),
                "relay device should expose {expected}, got: {port_names:?}"
            );
        }

        driver.relay_stop_host().expect("relay host should stop");
        assert!(!driver.relay_status().host_active);
    }

    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    #[test]
    fn native_backend_can_create_and_destroy_a_link_when_enabled() {
        if std::env::var_os("PW_GRAPH_TEST_LINKS").is_none() {
            return;
        }
        let mut driver = PipewireDriver::new().expect("PipeWire daemon should be available");
        driver
            .refresh()
            .expect("PipeWire registry snapshot should succeed");
        let existing = existing_connections(&driver);
        let pair = driver.graph().ports.values().find_map(|output| {
            if !output.direction.is_source() {
                return None;
            }
            driver.graph().ports.values().find_map(|input| {
                if !input.direction.is_sink()
                    || (output.port_type != input.port_type
                        && output.port_type != PortType::Unknown
                        && input.port_type != PortType::Unknown)
                    || existing.contains(&(output.id, input.id))
                {
                    return None;
                }
                Some((output.id, input.id))
            })
        });
        let Some((output, input)) = pair else {
            return;
        };
        let link = driver
            .connect(output, input)
            .expect("PipeWire link creation should succeed");
        assert!(driver.graph().link(link.id).is_some());
        driver
            .disconnect(link.id)
            .expect("PipeWire link destruction should succeed");
    }
}
