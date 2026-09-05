use pw_graph_i18n::I18n;
use std::time::{Duration, Instant};

use super::app::Application;
use super::patchbay::activate_patchbay;
#[cfg(feature = "relay")]
use super::relay::relay_mode_tab;
use super::relay::{
    handle_relay_direction_change, relay_codec_from_index, relay_frame_from_index,
    relay_mode_from_tab, relay_transport_from_index,
};
use super::utils::{language_code, localized_meter_policy, meter_policy_from_index};
use super::MainWindow;
#[cfg(feature = "relay")]
use pw_graph_backend::{RelayEndpointInfo, RelayReceiveSink, RelaySendSource};

pub(crate) fn read_window_state(window: &MainWindow, application: &mut Application) {
    #[cfg(feature = "relay")]
    if let Some(direction) = application.relay_direction_ui_sync.take() {
        if window.get_relay_tab() < 2 {
            window.set_relay_tab(relay_mode_tab(
                pw_graph_config::RelayMode::from_audio_direction(direction),
            ));
        }
    }
    let patchbay_was_activated = application.config.patchbay_activated;
    application.view.zoom = window.get_zoom().clamp(0.35, 2.5);
    application.view.pan = [window.get_pan_x(), window.get_pan_y()];
    application.view.search_query = window.get_search_text().to_string();
    application.view.thumbnail_mode = window.get_thumbnail_view();
    application.view.node_text_scale = window.get_node_text_scale().clamp(0.8, 2.0);
    application.config.statusbar = window.get_show_statusbar();
    application.config.toolbar = window.get_show_common_actions();
    application.config.patchbay_toolbar = window.get_show_patchbay_toolbar();
    application.config.repel_overlapping_nodes = window.get_repel_overlaps();
    application.config.connect_through_nodes = window.get_connect_through();
    application.config.thumbnail_view = application.view.thumbnail_mode;
    application.config.ui_text_scale = window.get_ui_text_scale().clamp(0.8, 2.0);
    application.config.panel_text_scale = window.get_panel_text_scale().clamp(0.8, 2.0);
    application.config.node_text_scale = application.view.node_text_scale;
    application.config.patchbay_exclusive = window.get_patchbay_exclusive();
    application.config.patchbay_auto_disconnect = window.get_patchbay_auto_disconnect();
    application.config.patchbay_auto_pin = window.get_patchbay_auto_pin();
    application.config.patchbay_activated = window.get_patchbay_activated();
    application.config.active_patchbay_profile = window.get_profile_name().to_string();
    let language = language_code(window.get_language_index());
    if application.config.language != language {
        application.config.language = language.into();
        application.i18n = I18n::from_language(language);
        application.status = application.i18n.text("status.language_changed");
    }
    application.config.window_width = window.get_width_().max(760.0);
    application.config.window_height = window.get_height_().max(520.0);
    application.config.relay_device_name = window.get_relay_device_name().to_string();
    application.config.relay_host_pin = window.get_relay_host_pin().to_string();
    application.config.relay_host_port = window
        .get_relay_host_port_text()
        .trim()
        .parse::<u16>()
        .unwrap_or(application.config.relay_host_port);
    application.config.relay_client_target = window.get_relay_client_target().to_string();
    application.config.relay_client_pin = window.get_relay_client_pin().to_string();
    application.config.relay_auto_connect_trusted = window.get_relay_auto_connect_trusted();
    let old_mode = application.config.relay_mode;
    let next_mode = relay_mode_from_tab(window.get_relay_tab(), old_mode);
    if next_mode != old_mode {
        application.config.set_relay_mode(
            next_mode,
            application.config.relay_mode_generation.saturating_add(1),
        );
        handle_relay_direction_change(application, old_mode, next_mode);
    }
    application.config.relay_codec = relay_codec_from_index(window.get_relay_codec_index()).into();
    application.config.relay_frame_ms = relay_frame_from_index(window.get_relay_frame_index());
    application.config.relay_transport =
        relay_transport_from_index(window.get_relay_transport_index()).into();
    #[cfg(feature = "relay")]
    {
        let send_options = application.source.relay_send_sources();
        #[cfg(target_os = "windows")]
        let send_options = if application.config.windows.enable_process_loopback {
            send_options
        } else {
            send_options
                .into_iter()
                .filter(|choice| !choice.id.starts_with("application:"))
                .collect()
        };
        let receive_options = application.source.relay_receive_sinks();
        let mut send_changed = false;
        let mut receive_changed = false;
        #[cfg(target_os = "windows")]
        if !application.config.windows.enable_process_loopback
            && application
                .config
                .relay_send_source
                .starts_with("application:")
        {
            application.config.relay_send_source = "default-input".into();
            send_changed = true;
        }
        if !send_options.is_empty() {
            let selected = relay_selector_id(
                window.get_relay_send_source_index(),
                &send_options,
                &application.config.relay_send_source,
            );
            send_changed = selected != application.config.relay_send_source;
            application.config.relay_send_source = selected;
        }
        if !receive_options.is_empty() {
            let selected = relay_selector_id(
                window.get_relay_receive_sink_index(),
                &receive_options,
                &application.config.relay_receive_sink,
            );
            receive_changed = selected != application.config.relay_receive_sink;
            application.config.relay_receive_sink = selected;
        }

        if application.source.relay_available()
            && (!application.relay_route_preferences_applied || send_changed || receive_changed)
        {
            if let Err(error) = application
                .source
                .relay_set_send_source(relay_send_source(&application.config.relay_send_source))
            {
                application.status = application.tf("relay.error", &[("error", error)]);
            }
            if let Err(error) = application
                .source
                .relay_set_receive_sink(relay_receive_sink(&application.config.relay_receive_sink))
            {
                application.status = application.tf("relay.error", &[("error", error)]);
            }
        }
        application.relay_route_preferences_applied = true;
    }

    let meter_policy = meter_policy_from_index(window.get_meter_policy_index());
    if meter_policy != application.source.meter_policy() {
        application.config.audio_meters = meter_policy.as_str().into();
        if let Err(error) = application.source.set_meter_policy(meter_policy) {
            application.status = application.tf("status.meter_policy_failed", &[("error", error)]);
        } else {
            application.meters.clear();
            application.status = application.tf(
                "status.meter_policy_changed",
                &[(
                    "policy",
                    localized_meter_policy(&application.i18n, meter_policy),
                )],
            );
        }
    }

    if !patchbay_was_activated && application.config.patchbay_activated {
        activate_patchbay(application);
    }
}

#[cfg(feature = "relay")]
fn relay_selector_id(index: i32, choices: &[RelayEndpointInfo], current: &str) -> String {
    choices
        .get(index.max(0) as usize)
        .map(|endpoint| endpoint.id.clone())
        .unwrap_or_else(|| current.to_owned())
}

#[cfg(feature = "relay")]
fn relay_send_source(selector: &str) -> RelaySendSource {
    match selector {
        "default-input" | "" => RelaySendSource::DefaultInput,
        "default-output-monitor" => RelaySendSource::DefaultOutputMonitor,
        "manual" => RelaySendSource::ManualGraph,
        selector => selector
            .strip_prefix("input:")
            .map(|id| RelaySendSource::InputDevice(id.to_owned()))
            .or_else(|| {
                selector
                    .strip_prefix("monitor:")
                    .map(|id| RelaySendSource::OutputMonitor(id.to_owned()))
            })
            .or_else(|| {
                selector
                    .strip_prefix("application:")
                    .map(|id| RelaySendSource::Application(id.to_owned()))
            })
            .unwrap_or_else(|| RelaySendSource::InputDevice(selector.to_owned())),
    }
}

#[cfg(all(feature = "relay", target_os = "windows"))]
#[allow(dead_code)]
fn relay_endpoint_id(
    index: i32,
    choices: &[(String, String)],
    current: Option<&str>,
) -> Option<String> {
    if index <= 0 {
        return None;
    }
    let position = (index as usize).checked_sub(1)?;
    choices
        .get(position)
        .map(|(id, _)| id.clone())
        .or_else(|| current.map(str::to_owned))
}

#[cfg(feature = "relay")]
fn relay_receive_sink(selector: &str) -> RelayReceiveSink {
    match selector {
        "default-output" | "" => RelayReceiveSink::DefaultOutput,
        "virtual-microphone" => RelayReceiveSink::VirtualMicrophone,
        "manual" => RelayReceiveSink::ManualGraph,
        selector => RelayReceiveSink::OutputDevice(
            selector
                .strip_prefix("output:")
                .unwrap_or(selector)
                .to_owned(),
        ),
    }
}

fn sync_config(application: &mut Application) {
    application.config.zoom = application.view.zoom;
    application.config.thumbnail_view = application.view.thumbnail_mode;
    application.config.minimap_visible = application.view.minimap_visible;
    application.config.connect_mode = application.view.connect_mode.as_str().into();
    application.config.media_filter = application.view.media_filter.as_str().into();
    application.config.graph_search = application.view.search_query.clone();
    application.config.node_text_scale = application.view.node_text_scale;
    application.config.sort_type = if application.view.sort_ports_by_name {
        "name"
    } else {
        "id"
    }
    .into();
    application.config.sort_order = if application.view.sort_ports_descending {
        "descending"
    } else {
        "ascending"
    }
    .into();
    application
        .view
        .write_to_config(application.source.graph(), &mut application.config);
}

pub(crate) fn autosave_config(application: &mut Application) {
    sync_config(application);
    if application.config == application.config_saved_snapshot {
        application.config_dirty_since = None;
        return;
    }
    let dirty_since = application
        .config_dirty_since
        .get_or_insert_with(Instant::now);
    if dirty_since.elapsed() >= Duration::from_millis(500) {
        save_config(application, false);
    }
}

pub(crate) fn save_config(application: &mut Application, report_success: bool) {
    sync_config(application);
    match application.config.save_to(&application.config_file) {
        Ok(()) => {
            application.config_saved_snapshot = application.config.clone();
            application.config_dirty_since = None;
            if report_success {
                application.status = application.tf(
                    "status.config_saved_to",
                    &[("path", application.config_file.display().to_string())],
                );
            }
        }
        Err(error) => {
            application.status =
                application.tf("status.config_save_failed", &[("error", error.to_string())]);
            application.config_dirty_since = Some(Instant::now());
        }
    }
}

#[cfg(all(test, feature = "relay", target_os = "windows"))]
mod tests {
    use super::{relay_endpoint_id, relay_send_source};
    use pw_graph_backend::RelaySendSource;

    #[test]
    fn relay_endpoint_indices_round_trip_stable_ids() {
        let choices = vec![
            ("endpoint-a".into(), "Speakers".into()),
            ("endpoint-b".into(), "Headphones".into()),
        ];
        assert_eq!(relay_endpoint_id(0, &choices, Some("endpoint-a")), None);
        assert_eq!(
            relay_endpoint_id(1, &choices, None),
            Some("endpoint-a".into())
        );
        assert_eq!(
            relay_endpoint_id(2, &choices, None),
            Some("endpoint-b".into())
        );
    }

    #[test]
    fn missing_relay_endpoint_keeps_its_saved_id_until_the_user_changes_it() {
        let choices = vec![("endpoint-a".into(), "Speakers".into())];
        assert_eq!(
            relay_endpoint_id(99, &choices, Some("removed-endpoint")),
            Some("removed-endpoint".into())
        );
    }

    #[test]
    fn application_relay_selector_round_trips_without_a_pid() {
        assert_eq!(
            relay_send_source("application:sha256:abc"),
            RelaySendSource::Application("sha256:abc".into())
        );
    }
}
