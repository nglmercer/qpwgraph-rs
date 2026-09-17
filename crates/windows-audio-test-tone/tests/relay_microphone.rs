//! Opt-in end-to-end check for the optional Windows Relay Microphone.
//!
//! This test uses an ordinary shared-mode WASAPI capture client, rather than
//! the QPWGraph backend's internal worker. It therefore exercises the public
//! driver cable: peer PCM is rendered to `relay-render` and must be visible on
//! the `relay-capture` endpoint that a normal Windows application can open.
//!
//! The test is deliberately opt-in. Set `PW_GRAPH_TEST_RELAY_MICROPHONE=1` on
//! a Windows host with the signed QPWGraph audio package installed.
//! Set `PW_GRAPH_TEST_RELAY_MICROPHONE_CYCLES=3` (bounded to 1..=8) to repeat
//! the authenticated disconnect/silence/reconnect cycle; the default is one
//! cycle for a short smoke test.
//! Additional opt-in probes cover local-output preservation during ordinary
//! app relay, isolated-application effects and bypass, and persisted route
//! rebind after the helper restarts with a new PID. The automatic policy probe
//! additionally starts the helper on its ordinary default endpoint, verifies
//! the three-role persisted route transaction, and checks restoration. A
//! separate opt-in probe leaves that lease active while the backend is dropped
//! to verify qpwgraph-shutdown restoration.

#![cfg(target_os = "windows")]

#[cfg(feature = "relay-tests")]
mod live {
    use pw_graph_backend::{
        AppRoutePolicy, AudioFlow, AudioRole, EffectCreateRequest, EffectDriver, EffectEvent,
        EffectTarget, GraphDriver, ProcessIdentity, RelayCodecKind, RelayDirection, RelayDriver,
        RelayHostRequest, RelayMode, RelayReceiveSink, RelaySendSource, RelayTransportPreference,
        VerifiedAudioPolicyConfig, WindowsAudioDriver,
    };
    use pw_graph_config::{AppConfig, WindowsApplicationRoute};
    use std::collections::BTreeMap;
    use std::ffi::c_void;
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};
    use windows::core::GUID;
    use windows::Win32::Media::{Audio, KernelStreaming, Multimedia};
    use windows::Win32::System::Com::{
        self, CoCreateInstance, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
    };
    use windows::Win32::System::Variant::VT_LPWSTR;
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::Win32::{Devices::Properties, Foundation::PROPERTYKEY};

    const ROLE_KEY: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0x3c8e_8ef9_1f7f_4fcb_9c36_4a7e_19f3_6d12),
        pid: 2,
    };
    const RELAY_CAPTURE_ROLE: &str = "relay-capture";
    const APP_RENDER_ROLE: &str = "app-render";
    const TONE_HZ: [f64; 2] = [1_000.0, 2_000.0];

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Flow {
        Render,
        Capture,
    }

    impl Flow {
        fn data_flow(self) -> Audio::EDataFlow {
            match self {
                Self::Render => Audio::eRender,
                Self::Capture => Audio::eCapture,
            }
        }

        fn name(self) -> &'static str {
            match self {
                Self::Render => "render",
                Self::Capture => "capture",
            }
        }
    }

    #[derive(Clone)]
    struct Endpoint {
        device: Audio::IMMDevice,
        id: String,
        name: String,
    }

    struct AudioStream {
        client: Audio::IAudioClient,
        format: *mut Audio::WAVEFORMATEX,
        buffer_frames: u32,
        sample_rate: u32,
        channels: u16,
        bits: u16,
        block_align: u16,
        is_float: bool,
    }

    // The COM interfaces are apartment-bound, but this test uses all of them
    // on the thread that called CoInitializeEx. Keeping the stream !Send also
    // prevents accidental movement into the relay worker.
    impl std::fmt::Debug for AudioStream {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("AudioStream")
                .field("sample_rate", &self.sample_rate)
                .field("channels", &self.channels)
                .field("bits", &self.bits)
                .finish_non_exhaustive()
        }
    }

    impl Drop for AudioStream {
        fn drop(&mut self) {
            unsafe {
                let _ = self.client.Stop();
                let _ = self.client.Reset();
                Com::CoTaskMemFree(Some(self.format.cast()));
            }
        }
    }

    #[derive(Debug)]
    struct ToneProbe {
        sample_rate: u32,
        sample_count: u64,
        sum_cos: [f64; 2],
        sum_sin: [f64; 2],
        peak: f32,
        invalid_samples: u64,
    }

    impl ToneProbe {
        fn new(sample_rate: u32) -> Self {
            Self {
                sample_rate,
                sample_count: 0,
                sum_cos: [0.0; 2],
                sum_sin: [0.0; 2],
                peak: 0.0,
                invalid_samples: 0,
            }
        }

        fn push(&mut self, sample: f32) {
            if !sample.is_finite() {
                self.invalid_samples += 1;
                return;
            }
            self.peak = self.peak.max(sample.abs());
            let index = self.sample_count as f64;
            for (tone, frequency) in TONE_HZ.into_iter().enumerate() {
                let angle = std::f64::consts::TAU * frequency * index / f64::from(self.sample_rate);
                self.sum_cos[tone] += f64::from(sample) * angle.cos();
                self.sum_sin[tone] += f64::from(sample) * angle.sin();
            }
            self.sample_count += 1;
        }

        fn amplitude(&self, tone: usize) -> f32 {
            if self.sample_count == 0 {
                return 0.0;
            }
            (2.0 * (self.sum_cos[tone]
                .mul_add(self.sum_cos[tone], self.sum_sin[tone] * self.sum_sin[tone]))
            .sqrt()
                / self.sample_count as f64) as f32
        }
    }

    #[test]
    fn peer_audio_reaches_ordinary_relay_microphone_client() {
        if std::env::var("PW_GRAPH_TEST_RELAY_MICROPHONE")
            .ok()
            .as_deref()
            != Some("1")
        {
            return;
        }
        let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_err() {
            panic!("could not initialize COM: {initialized:?}");
        }
        let result = run_test();
        unsafe { Com::CoUninitialize() };
        result.expect("Relay Microphone WASAPI round-trip failed");
    }

    #[test]
    fn isolated_application_effect_applies_and_bypass_restores_audio() {
        if std::env::var("PW_GRAPH_TEST_WINDOWS_EFFECTS")
            .ok()
            .as_deref()
            != Some("1")
        {
            return;
        }
        let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_err() {
            panic!("could not initialize COM: {initialized:?}");
        }
        let result = run_effect_test();
        unsafe { Com::CoUninitialize() };
        result.expect("Windows isolated-application effect probe failed");
    }

    #[test]
    fn ordinary_application_relay_preserves_local_output() {
        if std::env::var("PW_GRAPH_TEST_RELAY_LOCAL_OUTPUT")
            .ok()
            .as_deref()
            != Some("1")
        {
            return;
        }
        let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_err() {
            panic!("could not initialize COM: {initialized:?}");
        }
        let result = run_local_output_test();
        unsafe { Com::CoUninitialize() };
        result.expect("ordinary application relay changed local output");
    }

    #[test]
    fn isolated_application_route_rebinds_after_helper_restart() {
        if std::env::var("PW_GRAPH_TEST_WINDOWS_APP_ROUTE_RESTART")
            .ok()
            .as_deref()
            != Some("1")
        {
            return;
        }
        let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_err() {
            panic!("could not initialize COM: {initialized:?}");
        }
        let result = run_application_route_restart_test();
        unsafe { Com::CoUninitialize() };
        result.expect("isolated application route did not survive helper restart");
    }

    #[test]
    fn experimental_application_route_rebinds_default_helper_and_restores() {
        if std::env::var("PW_GRAPH_TEST_WINDOWS_AUTO_APP_ROUTE")
            .ok()
            .as_deref()
            != Some("1")
        {
            return;
        }
        let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_err() {
            panic!("could not initialize COM: {initialized:?}");
        }
        let result = run_automatic_application_route_test();
        unsafe { Com::CoUninitialize() };
        result.expect("experimental automatic application route failed");
    }

    #[test]
    fn experimental_application_route_restores_on_driver_shutdown() {
        if std::env::var("PW_GRAPH_TEST_WINDOWS_AUTO_APP_ROUTE_DROP")
            .ok()
            .as_deref()
            != Some("1")
        {
            return;
        }
        let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_err() {
            panic!("could not initialize COM: {initialized:?}");
        }
        let result = run_automatic_application_route_shutdown_test();
        unsafe { Com::CoUninitialize() };
        result.expect("experimental automatic application route was not restored on shutdown");
    }

    #[test]
    fn manual_override_of_automatic_route_is_preserved() {
        if std::env::var("PW_GRAPH_TEST_WINDOWS_APP_ROUTE_OVERRIDE")
            .ok()
            .as_deref()
            != Some("1")
        {
            return;
        }
        let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_err() {
            panic!("could not initialize COM: {initialized:?}");
        }
        let result = run_manual_override_test();
        unsafe { Com::CoUninitialize() };
        result.expect("manual override of automatic route was not preserved");
    }

    #[test]
    fn backend_restart_reconciles_automatic_route_safely() {
        if std::env::var("PW_GRAPH_TEST_WINDOWS_BACKEND_RESTART")
            .ok()
            .as_deref()
            != Some("1")
        {
            return;
        }
        let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_err() {
            panic!("could not initialize COM: {initialized:?}");
        }
        let result = run_backend_restart_test();
        unsafe { Com::CoUninitialize() };
        result.expect("backend restart did not reconcile the automatic route safely");
    }

    #[test]
    fn backend_crash_during_active_stream_recovers() {
        if std::env::var("PW_GRAPH_TEST_WINDOWS_BACKEND_CRASH")
            .ok()
            .as_deref()
            != Some("1")
        {
            return;
        }
        let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_err() {
            panic!("could not initialize COM: {initialized:?}");
        }
        let result = run_backend_crash_test();
        unsafe { Com::CoUninitialize() };
        result.expect("backend crash during active stream did not recover");
    }

    #[test]
    fn gui_crash_during_active_stream_recovers() {
        if std::env::var("PW_GRAPH_TEST_WINDOWS_GUI_CRASH")
            .ok()
            .as_deref()
            != Some("1")
        {
            return;
        }
        let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_err() {
            panic!("could not initialize COM: {initialized:?}");
        }
        let result = run_gui_crash_test();
        unsafe { Com::CoUninitialize() };
        result.expect("GUI crash during active stream did not recover");
    }

    #[test]
    fn physical_destination_churn_restores_route() {
        if std::env::var("PW_GRAPH_TEST_WINDOWS_ENDPOINT_CHURN")
            .ok()
            .as_deref()
            != Some("1")
        {
            return;
        }
        let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_err() {
            panic!("could not initialize COM: {initialized:?}");
        }
        let result = run_endpoint_churn_test();
        unsafe { Com::CoUninitialize() };
        result.expect("physical destination churn did not restore the route");
    }

    #[test]
    fn packaged_msix_application_route_rebinds_and_restores() {
        if std::env::var("PW_GRAPH_TEST_MSIX_APP_ROUTE")
            .ok()
            .as_deref()
            != Some("1")
        {
            return;
        }
        let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_err() {
            panic!("could not initialize COM: {initialized:?}");
        }
        let result = run_packaged_msix_route_test();
        unsafe { Com::CoUninitialize() };
        result.expect("packaged MSIX application route failed");
    }

    const MSIX_PACKAGE_FAMILY_PREFIX: &str = "QPWGraph.TestTone_";
    const MSIX_ALIAS_FILE_NAME: &str = "QPWGraphTestTone.exe";
    const MSIX_HELPER_EXE_NAME: &str = "windows-audio-test-tone.exe";
    // The family hash is pinned by the manifest Publisher "CN=QPWGraph Test";
    // changing the Publisher re-hashes it and must update this AUMID too.
    const MSIX_HELPER_AUMID: &str = "QPWGraph.TestTone_0aet1w1jqgqs2!Tone";

    fn packaged_helper_alias() -> Result<std::path::PathBuf, String> {
        let local_app_data =
            std::env::var_os("LOCALAPPDATA").ok_or_else(|| "LOCALAPPDATA is not set".to_owned())?;
        let alias = std::path::Path::new(&local_app_data)
            .join("Microsoft")
            .join("WindowsApps")
            .join(MSIX_ALIAS_FILE_NAME);
        if !alias.is_file() {
            return Err(format!(
                "packaged tone helper is not installed (run crates/windows-audio-test-tone/msix/build-test-msix.ps1): {}",
                alias.display()
            ));
        }
        Ok(alias)
    }

    struct PackagedTone {
        pid: u32,
    }

    /// Activate the packaged helper through the documented activation
    /// manager (the programmatic equivalent of an AppsFolder launch) and
    /// return its real PID. The execution alias is deliberately not used:
    /// its stub reports E_APPLICATION_ACTIVATION_EXEC_FAILURE and its image
    /// resolves to the same packaged executable, so alias spawns race a
    /// short-lived lookalike process.
    fn spawn_packaged_helper(
        exclude: &[u32],
        reopen_after_ms: Option<u64>,
    ) -> Result<PackagedTone, String> {
        use windows::Win32::UI::Shell::{
            ApplicationActivationManager, IApplicationActivationManager, AO_NONE,
        };

        // Three minutes covers cold-start activation plus the full flow; the
        // helper is killed explicitly on every completion path.
        let mut arguments = String::from("--duration-ms 180000 --frequency 1000 --amplitude 0.25");
        if let Some(reopen_after_ms) = reopen_after_ms {
            arguments.push_str(&format!(" --reopen-after-ms {reopen_after_ms}"));
        }
        let manager: IApplicationActivationManager =
            unsafe { CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_ALL) }
                .map_err(|error| format!("create activation manager: {error}"))?;
        unsafe {
            manager
                .ActivateApplication(
                    &windows::core::HSTRING::from(MSIX_HELPER_AUMID),
                    &windows::core::HSTRING::from(arguments.as_str()),
                    AO_NONE,
                )
                .map_err(|error| {
                    format!("activate packaged tone helper {MSIX_HELPER_AUMID}: {error}")
                })?;
        }
        // The activation call reports no PID, so discover the new process by
        // its package identity, excluding anything already running.
        let pid = wait_for_packaged_tone(exclude, Duration::from_secs(30))?;
        Ok(PackagedTone { pid })
    }

    /// Single sweep for live packaged helpers, used to exclude strays from an
    /// earlier run before spawning.
    fn packaged_tone_pids_now() -> Vec<u32> {
        use windows::Win32::System::ProcessStatus::EnumProcesses;

        let mut pids = vec![0u32; 4096];
        let mut returned = 0u32;
        let bytes = (pids.len() * size_of::<u32>()) as u32;
        if unsafe { EnumProcesses(pids.as_mut_ptr(), bytes, &mut returned) }.is_err() {
            return Vec::new();
        }
        let count = returned as usize / size_of::<u32>();
        pids.iter()
            .take(count)
            .copied()
            .filter(|pid| *pid != 0 && is_packaged_tone(*pid))
            .collect()
    }

    fn is_packaged_tone(pid: u32) -> bool {
        let Ok(identity) = ProcessIdentity::from_pid(pid) else {
            return false;
        };
        identity
            .package_family_name
            .as_deref()
            .is_some_and(|family| family.starts_with(MSIX_PACKAGE_FAMILY_PREFIX))
            && identity.executable_name.as_deref() == Some(MSIX_HELPER_EXE_NAME)
    }

    fn wait_for_packaged_tone(exclude: &[u32], timeout: Duration) -> Result<u32, String> {
        let deadline = Instant::now() + timeout;
        loop {
            for pid in packaged_tone_pids_now() {
                if exclude.contains(&pid) {
                    continue;
                }
                // A candidate that vanishes within the settle window is a
                // startup transient, not the helper; keep polling.
                std::thread::sleep(Duration::from_millis(1500));
                if is_packaged_tone(pid) {
                    return Ok(pid);
                }
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "packaged tone helper (family {MSIX_PACKAGE_FAMILY_PREFIX}*) did not appear within {} s",
                    timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn terminate_process_by_pid(pid: u32) {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

        let Ok(process) = (unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) }) else {
            return;
        };
        let _ = unsafe { TerminateProcess(process, 1) };
        let _ = unsafe { CloseHandle(process) };
    }

    fn stop_packaged_tone(packaged: &PackagedTone) {
        terminate_process_by_pid(packaged.pid);
    }

    fn sweep_packaged_tones() {
        for pid in packaged_tone_pids_now() {
            terminate_process_by_pid(pid);
        }
    }

    /// The packaged helper's audio session appears a moment after the process
    /// does; GetPersistedDefaultAudioEndpoint reports E_INVALIDARG until then.
    /// Retry a bounded wait, then surface the last error unchanged.
    fn read_original_policy_endpoints(
        policy: &VerifiedAudioPolicyConfig,
        process: &ProcessIdentity,
        roles: [AudioRole; 3],
        timeout: Duration,
    ) -> Result<BTreeMap<AudioRole, Option<String>>, String> {
        let deadline = Instant::now() + timeout;
        loop {
            match read_policy_endpoints(policy, process, roles) {
                Ok(endpoints) => return Ok(endpoints),
                Err(error) => {
                    if Instant::now() >= deadline {
                        return Err(error);
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
    }

    fn run_packaged_msix_route_test() -> Result<(), String> {
        let _alias = packaged_helper_alias()?;
        let enumerator: Audio::IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| format!("create MMDeviceEnumerator: {error}"))?;
        let app_render = wait_for_role(
            &enumerator,
            Flow::Render,
            APP_RENDER_ROLE,
            Duration::from_secs(8),
        )?;
        let physical_render = choose_external_render(&enumerator)?;
        let loopback = open_loopback_stream(&physical_render.device)?;
        let loopback_client = unsafe { loopback.client.GetService::<Audio::IAudioCaptureClient>() }
            .map_err(|error| format!("get physical render loopback service: {error}"))?;
        unsafe {
            loopback
                .client
                .Start()
                .map_err(|error| format!("start physical render loopback: {error}"))?;
        }

        let strays = packaged_tone_pids_now();
        if !strays.is_empty() {
            println!(
                "ignoring {} stray packaged helper(s): {strays:?}",
                strays.len()
            );
        }
        let mut packaged = spawn_packaged_helper(&strays, None)?;
        let mut driver = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
        driver.set_experimental_app_routing(true);
        let policy = VerifiedAudioPolicyConfig::new(true);
        let original_process = ProcessIdentity::from_pid(packaged.pid)
            .map_err(|error| format!("read packaged helper identity: {error}"))?;
        println!(
            "packaged route subject: pid={} family={:?} aumid={:?}",
            packaged.pid, original_process.package_family_name, original_process.app_user_model_id
        );
        if !original_process.is_stable() {
            stop_packaged_tone(&packaged);
            return Err("packaged helper identity is not stable".into());
        }
        let roles = [
            AudioRole::Console,
            AudioRole::Multimedia,
            AudioRole::Communications,
        ];
        let original = read_original_policy_endpoints(
            &policy,
            &original_process,
            roles,
            Duration::from_secs(10),
        )?;
        let route = WindowsApplicationRoute {
            application: original_process.application_selector(),
            destination_mmdevice_id: Some(physical_render.id.clone()),
            virtualization_required: true,
            gain: 1.0,
            enabled: true,
            ..WindowsApplicationRoute::default()
        };

        let result = (|| -> Result<(), String> {
            let plans = driver
                .reconcile_application_routes(vec![route])
                .map_err(|error| format!("install packaged application route: {error}"))?;
            println!(
                "packaged application route initial plan: {plans:?}; original_endpoints={original:?}"
            );
            let expected_virtual: BTreeMap<_, _> = roles
                .iter()
                .map(|role| (*role, Some(app_render.id.clone())))
                .collect();
            wait_for_policy_endpoints(
                &mut driver,
                &policy,
                &original_process,
                &roles,
                &expected_virtual,
                Duration::from_secs(12),
            )?;

            // A running WASAPI client can keep its original endpoint. Start a
            // fresh packaged process only after the policy transaction has
            // succeeded; its ordinary default open must now land on AppRender.
            let mut excluded = strays.clone();
            excluded.push(packaged.pid);
            let replacement = spawn_packaged_helper(&excluded, Some(5_000))?;
            stop_packaged_tone(&packaged);
            packaged = replacement;
            wait_for_application_route(&mut driver, true, Duration::from_secs(15))?;
            let after_restart =
                observe_loopback_tone(&loopback, &loopback_client, Duration::from_secs(2))?;
            if after_restart < 0.01 {
                return Err(format!(
                    "packaged route restart remained silent: amplitude={after_restart:.4}"
                ));
            }
            let replacement_process = ProcessIdentity::from_pid(packaged.pid)
                .map_err(|error| format!("read replacement packaged identity: {error}"))?;
            let current = read_policy_endpoints(&policy, &replacement_process, roles)?;
            if !policy_endpoints_match(&expected_virtual, &current) {
                return Err(format!(
                    "replacement packaged policy did not remain on AppRender: expected={expected_virtual:?}, actual={current:?}"
                ));
            }
            println!(
                "packaged application route after restart: 1 kHz amplitude {after_restart:.4}; current_endpoints={current:?}"
            );
            Ok(())
        })();

        // Removing the rule is part of the test, not just teardown: it must
        // restore the exact role-specific values captured before activation.
        let cleanup = (|| -> Result<(), String> {
            driver
                .reconcile_application_routes(Vec::new())
                .map_err(|error| format!("remove packaged application route: {error}"))?;
            let current_process = ProcessIdentity::from_pid(packaged.pid)
                .map_err(|error| format!("read cleanup packaged identity: {error}"))?;
            wait_for_policy_endpoints(
                &mut driver,
                &policy,
                &current_process,
                &roles,
                &original,
                Duration::from_secs(12),
            )
        })();
        stop_packaged_tone(&packaged);
        sweep_packaged_tones();

        match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(test), Ok(())) => Err(test),
            (Ok(()), Err(cleanup)) => Err(cleanup),
            (Err(test), Err(cleanup)) => Err(format!("{test}; cleanup also failed: {cleanup}")),
        }
    }

    fn run_application_route_restart_test() -> Result<(), String> {
        let helper = std::env::var_os("CARGO_BIN_EXE_windows-audio-test-tone")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "Cargo did not provide the tone helper executable".to_owned())?;
        let enumerator: Audio::IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| format!("create MMDeviceEnumerator: {error}"))?;
        let app_render = wait_for_role(
            &enumerator,
            Flow::Render,
            APP_RENDER_ROLE,
            Duration::from_secs(8),
        )?;
        let physical_render = choose_external_render(&enumerator)?;
        let loopback = open_loopback_stream(&physical_render.device)?;
        let loopback_client = unsafe { loopback.client.GetService::<Audio::IAudioCaptureClient>() }
            .map_err(|error| format!("get physical render loopback service: {error}"))?;
        unsafe {
            loopback
                .client
                .Start()
                .map_err(|error| format!("start physical render loopback: {error}"))?;
        }

        let mut tone = spawn_isolated_helper(&helper, &app_render.id)?;
        let result = (|| -> Result<(), String> {
            let mut driver = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
            let selector = {
                let deadline = Instant::now() + Duration::from_secs(10);
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
                            .ok_or_else(|| {
                                "helper application source had an invalid ID".to_owned()
                            })?
                            .to_owned();
                    }
                    if Instant::now() >= deadline {
                        return Err("helper application source did not appear".into());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            };
            let identity = ProcessIdentity::from_pid(tone.id())
                .map_err(|error| format!("read helper identity: {error}"))?;
            let route = WindowsApplicationRoute {
                application: identity.application_selector(),
                destination_mmdevice_id: Some(physical_render.id.clone()),
                virtualization_required: true,
                gain: 1.0,
                enabled: true,
                ..WindowsApplicationRoute::default()
            };
            let plans = driver
                .reconcile_application_routes(vec![route.clone()])
                .map_err(|error| format!("install application route: {error}"))?;
            println!("application route initial plan: {plans:?}");
            wait_for_application_route(&mut driver, true, Duration::from_secs(12))?;
            let first = observe_loopback_tone(&loopback, &loopback_client, Duration::from_secs(2))?;
            println!("application route before restart: 1 kHz amplitude {first:.4}");

            stop_child(&mut tone);
            wait_for_application_route(&mut driver, false, Duration::from_secs(10))?;
            tone = spawn_isolated_helper(&helper, &app_render.id)?;
            wait_for_application_route(&mut driver, true, Duration::from_secs(12))?;
            let second =
                observe_loopback_tone(&loopback, &loopback_client, Duration::from_secs(2))?;
            if second < first * 0.5 || second < 0.01 {
                return Err(format!(
                    "application route did not restore a non-silent tone after restart: before={first:.4}, after={second:.4}"
                ));
            }
            println!(
                "application route after restart: 1 kHz amplitude {second:.4}; selector={selector}"
            );
            Ok(())
        })();
        stop_child(&mut tone);
        result
    }

    fn run_automatic_application_route_test() -> Result<(), String> {
        let helper = std::env::var_os("CARGO_BIN_EXE_windows-audio-test-tone")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "Cargo did not provide the tone helper executable".to_owned())?;
        let enumerator: Audio::IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| format!("create MMDeviceEnumerator: {error}"))?;
        let app_render = wait_for_role(
            &enumerator,
            Flow::Render,
            APP_RENDER_ROLE,
            Duration::from_secs(8),
        )?;
        let physical_render = choose_external_render(&enumerator)?;
        let loopback = open_loopback_stream(&physical_render.device)?;
        let loopback_client = unsafe { loopback.client.GetService::<Audio::IAudioCaptureClient>() }
            .map_err(|error| format!("get physical render loopback service: {error}"))?;
        unsafe {
            loopback
                .client
                .Start()
                .map_err(|error| format!("start physical render loopback: {error}"))?;
        }

        let mut tone = spawn_default_helper(&helper)?;
        let mut driver = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
        driver.set_experimental_app_routing(true);
        let policy = VerifiedAudioPolicyConfig::new(true);
        let original_process = ProcessIdentity::from_pid(tone.id())
            .map_err(|error| format!("read helper identity: {error}"))?;
        let roles = [
            AudioRole::Console,
            AudioRole::Multimedia,
            AudioRole::Communications,
        ];
        let original = read_policy_endpoints(&policy, &original_process, roles)?;
        let route = WindowsApplicationRoute {
            application: original_process.application_selector(),
            destination_mmdevice_id: Some(physical_render.id.clone()),
            virtualization_required: true,
            gain: 1.0,
            enabled: true,
            ..WindowsApplicationRoute::default()
        };

        let result = (|| -> Result<(), String> {
            let plans = driver
                .reconcile_application_routes(vec![route])
                .map_err(|error| format!("install automatic application route: {error}"))?;
            println!(
                "automatic application route initial plan: {plans:?}; original_endpoints={original:?}"
            );
            let expected_virtual: BTreeMap<_, _> = roles
                .iter()
                .map(|role| (*role, Some(app_render.id.clone())))
                .collect();
            wait_for_policy_endpoints(
                &mut driver,
                &policy,
                &original_process,
                &roles,
                &expected_virtual,
                Duration::from_secs(12),
            )?;

            // A running WASAPI client can keep its original endpoint. Start a
            // fresh process only after the policy transaction has succeeded;
            // its ordinary default open must now land on AppRender.
            let replacement = spawn_default_helper_with_reopen(&helper, 5_000)?;
            stop_child(&mut tone);
            tone = replacement;
            wait_for_application_route(&mut driver, true, Duration::from_secs(15))?;
            let after_restart =
                observe_loopback_tone(&loopback, &loopback_client, Duration::from_secs(2))?;
            if after_restart < 0.01 {
                return Err(format!(
                    "automatic route restart remained silent: amplitude={after_restart:.4}"
                ));
            }
            let replacement_process = ProcessIdentity::from_pid(tone.id())
                .map_err(|error| format!("read replacement helper identity: {error}"))?;
            let current = read_policy_endpoints(&policy, &replacement_process, roles)?;
            if !policy_endpoints_match(&expected_virtual, &current) {
                return Err(format!(
                    "replacement helper policy did not remain on AppRender: expected={expected_virtual:?}, actual={current:?}"
                ));
            }
            println!(
                "automatic application route after restart: 1 kHz amplitude {after_restart:.4}; current_endpoints={current:?}"
            );
            Ok(())
        })();

        // Removing the rule is part of the test, not just teardown: it must
        // restore the exact role-specific values captured before activation.
        let cleanup = (|| -> Result<(), String> {
            driver
                .reconcile_application_routes(Vec::new())
                .map_err(|error| format!("remove automatic application route: {error}"))?;
            let current_process = ProcessIdentity::from_pid(tone.id())
                .map_err(|error| format!("read cleanup helper identity: {error}"))?;
            wait_for_policy_endpoints(
                &mut driver,
                &policy,
                &current_process,
                &roles,
                &original,
                Duration::from_secs(12),
            )
        })();
        stop_child(&mut tone);

        match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(test), Ok(())) => Err(test),
            (Ok(()), Err(cleanup)) => Err(cleanup),
            (Err(test), Err(cleanup)) => Err(format!("{test}; cleanup also failed: {cleanup}")),
        }
    }

    fn run_automatic_application_route_shutdown_test() -> Result<(), String> {
        let helper = std::env::var_os("CARGO_BIN_EXE_windows-audio-test-tone")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "Cargo did not provide the tone helper executable".to_owned())?;
        let enumerator: Audio::IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| format!("create MMDeviceEnumerator: {error}"))?;
        let app_render = wait_for_role(
            &enumerator,
            Flow::Render,
            APP_RENDER_ROLE,
            Duration::from_secs(8),
        )?;
        let physical_render = choose_external_render(&enumerator)?;
        let loopback = open_loopback_stream(&physical_render.device)?;
        let loopback_client = unsafe { loopback.client.GetService::<Audio::IAudioCaptureClient>() }
            .map_err(|error| format!("get physical render loopback service: {error}"))?;
        unsafe {
            loopback
                .client
                .Start()
                .map_err(|error| format!("start physical render loopback: {error}"))?;
        }

        let mut tone = spawn_default_helper(&helper)?;
        let mut driver = match WindowsAudioDriver::new() {
            Ok(driver) => driver,
            Err(error) => {
                stop_child(&mut tone);
                return Err(error.to_string());
            }
        };
        driver.set_experimental_app_routing(true);
        let policy = VerifiedAudioPolicyConfig::new(true);
        let roles = [
            AudioRole::Console,
            AudioRole::Multimedia,
            AudioRole::Communications,
        ];
        let original_process = match ProcessIdentity::from_pid(tone.id()) {
            Ok(process) => process,
            Err(error) => {
                drop(driver);
                stop_child(&mut tone);
                return Err(format!("read helper identity: {error}"));
            }
        };
        let original = match read_policy_endpoints(&policy, &original_process, roles) {
            Ok(original) => original,
            Err(error) => {
                drop(driver);
                stop_child(&mut tone);
                return Err(error);
            }
        };
        let route = WindowsApplicationRoute {
            application: original_process.application_selector(),
            destination_mmdevice_id: Some(physical_render.id.clone()),
            virtualization_required: true,
            gain: 1.0,
            enabled: true,
            ..WindowsApplicationRoute::default()
        };
        let expected_virtual: BTreeMap<_, _> = roles
            .iter()
            .map(|role| (*role, Some(app_render.id.clone())))
            .collect();

        let result = (|| -> Result<(), String> {
            let plans = driver
                .reconcile_application_routes(vec![route])
                .map_err(|error| format!("install automatic application route: {error}"))?;
            println!(
                "automatic shutdown-restore route initial plan: {plans:?}; original_endpoints={original:?}"
            );
            wait_for_policy_endpoints(
                &mut driver,
                &policy,
                &original_process,
                &roles,
                &expected_virtual,
                Duration::from_secs(12),
            )?;

            // Rebind the lease to a fresh PID before shutdown. This makes the
            // probe cover both restart recovery and Drop-time restoration.
            let replacement = spawn_default_helper_with_reopen(&helper, 5_000)?;
            stop_child(&mut tone);
            tone = replacement;
            wait_for_application_route(&mut driver, true, Duration::from_secs(15))?;
            let after_restart =
                observe_loopback_tone(&loopback, &loopback_client, Duration::from_secs(2))?;
            if after_restart < 0.01 {
                return Err(format!(
                    "automatic route shutdown probe remained silent after restart: amplitude={after_restart:.4}"
                ));
            }
            let replacement_process = ProcessIdentity::from_pid(tone.id())
                .map_err(|error| format!("read replacement helper identity: {error}"))?;
            let current = read_policy_endpoints(&policy, &replacement_process, roles)?;
            if !policy_endpoints_match(&expected_virtual, &current) {
                return Err(format!(
                    "replacement helper policy did not remain on AppRender before shutdown: expected={expected_virtual:?}, actual={current:?}"
                ));
            }
            println!(
                "automatic shutdown-restore route before driver drop: 1 kHz amplitude {after_restart:.4}; current_endpoints={current:?}"
            );
            Ok(())
        })();

        let current_process = ProcessIdentity::from_pid(tone.id()).ok();
        // Do not remove the rule: the driver destructor must restore its live
        // lease while the helper is still running.
        drop(driver);
        let restore = current_process.map_or_else(
            || Err("could not read the live helper identity after driver shutdown".to_owned()),
            |process| {
                wait_for_policy_snapshot(
                    &policy,
                    &process,
                    &roles,
                    &original,
                    Duration::from_secs(12),
                )
            },
        );
        stop_child(&mut tone);

        match (result, restore) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(test), Ok(())) => Err(test),
            (Ok(()), Err(restore)) => {
                Err(format!("driver shutdown policy restore failed: {restore}"))
            }
            (Err(test), Err(restore)) => Err(format!(
                "{test}; driver shutdown policy restore also failed: {restore}"
            )),
        }
    }

    fn run_manual_override_test() -> Result<(), String> {
        let helper = std::env::var_os("CARGO_BIN_EXE_windows-audio-test-tone")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "Cargo did not provide the tone helper executable".to_owned())?;
        let enumerator: Audio::IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| format!("create MMDeviceEnumerator: {error}"))?;
        let app_render = wait_for_role(
            &enumerator,
            Flow::Render,
            APP_RENDER_ROLE,
            Duration::from_secs(8),
        )?;
        let physical_render = choose_external_render(&enumerator)?;
        let mut tone = spawn_default_helper(&helper)?;
        let mut driver = match WindowsAudioDriver::new() {
            Ok(driver) => driver,
            Err(error) => {
                stop_child(&mut tone);
                return Err(error.to_string());
            }
        };
        driver.set_experimental_app_routing(true);
        // A separate policy instance acts as the external actor. Volume Mixer
        // writes the same persisted store, so a write through this handle is
        // behaviorally identical to a user moving the app there.
        let external = VerifiedAudioPolicyConfig::new(true);
        let process = ProcessIdentity::from_pid(tone.id())
            .map_err(|error| format!("read helper identity: {error}"))?;
        let roles = [
            AudioRole::Console,
            AudioRole::Multimedia,
            AudioRole::Communications,
        ];
        let (policy, original) =
            wait_for_readable_policy(&process, roles, Duration::from_secs(10))?;
        for role in roles {
            if original.get(&role).is_some_and(|endpoint| {
                endpoint
                    .as_deref()
                    .is_some_and(|id| id.eq_ignore_ascii_case(&physical_render.id))
            }) {
                stop_child(&mut tone);
                return Err(format!(
                    "helper already persists {role:?} on the override target; state not clean"
                ));
            }
        }
        let route = WindowsApplicationRoute {
            application: process.application_selector(),
            destination_mmdevice_id: Some(physical_render.id.clone()),
            virtualization_required: true,
            gain: 1.0,
            enabled: true,
            ..WindowsApplicationRoute::default()
        };

        let result = (|| -> Result<(), String> {
            driver
                .reconcile_application_routes(vec![route.clone()])
                .map_err(|error| format!("install automatic application route: {error}"))?;
            let expected_virtual: BTreeMap<_, _> = roles
                .iter()
                .map(|role| (*role, Some(app_render.id.clone())))
                .collect();
            wait_for_policy_endpoints(
                &mut driver,
                &policy,
                &process,
                &roles,
                &expected_virtual,
                Duration::from_secs(12),
            )?;

            for role in roles {
                external
                    .set_persisted_endpoint(
                        &process,
                        AudioFlow::Render,
                        role,
                        Some(&physical_render.id),
                    )
                    .map_err(|error| format!("simulate user override for {role:?}: {error}"))?;
            }
            let expected_override: BTreeMap<_, _> = roles
                .iter()
                .map(|role| (*role, Some(physical_render.id.clone())))
                .collect();
            wait_for_policy_snapshot(
                &external,
                &process,
                &roles,
                &expected_override,
                Duration::from_secs(8),
            )?;

            // Removing the rule must not restore over the user's choice, and
            // refreshes while the rule is gone must leave it untouched.
            driver
                .reconcile_application_routes(Vec::new())
                .map_err(|error| format!("remove overridden application route: {error}"))?;
            assert_endpoints_stable(
                &mut driver,
                &policy,
                &process,
                &roles,
                &expected_override,
                Duration::from_secs(6),
            )?;

            // The released rule can be re-applied cleanly afterwards.
            driver
                .reconcile_application_routes(vec![route.clone()])
                .map_err(|error| format!("re-apply application route: {error}"))?;
            wait_for_policy_endpoints(
                &mut driver,
                &policy,
                &process,
                &roles,
                &expected_virtual,
                Duration::from_secs(12),
            )?;
            Ok(())
        })();

        // The re-apply left driver leases rooted at the override values, so
        // release them first and then restore the exact pre-test values.
        let cleanup = (|| -> Result<(), String> {
            driver
                .reconcile_application_routes(Vec::new())
                .map_err(|error| format!("remove application route: {error}"))?;
            let current = ProcessIdentity::from_pid(tone.id())
                .map_err(|error| format!("read cleanup helper identity: {error}"))?;
            for role in roles {
                let value = original.get(&role).and_then(|endpoint| endpoint.as_deref());
                policy
                    .set_persisted_endpoint(&current, AudioFlow::Render, role, value)
                    .map_err(|error| format!("restore {role:?} endpoint: {error}"))?;
            }
            wait_for_policy_snapshot(&policy, &current, &roles, &original, Duration::from_secs(8))
        })();
        stop_child(&mut tone);

        match (result, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(test), Ok(())) => Err(test),
            (Ok(()), Err(cleanup)) => Err(cleanup),
            (Err(test), Err(cleanup)) => Err(format!("{test}; cleanup also failed: {cleanup}")),
        }
    }

    fn run_backend_restart_test() -> Result<(), String> {
        let helper = std::env::var_os("CARGO_BIN_EXE_windows-audio-test-tone")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "Cargo did not provide the tone helper executable".to_owned())?;
        let enumerator: Audio::IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| format!("create MMDeviceEnumerator: {error}"))?;
        let app_render = wait_for_role(
            &enumerator,
            Flow::Render,
            APP_RENDER_ROLE,
            Duration::from_secs(8),
        )?;
        let physical_render = choose_external_render(&enumerator)?;
        let loopback = open_loopback_stream(&physical_render.device)?;
        let loopback_client = unsafe { loopback.client.GetService::<Audio::IAudioCaptureClient>() }
            .map_err(|error| format!("get physical render loopback service: {error}"))?;
        unsafe {
            loopback
                .client
                .Start()
                .map_err(|error| format!("start physical render loopback: {error}"))?;
        }

        let mut tone = spawn_default_helper(&helper)?;
        let mut driver = match WindowsAudioDriver::new() {
            Ok(driver) => driver,
            Err(error) => {
                stop_child(&mut tone);
                return Err(error.to_string());
            }
        };
        driver.set_experimental_app_routing(true);
        let process = ProcessIdentity::from_pid(tone.id())
            .map_err(|error| format!("read helper identity: {error}"))?;
        let roles = [
            AudioRole::Console,
            AudioRole::Multimedia,
            AudioRole::Communications,
        ];
        let (policy, original) =
            wait_for_readable_policy(&process, roles, Duration::from_secs(10))?;
        let route = WindowsApplicationRoute {
            application: process.application_selector(),
            destination_mmdevice_id: Some(physical_render.id.clone()),
            virtualization_required: true,
            gain: 1.0,
            enabled: true,
            ..WindowsApplicationRoute::default()
        };
        let expected_virtual: BTreeMap<_, _> = roles
            .iter()
            .map(|role| (*role, Some(app_render.id.clone())))
            .collect();

        let phase1 = (|| -> Result<ProcessIdentity, String> {
            driver
                .reconcile_application_routes(vec![route.clone()])
                .map_err(|error| format!("install automatic application route: {error}"))?;
            wait_for_policy_endpoints(
                &mut driver,
                &policy,
                &process,
                &roles,
                &expected_virtual,
                Duration::from_secs(12),
            )?;
            // A running WASAPI client can keep its original endpoint, so a
            // fresh process proves the new sessions isolate on AppRender.
            let live =
                replace_helper_with_fresh_session(&helper, &mut tone, &roles, &expected_virtual)?;
            wait_for_application_route(&mut driver, true, Duration::from_secs(15))?;
            let amplitude =
                observe_loopback_tone(&loopback, &loopback_client, Duration::from_secs(2))?;
            if amplitude < 0.01 {
                return Err(format!(
                    "route silent before restart: amplitude={amplitude:.4}"
                ));
            }
            println!("route audible before restart: amplitude={amplitude:.4}");
            Ok(live)
        })();
        let process = match phase1 {
            Ok(process) => process,
            Err(error) => {
                let cleanup =
                    release_route_and_restore(&mut driver, &policy, tone.id(), &roles, &original);
                stop_child(&mut tone);
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(format!("{error}; cleanup also failed: {cleanup}")),
                };
            }
        };

        // A graceful backend restart drops the driver without removing the
        // rule; the destructor must restore the live lease first.
        drop(driver);
        let restored = wait_for_policy_snapshot(
            &policy,
            &process,
            &roles,
            &original,
            Duration::from_secs(12),
        )
        .map_err(|error| format!("drop did not restore: {error}"));

        let mut driver = match WindowsAudioDriver::new() {
            Ok(driver) => driver,
            Err(error) => {
                stop_child(&mut tone);
                return Err(format!("recreate backend after restart: {error}"));
            }
        };
        driver.set_experimental_app_routing(true);
        let phase2 = (|| -> Result<(), String> {
            driver
                .reconcile_application_routes(vec![route.clone()])
                .map_err(|error| format!("reinstall application route after restart: {error}"))?;
            wait_for_policy_endpoints(
                &mut driver,
                &policy,
                &process,
                &roles,
                &expected_virtual,
                Duration::from_secs(12),
            )?;
            replace_helper_with_fresh_session(&helper, &mut tone, &roles, &expected_virtual)?;
            wait_for_application_route(&mut driver, true, Duration::from_secs(15))?;
            let amplitude =
                observe_loopback_tone(&loopback, &loopback_client, Duration::from_secs(2))?;
            if amplitude < 0.01 {
                return Err(format!(
                    "route silent after restart: amplitude={amplitude:.4}"
                ));
            }
            println!("route audible after restart: amplitude={amplitude:.4}");
            Ok(())
        })();

        let cleanup = release_route_and_restore(&mut driver, &policy, tone.id(), &roles, &original);
        stop_child(&mut tone);

        let test = restored.and(phase2);
        match (test, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(test), Ok(())) => Err(test),
            (Ok(()), Err(cleanup)) => Err(cleanup),
            (Err(test), Err(cleanup)) => Err(format!("{test}; cleanup also failed: {cleanup}")),
        }
    }

    /// Wait until the helper has an audio session the private interface
    /// will talk about. A freshly spawned helper may not have rendered yet,
    /// and a too-early read fails closed; each attempt therefore uses a
    /// fresh policy instance so only disposable handles demote. The
    /// returned instance has never failed.
    fn wait_for_readable_policy(
        process: &ProcessIdentity,
        roles: [AudioRole; 3],
        timeout: Duration,
    ) -> Result<
        (
            VerifiedAudioPolicyConfig,
            BTreeMap<AudioRole, Option<String>>,
        ),
        String,
    > {
        let deadline = Instant::now() + timeout;
        let mut last = String::from("no policy observation");
        while Instant::now() < deadline {
            let policy = VerifiedAudioPolicyConfig::new(true);
            match read_policy_endpoints(&policy, process, roles) {
                Ok(endpoints) => return Ok((policy, endpoints)),
                Err(error) => last = error,
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(format!(
            "helper never became readable via AudioPolicyConfig: {last}"
        ))
    }

    /// Replace the running tone helper so the new audio session lands on the
    /// already-persisted endpoint, and return the replacement identity after
    /// verifying its persisted endpoints match. The replacement is
    /// long-lived (some callers wait on operator-confirmed UAC prompts) and
    /// reopens once after 5 s, so a session opened during persisted-store
    /// propagation lag still isolates.
    fn replace_helper_with_fresh_session(
        helper: &std::path::Path,
        tone: &mut Child,
        roles: &[AudioRole; 3],
        expected: &BTreeMap<AudioRole, Option<String>>,
    ) -> Result<ProcessIdentity, String> {
        let replacement = spawn_long_helper(helper, Some(5_000))?;
        stop_child(tone);
        *tone = replacement;
        let process = ProcessIdentity::from_pid(tone.id())
            .map_err(|error| format!("read replacement helper identity: {error}"))?;
        let (_, current) = wait_for_readable_policy(&process, *roles, Duration::from_secs(10))?;
        if !policy_endpoints_match(expected, &current) {
            return Err(format!(
                "replacement helper policy did not match: expected={expected:?}, actual={current:?}"
            ));
        }
        Ok(process)
    }

    /// Release any lease the driver holds, then restore the exact pre-test
    /// values explicitly: after a mid-test failure the leases may be rooted
    /// somewhere else.
    fn release_route_and_restore(
        driver: &mut WindowsAudioDriver,
        policy: &VerifiedAudioPolicyConfig,
        pid: u32,
        roles: &[AudioRole; 3],
        original: &BTreeMap<AudioRole, Option<String>>,
    ) -> Result<(), String> {
        driver
            .reconcile_application_routes(Vec::new())
            .map_err(|error| format!("remove application route: {error}"))?;
        restore_policy_explicit(policy, pid, roles, original)
    }

    /// Restore exact endpoint values without a driver: used after a backend
    /// crash, where no live leases exist to release.
    fn restore_policy_explicit(
        policy: &VerifiedAudioPolicyConfig,
        pid: u32,
        roles: &[AudioRole; 3],
        original: &BTreeMap<AudioRole, Option<String>>,
    ) -> Result<(), String> {
        let current = ProcessIdentity::from_pid(pid)
            .map_err(|error| format!("read cleanup helper identity: {error}"))?;
        for role in roles {
            let value = original.get(role).and_then(|endpoint| endpoint.as_deref());
            policy
                .set_persisted_endpoint(&current, AudioFlow::Render, *role, value)
                .map_err(|error| format!("restore {role:?} endpoint: {error}"))?;
        }
        wait_for_policy_snapshot(policy, &current, roles, original, Duration::from_secs(8))
    }

    fn run_backend_crash_test() -> Result<(), String> {
        let helper = std::env::var_os("CARGO_BIN_EXE_windows-audio-test-tone")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "Cargo did not provide the tone helper executable".to_owned())?;
        let crash_host = std::env::var_os("CARGO_BIN_EXE_qpwgraph-backend-crash-host")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "Cargo did not provide the crash host executable".to_owned())?;
        let enumerator: Audio::IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| format!("create MMDeviceEnumerator: {error}"))?;
        let app_render = wait_for_role(
            &enumerator,
            Flow::Render,
            APP_RENDER_ROLE,
            Duration::from_secs(8),
        )?;
        let physical_render = choose_external_render(&enumerator)?;
        let loopback = open_loopback_stream(&physical_render.device)?;
        let loopback_client = unsafe { loopback.client.GetService::<Audio::IAudioCaptureClient>() }
            .map_err(|error| format!("get physical render loopback service: {error}"))?;
        unsafe {
            loopback
                .client
                .Start()
                .map_err(|error| format!("start physical render loopback: {error}"))?;
        }

        let mut tone = spawn_default_helper(&helper)?;
        let first = ProcessIdentity::from_pid(tone.id())
            .map_err(|error| format!("read helper identity: {error}"))?;
        let roles = [
            AudioRole::Console,
            AudioRole::Multimedia,
            AudioRole::Communications,
        ];
        let (policy, original) = wait_for_readable_policy(&first, roles, Duration::from_secs(10))?;
        let route = WindowsApplicationRoute {
            application: first.application_selector(),
            destination_mmdevice_id: Some(physical_render.id.clone()),
            virtualization_required: true,
            gain: 1.0,
            enabled: true,
            ..WindowsApplicationRoute::default()
        };
        let expected_virtual: BTreeMap<_, _> = roles
            .iter()
            .map(|role| (*role, Some(app_render.id.clone())))
            .collect();
        let ready_file =
            std::env::temp_dir().join(format!("qpwgraph-crash-host-{}.ready", std::process::id()));
        let _ = std::fs::remove_file(&ready_file);
        let mut host = std::process::Command::new(&crash_host)
            .args([
                "--helper-pid",
                &tone.id().to_string(),
                "--physical-id",
                &physical_render.id,
                "--app-render-id",
                &app_render.id,
                "--ready-file",
                &ready_file.to_string_lossy(),
            ])
            .spawn()
            .map_err(|error| format!("spawn crash host: {error}"))?;

        let phase1 = (|| -> Result<ProcessIdentity, String> {
            let deadline = Instant::now() + Duration::from_secs(25);
            loop {
                if ready_file.is_file() {
                    break;
                }
                if host
                    .try_wait()
                    .map_err(|error| format!("poll crash host: {error}"))?
                    .is_some()
                {
                    return Err("crash host exited before reporting READY".to_owned());
                }
                if Instant::now() >= deadline {
                    let _ = host.kill();
                    let _ = host.wait();
                    return Err("crash host never reported READY".to_owned());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            // A fresh session lands on the host-applied endpoint so the
            // host's own route goes active before the crash.
            let live =
                replace_helper_with_fresh_session(&helper, &mut tone, &roles, &expected_virtual)?;
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let amplitude =
                    observe_loopback_tone(&loopback, &loopback_client, Duration::from_secs(1))?;
                if amplitude >= 0.01 {
                    println!("host stream audible before crash: amplitude={amplitude:.4}");
                    return Ok(live);
                }
                if Instant::now() >= deadline {
                    return Err("host stream never became audible".to_owned());
                }
            }
        })();
        let process = match phase1 {
            Ok(process) => process,
            Err(error) => {
                let _ = host.kill();
                let _ = host.wait();
                let cleanup = restore_policy_explicit(&policy, tone.id(), &roles, &original);
                let _ = std::fs::remove_file(&ready_file);
                stop_child(&mut tone);
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(format!("{error}; cleanup also failed: {cleanup}")),
                };
            }
        };

        // Crash: terminate without cleanup, so no Drop-time restore runs.
        host.kill()
            .map_err(|error| format!("kill crash host: {error}"))?;
        let _ = host.wait();
        let stuck = read_policy_endpoints(&policy, &process, roles)
            .map_err(|error| format!("read post-crash endpoints: {error}"))?;
        if !policy_endpoints_match(&expected_virtual, &stuck) {
            let cleanup = restore_policy_explicit(&policy, tone.id(), &roles, &original);
            let _ = std::fs::remove_file(&ready_file);
            stop_child(&mut tone);
            let error = format!(
                "crash remnant did not stick at AppRender: expected={expected_virtual:?}, actual={stuck:?}"
            );
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!("{error}; cleanup also failed: {cleanup}")),
            };
        }
        println!("crash remnant stuck at AppRender as expected; recovering with a fresh backend");

        let mut driver = match WindowsAudioDriver::new() {
            Ok(driver) => driver,
            Err(error) => {
                let cleanup = restore_policy_explicit(&policy, tone.id(), &roles, &original);
                let _ = std::fs::remove_file(&ready_file);
                stop_child(&mut tone);
                let error = format!("recreate backend after crash: {error}");
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(format!("{error}; cleanup also failed: {cleanup}")),
                };
            }
        };
        driver.set_experimental_app_routing(true);
        let phase2 = (|| -> Result<(), String> {
            driver
                .reconcile_application_routes(vec![route.clone()])
                .map_err(|error| format!("reinstall application route after crash: {error}"))?;
            wait_for_policy_endpoints(
                &mut driver,
                &policy,
                &process,
                &roles,
                &expected_virtual,
                Duration::from_secs(12),
            )?;
            wait_for_application_route(&mut driver, true, Duration::from_secs(15))?;
            let amplitude =
                observe_loopback_tone(&loopback, &loopback_client, Duration::from_secs(2))?;
            if amplitude < 0.01 {
                return Err(format!(
                    "route silent after crash recovery: amplitude={amplitude:.4}"
                ));
            }
            println!("route audible after crash recovery: amplitude={amplitude:.4}");
            let renders = enumerate(&enumerator, Flow::Render)?;
            let captures = enumerate(&enumerator, Flow::Capture)?;
            let virtual_renders = renders
                .iter()
                .filter(|endpoint| endpoint.name.to_ascii_lowercase().contains("qpwgraph"))
                .count();
            let virtual_captures = captures
                .iter()
                .filter(|endpoint| endpoint.name.to_ascii_lowercase().contains("qpwgraph"))
                .count();
            if virtual_renders != 2 || virtual_captures != 2 {
                return Err(format!(
                    "virtual endpoints degraded after crash: {virtual_renders} render + {virtual_captures} capture"
                ));
            }
            Ok(())
        })();

        let cleanup = release_route_and_restore(&mut driver, &policy, tone.id(), &roles, &original);
        let _ = std::fs::remove_file(&ready_file);
        stop_child(&mut tone);
        match (phase2, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(test), Ok(())) => Err(test),
            (Ok(()), Err(cleanup)) => Err(cleanup),
            (Err(test), Err(cleanup)) => Err(format!("{test}; cleanup also failed: {cleanup}")),
        }
    }

    /// Byte snapshot of the GUI-owned config files, restored in all paths.
    struct GuiConfigSnapshot {
        files: Vec<(std::path::PathBuf, Option<Vec<u8>>)>,
    }

    impl GuiConfigSnapshot {
        fn capture() -> Result<Self, String> {
            let dir = pw_graph_config::config_dir("qpwgraph-rs");
            let mut files = Vec::new();
            for name in [
                "config.toml",
                "default.qpwgraph",
                "default.qpwgraph.qpwgraph-rs-selectors.json",
            ] {
                let path = dir.join(name);
                let bytes = match std::fs::read(&path) {
                    Ok(bytes) => Some(bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => {
                        return Err(format!("snapshot {}: {error}", path.display()));
                    }
                };
                files.push((path, bytes));
            }
            Ok(Self { files })
        }

        fn restore(&self) -> Result<(), String> {
            for (path, bytes) in &self.files {
                match bytes {
                    Some(bytes) => std::fs::write(path, bytes)
                        .map_err(|error| format!("restore {}: {error}", path.display()))?,
                    None => match std::fs::remove_file(path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(format!("remove test {}: {error}", path.display()));
                        }
                    },
                }
            }
            Ok(())
        }
    }

    fn ensure_no_gui_running() -> Result<(), String> {
        let output = Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq qpwgraph-rs.exe", "/FO", "CSV", "/NH"])
            .output()
            .map_err(|error| format!("tasklist probe: {error}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.to_ascii_lowercase().contains("qpwgraph-rs.exe") {
            return Err(
                "a qpwgraph-rs GUI instance is already running; stop it before the GUI crash test"
                    .to_owned(),
            );
        }
        Ok(())
    }

    fn gui_binary_path() -> Result<std::path::PathBuf, String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/release/qpwgraph-rs.exe");
        if !path.is_file() {
            return Err(format!(
                "GUI binary not found (build the workspace release first): {}",
                path.display()
            ));
        }
        Ok(path)
    }

    fn spawn_gui_process(gui: &std::path::Path) -> Result<Child, String> {
        let mut child = Command::new(gui)
            .arg("--minimized")
            .spawn()
            .map_err(|error| format!("spawn GUI: {error}"))?;
        std::thread::sleep(Duration::from_secs(2));
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("poll GUI: {error}"))?
        {
            return Err(format!("GUI exited immediately with status {status}"));
        }
        Ok(child)
    }

    fn wait_for_audible_loopback(
        stream: &AudioStream,
        client: &Audio::IAudioCaptureClient,
        timeout: Duration,
    ) -> Result<f32, String> {
        let deadline = Instant::now() + timeout;
        let mut last = String::from("no loopback observation");
        while Instant::now() < deadline {
            match observe_loopback_tone(stream, client, Duration::from_secs(1)) {
                Ok(amplitude) => return Ok(amplitude),
                Err(error) => last = error,
            }
        }
        Err(format!("route never became audible: {last}"))
    }

    /// Full GUI app-process kill during an audible route.
    ///
    /// Unlike `run_backend_crash_test` (minimal crash host), the killed
    /// process is the real `qpwgraph-rs.exe` GUI. The test pre-seeds the app
    /// config so the GUI restores the route at startup, kills it mid-stream,
    /// asserts the stuck AppRender remnant, relaunches, and requires an
    /// audible route plus all four virtual endpoints again.
    fn run_gui_crash_test() -> Result<(), String> {
        ensure_no_gui_running()?;
        let helper = std::env::var_os("CARGO_BIN_EXE_windows-audio-test-tone")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "Cargo did not provide the tone helper executable".to_owned())?;
        let gui = gui_binary_path()?;
        let enumerator: Audio::IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| format!("create MMDeviceEnumerator: {error}"))?;
        let app_render = wait_for_role(
            &enumerator,
            Flow::Render,
            APP_RENDER_ROLE,
            Duration::from_secs(8),
        )?;
        let physical_render = choose_external_render(&enumerator)?;
        let loopback = open_loopback_stream(&physical_render.device)?;
        let loopback_client = unsafe { loopback.client.GetService::<Audio::IAudioCaptureClient>() }
            .map_err(|error| format!("get physical render loopback service: {error}"))?;
        unsafe {
            loopback
                .client
                .Start()
                .map_err(|error| format!("start physical render loopback: {error}"))?;
        }

        let mut tone = spawn_default_helper(&helper)?;
        let first = ProcessIdentity::from_pid(tone.id())
            .map_err(|error| format!("read helper identity: {error}"))?;
        let roles = [
            AudioRole::Console,
            AudioRole::Multimedia,
            AudioRole::Communications,
        ];
        let (policy, original) = wait_for_readable_policy(&first, roles, Duration::from_secs(10))?;
        let route = WindowsApplicationRoute {
            application: first.application_selector(),
            destination_mmdevice_id: Some(physical_render.id.clone()),
            virtualization_required: true,
            gain: 1.0,
            enabled: true,
            ..WindowsApplicationRoute::default()
        };
        let expected_virtual: BTreeMap<_, _> = roles
            .iter()
            .map(|role| (*role, Some(app_render.id.clone())))
            .collect();

        let snapshot = GuiConfigSnapshot::capture()?;
        let config_file = pw_graph_config::config_path("qpwgraph-rs");
        if let Some(dir) = config_file.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("create GUI config dir: {error}"))?;
        }
        let mut config = AppConfig::default();
        config.windows.experimental_app_routing = true;
        config.windows_application_routes = vec![route];
        if let Err(error) = config.save_to(&config_file) {
            let _ = snapshot.restore();
            return Err(format!("write GUI test config: {error}"));
        }

        let mut gui_child: Option<Child> = None;
        let result = (|| -> Result<(), String> {
            gui_child = Some(spawn_gui_process(&gui)?);
            println!(
                "GUI started (pid {}) with test config; waiting for route",
                gui_child.as_ref().map(|child| child.id()).unwrap_or(0)
            );
            wait_for_policy_snapshot(
                &policy,
                &first,
                &roles,
                &expected_virtual,
                Duration::from_secs(60),
            )?;
            let amplitude =
                wait_for_audible_loopback(&loopback, &loopback_client, Duration::from_secs(30))?;
            println!("GUI route audible before kill: amplitude={amplitude:.4}");

            // Crash: terminate without cleanup, so no Drop-time restore runs.
            let mut killed = gui_child.take().expect("GUI child tracked");
            let kill_result = killed.kill().map_err(|error| format!("kill GUI: {error}"));
            let _ = killed.wait();
            kill_result?;
            let stuck = read_policy_endpoints(&policy, &first, roles)
                .map_err(|error| format!("read post-kill endpoints: {error}"))?;
            if !policy_endpoints_match(&expected_virtual, &stuck) {
                return Err(format!(
                    "GUI kill remnant did not stick at AppRender: expected={expected_virtual:?}, actual={stuck:?}"
                ));
            }
            println!("GUI kill remnant stuck at AppRender as expected; relaunching");

            gui_child = Some(spawn_gui_process(&gui)?);
            wait_for_policy_snapshot(
                &policy,
                &first,
                &roles,
                &expected_virtual,
                Duration::from_secs(60),
            )?;
            let recovered =
                wait_for_audible_loopback(&loopback, &loopback_client, Duration::from_secs(30))?;
            println!("GUI route audible after relaunch: amplitude={recovered:.4}");
            let renders = enumerate(&enumerator, Flow::Render)?;
            let captures = enumerate(&enumerator, Flow::Capture)?;
            let virtual_renders = renders
                .iter()
                .filter(|endpoint| endpoint.name.to_ascii_lowercase().contains("qpwgraph"))
                .count();
            let virtual_captures = captures
                .iter()
                .filter(|endpoint| endpoint.name.to_ascii_lowercase().contains("qpwgraph"))
                .count();
            if virtual_renders != 2 || virtual_captures != 2 {
                return Err(format!(
                    "virtual endpoints degraded after GUI relaunch: {virtual_renders} render + {virtual_captures} capture"
                ));
            }
            Ok(())
        })();

        if let Some(mut child) = gui_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let mut failures = Vec::new();
        if let Err(error) = snapshot.restore() {
            failures.push(format!("config restore: {error}"));
        }
        if let Err(error) = restore_policy_explicit(&policy, tone.id(), &roles, &original) {
            failures.push(format!("policy restore: {error}"));
        }
        stop_child(&mut tone);
        match (result, failures.is_empty()) {
            (Ok(()), true) => Ok(()),
            (Ok(()), false) => Err(format!("cleanup failed: {}", failures.join("; "))),
            (Err(test), true) => Err(test),
            (Err(test), false) => Err(format!(
                "{test}; cleanup also failed: {}",
                failures.join("; ")
            )),
        }
    }

    /// Pump driver refreshes for `duration`, failing if the observed
    /// endpoints ever deviate from `expected`. This asserts a negative —
    /// that no restore is written — so it requires at least one successful
    /// read and tolerates only transient read errors.
    fn assert_endpoints_stable(
        driver: &mut WindowsAudioDriver,
        policy: &VerifiedAudioPolicyConfig,
        process: &ProcessIdentity,
        roles: &[AudioRole; 3],
        expected: &BTreeMap<AudioRole, Option<String>>,
        duration: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + duration;
        let mut last = String::from("no policy observation");
        let mut saw_ok = false;
        while Instant::now() < deadline {
            if let Err(error) = driver.refresh() {
                last = format!("refresh failed: {error}");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            match read_policy_endpoints(policy, process, *roles) {
                Ok(actual) => {
                    saw_ok = true;
                    if !policy_endpoints_match(expected, &actual) {
                        return Err(format!(
                            "user override was not preserved: expected={expected:?}, actual={actual:?}"
                        ));
                    }
                }
                Err(error) => last = error,
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if saw_ok {
            Ok(())
        } else {
            Err(format!(
                "no successful policy observation during stability window: {last}"
            ))
        }
    }

    fn default_render_id(enumerator: &Audio::IMMDeviceEnumerator) -> Result<String, String> {
        let device =
            unsafe { enumerator.GetDefaultAudioEndpoint(Audio::eRender, Audio::eMultimedia) }
                .map_err(|error| format!("read default render endpoint: {error}"))?;
        endpoint_id(&device)
    }

    /// Wait until no render endpoint with `name` enumerates anymore.
    fn wait_for_endpoint_absence(
        enumerator: &Audio::IMMDeviceEnumerator,
        name: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            let renders = enumerate(enumerator, Flow::Render)?;
            if !renders.iter().any(|endpoint| endpoint.name == name) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("endpoint '{name}' never left"));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Wait until a render endpoint with `name` enumerates again.
    fn wait_for_endpoint_return(
        enumerator: &Audio::IMMDeviceEnumerator,
        name: &str,
        timeout: Duration,
    ) -> Result<Endpoint, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let renders = enumerate(enumerator, Flow::Render)?;
            if let Some(endpoint) = renders.iter().find(|endpoint| endpoint.name == name) {
                return Ok(endpoint.clone());
            }
            if Instant::now() >= deadline {
                return Err(format!("endpoint '{name}' never returned after restore"));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Wait until rule 0 reports exactly `state` and, for any non-active
    /// state, no active route remains.
    fn wait_for_route_state(
        driver: &mut WindowsAudioDriver,
        state: &str,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            driver.refresh().map_err(|error| error.to_string())?;
            let report = driver.windows_audio_report();
            let marked = report.contains(&format!("application_route rule=0 state={state}"));
            let active = report.contains("application_route rule=0 state=Active");
            if marked && (state == "Active" || !active) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let routes: Vec<&str> = report
                    .lines()
                    .filter(|line| line.contains("application_route rule="))
                    .collect();
                return Err(format!(
                    "route did not reach state={state}; route lines: {routes:?}"
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn run_endpoint_churn_test() -> Result<(), String> {
        let helper = std::env::var_os("CARGO_BIN_EXE_windows-audio-test-tone")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "Cargo did not provide the tone helper executable".to_owned())?;
        let enumerator: Audio::IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| format!("create MMDeviceEnumerator: {error}"))?;
        let app_render = wait_for_role(
            &enumerator,
            Flow::Render,
            APP_RENDER_ROLE,
            Duration::from_secs(8),
        )?;
        let physical_render = choose_churn_render(&enumerator)?;
        let default_before = default_render_id(&enumerator)?;
        println!("default render before churn: {default_before}");
        // Operator-assisted churn: no software mechanism faithfully
        // removes the endpoint (SWD disable leaves MMDevice-ACTIVE,
        // function disable is vetoed by the streaming route, and function
        // removal orphans MMDevice state). The operator physically
        // unplugs/replugs the USB device while the test watches endpoint
        // presence; every wait fails safe on timeout.
        println!(
            "OPERATOR: leave '{}' plugged in until asked",
            physical_render.name
        );
        let mut loopback = open_loopback_stream(&physical_render.device)?;
        let mut loopback_client =
            unsafe { loopback.client.GetService::<Audio::IAudioCaptureClient>() }
                .map_err(|error| format!("get physical render loopback service: {error}"))?;
        unsafe {
            loopback
                .client
                .Start()
                .map_err(|error| format!("start physical render loopback: {error}"))?;
        }

        let mut tone = spawn_long_helper(&helper, None)?;
        let first = ProcessIdentity::from_pid(tone.id())
            .map_err(|error| format!("read helper identity: {error}"))?;
        let roles = [
            AudioRole::Console,
            AudioRole::Multimedia,
            AudioRole::Communications,
        ];
        let (policy, original) = wait_for_readable_policy(&first, roles, Duration::from_secs(10))?;
        // Physical endpoints typically carry no stable id, so the rule holds
        // the current id plus the friendly name: after churn the route must
        // resolve through whichever still matches, and never attach to a
        // wrong device.
        let route = WindowsApplicationRoute {
            application: first.application_selector(),
            destination_mmdevice_id: Some(physical_render.id.clone()),
            destination_name: Some(physical_render.name.clone()),
            virtualization_required: true,
            gain: 1.0,
            enabled: true,
            ..WindowsApplicationRoute::default()
        };
        let expected_virtual: BTreeMap<_, _> = roles
            .iter()
            .map(|role| (*role, Some(app_render.id.clone())))
            .collect();
        let mut driver = match WindowsAudioDriver::new() {
            Ok(driver) => driver,
            Err(error) => {
                stop_child(&mut tone);
                return Err(error.to_string());
            }
        };
        driver.set_experimental_app_routing(true);

        let phase1 = (|| -> Result<ProcessIdentity, String> {
            driver
                .reconcile_application_routes(vec![route.clone()])
                .map_err(|error| format!("install application route: {error}"))?;
            wait_for_policy_endpoints(
                &mut driver,
                &policy,
                &first,
                &roles,
                &expected_virtual,
                Duration::from_secs(12),
            )?;
            let live =
                replace_helper_with_fresh_session(&helper, &mut tone, &roles, &expected_virtual)?;
            wait_for_application_route(&mut driver, true, Duration::from_secs(15))?;
            let amplitude =
                observe_loopback_tone(&loopback, &loopback_client, Duration::from_secs(2))?;
            if amplitude < 0.01 {
                return Err(format!(
                    "route silent before churn: amplitude={amplitude:.4}"
                ));
            }
            println!("route audible before churn: amplitude={amplitude:.4}");
            Ok(live)
        })();
        if let Err(error) = phase1 {
            let cleanup =
                release_route_and_restore(&mut driver, &policy, tone.id(), &roles, &original);
            stop_child(&mut tone);
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(format!("{error}; cleanup also failed: {cleanup}")),
            };
        }

        let churn = (|| -> Result<(), String> {
            println!(
                "OPERATOR: UNPLUG the '{}' USB device now (waiting 300 s)",
                physical_render.name
            );
            wait_for_endpoint_absence(
                &enumerator,
                &physical_render.name,
                Duration::from_secs(300),
            )?;
            println!("endpoint removed; waiting for the route to degrade");
            wait_for_route_state(&mut driver, "DestinationMissing", Duration::from_secs(30))?;
            println!("route degraded to DestinationMissing while the endpoint is removed");
            println!(
                "OPERATOR: REPLUG the '{}' USB device now (waiting 300 s)",
                physical_render.name
            );
            let returned = wait_for_endpoint_return(
                &enumerator,
                &physical_render.name,
                Duration::from_secs(300),
            )?;
            if returned.id == physical_render.id {
                println!("endpoint returned under the same MMDevice id; current-id match");
            } else {
                println!(
                    "endpoint id churned: {} -> {}; name fallback must resolve it",
                    physical_render.id, returned.id
                );
            }
            // If the helper expired during the human waits, a fresh
            // process re-resolves the same rule; the route must restore.
            let helper_dead = tone
                .try_wait()
                .map_err(|error| format!("poll helper liveness: {error}"))?
                .is_some();
            if helper_dead {
                println!("helper expired during operator waits; respawning");
                replace_helper_with_fresh_session(&helper, &mut tone, &roles, &expected_virtual)?;
            }
            wait_for_route_frames_advancing(&mut driver, 4_800, Duration::from_secs(30))?;
            println!(
                "post-churn route state: {}",
                driver.windows_audio_report().replace('\n', " | ")
            );
            // The old loopback handle died with the device; capture the
            // returned endpoint to prove audio reaches the right device.
            loopback = open_loopback_stream(&returned.device)?;
            loopback_client = unsafe { loopback.client.GetService::<Audio::IAudioCaptureClient>() }
                .map_err(|error| format!("get returned render loopback service: {error}"))?;
            unsafe {
                loopback
                    .client
                    .Start()
                    .map_err(|error| format!("start returned render loopback: {error}"))?;
            }
            let amplitude =
                observe_loopback_tone(&loopback, &loopback_client, Duration::from_secs(2))?;
            if amplitude < 0.01 {
                return Err(format!(
                    "route silent after churn: amplitude={amplitude:.4}"
                ));
            }
            println!("route audible after churn: amplitude={amplitude:.4}");
            Ok(())
        })();

        let mut failures = Vec::new();
        if let Err(error) = churn {
            failures.push(error);
        }
        // Whatever failed, leave the machine whole: if the operator's
        // unplug is still in effect, ask for the device back. There is no
        // software restore; the physical state belongs to the operator.
        let present = enumerate(&enumerator, Flow::Render)
            .map(|renders| {
                renders
                    .iter()
                    .any(|endpoint| endpoint.name == physical_render.name)
            })
            .unwrap_or(false);
        if !present {
            println!(
                "OPERATOR: REPLUG the '{}' USB device now (waiting 300 s)",
                physical_render.name
            );
            if let Err(error) = wait_for_endpoint_return(
                &enumerator,
                &physical_render.name,
                Duration::from_secs(300),
            ) {
                failures.push(format!(
                    "endpoint left unplugged after failure ({error}); replug the USB audio device"
                ));
            }
        }
        match default_render_id(&enumerator) {
            Ok(after) => println!("default render after churn: {after} (before: {default_before})"),
            Err(error) => println!("could not read default render after churn: {error}"),
        }
        let cleanup = release_route_and_restore(&mut driver, &policy, tone.id(), &roles, &original);
        stop_child(&mut tone);
        if let Err(error) = cleanup {
            failures.push(error);
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        }
    }

    fn read_policy_endpoints(
        policy: &VerifiedAudioPolicyConfig,
        process: &ProcessIdentity,
        roles: [AudioRole; 3],
    ) -> Result<BTreeMap<AudioRole, Option<String>>, String> {
        roles
            .into_iter()
            .map(|role| {
                policy
                    .get_persisted_endpoint(process, AudioFlow::Render, role)
                    .map(|endpoint| (role, endpoint))
                    .map_err(|error| format!("read persisted {role:?} endpoint: {error}"))
            })
            .collect()
    }

    fn wait_for_policy_endpoints(
        driver: &mut WindowsAudioDriver,
        policy: &VerifiedAudioPolicyConfig,
        process: &ProcessIdentity,
        roles: &[AudioRole; 3],
        expected: &BTreeMap<AudioRole, Option<String>>,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut last = String::from("no policy observation");
        while Instant::now() < deadline {
            if let Err(error) = driver.refresh() {
                last = format!("refresh failed: {error}");
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            match read_policy_endpoints(policy, process, *roles) {
                Ok(actual) if policy_endpoints_match(expected, &actual) => return Ok(()),
                Ok(actual) => last = format!("actual={actual:?}"),
                Err(error) => last = error,
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(format!(
            "persisted application endpoint transaction did not reach expected values: expected={expected:?}, {last}"
        ))
    }

    fn wait_for_policy_snapshot(
        policy: &VerifiedAudioPolicyConfig,
        process: &ProcessIdentity,
        roles: &[AudioRole; 3],
        expected: &BTreeMap<AudioRole, Option<String>>,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut last = String::from("no policy observation");
        while Instant::now() < deadline {
            match read_policy_endpoints(policy, process, *roles) {
                Ok(actual) if policy_endpoints_match(expected, &actual) => return Ok(()),
                Ok(actual) => last = format!("actual={actual:?}"),
                Err(error) => last = error,
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Err(format!(
            "persisted application endpoint restore did not reach expected values: expected={expected:?}, {last}"
        ))
    }

    fn policy_endpoints_match(
        expected: &BTreeMap<AudioRole, Option<String>>,
        actual: &BTreeMap<AudioRole, Option<String>>,
    ) -> bool {
        expected.iter().all(|(role, expected_endpoint)| {
            actual.get(role).is_some_and(|actual_endpoint| {
                match (expected_endpoint, actual_endpoint) {
                    (None, None) => true,
                    (Some(expected), Some(actual)) => expected.eq_ignore_ascii_case(actual),
                    _ => false,
                }
            })
        })
    }

    fn spawn_isolated_helper(
        helper: &std::path::Path,
        app_render_id: &str,
    ) -> Result<Child, String> {
        spawn_tone_helper(helper, Some(app_render_id))
    }

    fn spawn_default_helper(helper: &std::path::Path) -> Result<Child, String> {
        spawn_tone_helper(helper, None)
    }

    /// Ten-minute helper for operator-assisted tests: the standard 60 s
    /// helper could expire while waiting on a human act.
    fn spawn_long_helper(
        helper: &std::path::Path,
        reopen_after_ms: Option<u64>,
    ) -> Result<Child, String> {
        let mut command = Command::new(helper);
        command.args([
            "--duration-ms",
            "600000",
            "--frequency",
            "1000",
            "--amplitude",
            "0.25",
        ]);
        if let Some(reopen_after_ms) = reopen_after_ms {
            command.args(["--reopen-after-ms", &reopen_after_ms.to_string()]);
        }
        command
            .spawn()
            .map_err(|error| format!("start long-lived tone helper: {error}"))
    }

    fn spawn_default_helper_with_reopen(
        helper: &std::path::Path,
        reopen_after_ms: u64,
    ) -> Result<Child, String> {
        spawn_tone_helper_with_reopen(helper, None, Some(reopen_after_ms))
    }

    fn spawn_tone_helper(
        helper: &std::path::Path,
        render_id: Option<&str>,
    ) -> Result<Child, String> {
        spawn_tone_helper_with_reopen(helper, render_id, None)
    }

    fn spawn_tone_helper_with_reopen(
        helper: &std::path::Path,
        render_id: Option<&str>,
        reopen_after_ms: Option<u64>,
    ) -> Result<Child, String> {
        let mut command = Command::new(helper);
        command.args([
            "--duration-ms",
            "60000",
            "--frequency",
            "1000",
            "--amplitude",
            "0.25",
        ]);
        if let Some(render_id) = render_id {
            command.args(["--render-id", render_id]);
        }
        if let Some(reopen_after_ms) = reopen_after_ms {
            command.args(["--reopen-after-ms", &reopen_after_ms.to_string()]);
        }
        command
            .spawn()
            .map_err(|error| format!("start tone helper: {error}"))
    }

    fn wait_for_application_route(
        driver: &mut WindowsAudioDriver,
        active: bool,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            driver.refresh().map_err(|error| error.to_string())?;
            let carrying_audio = driver
                .route_metrics()
                .into_iter()
                .any(|(_, metrics)| metrics.frames_processed > 0);
            let report_active = driver.windows_audio_report().contains("state=Active");
            if (active && carrying_audio && report_active) || (!active && !carrying_audio) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "application route did not reach active={active}: carrying_audio={carrying_audio}, report_active={report_active}\n{}",
                    driver.windows_audio_report()
                ));
            }
            std::thread::sleep(Duration::from_millis(75));
        }
    }

    fn route_frames_total(driver: &WindowsAudioDriver) -> u64 {
        driver
            .route_metrics()
            .into_iter()
            .fold(0u64, |total, (_, metrics)| {
                total.saturating_add(metrics.frames_processed)
            })
    }

    /// Post-churn variant of `wait_for_application_route`: `frames_processed`
    /// is a monotonic lifetime counter, so after an unplug/replug cycle a
    /// plain `> 0` check passes on stale pre-churn counts. Require the count
    /// to actually advance past this call's baseline instead.
    fn wait_for_route_frames_advancing(
        driver: &mut WindowsAudioDriver,
        min_delta: u64,
        timeout: Duration,
    ) -> Result<(), String> {
        let baseline = route_frames_total(driver);
        let deadline = Instant::now() + timeout;
        loop {
            driver.refresh().map_err(|error| error.to_string())?;
            let current = route_frames_total(driver);
            let advanced = current.saturating_sub(baseline);
            let report_active = driver.windows_audio_report().contains("state=Active");
            if report_active && advanced >= min_delta {
                println!(
                    "route frames advanced after churn: baseline={baseline} current={current}"
                );
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "route frames did not advance after churn: baseline={baseline} current={current} report_active={report_active}\n{}",
                    driver.windows_audio_report()
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn run_local_output_test() -> Result<(), String> {
        let helper = std::env::var_os("CARGO_BIN_EXE_windows-audio-test-tone")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "Cargo did not provide the tone helper executable".to_owned())?;
        let enumerator: Audio::IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| format!("create MMDeviceEnumerator: {error}"))?;
        let physical_render = choose_external_render(&enumerator)?;
        let mut tone = Command::new(&helper)
            .args([
                "--duration-ms",
                "60000",
                "--frequency",
                "1000",
                "--amplitude",
                "0.25",
                "--render-id",
                physical_render.id.as_str(),
            ])
            .spawn()
            .map_err(|error| format!("start deterministic WASAPI tone: {error}"))?;
        let result = (|| -> Result<(), String> {
            let mut driver = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
            let loopback = open_loopback_stream(&physical_render.device)?;
            let loopback_client =
                unsafe { loopback.client.GetService::<Audio::IAudioCaptureClient>() }
                    .map_err(|error| format!("get physical render loopback service: {error}"))?;
            unsafe {
                loopback
                    .client
                    .Start()
                    .map_err(|error| format!("start physical render loopback: {error}"))?;
            }
            let baseline =
                observe_loopback_tone(&loopback, &loopback_client, Duration::from_secs(2))?;

            let selector = wait_for_application_selector(&mut driver)?;
            driver
                .relay_set_send_source(RelaySendSource::Application(selector.clone()))
                .map_err(|error| format!("select helper process source: {error}"))?;
            let session = driver
                .relay_connect_mode(
                    "127.0.0.1:9".parse().expect("discard target is valid"),
                    "123456",
                    RelayMode::Emitter,
                    1,
                )
                .map_err(|error| format!("start local application relay: {error}"))?;
            let active_deadline = Instant::now() + Duration::from_secs(8);
            while Instant::now() < active_deadline {
                driver.refresh().map_err(|error| error.to_string())?;
                if driver.relay_devices_active() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if !driver.relay_devices_active() {
                return Err("application relay worker did not become active".into());
            }
            let during =
                observe_loopback_tone(&loopback, &loopback_client, Duration::from_secs(2))?;
            let _ = driver.relay_disconnect(session);
            if during < baseline * 0.5 || during < 0.01 {
                return Err(format!(
                    "local session peak changed while relay was active: baseline={baseline:.4}, during={during:.4}, selector={selector}"
                ));
            }
            println!(
                "ordinary application relay local output: selector={selector}, baseline_peak={baseline:.4}, during_peak={during:.4}"
            );
            Ok(())
        })();
        stop_child(&mut tone);
        result
    }

    fn observe_loopback_tone(
        stream: &AudioStream,
        client: &Audio::IAudioCaptureClient,
        timeout: Duration,
    ) -> Result<f32, String> {
        let deadline = Instant::now() + timeout;
        let mut probe = ToneProbe::new(stream.sample_rate);
        while Instant::now() < deadline {
            let _ = drain_capture(stream, client, Some(&mut probe))?;
            std::thread::sleep(Duration::from_millis(20));
        }
        if probe.sample_count == 0 || probe.invalid_samples != 0 || probe.amplitude(0) < 0.01 {
            return Err(format!(
                "physical render loopback stayed silent: frames={}, peak={:.6}, 1 kHz={:.6}, invalid={}",
                probe.sample_count,
                probe.peak,
                probe.amplitude(0),
                probe.invalid_samples,
            ));
        }
        Ok(probe.amplitude(0))
    }

    fn run_effect_test() -> Result<(), String> {
        let helper = std::env::var_os("CARGO_BIN_EXE_windows-audio-test-tone")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "Cargo did not provide the tone helper executable".to_owned())?;
        let enumerator: Audio::IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| format!("create MMDeviceEnumerator: {error}"))?;
        let app_render = wait_for_role(
            &enumerator,
            Flow::Render,
            APP_RENDER_ROLE,
            Duration::from_secs(8),
        )?;
        let physical_render = choose_external_render(&enumerator)?;
        println!(
            "effect helper endpoint: app-render {:?}; physical render {:?} ({})",
            app_render.id, physical_render.id, physical_render.name
        );
        let mut tone = Command::new(&helper)
            .args([
                "--duration-ms",
                "60000",
                "--frequency",
                "1000",
                "--amplitude",
                "0.25",
                "--render-id",
                app_render.id.as_str(),
            ])
            .spawn()
            .map_err(|error| format!("start deterministic WASAPI tone: {error}"))?;

        let result = (|| -> Result<(), String> {
            let mut driver = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
            let (source, destination) = wait_for_isolated_route(
                &mut driver,
                &physical_render.name,
                Duration::from_secs(10),
            )?;

            let loopback = open_loopback_stream(&physical_render.device)?;
            let loopback_client =
                unsafe { loopback.client.GetService::<Audio::IAudioCaptureClient>() }
                    .map_err(|error| format!("get physical render loopback service: {error}"))?;
            unsafe {
                loopback
                    .client
                    .Start()
                    .map_err(|error| format!("start physical render loopback: {error}"))?;
            }

            let mut parameters = BTreeMap::new();
            // The helper's 0.25 amplitude is below the 0 dBFS threshold, so
            // an enabled gate must suppress it while still carrying frames
            // through the route.
            parameters.insert("threshold-db".into(), 0.0);
            parameters.insert("attack-ms".into(), 0.0);
            parameters.insert("hold-ms".into(), 0.0);
            parameters.insert("release-ms".into(), 0.0);
            let ticket = driver
                .begin_create_effect(EffectCreateRequest {
                    instance_id: "live-isolated-gate".into(),
                    effect_id: "builtin.noise-gate".into(),
                    module_path: None,
                    enabled: true,
                    parameters,
                    channel_policy: pw_graph_effects::ChannelPolicy::Auto,
                    target: EffectTarget::Standalone {
                        position: [240.0, 160.0],
                    },
                })
                .map_err(|error| format!("queue live noise gate: {error}"))?;
            // Windows prepares effects off the control thread. Do not connect
            // graph ports until the matching request has actually activated.
            let activation_deadline = Instant::now() + Duration::from_secs(10);
            let instance = 'activation: loop {
                for event in driver
                    .poll_effect_events()
                    .map_err(|error| format!("poll live noise gate: {error}"))?
                {
                    match event {
                        EffectEvent::Ready {
                            ticket: ready,
                            instance,
                        } if ready == ticket => {
                            break 'activation *instance;
                        }
                        EffectEvent::Failed {
                            ticket: failed,
                            error,
                        } if failed == ticket => {
                            return Err(format!("prepare live noise gate: {error}"));
                        }
                        EffectEvent::Cancelled { ticket: cancelled } if cancelled == ticket => {
                            return Err("live noise gate preparation was cancelled".into());
                        }
                        _ => {}
                    }
                }
                if Instant::now() >= activation_deadline {
                    driver
                        .cancel_effect(ticket)
                        .map_err(|error| format!("cancel timed-out noise gate: {error}"))?;
                    return Err("live noise gate did not activate before the timeout".into());
                }
                std::thread::sleep(Duration::from_millis(2));
            };
            let input_link = driver
                .connect(source, instance.input_port)
                .map_err(|error| format!("connect isolated process to effect: {error}"))?;
            let output_link = driver
                .connect(instance.output_port, destination)
                .map_err(|error| format!("connect effect to physical render: {error}"))?;

            let mut suppressed = ToneProbe::new(loopback.sample_rate);
            let mut suppressed_frames = 0_u64;
            let suppressed_deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < suppressed_deadline {
                suppressed_frames += u64::from(drain_capture(
                    &loopback,
                    &loopback_client,
                    Some(&mut suppressed),
                )?);
                std::thread::sleep(Duration::from_millis(2));
            }
            let metrics = driver
                .route_metrics()
                .into_iter()
                .find(|(link, _)| *link == input_link.id || *link == output_link.id)
                .map(|(_, metrics)| metrics);
            if suppressed_frames == 0
                || suppressed.peak > 0.002
                || suppressed.amplitude(0) > 0.002
                || suppressed.invalid_samples != 0
                || metrics.is_none_or(|metrics| metrics.frames_processed == 0)
            {
                return Err(format!(
                    "enabled gate did not suppress a live routed tone: frames={suppressed_frames}, peak={:.6}, 1 kHz={:.6}, invalid={}, metrics={metrics:?}",
                    suppressed.peak,
                    suppressed.amplitude(0),
                    suppressed.invalid_samples,
                ));
            }
            println!(
                "effect applied: {} frames, peak {:.6}, 1 kHz {:.6}",
                suppressed_frames,
                suppressed.peak,
                suppressed.amplitude(0)
            );

            driver
                .set_effect_enabled("live-isolated-gate", false)
                .map_err(|error| format!("bypass live noise gate: {error}"))?;
            let mut restored = ToneProbe::new(loopback.sample_rate);
            let mut restored_frames = 0_u64;
            let restored_deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < restored_deadline {
                restored_frames += u64::from(drain_capture(
                    &loopback,
                    &loopback_client,
                    Some(&mut restored),
                )?);
                std::thread::sleep(Duration::from_millis(2));
            }
            if restored_frames == 0
                || restored.peak < 0.01
                || restored.amplitude(0) < 0.01
                || restored.invalid_samples != 0
            {
                return Err(format!(
                    "bypassing the live gate did not restore the helper tone: frames={restored_frames}, peak={:.6}, 1 kHz={:.6}, invalid={}",
                    restored.peak,
                    restored.amplitude(0),
                    restored.invalid_samples,
                ));
            }
            println!(
                "effect bypass restored: {} frames, peak {:.4}, 1 kHz {:.4}",
                restored_frames,
                restored.peak,
                restored.amplitude(0)
            );

            driver
                .remove_effect("live-isolated-gate")
                .map_err(|error| format!("remove live noise gate: {error}"))?;
            Ok(())
        })();
        stop_child(&mut tone);
        result
    }

    fn wait_for_isolated_route(
        driver: &mut WindowsAudioDriver,
        destination_name: &str,
        timeout: Duration,
    ) -> Result<(pw_graph_core::PortId, pw_graph_core::PortId), String> {
        let deadline = Instant::now() + timeout;
        loop {
            driver.refresh().map_err(|error| error.to_string())?;
            let source = driver.graph().nodes.values().find_map(|node| {
                if !node.name.eq_ignore_ascii_case("windows-audio-test-tone")
                    || !driver
                        .process_audio_capabilities(node.id)
                        .is_some_and(|capabilities| capabilities.mutable_route)
                {
                    return None;
                }
                node.ports.iter().copied().find(|port| {
                    driver
                        .graph()
                        .port(*port)
                        .is_some_and(|port| port.direction.is_source())
                })
            });
            let destination = driver.graph().nodes.values().find_map(|node| {
                if !node.name.eq_ignore_ascii_case(destination_name) {
                    return None;
                }
                node.ports.iter().copied().find(|port| {
                    driver
                        .graph()
                        .port(*port)
                        .is_some_and(|port| port.direction.is_sink())
                })
            });
            if let (Some(source), Some(destination)) = (source, destination) {
                return Ok((source, destination));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "isolated helper source or physical destination {destination_name:?} did not appear"
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn run_test() -> Result<(), String> {
        let helper = std::env::var_os("CARGO_BIN_EXE_windows-audio-test-tone")
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "Cargo did not provide the tone helper executable".to_owned())?;
        let enumerator: Audio::IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|error| format!("create MMDeviceEnumerator: {error}"))?;
        let helper_render_id = choose_external_render_id(&enumerator)?;
        println!("tone helper render endpoint: {helper_render_id}");
        let mut tone = Command::new(&helper)
            .args([
                "--duration-ms",
                "60000",
                "--frequency",
                "1000",
                "--amplitude",
                "0.25",
                "--sessions",
                "1",
                "--render-id",
                helper_render_id.as_str(),
            ])
            .spawn()
            .map_err(|error| format!("start deterministic WASAPI tone: {error}"))?;

        let result = (|| -> Result<(), String> {
            let mut host = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
            // The receiver renders peer PCM into the driver's render cable;
            // the ordinary capture client below reads the paired microphone.
            host.relay_set_receive_sink(RelayReceiveSink::VirtualMicrophone)
                .map_err(|error| format!("select Relay Microphone sink: {error}"))?;
            let port = host
                .relay_start_host(RelayHostRequest {
                    device_id: "qpwgraph-relay-microphone-test-host".into(),
                    trusted_peers: Vec::new(),
                    trust_new_peers: true,
                    device_name: "qpwgraph-rs Relay Microphone test host".into(),
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

            let relay_capture = wait_for_role(
                &enumerator,
                Flow::Capture,
                RELAY_CAPTURE_ROLE,
                Duration::from_secs(8),
            )?;
            let app_render = wait_for_role(
                &enumerator,
                Flow::Render,
                APP_RENDER_ROLE,
                Duration::from_secs(8),
            )?;
            println!(
                "Relay Microphone endpoints: capture {:?} ({}), app render {:?} ({})",
                relay_capture.name, relay_capture.id, app_render.name, app_render.id
            );

            let capture = open_stream(&relay_capture.device, Flow::Capture)?;
            let render = open_stream(&app_render.device, Flow::Render)?;
            let capture_client =
                unsafe { capture.client.GetService::<Audio::IAudioCaptureClient>() }
                    .map_err(|error| format!("get Relay Microphone capture service: {error}"))?;
            let render_client = unsafe { render.client.GetService::<Audio::IAudioRenderClient>() }
                .map_err(|error| format!("get app-render service: {error}"))?;
            unsafe {
                capture
                    .client
                    .Start()
                    .map_err(|error| format!("start Relay Microphone capture: {error}"))?;
                render
                    .client
                    .Start()
                    .map_err(|error| format!("start app-render cable: {error}"))?;
            }

            let mut client = WindowsAudioDriver::new().map_err(|error| error.to_string())?;
            let selector = wait_for_application_selector(&mut client)?;
            client
                .relay_set_send_source(RelaySendSource::Application(selector.clone()))
                .map_err(|error| format!("select helper process source: {error}"))?;
            let cycles = std::env::var("PW_GRAPH_TEST_RELAY_MICROPHONE_CYCLES")
                .ok()
                .map(|value| {
                    value.parse::<usize>().map_err(|error| {
                        format!("invalid relay microphone cycle count {value:?}: {error}")
                    })
                })
                .transpose()?
                .unwrap_or(1);
            if !(1..=8).contains(&cycles) {
                return Err(format!(
                    "relay microphone cycle count must be between 1 and 8, got {cycles}"
                ));
            }
            let first_session = client
                .relay_connect_mode(
                    format!("127.0.0.1:{port}")
                        .parse()
                        .map_err(|error| format!("invalid local relay target: {error}"))?,
                    "123456",
                    RelayMode::Emitter,
                    1,
                )
                .map_err(|error| format!("connect local relay emitter: {error}"))?;
            wait_for_active_session(
                &mut host,
                &mut client,
                first_session,
                Duration::from_secs(10),
            )?;

            // The 1 kHz helper is the peer signal. Simultaneously render a
            // distinct 2 kHz tone into the app cable: that tone must never
            // appear at Relay Microphone.
            let mut app_phase = 0.0_f64;
            let mut peer_probe = ToneProbe::new(capture.sample_rate);
            let mut peer_frames = 0_u64;
            let active_deadline = Instant::now() + Duration::from_secs(4);
            while Instant::now() < active_deadline {
                fill_tone(&render, &render_client, &mut app_phase, 2_000.0)?;
                let frames = drain_capture(&capture, &capture_client, Some(&mut peer_probe))?;
                peer_frames += u64::from(frames);
                std::thread::sleep(Duration::from_millis(2));
            }
            require_peer_signal("first connection", peer_frames, &peer_probe)?;
            println!(
                "first connection: {} frames, peak {:.4}, 1 kHz {:.4}, 2 kHz {:.4}",
                peer_frames,
                peer_probe.peak,
                peer_probe.amplitude(0),
                peer_probe.amplitude(1)
            );

            RelayProbeContext {
                host: &mut host,
                client: &mut client,
                capture: &capture,
                capture_client: &capture_client,
                render: &render,
                render_client: &render_client,
                app_phase: &mut app_phase,
            }
            .disconnect_and_assert_silence(first_session, "cycle 1")?;

            let second_session = client
                .relay_connect_mode(
                    format!("127.0.0.1:{port}")
                        .parse()
                        .map_err(|error| format!("invalid reconnect target: {error}"))?,
                    "123456",
                    RelayMode::Emitter,
                    2,
                )
                .map_err(|error| format!("reconnect local relay emitter: {error}"))?;
            wait_for_active_session(
                &mut host,
                &mut client,
                second_session,
                Duration::from_secs(10),
            )?;
            let mut reconnect_probe = ToneProbe::new(capture.sample_rate);
            let mut reconnect_frames = 0_u64;
            let reconnect_deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < reconnect_deadline {
                fill_tone(&render, &render_client, &mut app_phase, 2_000.0)?;
                reconnect_frames += u64::from(drain_capture(
                    &capture,
                    &capture_client,
                    Some(&mut reconnect_probe),
                )?);
                std::thread::sleep(Duration::from_millis(2));
            }
            require_peer_signal("reconnect", reconnect_frames, &reconnect_probe)?;
            if reconnect_probe.amplitude(1) > 0.01 {
                return Err(format!(
                    "app cable leaked into Relay Microphone after reconnect: 2 kHz amplitude {:.4}",
                    reconnect_probe.amplitude(1)
                ));
            }
            println!(
                "reconnect: {} frames, peak {:.4}, 1 kHz {:.4}, 2 kHz {:.4}; driver stayed running",
                reconnect_frames,
                reconnect_probe.peak,
                reconnect_probe.amplitude(0),
                reconnect_probe.amplitude(1)
            );
            let mut current_session = second_session;
            for cycle in 2..=cycles {
                RelayProbeContext {
                    host: &mut host,
                    client: &mut client,
                    capture: &capture,
                    capture_client: &capture_client,
                    render: &render,
                    render_client: &render_client,
                    app_phase: &mut app_phase,
                }
                .disconnect_and_assert_silence(current_session, &format!("cycle {cycle}"))?;
                current_session = client
                    .relay_connect_mode(
                        format!("127.0.0.1:{port}")
                            .parse()
                            .map_err(|error| format!("invalid stress reconnect target: {error}"))?,
                        "123456",
                        RelayMode::Emitter,
                        cycle as u64 + 1,
                    )
                    .map_err(|error| format!("stress reconnect local relay emitter: {error}"))?;
                wait_for_active_session(
                    &mut host,
                    &mut client,
                    current_session,
                    Duration::from_secs(10),
                )?;
                let mut cycle_probe = ToneProbe::new(capture.sample_rate);
                let mut cycle_frames = 0_u64;
                let cycle_deadline = Instant::now() + Duration::from_secs(3);
                while Instant::now() < cycle_deadline {
                    fill_tone(&render, &render_client, &mut app_phase, 2_000.0)?;
                    cycle_frames += u64::from(drain_capture(
                        &capture,
                        &capture_client,
                        Some(&mut cycle_probe),
                    )?);
                    std::thread::sleep(Duration::from_millis(2));
                }
                require_peer_signal(
                    &format!("reconnect cycle {cycle}"),
                    cycle_frames,
                    &cycle_probe,
                )?;
                println!(
                    "reconnect cycle {cycle}: {cycle_frames} frames, peak {:.4}, 1 kHz {:.4}, 2 kHz {:.4}; driver stayed running",
                    cycle_probe.peak,
                    cycle_probe.amplitude(0),
                    cycle_probe.amplitude(1)
                );
            }
            let _ = client.relay_disconnect(current_session);
            let _ = host.relay_stop_host();
            Ok(())
        })();
        stop_child(&mut tone);
        result
    }

    fn wait_for_application_selector(driver: &mut WindowsAudioDriver) -> Result<String, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            driver.refresh().map_err(|error| error.to_string())?;
            if let Some(source) = driver
                .relay_send_sources()
                .into_iter()
                .find(|source| source.name.eq_ignore_ascii_case("windows-audio-test-tone"))
            {
                return source
                    .id
                    .strip_prefix("application:")
                    .map(str::to_owned)
                    .ok_or_else(|| "helper application source had an invalid ID".to_owned());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err("the live helper session did not appear as an application relay source".into())
    }

    fn wait_for_active_session(
        host: &mut WindowsAudioDriver,
        client: &mut WindowsAudioDriver,
        session: pw_graph_backend::RelaySessionId,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            host.refresh().map_err(|error| error.to_string())?;
            client.refresh().map_err(|error| error.to_string())?;
            let active = |driver: &WindowsAudioDriver| {
                driver
                    .relay_status()
                    .sessions
                    .iter()
                    .any(|status| status.id == session && status.control_state == "active")
            };
            if active(host) && active(client) && client.relay_devices_active() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(format!(
            "relay session {session} did not become active (host={:?}, client={:?}, client_endpoint_active={})",
            host.relay_status(),
            client.relay_status(),
            client.relay_devices_active()
        ))
    }

    fn wait_for_disconnected(
        host: &mut WindowsAudioDriver,
        client: &mut WindowsAudioDriver,
        session: pw_graph_backend::RelaySessionId,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            host.refresh().map_err(|error| error.to_string())?;
            client.refresh().map_err(|error| error.to_string())?;
            let _ = host.relay_events();
            let _ = client.relay_events();
            let host_status = host.relay_status();
            let client_status = client.relay_status();
            if host_status.sessions.is_empty() && client_status.sessions.is_empty() {
                println!(
                    "relay disconnect observed: host sessions={:?}, client sessions={:?}, host_endpoint_active={}, client_endpoint_active={}",
                    host_status.sessions,
                    client_status.sessions,
                    host.relay_devices_active(),
                    client.relay_devices_active()
                );
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(format!(
            "relay session {session} stayed present after disconnect (host={:?}, client={:?})",
            host.relay_status(),
            client.relay_status()
        ))
    }

    fn require_peer_signal(label: &str, frames: u64, probe: &ToneProbe) -> Result<(), String> {
        if frames == 0
            || probe.peak < 0.01
            || probe.amplitude(0) < 0.01
            || probe.amplitude(1) > 0.01
            || probe.invalid_samples != 0
        {
            return Err(format!(
                "{label} did not deliver isolated peer audio: {frames} frames, peak {:.6}, 1 kHz {:.6}, 2 kHz {:.6}, invalid {}",
                probe.peak,
                probe.amplitude(0),
                probe.amplitude(1),
                probe.invalid_samples
            ));
        }
        Ok(())
    }

    struct RelayProbeContext<'a> {
        host: &'a mut WindowsAudioDriver,
        client: &'a mut WindowsAudioDriver,
        capture: &'a AudioStream,
        capture_client: &'a Audio::IAudioCaptureClient,
        render: &'a AudioStream,
        render_client: &'a Audio::IAudioRenderClient,
        app_phase: &'a mut f64,
    }

    impl RelayProbeContext<'_> {
        fn disconnect_and_assert_silence(
            &mut self,
            session: pw_graph_backend::RelaySessionId,
            label: &str,
        ) -> Result<(), String> {
            self.client
                .relay_disconnect(session)
                .map_err(|error| format!("disconnect relay {label}: {error}"))?;
            wait_for_disconnected(self.host, self.client, session, Duration::from_secs(5))?;

            // Give the render worker and the driver cable time to retire any
            // in-flight packets, then keep the app cable active while testing
            // that a disconnected relay is silent and isolated. Keep draining
            // during this interval so stale packets cannot pollute the next
            // measurement.
            let mut settle_probe = ToneProbe::new(self.capture.sample_rate);
            let settle_deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < settle_deadline {
                fill_tone(self.render, self.render_client, self.app_phase, 2_000.0)?;
                let _ = drain_capture(self.capture, self.capture_client, Some(&mut settle_probe))?;
                std::thread::sleep(Duration::from_millis(2));
            }
            println!(
                "disconnect settle ({label}): {} frames, peak {:.4}, 1 kHz {:.4}, 2 kHz {:.4}",
                settle_probe.sample_count,
                settle_probe.peak,
                settle_probe.amplitude(0),
                settle_probe.amplitude(1)
            );

            let mut silent_probe = ToneProbe::new(self.capture.sample_rate);
            let mut silent_frames = 0_u64;
            let silent_deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < silent_deadline {
                fill_tone(self.render, self.render_client, self.app_phase, 2_000.0)?;
                silent_frames += u64::from(drain_capture(
                    self.capture,
                    self.capture_client,
                    Some(&mut silent_probe),
                )?);
                std::thread::sleep(Duration::from_millis(2));
            }
            if silent_frames == 0
                || silent_probe.peak > 0.001
                || silent_probe.amplitude(0) > 0.001
                || silent_probe.amplitude(1) > 0.001
                || silent_probe.invalid_samples != 0
            {
                return Err(format!(
                    "relay disconnect {label} was not silent/isolated: {silent_frames} frames, peak {:.6}, 1 kHz {:.6}, 2 kHz {:.6}",
                    silent_probe.peak,
                    silent_probe.amplitude(0),
                    silent_probe.amplitude(1)
                ));
            }
            println!(
                "disconnect silence ({label}): {silent_frames} frames, peak {:.6}",
                silent_probe.peak
            );
            Ok(())
        }
    }

    fn stop_child(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    fn wait_for_role(
        enumerator: &Audio::IMMDeviceEnumerator,
        flow: Flow,
        role: &str,
        timeout: Duration,
    ) -> Result<Endpoint, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let endpoints = enumerate(enumerator, flow)?;
            let matches: Vec<_> = endpoints
                .into_iter()
                .filter(|endpoint| {
                    property_string(&endpoint.device, &ROLE_KEY as *const PROPERTYKEY)
                        .is_some_and(|value| value.eq_ignore_ascii_case(role))
                })
                .collect();
            match matches.as_slice() {
                [endpoint] => return Ok(endpoint.clone()),
                [] if Instant::now() < deadline => {}
                [] => return Err(format!("no active {flow:?} endpoint has role {role:?}")),
                _ => {
                    return Err(format!(
                        "multiple active {flow:?} endpoints have role {role:?}"
                    ))
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn choose_external_render_id(
        enumerator: &Audio::IMMDeviceEnumerator,
    ) -> Result<String, String> {
        choose_external_render(enumerator).map(|endpoint| endpoint.id)
    }

    /// Churn-test render selection. `PW_GRAPH_TEST_WINDOWS_CHURN_RENDER`
    /// optionally pins the physical endpoint by friendly-name substring, so
    /// the operator-assisted run targets the device they can physically
    /// unplug instead of whatever enumerates first (a Bluetooth endpoint
    /// never leaves on a USB unplug). Unset means the default first-external
    /// pick.
    fn choose_churn_render(enumerator: &Audio::IMMDeviceEnumerator) -> Result<Endpoint, String> {
        let Ok(want) = std::env::var("PW_GRAPH_TEST_WINDOWS_CHURN_RENDER") else {
            return choose_external_render(enumerator);
        };
        if want.trim().is_empty() {
            return choose_external_render(enumerator);
        }
        let endpoints = enumerate(enumerator, Flow::Render)?;
        let Some(hit) = endpoints.into_iter().find(|endpoint| {
            let provider_role = property_string(&endpoint.device, &ROLE_KEY as *const PROPERTYKEY)
                .is_some_and(|role| {
                    role.eq_ignore_ascii_case("app-render")
                        || role.eq_ignore_ascii_case("relay-render")
                });
            !provider_role
                && !endpoint.name.to_ascii_lowercase().contains("qpwgraph")
                && endpoint.name.contains(&want)
        }) else {
            return Err(format!(
                "no active non-QPWGraph render endpoint name contains {want:?}"
            ));
        };
        println!("churn render pinned by env: '{}' ({})", hit.name, hit.id);
        Ok(hit)
    }

    fn choose_external_render(enumerator: &Audio::IMMDeviceEnumerator) -> Result<Endpoint, String> {
        let endpoints = enumerate(enumerator, Flow::Render)?;
        endpoints
            .into_iter()
            .find(|endpoint| {
                let provider_role =
                    property_string(&endpoint.device, &ROLE_KEY as *const PROPERTYKEY).is_some_and(
                        |role| {
                            role.eq_ignore_ascii_case("app-render")
                                || role.eq_ignore_ascii_case("relay-render")
                        },
                    );
                !provider_role && !endpoint.name.to_ascii_lowercase().contains("qpwgraph")
            })
            .ok_or_else(|| {
                "no non-QPWGraph active render endpoint is available for the tone helper".into()
            })
    }

    fn enumerate(
        enumerator: &Audio::IMMDeviceEnumerator,
        flow: Flow,
    ) -> Result<Vec<Endpoint>, String> {
        let collection =
            unsafe { enumerator.EnumAudioEndpoints(flow.data_flow(), Audio::DEVICE_STATE_ACTIVE) }
                .map_err(|error| format!("enumerate {} endpoints: {error}", flow.name()))?;
        let count = unsafe { collection.GetCount() }
            .map_err(|error| format!("count {} endpoints: {error}", flow.name()))?;
        let mut endpoints = Vec::with_capacity(count as usize);
        for index in 0..count {
            let device = unsafe { collection.Item(index) }
                .map_err(|error| format!("read {} endpoint {index}: {error}", flow.name()))?;
            let id = endpoint_id(&device)?;
            let name = property_string(
                &device,
                &Properties::DEVPKEY_Device_FriendlyName as *const _ as *const PROPERTYKEY,
            )
            .unwrap_or_else(|| id.clone());
            endpoints.push(Endpoint { device, id, name });
        }
        Ok(endpoints)
    }

    fn endpoint_id(device: &Audio::IMMDevice) -> Result<String, String> {
        let value =
            unsafe { device.GetId() }.map_err(|error| format!("read endpoint id: {error}"))?;
        let id =
            unsafe { value.to_string() }.map_err(|error| format!("decode endpoint id: {error}"))?;
        unsafe { Com::CoTaskMemFree(Some(value.0 as *const c_void)) };
        Ok(id)
    }

    fn property_string(device: &Audio::IMMDevice, key: *const PROPERTYKEY) -> Option<String> {
        let store: IPropertyStore = unsafe { device.OpenPropertyStore(STGM_READ).ok()? };
        let mut value = unsafe { store.GetValue(key).ok()? };
        let variant = unsafe { &value.Anonymous.Anonymous };
        if variant.vt != VT_LPWSTR {
            let _ = unsafe { Com::StructuredStorage::PropVariantClear(&mut value) };
            return None;
        }
        let pointer = unsafe { *(&variant.Anonymous as *const _ as *const *const u16) };
        if pointer.is_null() {
            let _ = unsafe { Com::StructuredStorage::PropVariantClear(&mut value) };
            return None;
        }
        let mut length = 0usize;
        while length < 32_768 && unsafe { *pointer.add(length) } != 0 {
            length += 1;
        }
        let result = (length < 32_768).then(|| {
            String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(pointer, length) })
        });
        let _ = unsafe { Com::StructuredStorage::PropVariantClear(&mut value) };
        result.filter(|text| !text.is_empty())
    }

    fn open_stream(device: &Audio::IMMDevice, flow: Flow) -> Result<AudioStream, String> {
        open_stream_with_flags(device, flow, 0)
    }

    fn open_loopback_stream(device: &Audio::IMMDevice) -> Result<AudioStream, String> {
        open_stream_with_flags(device, Flow::Render, Audio::AUDCLNT_STREAMFLAGS_LOOPBACK)
    }

    fn open_stream_with_flags(
        device: &Audio::IMMDevice,
        flow: Flow,
        flags: u32,
    ) -> Result<AudioStream, String> {
        let client: Audio::IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(|error| format!("activate {} client: {error}", flow.name()))?;
        let format = unsafe { client.GetMixFormat() }
            .map_err(|error| format!("read {} mix format: {error}", flow.name()))?;
        if format.is_null() {
            return Err(format!(
                "{} endpoint returned a null mix format",
                flow.name()
            ));
        }
        let (sample_rate, channels, bits, tag, align, subtype) = unsafe {
            let tag = std::ptr::read_unaligned(std::ptr::addr_of!((*format).wFormatTag));
            let cb_size = std::ptr::read_unaligned(std::ptr::addr_of!((*format).cbSize));
            let subtype = if tag == KernelStreaming::WAVE_FORMAT_EXTENSIBLE as u16 && cb_size >= 22
            {
                Some(std::ptr::read_unaligned(std::ptr::addr_of!(
                    (*format.cast::<Audio::WAVEFORMATEXTENSIBLE>()).SubFormat
                )))
            } else {
                None
            };
            (
                std::ptr::read_unaligned(std::ptr::addr_of!((*format).nSamplesPerSec)),
                std::ptr::read_unaligned(std::ptr::addr_of!((*format).nChannels)),
                std::ptr::read_unaligned(std::ptr::addr_of!((*format).wBitsPerSample)),
                tag,
                std::ptr::read_unaligned(std::ptr::addr_of!((*format).nBlockAlign)),
                subtype,
            )
        };
        let is_float = match validate_pcm_format(tag, subtype, sample_rate, channels, bits, align) {
            Ok(value) => value,
            Err(error) => {
                unsafe { Com::CoTaskMemFree(Some(format.cast())) };
                return Err(format!(
                    "{} endpoint has malformed PCM format: {error}",
                    flow.name()
                ));
            }
        };
        unsafe {
            client
                .Initialize(
                    Audio::AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    2_000_000,
                    0,
                    format,
                    None,
                )
                .map_err(|error| {
                    Com::CoTaskMemFree(Some(format.cast()));
                    format!("initialize {} client: {error}", flow.name())
                })?;
        }
        let buffer_frames = match unsafe { client.GetBufferSize() } {
            Ok(frames) => frames,
            Err(error) => {
                unsafe { Com::CoTaskMemFree(Some(format.cast())) };
                return Err(format!("read {} buffer size: {error}", flow.name()));
            }
        };
        Ok(AudioStream {
            client,
            format,
            buffer_frames,
            sample_rate,
            channels,
            bits,
            block_align: align,
            is_float,
        })
    }

    fn fill_tone(
        stream: &AudioStream,
        client: &Audio::IAudioRenderClient,
        phase: &mut f64,
        frequency: f64,
    ) -> Result<u32, String> {
        let padding = unsafe { stream.client.GetCurrentPadding() }
            .map_err(|error| format!("read render padding: {error}"))?;
        let frames = stream.buffer_frames.saturating_sub(padding);
        if frames == 0 {
            return Ok(0);
        }
        let buffer = unsafe { client.GetBuffer(frames) }
            .map_err(|error| format!("acquire render buffer: {error}"))?;
        if buffer.is_null() {
            return Err("render endpoint returned a null buffer".into());
        }
        unsafe {
            let bytes_per_frame = usize::from(stream.block_align);
            let bytes_per_sample = usize::from(stream.bits).div_ceil(8);
            std::ptr::write_bytes(buffer, 0, frames as usize * bytes_per_frame);
            for frame in 0..frames as usize {
                let value = (*phase * std::f64::consts::TAU * frequency).sin() as f32 * 0.2;
                *phase += 1.0 / f64::from(stream.sample_rate);
                for channel in 0..usize::from(stream.channels) {
                    write_sample(
                        buffer.add(frame * bytes_per_frame + channel * bytes_per_sample),
                        bytes_per_sample,
                        stream.is_float,
                        value,
                    );
                }
            }
            client
                .ReleaseBuffer(frames, 0)
                .map_err(|error| format!("release render buffer: {error}"))?;
        }
        Ok(frames)
    }

    fn drain_capture(
        stream: &AudioStream,
        client: &Audio::IAudioCaptureClient,
        mut probe: Option<&mut ToneProbe>,
    ) -> Result<u32, String> {
        let mut total = 0_u32;
        loop {
            let available = unsafe { client.GetNextPacketSize() }
                .map_err(|error| format!("read capture packet size: {error}"))?;
            if available == 0 {
                break;
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0_u32;
            let mut flags = 0_u32;
            unsafe {
                client
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .map_err(|error| format!("acquire capture buffer: {error}"))?;
                if flags & Audio::AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    if let Some(probe) = probe.as_deref_mut() {
                        for _ in 0..frames {
                            probe.push(0.0);
                        }
                    }
                } else {
                    if data.is_null() {
                        let _ = client.ReleaseBuffer(frames);
                        return Err("capture endpoint returned a null non-silent buffer".into());
                    }
                    read_samples(data, stream, frames, probe.as_deref_mut());
                }
                client
                    .ReleaseBuffer(frames)
                    .map_err(|error| format!("release capture buffer: {error}"))?;
            }
            total = total.saturating_add(frames);
        }
        Ok(total)
    }

    unsafe fn read_samples(
        data: *const u8,
        stream: &AudioStream,
        frames: u32,
        mut probe: Option<&mut ToneProbe>,
    ) {
        let bytes_per_frame = usize::from(stream.block_align);
        let bytes_per_sample = usize::from(stream.bits).div_ceil(8);
        for frame in 0..frames as usize {
            let sample = data.add(frame * bytes_per_frame);
            let value = read_sample(sample, bytes_per_sample, stream.is_float);
            if let Some(probe) = probe.as_deref_mut() {
                probe.push(value);
            }
        }
    }

    unsafe fn write_sample(target: *mut u8, bytes: usize, is_float: bool, value: f32) {
        if is_float && bytes >= 4 {
            target.cast::<f32>().write_unaligned(value);
        } else if bytes == 1 {
            target.write((128.0 + value * 100.0) as u8);
        } else if bytes == 2 {
            target
                .cast::<i16>()
                .write_unaligned((value * 32_000.0) as i16);
        } else if bytes == 3 {
            let sample = (value * 8_388_607.0) as i32;
            target.write(sample as u8);
            target.add(1).write((sample >> 8) as u8);
            target.add(2).write((sample >> 16) as u8);
        } else if bytes >= 4 {
            target
                .cast::<i32>()
                .write_unaligned((value * 2_000_000_000.0) as i32);
        }
    }

    unsafe fn read_sample(data: *const u8, bytes: usize, is_float: bool) -> f32 {
        if is_float && bytes >= 4 {
            data.cast::<f32>().read_unaligned()
        } else if bytes == 1 {
            (f32::from(data.read()) - 128.0) / 128.0
        } else if bytes == 2 {
            f32::from(data.cast::<i16>().read_unaligned()) / 32_768.0
        } else if bytes == 3 {
            let raw = i32::from(data.read())
                | (i32::from(data.add(1).read()) << 8)
                | (i32::from(data.add(2).read()) << 16);
            let signed = if raw & 0x80_0000 != 0 {
                raw | !0xFF_FFFF
            } else {
                raw
            };
            signed as f32 / 8_388_608.0
        } else if bytes >= 4 {
            data.cast::<i32>().read_unaligned() as f32 / 2_147_483_648.0
        } else {
            0.0
        }
    }

    fn validate_pcm_format(
        tag: u16,
        subtype: Option<GUID>,
        rate: u32,
        channels: u16,
        bits: u16,
        align: u16,
    ) -> Result<bool, String> {
        const PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
        const FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
        let encoding = if tag == KernelStreaming::WAVE_FORMAT_EXTENSIBLE as u16 {
            subtype
        } else if tag == Multimedia::WAVE_FORMAT_IEEE_FLOAT as u16 {
            Some(FLOAT)
        } else if tag == 1 {
            Some(PCM)
        } else {
            None
        };
        let supported = (encoding == Some(FLOAT) && bits == 32)
            || (encoding == Some(PCM) && matches!(bits, 8 | 16 | 24 | 32));
        if !supported
            || rate == 0
            || channels == 0
            || u32::from(channels) * u32::from(bits / 8) != u32::from(align)
        {
            return Err(format!(
                "tag={tag:#x}, subtype={subtype:?}, {rate} Hz, {channels} channels, {bits} bits, alignment={align}"
            ));
        }
        Ok(encoding == Some(FLOAT))
    }
}
