use pw_graph_core::PortKey;
use pw_graph_effects::EffectInstanceConfig;
use std::collections::BTreeMap;

use super::*;
use pw_graph_effects::ChannelPolicy;

#[test]
fn defaults_round_trip() {
    // No shipped default: a fresh install must not host behind a PIN
    // every other install also has.
    assert!(AppConfig::default().relay_host_pin.is_empty());
    assert!(AppConfig::default().relay_client_pin.is_empty());
    assert_eq!(AppConfig::default().relay_host_port, 48123);
    assert_eq!(
        AppConfig::default().relay_direction,
        AudioDirection::MobileToDesktop
    );
    assert_eq!(AppConfig::default().relay_direction_generation, 0);
    let directory = std::env::temp_dir().join(format!("pw-graph-config-{}", std::process::id()));
    let path = directory.join("config.toml");
    let expected = AppConfig {
        relay_device_id: "studio-installation".into(),
        relay_trusted_peers: vec![PersistedRelayPeer {
            peer_id: "phone-installation".into(),
            secret: "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff".into(),
            name: "phone".into(),
            address: "192.168.42.2:48123".into(),
            preferred_mode: None,
        }],
        relay_auto_connect_trusted: false,
        relay_device_name: "studio-pc".into(),
        relay_host_pin: String::new(),
        relay_host_port: 0,
        relay_client_target: "192.168.1.20:48123".into(),
        relay_send_source: "monitor:capture-endpoint".into(),
        relay_receive_sink: "output:playback-endpoint".into(),
        ..AppConfig::default()
    };
    expected.save_to(&path).unwrap();
    assert_eq!(AppConfig::load_from(&path).unwrap(), expected);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn recording_defaults_and_save_mode_are_backward_compatible() {
    let defaults = AppConfig::default();
    assert_eq!(defaults.recording_dir, None);
    assert_eq!(defaults.recording_save_mode, "ask");
    assert_eq!(
        defaults.recording_filename_template,
        "Recording {date} {time}"
    );
    assert_eq!(defaults.recording_format, "wav-f32");
    assert_eq!(
        RecordingSaveMode::parse(&defaults.recording_save_mode),
        RecordingSaveMode::AskOnStop
    );
    assert_eq!(
        RecordingSaveMode::parse("AUTO"),
        RecordingSaveMode::AutoSave
    );

    let config: AppConfig = toml::from_str("language = 'en'\n").unwrap();
    assert_eq!(config.recording_save_mode, "ask");
    assert_eq!(config.recording_format, "wav-f32");

    let auto: AppConfig =
        toml::from_str("language = 'en'\nrecording_save_mode = 'auto'\n").unwrap();
    assert_eq!(
        RecordingSaveMode::parse(&auto.recording_save_mode),
        RecordingSaveMode::AutoSave
    );
}

#[test]
fn relay_direction_migrates_legacy_desktop_roles_and_writes_only_the_generic_key() {
    let cases = [
        ("emit", AudioDirection::DesktopToMobile),
        ("receive", AudioDirection::MobileToDesktop),
        ("both", AudioDirection::MobileToDesktop),
    ];
    for (legacy_role, expected) in cases {
        let mut config: AppConfig =
            toml::from_str(&format!("language = 'en'\nrelay_role = '{legacy_role}'\n")).unwrap();
        config.migrate_relay_mode();
        assert_eq!(config.relay_direction, expected);
        let serialized = toml::to_string(&config).unwrap();
        assert!(serialized.contains("relay_mode ="));
        assert!(!serialized.contains("relay_direction ="));
        assert!(!serialized.contains("relay_role ="));
    }
}

#[test]
fn relay_direction_accepts_canonical_values_and_canonicalizes_pc_alias() {
    let mobile_to_desktop: AppConfig =
        toml::from_str("language = 'en'\nrelay_direction = 'mobile_to_desktop'\n").unwrap();
    assert_eq!(
        mobile_to_desktop.relay_direction,
        AudioDirection::MobileToDesktop
    );

    let mut desktop_to_mobile: AppConfig =
        toml::from_str("language = 'en'\nrelay_direction = 'pc_to_mobile'\n").unwrap();
    assert_eq!(
        desktop_to_mobile.relay_direction,
        AudioDirection::DesktopToMobile
    );
    desktop_to_mobile.migrate_relay_mode();
    assert_eq!(desktop_to_mobile.relay_mode, RelayMode::Emitter);
    let serialized = toml::to_string(&desktop_to_mobile).unwrap();
    assert!(serialized.contains("relay_mode = \"emitter\""));
    assert!(!serialized.contains("relay_direction ="));
}

#[test]
fn relay_direction_generation_is_persisted_with_the_canonical_direction() {
    let mut config: AppConfig = toml::from_str(
        "language = 'en'\nrelay_direction = 'desktop_to_mobile'\nrelay_direction_generation = 17\n",
    )
    .unwrap();
    config.migrate_relay_mode();
    assert_eq!(config.relay_direction, AudioDirection::DesktopToMobile);
    assert_eq!(config.relay_direction_generation, 17);

    let serialized = toml::to_string(&config).unwrap();
    assert!(serialized.contains("relay_mode = \"emitter\""));
    assert!(serialized.contains("relay_mode_generation = 17"));
    assert!(!serialized.contains("relay_direction ="));
    assert!(!serialized.contains("relay_direction_generation ="));
    assert!(!serialized.contains("relay_role ="));
}

#[test]
fn pairing_pins_never_reach_disk() {
    // Keep this directory distinct from `defaults_round_trip`: tests run
    // concurrently, and that test removes its directory after saving.
    let directory =
        std::env::temp_dir().join(format!("pw-graph-config-pins-{}", std::process::id()));
    let path = directory.join("pins.toml");
    let config = AppConfig {
        relay_host_pin: "864209".into(),
        relay_client_pin: "135790".into(),
        ..AppConfig::default()
    };
    config.save_to(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("864209"), "the host PIN was written to disk");
    assert!(
        !text.contains("135790"),
        "the client PIN was written to disk"
    );
    let reloaded = AppConfig::load_from(&path).unwrap();
    assert!(reloaded.relay_host_pin.is_empty());
    assert!(reloaded.relay_client_pin.is_empty());
    let _ = std::fs::remove_dir_all(directory);
}

#[test]
fn config_debug_redacts_pairing_pins_and_trusted_secrets() {
    let config = AppConfig {
        relay_host_pin: "864209".into(),
        relay_client_pin: "135790".into(),
        relay_trusted_peers: vec![PersistedRelayPeer {
            peer_id: "phone".into(),
            secret: "ab".repeat(32),
            name: "phone".into(),
            address: "192.168.42.2:48123".into(),
            preferred_mode: None,
        }],
        ..AppConfig::default()
    };
    let debug = format!("{config:?}");
    assert!(!debug.contains("864209"));
    assert!(!debug.contains("135790"));
    assert!(!debug.contains(&"ab".repeat(32)));
    assert!(debug.contains("redacted"));
}

#[cfg(unix)]
#[test]
fn the_config_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir()
        .join(format!("pw-graph-config-mode-{}", std::process::id()))
        .join("config.toml");
    AppConfig::default().save_to(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn old_configs_default_relay_endpoint_choices_to_system_default() {
    let config: AppConfig = toml::from_str("language = 'en'\n").unwrap();
    assert_eq!(config.relay_capture_endpoint_id, None);
    assert_eq!(config.relay_playback_endpoint_id, None);
    assert_eq!(config.relay_send_source, "default-input");
    assert_eq!(config.relay_receive_sink, "default-output");
}

#[test]
fn old_windows_endpoint_choices_migrate_to_generic_route_selectors() {
    let mut config: AppConfig = toml::from_str(
        "language = 'en'\nrelay_capture_endpoint_id = 'capture-id'\nrelay_playback_endpoint_id = 'output-id'\n",
    )
    .unwrap();
    config.migrate_relay_mode();
    assert_eq!(config.relay_send_source, "monitor:capture-id");
    assert_eq!(config.relay_receive_sink, "output:output-id");
    let serialized = toml::to_string(&config).unwrap();
    assert!(!serialized.contains("relay_capture_endpoint_id"));
    assert!(!serialized.contains("relay_playback_endpoint_id"));
}

#[test]
fn node_positions_round_trip() {
    let directory =
        std::env::temp_dir().join(format!("pw-graph-config-positions-{}", std::process::id()));
    let path = directory.join("config.toml");
    let mut expected = AppConfig::default();
    expected.node_positions.insert("42".into(), [120.5, -18.0]);
    expected
        .node_positions
        .insert("9001".into(), [640.0, 240.25]);
    expected
        .node_positions_by_name
        .insert("PipeWire:Capture".into(), [120.5, -18.0]);
    expected.node_view_by_name.insert(
        "PipeWire:Capture".into(),
        NodeAppearance {
            collapsed: true,
            custom_name: Some("Microphone".into()),
            color: Some([82, 207, 133, 255]),
        },
    );
    expected.save_to(&path).unwrap();
    assert_eq!(AppConfig::load_from(&path).unwrap(), expected);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn effect_configuration_round_trips() {
    let directory =
        std::env::temp_dir().join(format!("pw-graph-config-effects-{}", std::process::id()));
    let path = directory.join("config.toml");
    let mut expected = AppConfig::default();
    expected.effects.push(PersistedEffect {
        instance: EffectInstanceConfig {
            instance_id: "effect-1".into(),
            effect_id: "builtin.noise-gate".into(),
            module_path: None,
            enabled: true,
            parameters: [("threshold-db".into(), -42.0)].into_iter().collect(),
            channel_policy: ChannelPolicy::Auto,
        },
        source: Some(PortKey {
            node_name: "Capture".into(),
            node_serial: None,
            node_type: pw_graph_core::NodeType::PipeWire,
            port_name: "out_FL".into(),
            channel: Some("FL".into()),
            direction: pw_graph_core::Direction::Source,
            port_type: pw_graph_core::PortType::Audio,
            identity: None,
            match_mode: pw_graph_core::EndpointMatchMode::Instance,
        }),
        destination: Some(PortKey {
            node_name: "Playback".into(),
            node_serial: None,
            node_type: pw_graph_core::NodeType::PipeWire,
            port_name: "in_FL".into(),
            channel: Some("FL".into()),
            direction: pw_graph_core::Direction::Sink,
            port_type: pw_graph_core::PortType::Audio,
            identity: None,
            match_mode: pw_graph_core::EndpointMatchMode::Instance,
        }),
        position: [260.0, 180.0],
    });
    expected.save_to(&path).unwrap();
    assert_eq!(AppConfig::load_from(&path).unwrap(), expected);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn channel_policy_round_trips_and_legacy_channels_migrate_without_losing_intent() {
    let mut config = AppConfig::default();
    config.effects.push(PersistedEffect {
        instance: EffectInstanceConfig {
            instance_id: "hush-mono".into(),
            effect_id: "builtin.hush-noise-suppressor".into(),
            module_path: None,
            enabled: true,
            parameters: BTreeMap::new(),
            channel_policy: ChannelPolicy::Auto,
        },
        source: None,
        destination: None,
        position: [10.0, 20.0],
    });
    let text = toml::to_string(&config).unwrap();
    let restored: AppConfig = toml::from_str(&text).unwrap();
    assert_eq!(
        restored.effects[0].instance.channel_policy,
        ChannelPolicy::Auto
    );

    let legacy_auto: AppConfig = toml::from_str(
        "effects = [{ instance = { instance_id = 'legacy-hush', effect_id = 'builtin.hush-noise-suppressor' } }]",
    )
    .unwrap();
    assert_eq!(
        legacy_auto.effects[0].instance.channel_policy,
        ChannelPolicy::Auto
    );

    let legacy_mono: AppConfig = toml::from_str(
        "effects = [{ instance = { instance_id = 'legacy-hush', effect_id = 'builtin.hush-noise-suppressor', channels = 1 } }]",
    )
    .unwrap();
    assert_eq!(
        legacy_mono.effects[0].instance.channel_policy,
        ChannelPolicy::Fixed(1)
    );
}

#[test]
fn legacy_effect_without_routing_or_position_loads_as_a_standalone_node() {
    let config: AppConfig = toml::from_str(
        r#"
effects = [{ instance = { instance_id = "legacy-effect", effect_id = "builtin.noise-gate" } }]
"#,
    )
    .unwrap();

    let effect = config.effects.first().unwrap();
    assert_eq!(effect.instance.instance_id, "legacy-effect");
    assert_eq!(effect.source, None);
    assert_eq!(effect.destination, None);
    assert_eq!(effect.position, [260.0, 180.0]);
}

#[test]
fn unknown_fields_survive_a_config_round_trip() {
    let directory =
        std::env::temp_dir().join(format!("pw-graph-config-extra-{}", std::process::id()));
    let path = directory.join("config.toml");
    let original = r#"
language = "es"
future_setting = "keep me"
[future_table]
enabled = true
"#;
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(&path, original).unwrap();

    let config = AppConfig::load_from(&path).unwrap();
    assert_eq!(
        config.extra.get("future_setting"),
        Some(&toml::Value::String("keep me".into()))
    );
    config.save_to(&path).unwrap();
    let restored = AppConfig::load_from(&path).unwrap();
    assert_eq!(restored.extra, config.extra);
    assert_eq!(
        restored.extra.get("future_table"),
        config.extra.get("future_table")
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn windows_capabilities_and_application_routes_round_trip() {
    let mut config = AppConfig::default();
    config.windows.experimental_app_routing = true;
    config.windows.relay.receive_target = WindowsRelayReceiveTarget::VirtualMicrophone;
    let mut effect_parameters = BTreeMap::new();
    effect_parameters.insert("threshold-db".into(), -42.0);
    config
        .windows_application_routes
        .push(WindowsApplicationRoute {
            application: WindowsApplicationSelector {
                executable_path_hash: Some("sha256:deadbeef".into()),
                executable_name: Some("tone.exe".into()),
                ..WindowsApplicationSelector::default()
            },
            destination_stable_id: Some("stable-speakers".into()),
            destination_mmdevice_id: Some("{current-speakers}".into()),
            destination_endpoint_id: Some("endpoint-instance-id".into()),
            destination_name: Some("Speakers".into()),
            effect_chain: vec!["builtin.noise-gate".into()],
            effect_instances: vec![EffectInstanceConfig {
                instance_id: "route-gate".into(),
                effect_id: "builtin.noise-gate".into(),
                module_path: None,
                enabled: false,
                parameters: effect_parameters,
                channel_policy: ChannelPolicy::Auto,
            }],
            ..WindowsApplicationRoute::default()
        });
    let text = toml::to_string_pretty(&config).unwrap();
    assert!(text.contains("experimental_app_routing = true"));
    assert!(text.contains("receive_target = \"virtual-microphone\""));
    let restored: AppConfig = toml::from_str(&text).unwrap();
    assert_eq!(restored.windows, config.windows);
    assert_eq!(
        restored.windows_application_routes,
        config.windows_application_routes
    );
    let route = restored.windows_application_routes.first().unwrap();
    let restored_effect = route.restorable_effect_instances().unwrap().pop().unwrap();
    assert_eq!(restored_effect.instance_id, "route-gate");
    assert_eq!(restored_effect.parameters["threshold-db"], -42.0);
    assert!(!restored_effect.enabled);

    let directory = std::env::temp_dir().join(format!(
        "pw-graph-config-windows-routes-{}",
        std::process::id()
    ));
    let path = directory.join("config.toml");
    config.save_to(&path).unwrap();
    let file_restored = AppConfig::load_from(&path).unwrap();
    assert_eq!(
        file_restored.windows_application_routes,
        config.windows_application_routes
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn legacy_route_effect_ids_fail_closed_until_upgraded() {
    let route = WindowsApplicationRoute {
        effect_chain: vec!["builtin.noise-gate".into()],
        ..WindowsApplicationRoute::default()
    };
    let error = route.restorable_effect_instances().unwrap_err();
    assert!(error.contains("legacy effect IDs"));
}

#[test]
fn route_effect_ids_must_match_complete_instances() {
    let route = WindowsApplicationRoute {
        effect_chain: vec!["builtin.noise-gate".into()],
        effect_instances: vec![EffectInstanceConfig {
            instance_id: "route-gate".into(),
            effect_id: "builtin.adaptive-noise-suppressor".into(),
            module_path: None,
            enabled: true,
            parameters: BTreeMap::new(),
            channel_policy: ChannelPolicy::Auto,
        }],
        ..WindowsApplicationRoute::default()
    };
    let error = route.restorable_effect_instances().unwrap_err();
    assert!(error.contains("does not match"));
}

#[test]
fn windows_virtual_microphone_preference_migrates_to_backend_selector() {
    let mut config: AppConfig =
        toml::from_str("[windows.relay]\nreceive_target = 'virtual-microphone'\n").unwrap();
    config.migrate_relay_mode();
    assert_eq!(config.relay_receive_sink, "virtual-microphone");
    let text = toml::to_string(&config).unwrap();
    assert!(text.contains("receive_target = \"virtual-microphone\""));
}

#[test]
fn application_selector_requires_stable_identity() {
    let selector = WindowsApplicationSelector {
        executable_path_hash: Some("sha256:abc".into()),
        executable_name: Some("player.exe".into()),
        ..WindowsApplicationSelector::default()
    };
    let candidate = WindowsApplicationSelector {
        executable_path_hash: Some("SHA256:ABC".into()),
        executable_name: Some("PLAYER.EXE".into()),
        display_name: Some("Player".into()),
        ..WindowsApplicationSelector::default()
    };
    assert!(selector.is_stable());
    assert_eq!(selector.stable_key(), Some("sha256:abc"));
    assert!(selector.matches(&candidate));
    assert!(!WindowsApplicationSelector {
        display_name: Some("Player".into()),
        ..WindowsApplicationSelector::default()
    }
    .is_stable());
}

#[test]
fn display_name_only_selector_never_matches_a_live_candidate() {
    let selector = WindowsApplicationSelector {
        display_name: Some("Player".into()),
        ..WindowsApplicationSelector::default()
    };
    let candidate = WindowsApplicationSelector {
        executable_path_hash: Some("sha256:unrelated".into()),
        executable_name: Some("other.exe".into()),
        display_name: Some("Player".into()),
        ..WindowsApplicationSelector::default()
    };

    assert!(!selector.is_stable());
    assert!(!selector.matches(&candidate));
}

#[test]
fn package_family_runtime_key_keeps_executables_distinct() {
    let first = WindowsApplicationSelector {
        package_family_name: Some("Example_123".into()),
        executable_name: Some("first.exe".into()),
        ..WindowsApplicationSelector::default()
    };
    let second = WindowsApplicationSelector {
        package_family_name: Some("Example_123".into()),
        executable_name: Some("second.exe".into()),
        ..WindowsApplicationSelector::default()
    };
    assert_ne!(first.runtime_key(), second.runtime_key());
    assert_eq!(
        first.runtime_key().as_deref(),
        Some("package-family:Example_123|executable:first.exe")
    );
    assert!(!WindowsApplicationSelector {
        package_family_name: Some("Example_123".into()),
        ..WindowsApplicationSelector::default()
    }
    .is_stable());
}

#[test]
fn package_family_runtime_key_survives_executable_path_changes() {
    let before = WindowsApplicationSelector {
        executable_path_hash: Some("sha256:old".into()),
        executable_name: Some("player.exe".into()),
        package_family_name: Some("Example_123".into()),
        ..WindowsApplicationSelector::default()
    };
    let after = WindowsApplicationSelector {
        executable_path_hash: Some("sha256:new".into()),
        executable_name: Some("PLAYER.EXE".into()),
        package_family_name: Some("example_123".into()),
        ..WindowsApplicationSelector::default()
    };
    assert_eq!(
        before.runtime_key().as_deref(),
        Some("package-family:Example_123|executable:player.exe")
    );
    assert_eq!(
        before.runtime_key().map(|key| key.to_ascii_lowercase()),
        after.runtime_key().map(|key| key.to_ascii_lowercase())
    );
}

#[test]
fn durable_packaged_identity_survives_path_and_display_name_changes() {
    let selector = WindowsApplicationSelector {
        executable_path_hash: Some("sha256:old".into()),
        executable_name: Some("player.exe".into()),
        package_family_name: Some("Player_123".into()),
        app_user_model_id: Some("Player_123!App".into()),
        display_name: Some("Old Player".into()),
    };
    let candidate = WindowsApplicationSelector {
        executable_path_hash: Some("sha256:new".into()),
        executable_name: Some("player.exe".into()),
        package_family_name: Some("player_123".into()),
        app_user_model_id: Some("player_123!app".into()),
        display_name: Some("New Player".into()),
    };
    assert!(selector.matches(&candidate));
}

#[test]
fn most_specific_enabled_application_route_wins_without_a_pid() {
    let candidate = WindowsApplicationSelector {
        executable_path_hash: Some("sha256:abc".into()),
        executable_name: Some("player.exe".into()),
        package_family_name: Some("Player_123!App".into()),
        ..WindowsApplicationSelector::default()
    };
    let config = AppConfig {
        windows_application_routes: vec![
            WindowsApplicationRoute {
                application: WindowsApplicationSelector {
                    executable_path_hash: Some("sha256:abc".into()),
                    ..WindowsApplicationSelector::default()
                },
                destination_name: Some("broad".into()),
                ..WindowsApplicationRoute::default()
            },
            WindowsApplicationRoute {
                application: WindowsApplicationSelector {
                    executable_path_hash: Some("sha256:abc".into()),
                    package_family_name: Some("player_123!app".into()),
                    executable_name: Some("player.exe".into()),
                    ..WindowsApplicationSelector::default()
                },
                destination_name: Some("specific".into()),
                ..WindowsApplicationRoute::default()
            },
        ],
        ..AppConfig::default()
    };
    assert_eq!(
        config
            .matching_windows_application_route(&candidate)
            .and_then(|route| route.destination_name.as_deref()),
        Some("specific")
    );
}
