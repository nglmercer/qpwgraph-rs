//! Windows endpoint smoke probe for the optional driver package.
//!
//! The probe is deliberately user-mode and does not install, remove, or
//! select a Windows default device. It enumerates active endpoints by display
//! name, MMDevice id, or provider-owned semantic role, opens a shared-mode
//! WASAPI client, starts it briefly, then stops and resets it. The optional
//! round-trip mode also writes a tone and requires non-silent capture. A
//! missing endpoint is an expected fail-closed result until the ACX package
//! has been installed on the test machine.

#[cfg(not(windows))]
fn main() {
    eprintln!("qpwgraph-audio-smoke requires a Windows test machine");
    std::process::exit(2);
}

#[cfg(windows)]
mod windows_smoke {
    use std::ffi::c_void;
    use std::time::{Duration, Instant};

    use windows::core::{Result as WindowsResult, GUID, PCWSTR};
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        CM_Get_DevNode_PropertyW, CM_Locate_DevNodeW, CM_LOCATE_DEVNODE_NORMAL, CR_SUCCESS,
    };
    use windows::Win32::Devices::Properties;
    use windows::Win32::Devices::Properties::{DEVPROPTYPE, DEVPROP_TYPE_STRING};
    use windows::Win32::Foundation::{DEVPROPKEY, PROPERTYKEY};
    use windows::Win32::Media::{Audio, KernelStreaming, Multimedia};
    use windows::Win32::System::Com::{
        self, CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED, STGM_READ,
    };
    use windows::Win32::System::Variant::VT_LPWSTR;
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

    const DEFAULT_RENDER_NAME: &str = "QPWGraph Virtual Output";
    const QPWGRAPH_DRIVER_SERVICE: &str = "qpwgraph_audio";
    const QPWGRAPH_ROOT_DEVICE_INSTANCE_ID: &str = "ROOT\\DEVGEN\\QPWGRAPH_AUDIO";
    const PKEY_QPWGRAPH_ENDPOINT_ROLE: PROPERTYKEY = PROPERTYKEY {
        fmtid: GUID::from_u128(0x3c8e_8ef9_1f7f_4fcb_9c36_4a7e_19f3_6d12),
        pid: 2,
    };

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

        fn label(self) -> &'static str {
            match self {
                Self::Render => "render",
                Self::Capture => "capture",
            }
        }
    }

    #[derive(Debug)]
    struct Options {
        render: Selector,
        capture: Option<Selector>,
        duration: Duration,
        list: bool,
        round_trip: bool,
        verify_roles: bool,
        verify_absent: bool,
    }

    #[derive(Debug)]
    enum Selector {
        Name(String),
        Id(String),
        Role(String),
    }

    #[derive(Clone, Debug)]
    struct Endpoint {
        id: String,
        name: String,
        device: Audio::IMMDevice,
    }

    pub(super) fn run() -> Result<(), SmokeError> {
        let options = parse_options()?;
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_err() {
            return Err(SmokeError::Failure(format!(
                "could not initialize COM: {initialized:?}"
            )));
        }

        let result = run_com(&options);
        unsafe { Com::CoUninitialize() };
        result
    }

    fn run_com(options: &Options) -> Result<(), SmokeError> {
        let enumerator: Audio::IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL) }.map_err(
                |error| {
                    SmokeError::Failure(format!("could not create MMDeviceEnumerator: {error}"))
                },
            )?;
        let renders = enumerate(&enumerator, Flow::Render)?;
        let captures = enumerate(&enumerator, Flow::Capture)?;

        if options.verify_roles || options.verify_absent {
            if options.verify_roles && options.verify_absent {
                return Err(SmokeError::Failure(
                    "--verify-roles and --verify-absent are mutually exclusive".into(),
                ));
            }
            return verify_provider_endpoints(&renders, &captures, options.verify_absent);
        }

        if options.list {
            print_endpoints(Flow::Render, &renders);
            print_endpoints(Flow::Capture, &captures);
            return Ok(());
        }

        let render = select(Flow::Render, &renders, &options.render)?;
        println!("opening render endpoint {:?} ({})", render.name, render.id);

        if options.round_trip {
            let selector = options.capture.as_ref().ok_or_else(|| {
                SmokeError::Failure("--round-trip requires a capture endpoint".into())
            })?;
            let capture = select(Flow::Capture, &captures, selector)?;
            println!(
                "opening round-trip capture endpoint {:?} ({})",
                capture.name, capture.id
            );
            open_round_trip(&render.device, &capture.device, options.duration)?;
        } else {
            open_start_stop(&render.device, Flow::Render, options.duration)?;
        }

        if !options.round_trip {
            if let Some(selector) = &options.capture {
                let capture = select(Flow::Capture, &captures, selector)?;
                println!(
                    "opening capture endpoint {:?} ({})",
                    capture.name, capture.id
                );
                open_start_stop(&capture.device, Flow::Capture, options.duration)?;
            }
        }

        println!("WASAPI shared-mode endpoint smoke passed");
        Ok(())
    }

    fn parse_options() -> Result<Options, SmokeError> {
        let mut render = std::env::var("QPWGRAPH_AUDIO_RENDER_NAME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(Selector::Name)
            .unwrap_or_else(|| Selector::Name(DEFAULT_RENDER_NAME.into()));
        let mut capture = std::env::var("QPWGRAPH_AUDIO_CAPTURE_NAME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(Selector::Name);
        let mut duration = Duration::from_millis(500);
        let mut list = false;
        let mut round_trip = false;
        let mut verify_roles = false;
        let mut verify_absent = false;
        let mut args = std::env::args().skip(1);
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                "--list" => list = true,
                "--round-trip" => {
                    render = Selector::Role("app-render".into());
                    capture = Some(Selector::Role("app-monitor".into()));
                    round_trip = true;
                }
                "--verify-roles" => verify_roles = true,
                "--verify-absent" => verify_absent = true,
                "--render-name" => {
                    render = Selector::Name(next_value(&mut args, "--render-name")?);
                }
                "--render-id" => {
                    render = Selector::Id(next_value(&mut args, "--render-id")?);
                }
                "--capture-name" => {
                    capture = Some(Selector::Name(next_value(&mut args, "--capture-name")?));
                }
                "--capture-id" => {
                    capture = Some(Selector::Id(next_value(&mut args, "--capture-id")?));
                }
                "--duration-ms" => {
                    let value = next_value(&mut args, "--duration-ms")?;
                    let millis = value.parse::<u64>().map_err(|error| {
                        SmokeError::Failure(format!(
                            "invalid --duration-ms value {value:?}: {error}"
                        ))
                    })?;
                    duration = Duration::from_millis(millis.min(60_000));
                }
                unknown => {
                    return Err(SmokeError::Failure(format!(
                        "unknown argument {unknown:?}; use --help"
                    )));
                }
            }
        }
        Ok(Options {
            render,
            capture,
            duration,
            list,
            round_trip,
            verify_roles,
            verify_absent,
        })
    }

    fn next_value(
        args: &mut impl Iterator<Item = String>,
        option: &str,
    ) -> Result<String, SmokeError> {
        args.next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| SmokeError::Failure(format!("{option} requires a non-empty value")))
    }

    fn print_help() {
        println!(
            "Usage: qpwgraph-audio-smoke [OPTIONS]\n\n\
             --list                       list active render/capture endpoints\n\
             --render-name NAME           select render endpoint by friendly name\n\
             --render-id ID               select render endpoint by MMDevice id\n\
             --capture-name NAME         also open a capture endpoint by name\n\
             --capture-id ID             also open a capture endpoint by MMDevice id\n\
             --round-trip                use app roles, write a tone, verify captured PCM\n\
             --verify-roles              require all four provider-owned QPWGraph endpoints\n\
             --verify-absent             require no provider-owned QPWGraph endpoints\n\
             --duration-ms N             start each client for N milliseconds (max 60000)"
        );
    }

    fn enumerate(
        enumerator: &Audio::IMMDeviceEnumerator,
        flow: Flow,
    ) -> Result<Vec<Endpoint>, SmokeError> {
        let collection =
            unsafe { enumerator.EnumAudioEndpoints(flow.data_flow(), Audio::DEVICE_STATE_ACTIVE) }
                .map_err(|error| {
                    SmokeError::Failure(format!(
                        "could not enumerate {} endpoints: {error}",
                        flow.label()
                    ))
                })?;
        let count = unsafe { collection.GetCount() }.map_err(|error| {
            SmokeError::Failure(format!(
                "could not count {} endpoints: {error}",
                flow.label()
            ))
        })?;
        let mut endpoints = Vec::with_capacity(count as usize);
        for index in 0..count {
            let device = unsafe { collection.Item(index) }.map_err(|error| {
                SmokeError::Failure(format!(
                    "could not read {} endpoint {index}: {error}",
                    flow.label()
                ))
            })?;
            let id = device_id(&device)?;
            let name = friendly_name(&device).unwrap_or_else(|_| id.clone());
            endpoints.push(Endpoint { id, name, device });
        }
        Ok(endpoints)
    }

    fn print_endpoints(flow: Flow, endpoints: &[Endpoint]) {
        println!("{} endpoints: {}", flow.label(), endpoints.len());
        for endpoint in endpoints {
            println!("  {:?} ({})", endpoint.name, endpoint.id);
        }
    }

    fn verify_provider_endpoints(
        renders: &[Endpoint],
        captures: &[Endpoint],
        expect_absent: bool,
    ) -> Result<(), SmokeError> {
        let expected = [
            (Flow::Render, "app-render"),
            (Flow::Capture, "app-monitor"),
            (Flow::Render, "relay-render"),
            (Flow::Capture, "relay-capture"),
        ];
        let mut provider_endpoints = Vec::new();
        for (flow, endpoints) in [(Flow::Render, renders), (Flow::Capture, captures)] {
            for endpoint in endpoints {
                let role = property_string(
                    &endpoint.device,
                    &PKEY_QPWGRAPH_ENDPOINT_ROLE as *const PROPERTYKEY,
                );
                let parent =
                    devnode_property_string(&endpoint.id, &Properties::DEVPKEY_Device_Parent);
                let service = parent
                    .as_deref()
                    .and_then(|parent_id| {
                        devnode_property_string_for_instance(
                            parent_id,
                            &Properties::DEVPKEY_Device_Service,
                        )
                    })
                    .or_else(|| {
                        devnode_property_string(&endpoint.id, &Properties::DEVPKEY_Device_Service)
                    });
                let provider_identity = service
                    .as_deref()
                    .is_some_and(|value| value.eq_ignore_ascii_case(QPWGRAPH_DRIVER_SERVICE))
                    && parent.as_deref().is_some_and(|value| {
                        value.eq_ignore_ascii_case(QPWGRAPH_ROOT_DEVICE_INSTANCE_ID)
                    });
                if !provider_identity {
                    if role.is_some() {
                        return Err(SmokeError::Failure(format!(
                            "provider-owned endpoint {} has a role but no matching service or parent identity (service={service:?}, parent={parent:?})",
                            endpoint.id
                        )));
                    }
                    continue;
                }
                let Some(role) = role else {
                    return Err(SmokeError::Failure(format!(
                        "provider-owned endpoint {} has no readable QPWGraph endpoint role ({})",
                        endpoint.id,
                        property_diagnostic(
                            &endpoint.device,
                            &PKEY_QPWGRAPH_ENDPOINT_ROLE as *const PROPERTYKEY,
                        )
                    )));
                };
                let Some((expected_flow, _)) = expected
                    .iter()
                    .find(|(_, expected_role)| role.eq_ignore_ascii_case(expected_role))
                else {
                    return Err(SmokeError::Failure(format!(
                        "provider-owned endpoint {} advertises unknown role {role:?}",
                        endpoint.id
                    )));
                };
                if *expected_flow != flow {
                    return Err(SmokeError::Failure(format!(
                        "provider-owned endpoint role {role:?} appeared on the {} flow; expected {}",
                        flow.label(),
                        expected_flow.label()
                    )));
                }
                provider_endpoints.push((flow, role, endpoint));
            }
        }

        if expect_absent && !provider_endpoints.is_empty() {
            let roles = provider_endpoints
                .iter()
                .map(|(flow, role, endpoint)| {
                    format!("{role} on {} ({})", flow.label(), endpoint.id)
                })
                .collect::<Vec<_>>()
                .join(", ");
            return Err(SmokeError::Failure(format!(
                "provider-owned QPWGraph endpoints are still present: {roles}"
            )));
        }

        if expect_absent {
            println!("provider-owned QPWGraph endpoint roles are absent");
        } else {
            let mut found = Vec::new();
            for (flow, expected_role) in expected {
                let matches: Vec<_> = provider_endpoints
                    .iter()
                    .filter(|(found_flow, role, _)| {
                        *found_flow == flow && role.eq_ignore_ascii_case(expected_role)
                    })
                    .collect();
                if matches.len() > 1 {
                    return Err(SmokeError::Failure(format!(
                        "provider-owned endpoint role {expected_role:?} appeared {} times on the {} flow",
                        matches.len(),
                        flow.label()
                    )));
                }
                let Some((_, _, endpoint)) = matches.first() else {
                    return Err(SmokeError::MissingEndpoint(format!(
                        "provider-owned endpoint role {expected_role:?} was not found on the {} flow",
                        flow.label()
                    )));
                };
                found.push((expected_role, *endpoint));
            }
            println!("provider-owned QPWGraph endpoint roles verified");
            for (role, endpoint) in found {
                println!("  {role}: {:?} ({})", endpoint.name, endpoint.id);
            }
        }
        Ok(())
    }

    fn select<'a>(
        flow: Flow,
        endpoints: &'a [Endpoint],
        selector: &Selector,
    ) -> Result<&'a Endpoint, SmokeError> {
        let matches: Vec<_> = endpoints
            .iter()
            .filter(|endpoint| match selector {
                Selector::Name(name) => endpoint.name.eq_ignore_ascii_case(name),
                Selector::Id(id) => endpoint.id == *id,
                Selector::Role(role) => {
                    let parent =
                        devnode_property_string(&endpoint.id, &Properties::DEVPKEY_Device_Parent);
                    let service = parent
                        .as_deref()
                        .and_then(|parent_id| {
                            devnode_property_string_for_instance(
                                parent_id,
                                &Properties::DEVPKEY_Device_Service,
                            )
                        })
                        .or_else(|| {
                            devnode_property_string(
                                &endpoint.id,
                                &Properties::DEVPKEY_Device_Service,
                            )
                        });
                    let provider_identity = service
                        .as_deref()
                        .is_some_and(|value| value.eq_ignore_ascii_case(QPWGRAPH_DRIVER_SERVICE))
                        && parent.as_deref().is_some_and(|value| {
                            value.eq_ignore_ascii_case(QPWGRAPH_ROOT_DEVICE_INSTANCE_ID)
                        });
                    if !provider_identity {
                        return false;
                    }
                    property_string(
                        &endpoint.device,
                        &PKEY_QPWGRAPH_ENDPOINT_ROLE as *const PROPERTYKEY,
                    )
                    .is_some_and(|value| value.eq_ignore_ascii_case(role))
                }
            })
            .collect();
        match matches.as_slice() {
            [endpoint] => Ok(endpoint),
            [] => {
                let wanted = match selector {
                    Selector::Name(name) => format!("friendly name {name:?}"),
                    Selector::Id(id) => format!("MMDevice id {id:?}"),
                    Selector::Role(role) => format!("provider role {role:?}"),
                };
                Err(SmokeError::MissingEndpoint(format!(
                    "no active {} endpoint matched {wanted}; run --list",
                    flow.label()
                )))
            }
            _ => Err(SmokeError::Failure(format!(
                "multiple active {} endpoints matched the selector; use --{}-id",
                flow.label(),
                flow.label()
            ))),
        }
    }

    fn device_id(device: &Audio::IMMDevice) -> Result<String, SmokeError> {
        let value = unsafe { device.GetId() }
            .map_err(|error| SmokeError::Failure(format!("could not read endpoint id: {error}")))?;
        let id = unsafe { value.to_string() }.map_err(|error| {
            SmokeError::Failure(format!("could not decode endpoint id: {error}"))
        })?;
        unsafe { Com::CoTaskMemFree(Some(value.0 as *const c_void)) };
        Ok(id)
    }

    fn friendly_name(device: &Audio::IMMDevice) -> WindowsResult<String> {
        property_string(
            device,
            &Properties::DEVPKEY_Device_FriendlyName as *const _ as *const PROPERTYKEY,
        )
        .ok_or_else(|| {
            windows::core::Error::new(
                windows::core::HRESULT(0x80004005u32 as i32),
                "friendly name was not a non-empty string",
            )
        })
    }

    fn property_string(device: &Audio::IMMDevice, key: *const PROPERTYKEY) -> Option<String> {
        let store: IPropertyStore = unsafe { device.OpenPropertyStore(STGM_READ).ok()? };
        let mut value = unsafe { store.GetValue(key as *const _).ok()? };
        let prop_variant = unsafe { &value.Anonymous.Anonymous };
        if prop_variant.vt != VT_LPWSTR {
            let _ = unsafe { Com::StructuredStorage::PropVariantClear(&mut value) };
            return None;
        }
        let ptr = unsafe { *(&prop_variant.Anonymous as *const _ as *const *const u16) };
        if ptr.is_null() {
            let _ = unsafe { Com::StructuredStorage::PropVariantClear(&mut value) };
            return None;
        }
        let mut length = 0usize;
        while length < 32_768 && unsafe { *ptr.add(length) } != 0 {
            length += 1;
        }
        let text = (length < 32_768).then(|| {
            let slice = unsafe { std::slice::from_raw_parts(ptr, length) };
            String::from_utf16_lossy(slice)
        });
        let _ = unsafe { Com::StructuredStorage::PropVariantClear(&mut value) };
        text.filter(|value| !value.is_empty())
    }

    fn property_diagnostic(device: &Audio::IMMDevice, key: *const PROPERTYKEY) -> String {
        let Ok(store) = (unsafe { device.OpenPropertyStore(STGM_READ) }) else {
            return "OpenPropertyStore failed".into();
        };
        let count = unsafe { store.GetCount() }.unwrap_or(0);
        let mut listed = false;
        for index in 0..count {
            let mut candidate: PROPERTYKEY = unsafe { std::mem::zeroed() };
            if unsafe { store.GetAt(index, &mut candidate) }.is_ok()
                && candidate.fmtid == unsafe { (*key).fmtid }
                && candidate.pid == unsafe { (*key).pid }
            {
                listed = true;
                break;
            }
        }
        match unsafe { store.GetValue(key) } {
            Ok(mut value) => {
                let variant_type = unsafe { value.Anonymous.Anonymous.vt };
                let _ = unsafe { Com::StructuredStorage::PropVariantClear(&mut value) };
                format!(
                    "property store entries={count}, key-listed={listed}, value-vt={variant_type:?}"
                )
            }
            Err(error) => format!(
                "property store entries={count}, key-listed={listed}, GetValue failed: {error}"
            ),
        }
    }

    fn devnode_property_string(endpoint_id: &str, key: &DEVPROPKEY) -> Option<String> {
        devnode_property_string_for_instance(&format!("SWD\\MMDEVAPI\\{endpoint_id}"), key)
    }

    fn devnode_property_string_for_instance(instance_id: &str, key: &DEVPROPKEY) -> Option<String> {
        let instance_id_wide: Vec<u16> = instance_id
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut devinst = 0u32;
        let locate_result = unsafe {
            CM_Locate_DevNodeW(
                &mut devinst,
                PCWSTR(instance_id_wide.as_ptr()),
                CM_LOCATE_DEVNODE_NORMAL,
            )
        };
        if locate_result != CR_SUCCESS {
            return None;
        }

        let mut property_type = DEVPROPTYPE(0);
        let mut property_size = 0u32;
        unsafe {
            CM_Get_DevNode_PropertyW(
                devinst,
                key,
                &mut property_type,
                None,
                &mut property_size,
                0,
            );
        }
        if property_size == 0 {
            return None;
        }
        let mut buffer = vec![0u8; property_size as usize];
        let result = unsafe {
            CM_Get_DevNode_PropertyW(
                devinst,
                key,
                &mut property_type,
                Some(buffer.as_mut_ptr()),
                &mut property_size,
                0,
            )
        };
        if result != CR_SUCCESS || property_type != DEVPROP_TYPE_STRING {
            return None;
        }
        let utf16 = buffer
            .chunks_exact(2)
            .map(|bytes| u16::from_ne_bytes([bytes[0], bytes[1]]))
            .take_while(|character| *character != 0)
            .collect::<Vec<_>>();
        (!utf16.is_empty()).then(|| String::from_utf16_lossy(&utf16))
    }

    fn open_start_stop(
        device: &Audio::IMMDevice,
        flow: Flow,
        duration: Duration,
    ) -> Result<(), SmokeError> {
        let client: Audio::IAudioClient =
            unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|error| {
                SmokeError::Failure(format!(
                    "could not activate {} client: {error}",
                    flow.label()
                ))
            })?;
        let format = unsafe { client.GetMixFormat() }.map_err(|error| {
            SmokeError::Failure(format!(
                "could not read {} mix format: {error}",
                flow.label()
            ))
        })?;
        if format.is_null() {
            return Err(SmokeError::Failure(format!(
                "{} endpoint returned a null mix format",
                flow.label()
            )));
        }
        let (sample_rate, channels, bits) = unsafe {
            (
                std::ptr::read_unaligned(std::ptr::addr_of!((*format).nSamplesPerSec)),
                std::ptr::read_unaligned(std::ptr::addr_of!((*format).nChannels)),
                std::ptr::read_unaligned(std::ptr::addr_of!((*format).wBitsPerSample)),
            )
        };
        let summary = format!("{sample_rate} Hz, {channels} channels, {bits} bits");
        let result = (|| {
            unsafe {
                client
                    .Initialize(
                        Audio::AUDCLNT_SHAREMODE_SHARED,
                        0,
                        2_000_000,
                        0,
                        format,
                        None,
                    )
                    .map_err(|error| {
                        SmokeError::Failure(format!(
                            "could not initialize {} client ({summary}): {error}",
                            flow.label()
                        ))
                    })?;
                client.Start().map_err(|error| {
                    SmokeError::Failure(format!("could not start {} client: {error}", flow.label()))
                })?;
            }
            std::thread::sleep(duration);
            unsafe {
                client.Stop().map_err(|error| {
                    SmokeError::Failure(format!("could not stop {} client: {error}", flow.label()))
                })?;
                client.Reset().map_err(|error| {
                    SmokeError::Failure(format!("could not reset {} client: {error}", flow.label()))
                })?;
            }
            println!("  {} stream passed ({summary})", flow.label());
            Ok(())
        })();
        unsafe { Com::CoTaskMemFree(Some(format.cast())) };
        result
    }

    struct AudioStream {
        client: Audio::IAudioClient,
        format: *mut Audio::WAVEFORMATEX,
        buffer_frames: u32,
        sample_rate: u32,
        channels: u16,
        bits: u16,
        format_tag: u16,
        block_align: u16,
    }

    impl AudioStream {
        fn summary(&self) -> String {
            format!(
                "{} Hz, {} channels, {} bits",
                self.sample_rate, self.channels, self.bits
            )
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

    fn open_stream(device: &Audio::IMMDevice, flow: Flow) -> Result<AudioStream, SmokeError> {
        let client: Audio::IAudioClient =
            unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|error| {
                SmokeError::Failure(format!(
                    "could not activate {} client: {error}",
                    flow.label()
                ))
            })?;
        let format = unsafe { client.GetMixFormat() }.map_err(|error| {
            SmokeError::Failure(format!(
                "could not read {} mix format: {error}",
                flow.label()
            ))
        })?;
        if format.is_null() {
            return Err(SmokeError::Failure(format!(
                "{} endpoint returned a null mix format",
                flow.label()
            )));
        }
        let (sample_rate, channels, bits, format_tag, block_align) = unsafe {
            (
                std::ptr::read_unaligned(std::ptr::addr_of!((*format).nSamplesPerSec)),
                std::ptr::read_unaligned(std::ptr::addr_of!((*format).nChannels)),
                std::ptr::read_unaligned(std::ptr::addr_of!((*format).wBitsPerSample)),
                std::ptr::read_unaligned(std::ptr::addr_of!((*format).wFormatTag)),
                std::ptr::read_unaligned(std::ptr::addr_of!((*format).nBlockAlign)),
            )
        };
        let summary = format!("{sample_rate} Hz, {channels} channels, {bits} bits");
        let initialized = unsafe {
            client.Initialize(
                Audio::AUDCLNT_SHAREMODE_SHARED,
                0,
                2_000_000,
                0,
                format,
                None,
            )
        };
        if let Err(error) = initialized {
            unsafe { Com::CoTaskMemFree(Some(format.cast())) };
            return Err(SmokeError::Failure(format!(
                "could not initialize {} client ({summary}): {error}",
                flow.label()
            )));
        }
        let buffer_frames = match unsafe { client.GetBufferSize() } {
            Ok(frames) => frames,
            Err(error) => {
                unsafe { Com::CoTaskMemFree(Some(format.cast())) };
                return Err(SmokeError::Failure(format!(
                    "could not read {} buffer size: {error}",
                    flow.label()
                )));
            }
        };
        Ok(AudioStream {
            client,
            format,
            buffer_frames,
            sample_rate,
            channels,
            bits,
            format_tag,
            block_align,
        })
    }

    fn open_round_trip(
        render_device: &Audio::IMMDevice,
        capture_device: &Audio::IMMDevice,
        duration: Duration,
    ) -> Result<(), SmokeError> {
        let render = open_stream(render_device, Flow::Render)?;
        let capture = open_stream(capture_device, Flow::Capture)?;
        let render_client = unsafe { render.client.GetService::<Audio::IAudioRenderClient>() }
            .map_err(|error| {
                SmokeError::Failure(format!("could not get render buffer service: {error}"))
            })?;
        let capture_client = unsafe { capture.client.GetService::<Audio::IAudioCaptureClient>() }
            .map_err(|error| {
            SmokeError::Failure(format!("could not get capture buffer service: {error}"))
        })?;

        unsafe {
            capture.client.Start().map_err(|error| {
                SmokeError::Failure(format!("could not start capture stream: {error}"))
            })?;
            render.client.Start().map_err(|error| {
                SmokeError::Failure(format!("could not start render stream: {error}"))
            })?;
        }

        let result = (|| {
            let deadline = Instant::now() + duration;
            let mut phase = 0.0_f64;
            let mut captured_frames = 0_u64;
            let mut captured_peak = 0.0_f32;
            while Instant::now() < deadline {
                fill_render(&render, &render_client, &mut phase)?;
                let (frames, peak) = drain_capture(&capture, &capture_client)?;
                captured_frames += u64::from(frames);
                captured_peak = captured_peak.max(peak);
                std::thread::sleep(Duration::from_millis(2));
            }
            if captured_frames == 0 {
                return Err(SmokeError::Failure(
                    "round-trip capture received no PCM packets".into(),
                ));
            }
            if captured_peak < 0.01 {
                return Err(SmokeError::Failure(format!(
                    "round-trip capture remained silent (peak {captured_peak:.4})"
                )));
            }
            println!(
                "  round-trip passed (render {}, capture {}, {} frames, peak {:.3})",
                render.summary(),
                capture.summary(),
                captured_frames,
                captured_peak
            );
            Ok(())
        })();
        unsafe {
            let _ = render.client.Stop();
            let _ = capture.client.Stop();
            let _ = render.client.Reset();
            let _ = capture.client.Reset();
        }
        result
    }

    fn fill_render(
        stream: &AudioStream,
        client: &Audio::IAudioRenderClient,
        phase: &mut f64,
    ) -> Result<u32, SmokeError> {
        let padding = unsafe { stream.client.GetCurrentPadding() }.map_err(|error| {
            SmokeError::Failure(format!("could not query render padding: {error}"))
        })?;
        let frames = stream.buffer_frames.saturating_sub(padding);
        if frames == 0 {
            return Ok(0);
        }
        let buffer = unsafe { client.GetBuffer(frames) }.map_err(|error| {
            SmokeError::Failure(format!("could not acquire render buffer: {error}"))
        })?;
        if buffer.is_null() {
            return Err(SmokeError::Failure(
                "render endpoint returned a null audio buffer".into(),
            ));
        }
        unsafe {
            write_tone(buffer, stream, frames, phase);
            client.ReleaseBuffer(frames, 0).map_err(|error| {
                SmokeError::Failure(format!("could not release render buffer: {error}"))
            })?;
        }
        Ok(frames)
    }

    fn drain_capture(
        stream: &AudioStream,
        client: &Audio::IAudioCaptureClient,
    ) -> Result<(u32, f32), SmokeError> {
        let mut total_frames = 0_u32;
        let mut peak = 0.0_f32;
        loop {
            let available = unsafe { client.GetNextPacketSize() }.map_err(|error| {
                SmokeError::Failure(format!("could not query capture packet size: {error}"))
            })?;
            if available == 0 {
                break;
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0_u32;
            let mut flags = 0_u32;
            unsafe {
                client
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .map_err(|error| {
                        SmokeError::Failure(format!("could not acquire capture buffer: {error}"))
                    })?;
                if flags & (Audio::AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) == 0 && !data.is_null() {
                    peak = peak.max(read_peak(data, stream, frames));
                }
                client.ReleaseBuffer(frames).map_err(|error| {
                    SmokeError::Failure(format!("could not release capture buffer: {error}"))
                })?;
            }
            total_frames = total_frames.saturating_add(frames);
        }
        Ok((total_frames, peak))
    }

    unsafe fn write_tone(buffer: *mut u8, stream: &AudioStream, frames: u32, phase: &mut f64) {
        let bytes_per_frame = usize::from(stream.block_align);
        let bytes_per_sample = usize::from(stream.bits).div_ceil(8);
        std::ptr::write_bytes(buffer, 0, frames as usize * bytes_per_frame);
        if bytes_per_sample == 0 || stream.channels == 0 || stream.sample_rate == 0 {
            return;
        }
        let is_float = stream.format_tag == Multimedia::WAVE_FORMAT_IEEE_FLOAT as u16
            || (stream.format_tag == KernelStreaming::WAVE_FORMAT_EXTENSIBLE as u16
                && stream.bits == 32);
        for frame in 0..frames as usize {
            let value = (*phase * std::f64::consts::TAU * 440.0).sin() as f32 * 0.25;
            *phase += 1.0 / f64::from(stream.sample_rate);
            for channel in 0..usize::from(stream.channels) {
                let offset = frame * bytes_per_frame + channel * bytes_per_sample;
                write_sample(buffer.add(offset), bytes_per_sample, is_float, value);
            }
        }
    }

    unsafe fn write_sample(target: *mut u8, bytes_per_sample: usize, is_float: bool, value: f32) {
        if is_float && bytes_per_sample >= 4 {
            target.cast::<f32>().write_unaligned(value);
        } else if bytes_per_sample == 1 {
            target.write((128.0 + value * 100.0) as u8);
        } else if bytes_per_sample == 2 {
            target
                .cast::<i16>()
                .write_unaligned((value * 32_000.0) as i16);
        } else if bytes_per_sample == 3 {
            let sample = (value * 8_388_607.0) as i32;
            target.write(sample as u8);
            target.add(1).write((sample >> 8) as u8);
            target.add(2).write((sample >> 16) as u8);
        } else {
            target
                .cast::<i32>()
                .write_unaligned((value * 2_000_000_000.0) as i32);
        }
    }

    unsafe fn read_peak(buffer: *const u8, stream: &AudioStream, frames: u32) -> f32 {
        let bytes_per_frame = usize::from(stream.block_align);
        let bytes_per_sample = usize::from(stream.bits).div_ceil(8);
        if bytes_per_sample == 0 || stream.channels == 0 {
            return 0.0;
        }
        let is_float = stream.format_tag == Multimedia::WAVE_FORMAT_IEEE_FLOAT as u16
            || (stream.format_tag == KernelStreaming::WAVE_FORMAT_EXTENSIBLE as u16
                && stream.bits == 32);
        let mut peak = 0.0_f32;
        for frame in 0..frames as usize {
            for channel in 0..usize::from(stream.channels) {
                let sample = buffer.add(frame * bytes_per_frame + channel * bytes_per_sample);
                let value = if is_float && bytes_per_sample >= 4 {
                    sample.cast::<f32>().read_unaligned().abs()
                } else if bytes_per_sample == 1 {
                    (f32::from(sample.read()) - 128.0) / 128.0
                } else if bytes_per_sample == 2 {
                    f32::from(sample.cast::<i16>().read_unaligned()).abs() / 32_768.0
                } else if bytes_per_sample == 3 {
                    let raw = i32::from(sample.read())
                        | (i32::from(sample.add(1).read()) << 8)
                        | (i32::from(sample.add(2).read()) << 16);
                    let signed = if raw & 0x80_0000 != 0 {
                        raw | !0xFF_FFFF
                    } else {
                        raw
                    };
                    (signed as f32).abs() / 8_388_608.0
                } else {
                    (sample.cast::<i32>().read_unaligned() as f32).abs() / 2_147_483_648.0
                };
                peak = peak.max(value);
            }
        }
        peak
    }

    #[derive(Debug)]
    pub(super) enum SmokeError {
        MissingEndpoint(String),
        Failure(String),
    }

    impl std::fmt::Display for SmokeError {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::MissingEndpoint(message) | Self::Failure(message) => {
                    formatter.write_str(message)
                }
            }
        }
    }
}

#[cfg(windows)]
fn main() {
    match windows_smoke::run() {
        Ok(()) => {}
        Err(error @ windows_smoke::SmokeError::MissingEndpoint(_)) => {
            eprintln!("ACX endpoint smoke skipped: {error}");
            std::process::exit(2);
        }
        Err(windows_smoke::SmokeError::Failure(error)) => {
            eprintln!("ACX endpoint smoke failed: {error}");
            std::process::exit(1);
        }
    }
}
