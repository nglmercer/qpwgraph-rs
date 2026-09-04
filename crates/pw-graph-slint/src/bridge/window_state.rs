//! Copying the opened application state into the window's properties.
//!
//! Slint properties are the window's own state, so each one has to be
//! seeded before the window is shown. Doing that in the constructor buried
//! the constructor; doing it here makes a forgotten property visible as the
//! one its group does not mention.

use super::*;

/// Resolve localized strings through the application's `I18n`.
pub(super) fn install_i18n(window: &MainWindow, app: &Rc<RefCell<Application>>) {
    let app_for_text = app.clone();
    window
        .global::<UiI18n>()
        .on_text(move |key| SharedString::from(app_for_text.borrow().i18n.text(key.as_str())));
    let app_for_format = app.clone();
    window.global::<UiI18n>().on_format_one(move |key, value| {
        let value = value.to_string();
        SharedString::from(app_for_format.borrow().i18n.format(
            key.as_str(),
            &[
                ("count", value.clone()),
                ("path", value.clone()),
                ("port", value.clone()),
                ("pin", value),
            ],
        ))
    });
    let application = app.borrow();
    window
        .global::<UiI18n>()
        .set_version(language_index(&application.config.language));
}

/// Seed every window property from the opened application state.
pub(super) fn apply_window_state(
    window: &MainWindow,
    app: &Rc<RefCell<Application>>,
    args: &Args,
    meter_policy: MeterPolicy,
) {
    let application = app.borrow();
    window.window().set_size(PhysicalSize::new(
        application.config.window_width.max(760.0).round() as u32,
        application.config.window_height.max(520.0).round() as u32,
    ));
    window.set_show_statusbar(application.config.statusbar);
    window.set_show_minimap(application.view.minimap_visible);
    window.set_sort_type(SharedString::from(if application.view.sort_ports_by_name {
        "name"
    } else {
        "id"
    }));
    window.set_sort_order(SharedString::from(
        if application.view.sort_ports_descending {
            "descending"
        } else {
            "ascending"
        },
    ));
    window.set_search_text(SharedString::from(application.view.search_query.clone()));
    window.set_media_filter(SharedString::from(application.view.media_filter.as_str()));
    window.set_connect_mode(SharedString::from(application.view.connect_mode.as_str()));
    window.set_pan_x(application.view.pan[0]);
    window.set_pan_y(application.view.pan[1]);
    window.set_zoom(application.view.zoom);
    window.set_show_common_actions(application.config.toolbar);
    window.set_show_patchbay_toolbar(application.config.patchbay_toolbar);
    window.set_repel_overlaps(application.config.repel_overlapping_nodes);
    window.set_connect_through(application.config.connect_through_nodes);
    window.set_thumbnail_view(application.view.thumbnail_mode);
    window.set_language_index(language_index(&application.config.language));
    window.set_meter_policy_index(meter_policy_index(meter_policy));
    window.set_ui_text_scale(application.config.ui_text_scale);
    window.set_panel_text_scale(application.config.panel_text_scale);
    window.set_node_text_scale(application.config.node_text_scale);
    window
        .global::<UiTheme>()
        .set_ui_scale(application.config.ui_text_scale);
    window
        .global::<UiTheme>()
        .set_panel_scale(application.config.panel_text_scale);
    window
        .global::<UiTheme>()
        .set_node_scale(application.config.node_text_scale);
    window.set_patchbay_exclusive(application.config.patchbay_exclusive);
    window.set_patchbay_auto_disconnect(application.config.patchbay_auto_disconnect);
    window.set_patchbay_auto_pin(application.config.patchbay_auto_pin);
    window.set_patchbay_activated(application.config.patchbay_activated);
    window.set_profile_name(SharedString::from(
        application.config.active_patchbay_profile.clone(),
    ));
    window.set_config_path(SharedString::from(
        application.config_file.display().to_string(),
    ));
    window.set_patchbay_path(SharedString::from(
        selected_patchbay_path(&application.config)
            .display()
            .to_string(),
    ));
    window.set_profile_options(string_model(profile_options(&application.config)));
    window.set_profile_index(profile_index(&application.config));
    window.set_recent_patchbay_paths(string_model(recent_patchbay_paths(&application.config)));
    window.set_relay_device_name(SharedString::from(
        application.config.relay_device_name.clone(),
    ));
    window.set_relay_host_pin(SharedString::from(
        application.config.relay_host_pin.clone(),
    ));
    window.set_relay_host_port_text(SharedString::from(
        application.config.relay_host_port.to_string(),
    ));
    window.set_relay_client_target(SharedString::from(
        application.config.relay_client_target.clone(),
    ));
    window.set_relay_client_pin(SharedString::from(
        application.config.relay_client_pin.clone(),
    ));
    window.set_relay_auto_connect_trusted(application.config.relay_auto_connect_trusted);
    if window.get_relay_tab() < 2 {
        window.set_relay_tab(relay_mode_tab(application.config.relay_mode));
    }
    window.set_relay_codec_index(relay_codec_index(&application.config.relay_codec));
    window.set_relay_frame_index(relay_frame_index(application.config.relay_frame_ms));
    window.set_relay_transport_index(relay_transport_index(&application.config.relay_transport));
    window.set_relay_codec_options(string_model([
        application.i18n.text("relay.codec_opus"),
        application.i18n.text("relay.codec_pcm"),
    ]));
    window.set_relay_frame_options(string_model(
        [5, 10, 20, 40, 60]
            .into_iter()
            .map(|frame| {
                application
                    .i18n
                    .format("relay.frame_option", &[("frame", frame.to_string())])
            })
            .collect::<Vec<_>>(),
    ));
    window.set_relay_transport_options(string_model([
        application.i18n.text("relay.transport_auto"),
        application.i18n.text("relay.transport_wifi"),
        application.i18n.text("relay.transport_bluetooth_pan"),
        application.i18n.text("relay.transport_lan"),
        application.i18n.text("relay.transport_adb"),
    ]));
    window.window().set_minimized(args.minimized);
}
