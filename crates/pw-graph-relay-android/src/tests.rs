use super::convert::{
    android_channels, catch_native, connected_json, local_links_json, native_error_code,
    parse_codec, parse_direction, parse_discovered_peers, parse_transport, port_u16, positive_u16,
    positive_u32, usb_link_json,
};
use super::pcm_scratch::PCM_SCRATCH;
use pw_graph_relay_sdk::{
    CodecKind, RelayClientBuilder, RelayDirection, SessionId, TransportPreference,
    MAX_DISCOVERED_PEER_ADDRESSES, MAX_REALTIME_QUANTUM_SAMPLES,
};
use serde_json::json;

#[test]
fn parses_android_client_options() {
    assert_eq!(
        parse_direction("emit").unwrap(),
        RelayDirection::MobileToDesktop
    );
    assert_eq!(
        parse_direction("receive").unwrap(),
        RelayDirection::DesktopToMobile
    );
    assert!(parse_direction("both").is_err());
    assert_eq!(parse_codec("pcm").unwrap(), CodecKind::Pcm);
    assert_eq!(parse_codec("opus").unwrap(), CodecKind::Opus);
    assert_eq!(parse_transport("wifi").unwrap(), TransportPreference::Wifi);
}

#[test]
fn invalid_android_enum_options_are_errors_instead_of_defaults() {
    assert!(parse_direction("not-a-role").is_err());
    assert!(parse_codec("not-a-codec").is_err());
    assert!(parse_transport("not-a-transport").is_err());
}

#[test]
fn lifecycle_error_codes_are_stable_and_not_message_parsed_by_clients() {
    assert_eq!(
        native_error_code("unknown client handle"),
        "unknown_client_handle"
    );
    assert_eq!(
        native_error_code("unknown host handle"),
        "unknown_host_handle"
    );
    assert_eq!(native_error_code("something else"), "internal_error");
}

#[test]
fn embedded_discovery_snapshot_has_a_hard_size_and_count_bound() {
    let peers = (0..=MAX_DISCOVERED_PEER_ADDRESSES)
        .map(|index| {
            json!({
                "id": format!("peer-{index}"),
                "name": "relay",
                "address": "192.168.1.20:48123"
            })
        })
        .collect::<Vec<_>>();
    let snapshot = serde_json::to_string(&peers).unwrap();
    assert!(parse_discovered_peers(&snapshot).is_err());
    assert!(parse_discovered_peers(&"x".repeat(2 * 1024 * 1024 + 1)).is_err());
}

#[test]
fn jint_audio_values_are_checked_before_narrowing() {
    for value in [0, -1, -65536] {
        assert!(positive_u16("channels", value).is_err());
        assert!(positive_u32("sample rate", value).is_err());
    }
    for value in [65_536, 65_538] {
        assert!(positive_u16("channels", value).is_err());
    }
    assert_eq!(positive_u16("channels", 1).unwrap(), 1);
    assert_eq!(positive_u16("channels", 2).unwrap(), 2);
    assert_eq!(positive_u32("sample rate", 48_000).unwrap(), 48_000);
    assert_eq!(port_u16(0).unwrap(), 0);
    assert!(port_u16(-1).is_err());
    assert!(port_u16(65_536).is_err());
}

#[test]
fn android_audio_accepts_the_negotiable_channel_set() {
    assert_eq!(android_channels(1).unwrap(), 1);
    assert_eq!(android_channels(2).unwrap(), 2);
    assert!(android_channels(0).is_err());
    assert!(android_channels(3).is_err());
}

#[test]
fn panic_in_a_native_operation_becomes_a_result_error() {
    let result: Result<(), String> = catch_native(|| panic!("test panic"));
    assert_eq!(result, Err("native relay operation panicked".to_string()));
}

#[test]
fn connected_response_contains_the_consumed_session_metadata() {
    let value = connected_json(SessionId(42), "studio-pc");
    assert_eq!(
        value.get("type").and_then(|value| value.as_str()),
        Some("connected")
    );
    assert_eq!(
        value.get("session").and_then(|value| value.as_u64()),
        Some(42)
    );
    assert_eq!(
        value.get("host").and_then(|value| value.as_str()),
        Some("studio-pc")
    );
}

#[test]
fn checked_frame_values_reach_builder_validation_unchanged() {
    for frame_ms in [5, 10, 20, 40, 60] {
        let frame_ms = positive_u16("frame duration", frame_ms).unwrap();
        assert!(RelayClientBuilder::new()
            .audio(48_000, 1, frame_ms)
            .build()
            .is_ok());
    }
    for frame_ms in [1, 7, 61] {
        let frame_ms = positive_u16("frame duration", frame_ms).unwrap();
        assert!(RelayClientBuilder::new()
            .audio(48_000, 1, frame_ms)
            .build()
            .is_err());
    }
}

#[test]
fn pcm_scratch_capacity_is_reused_after_the_first_quantum() {
    PCM_SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        scratch.clear();
        scratch.resize(MAX_REALTIME_QUANTUM_SAMPLES, 0.0);
        let capacity = scratch.capacity();
        for _ in 0..16 {
            scratch.resize(MAX_REALTIME_QUANTUM_SAMPLES, 0.0);
        }
        assert_eq!(scratch.capacity(), capacity);
    });
}

#[test]
fn usb_link_json_is_well_formed_without_a_tether() {
    // A desktop test box normally has no USB tether up; whatever the
    // result, it must be a JSON object with a `type` field.
    let value = usb_link_json();
    let kind = value.get("type").and_then(|field| field.as_str());
    assert!(matches!(kind, Some("usb_link") | Some("none")));
    if kind == Some("usb_link") {
        assert!(value.get("name").is_some());
        assert!(value.get("addr").is_some());
    }
}

#[test]
fn local_links_json_lists_every_link_with_kind() {
    let value = local_links_json();
    assert_eq!(
        value.get("type").and_then(|field| field.as_str()),
        Some("links")
    );
    let links = value
        .get("links")
        .and_then(|field| field.as_array())
        .unwrap();
    for link in links {
        assert!(matches!(
            link.get("kind").and_then(|field| field.as_str()),
            Some("usb") | Some("wifi") | Some("bluetooth") | Some("lan")
        ));
        assert!(link.get("name").is_some());
        assert!(link.get("addr").is_some());
    }
}
