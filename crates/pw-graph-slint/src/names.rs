//! Human-friendly node and port labels used by the canvas.
//!
//! Backend names are deliberately kept as the stable identity used for
//! persistence and matching. This module only changes their presentation.

use crate::model::{RELAY_SINK_NAME, RELAY_SOURCE_NAME};
use pw_graph_i18n::I18n;

pub(crate) fn display_node_name(name: &str, i18n: &I18n) -> String {
    let (kind_key, detail) = if let Some(detail) = name.strip_prefix("alsa_input.") {
        ("canvas.node_name_alsa_input", short_device_name(detail))
    } else if let Some(detail) = name.strip_prefix("alsa_output.") {
        ("canvas.node_name_alsa_output", short_device_name(detail))
    } else if let Some(detail) = name.strip_prefix("bluez_input.") {
        ("canvas.node_name_bluetooth_input", bluetooth_suffix(detail))
    } else if let Some(detail) = name.strip_prefix("bluez_output.") {
        (
            "canvas.node_name_bluetooth_output",
            bluetooth_suffix(detail),
        )
    } else if let Some(detail) = name.strip_prefix("bluez_capture_internal.") {
        (
            "canvas.node_name_bluetooth_capture",
            bluetooth_suffix(detail),
        )
    } else if name.starts_with("bluez_midi.") {
        ("canvas.node_name_bluetooth_midi", None)
    } else if name.starts_with("v4l2_input.") || name.starts_with("libcamera_input.") {
        ("canvas.node_name_camera_input", None)
    } else if name.starts_with("Midi Through:") {
        ("canvas.node_name_midi_through", None)
    } else if name == RELAY_SOURCE_NAME {
        // The relay's own virtual devices. Their backend names are the stable
        // identity used for matching, so only the presentation is localised.
        ("canvas.node_name_relay_microphone", None)
    } else if name == RELAY_SINK_NAME {
        ("canvas.node_name_relay_speaker", None)
    } else {
        return name.replace(['_', '-'], " ");
    };
    let kind = i18n.text(kind_key);
    detail
        .filter(|detail| !detail.is_empty())
        .map(|detail| format!("{kind} - {detail}"))
        .unwrap_or(kind)
}

fn short_device_name(detail: &str) -> Option<String> {
    let device = detail.split("usb-").nth(1).unwrap_or(detail);
    let words: Vec<_> = device
        .split(|character: char| !character.is_ascii_alphabetic())
        .filter(|word| !word.is_empty())
        .take(3)
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

fn bluetooth_suffix(detail: &str) -> Option<String> {
    if !detail
        .chars()
        .all(|character| character.is_ascii_hexdigit() || matches!(character, ':' | '_' | '-'))
    {
        return None;
    }
    let digits: String = detail
        .chars()
        .filter(|character| character.is_ascii_hexdigit())
        .collect();
    (digits.len() >= 4).then(|| {
        let suffix = &digits[digits.len() - 4..];
        format!("{}:{}", &suffix[..2], &suffix[2..])
    })
}

pub(crate) fn display_port_name(name: &str, i18n: &I18n) -> String {
    let name = name.rsplit(": ").next().unwrap_or(name);
    let name = name
        .replace("(capture)", &i18n.text("canvas.capture"))
        .replace("(playback)", &i18n.text("canvas.playback"))
        .replace(['_', '-'], " ");
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return String::new();
    };
    format!("{}{}", first.to_uppercase(), characters.as_str())
}

pub(crate) fn compact_label(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let compact: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{compact}...")
    } else {
        compact
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_preserve_backend_identity_without_transport_prefixes() {
        let i18n = I18n::default();
        assert_eq!(display_node_name("Dummy-Driver", &i18n), "Dummy Driver");
        assert_eq!(
            display_node_name("bluez_output.B0:F0:0C:5E:99:5A", &i18n),
            "Bluetooth Output - 99:5A"
        );
        assert_eq!(
            display_port_name("Midi Through: Port-0 (capture)", &i18n),
            "Port 0 Capture"
        );
    }

    #[test]
    fn relay_devices_are_named_by_role_in_the_selected_locale() {
        // The raw filter names ("qpwgraph-rs.relay.sink") used to leak onto
        // the canvas because nothing claimed them before the fallback.
        let english = I18n::default();
        assert_eq!(
            display_node_name(RELAY_SOURCE_NAME, &english),
            "Relay Input"
        );
        assert_eq!(display_node_name(RELAY_SINK_NAME, &english), "Relay Output");

        let spanish = I18n::from_language("es");
        assert_eq!(
            display_node_name(RELAY_SOURCE_NAME, &spanish),
            "Entrada del relé"
        );
        assert_eq!(
            display_node_name(RELAY_SINK_NAME, &spanish),
            "Salida del relé"
        );
    }

    #[test]
    fn display_names_follow_the_selected_locale() {
        let i18n = I18n::from_language("es");
        assert_eq!(
            display_node_name("bluez_input.B0:F0:0C:5E:99:5A", &i18n),
            "Entrada Bluetooth - 99:5A"
        );
        assert_eq!(
            display_port_name("Midi Through: Port-0 (capture)", &i18n),
            "Port 0 Captura"
        );
    }

    #[test]
    fn cameras_are_named_by_role_regardless_of_monitor() {
        let i18n = I18n::default();
        let camera = i18n.text("canvas.node_name_camera_input");
        assert_eq!(
            display_node_name("v4l2_input.pci-0000_00_10.0-usb-0_2_1.0", &i18n),
            camera
        );
        assert_eq!(
            display_node_name("libcamera_input.ipu6-ov01a10-uf-0", &i18n),
            camera
        );
    }
}
