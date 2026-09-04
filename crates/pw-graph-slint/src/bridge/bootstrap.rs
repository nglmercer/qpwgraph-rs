//! Opening the application state: configuration, localization, the backend
//! driver, and the patchbay and node layout restored onto it.
//!
//! Everything here has to exist before there is a window to show it in,
//! which is why it does not sit in the constructor with the window wiring.

use super::*;
#[cfg(feature = "relay")]
use std::collections::BTreeSet;

/// The opened application state, plus the meter policy the window shows.
pub(super) fn bootstrap_application(args: &Args) -> (Rc<RefCell<Application>>, MeterPolicy) {
    let config_file = config_path("qpwgraph-rs");
    let config = AppConfig::load_from(&config_file).unwrap_or_default();
    let language = args
        .language
        .clone()
        .unwrap_or_else(|| config.language.clone());
    let i18n = I18n::from_language(&language);
    let meter_policy = MeterPolicy::parse(&config.audio_meters);
    let (mut source, mut status) = ApplicationDriver::new(args, meter_policy, &i18n);
    restore_node_positions(&mut source, &config);
    let patchbay_file = selected_patchbay_path(&config);
    let patchbay = Patchbay::load_from(&patchbay_file)
        .unwrap_or_else(|_| Patchbay::new(patchbay_file.display().to_string()));
    restore_standalone_effects(&mut source, &config, &mut status, &i18n);
    if config.patchbay_activated {
        match patchbay.activate(
            &mut source,
            config.patchbay_exclusive,
            config.patchbay_auto_disconnect,
        ) {
            Ok(report) if report.failed.is_empty() => {}
            Ok(report) => status.push_str(&format!(
                " · {}",
                i18n.format(
                    "status.activation_failed",
                    &[("error", report.failed.join("; "))],
                )
            )),
            Err(error) => status.push_str(&format!(
                " · {}",
                i18n.format("status.activation_failed", &[("error", error.to_string())])
            )),
        }
    }
    restore_inserted_effects(&mut source, &config, &mut status, &i18n);
    if !config.patchbay_activated {
        restore_effect_connections(&mut source, &patchbay, &mut status, &i18n);
    }
    if let Err(error) = source.refresh() {
        status = format!(
            "{status} · {}",
            i18n.format("status.refresh_failed", &[("error", error)])
        );
    }
    let view = UiGraphState::from_config(&config);
    let app = Rc::new(RefCell::new(Application {
        source,
        commands: pw_graph_command::CommandStack::new(),
        patchbay,
        patchbay_file,
        config: config.clone(),
        config_file,
        config_saved_snapshot: config,
        config_dirty_since: None,
        i18n,
        view,
        snapshot: GraphSnapshot::default(),
        status,
        toast_message: String::new(),
        toast_until: None,
        toast_error: false,
        pending_connection_pin: None,
        effect_draft_id: None,
        effect_draft_enabled: true,
        effect_draft_parameters: BTreeMap::new(),
        debug: args.debug,
        last_refresh: Instant::now(),
        meters: BTreeMap::new(),
        meter_error: None,
        #[cfg(feature = "relay")]
        relay_levels: BTreeMap::new(),
        #[cfg(feature = "relay")]
        relay_connecting: None,
        #[cfg(feature = "relay")]
        relay_discovery_active: false,
        #[cfg(feature = "relay")]
        relay_usb_present: false,
        #[cfg(feature = "relay")]
        relay_usb_last_poll: None,
        #[cfg(feature = "relay")]
        relay_usb_auto_attempted: false,
        #[cfg(feature = "relay")]
        relay_trusted_auto_attempt_at: None,
        #[cfg(feature = "relay")]
        relay_trusted_candidate_failures: BTreeMap::new(),
        #[cfg(feature = "relay")]
        relay_trusted_refused: BTreeSet::new(),
        #[cfg(feature = "relay")]
        relay_pending_enrollment: None,
        #[cfg(feature = "relay")]
        relay_reconnect_pending: None,
        #[cfg(feature = "relay")]
        relay_direction_switch: None,
        #[cfg(feature = "relay")]
        relay_direction_ui_sync: None,
        #[cfg(feature = "relay")]
        relay_route_preferences_applied: false,
    }));

    (app, meter_policy)
}
