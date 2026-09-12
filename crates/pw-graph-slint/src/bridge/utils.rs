use crate::model::MeterState;
use pw_graph_backend::MeterPolicy;
use pw_graph_core::NodeType;
use pw_graph_i18n::I18n;
use slint::Color;

pub(crate) fn localized_node_type(i18n: &I18n, node_type: NodeType) -> String {
    let key = match node_type {
        NodeType::PipeWire => "canvas.node_type_pipewire",
        NodeType::Effect => "canvas.node_type_effect",
        NodeType::Recorder => "canvas.node_type_recorder",
        NodeType::AlsaMidi => "canvas.node_type_alsa_midi",
        NodeType::WindowsAudioEndpoint | NodeType::WindowsAudioSession => {
            "canvas.node_type_windows_audio"
        }
        NodeType::WindowsMidi => "canvas.node_type_windows_midi",
        NodeType::Unknown => "canvas.node_type_unknown",
    };
    i18n.text(key)
}

pub(crate) fn localized_meter_label(i18n: &I18n, state: MeterState) -> String {
    let key = match state {
        MeterState::Unavailable => "canvas.unknown",
        MeterState::Disabled => "meters.off",
        MeterState::Waiting => "canvas.audio_meter_starting",
        MeterState::Live => "canvas.audio_meter_live",
        MeterState::Demo => "canvas.audio_meter_demo",
    };
    i18n.text(key)
}

pub(crate) fn meter_policy_index(policy: MeterPolicy) -> i32 {
    match policy {
        MeterPolicy::Disabled => 0,
        MeterPolicy::OnDemand => 1,
        MeterPolicy::Always => 2,
    }
}

pub(crate) fn meter_policy_from_index(index: i32) -> MeterPolicy {
    match index {
        0 => MeterPolicy::Disabled,
        2 => MeterPolicy::Always,
        _ => MeterPolicy::OnDemand,
    }
}

pub(crate) fn localized_meter_policy(i18n: &I18n, policy: MeterPolicy) -> String {
    let key = match policy {
        MeterPolicy::Disabled => "meters.off",
        MeterPolicy::OnDemand => "meters.on_demand",
        MeterPolicy::Always => "meters.always",
    };
    i18n.text(key)
}

pub(crate) fn language_index(language: &str) -> i32 {
    match language.trim().to_ascii_lowercase().as_str() {
        "es" | "es-es" => 1,
        "fr" | "fr-fr" => 2,
        _ => 0,
    }
}

pub(crate) fn language_code(index: i32) -> &'static str {
    match index {
        1 => "es",
        2 => "fr",
        _ => "en",
    }
}

/// Where unity gain sits on the track for a node whose ceiling is `max_volume`.
///
/// A node that can boost keeps unity at 90%, leaving the top tenth for gain
/// above 1.0. A node clamped at unity has no boost region, so unity is the top
/// of the track: otherwise the last tenth of the fader would be dead travel
/// that silently clamps, which is what the Windows cards used to do.
fn unity_track_position(max_volume: f32) -> f32 {
    if max_volume > 1.0 {
        0.9
    } else {
        1.0
    }
}

pub(crate) fn volume_from_track_position(position: f32, max_volume: f32) -> f32 {
    let max_volume = max_volume.max(0.01);
    let unity = unity_track_position(max_volume);
    let position = position.clamp(0.0, 1.0);
    if position <= unity {
        position / unity
    } else {
        1.0 + (position - unity) / (1.0 - unity) * (max_volume - 1.0)
    }
}

/// Inverse of [`volume_from_track_position`], for drawing a backend-reported
/// level on the same track the user drags.
pub(crate) fn track_position_from_volume(volume: f32, max_volume: f32) -> f32 {
    let max_volume = max_volume.max(0.01);
    let unity = unity_track_position(max_volume);
    let volume = volume.clamp(0.0, max_volume);
    if volume <= 1.0 {
        volume * unity
    } else {
        unity + (volume - 1.0) / (max_volume - 1.0) * (1.0 - unity)
    }
}

/// Match the canvas meter scale: audio levels are shown over a -60 dBFS to
/// 0 dBFS range, not as a linear 0.0–1.0 amplitude fraction.
pub(crate) fn meter_fraction(value: f32) -> f32 {
    let value = value.clamp(0.000001, 1.0);
    ((20.0 * value.log10() + 60.0) / 60.0).clamp(0.0, 1.0)
}

pub(crate) fn color(rgba: [u8; 4]) -> Color {
    Color::from_argb_u8(rgba[3], rgba[0], rgba[1], rgba[2])
}
