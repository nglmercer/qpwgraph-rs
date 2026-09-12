use crate::model::{GraphSnapshot, MeterReading, UiGraphState};
use crate::source::ApplicationDriver;
use pw_graph_command::CommandStack;
use pw_graph_config::AppConfig;
use pw_graph_core::PortKey;
use pw_graph_i18n::I18n;
use pw_graph_patchbay::{Patchbay, PatchbayReconciler};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[cfg(feature = "relay")]
use pw_graph_backend::RelayPeerInfo;

pub(crate) enum UiEvent {
    Action(String),
    EffectSelected(i32),
    EffectCreateRequested,
    EffectConfigBack,
    EffectToggle {
        instance_id: String,
    },
    EffectRemove {
        instance_id: String,
    },
    EffectInspect {
        instance_id: String,
    },
    EffectDebug {
        instance_id: String,
    },
    EffectDebugClose,
    EffectDebugCopy,
    EffectCancel {
        ticket: u64,
    },
    EffectParameterChanged {
        instance_id: String,
        parameter_id: String,
        value: f32,
    },
    EffectDraftParameterChanged {
        parameter_id: String,
        value: f32,
    },
    EffectDraftEnabledChanged(bool),
    RecorderRecord(i32),
    RecorderStop(i32),
    RecorderSave(i32),
    RecorderDiscard(i32),
    SelectNode(i32, bool),
    SelectLink(i32, bool),
    ClearSelection,
    SelectBox(f32, f32, f32, f32, bool),
    LinkRequested(i32, i32),
    LinkCancelled,
    /// An Easy-mode drag of a whole card was dropped at a world position.
    NodeConnectDropped(i32, f32, f32, i32),
    /// A pin drag was dropped away from any pin.
    LinkDropped(i32, f32, f32),
    ToggleCollapse(i32),
    DragCommitted(i32, f32, f32),
    /// An existing link was dragged onto a different pin.
    LinkRerouted(/* link */ i32, /* new pin */ i32),
    SetAudioVolume(i32, f32),
    ToggleAudioMute(i32),
}

/// A relay connection attempt whose outcome the UI is still waiting for.
///
/// Only constructed when the relay feature is compiled in; the field that
/// holds it stays present either way so the rest of the application state is
/// one shape across feature combinations.
#[derive(Clone, Debug)]
#[cfg_attr(not(feature = "relay"), allow(dead_code))]
pub(crate) struct RelayAttempt {
    pub(crate) target: String,
    pub(crate) session: u64,
    /// Stable peer identity, when this attempt came from discovery. Keeping
    /// it separate from the socket address prevents a Wi-Fi/USB address
    /// change from looking like a second unrelated connection attempt.
    pub(crate) peer_id: Option<String>,
}

/// The desktop endpoint switch waits for every active authenticated session
/// to agree on the new direction before replacing the local audio endpoint.
/// Keeping this state in the bridge makes rapid tab changes coalesce without
/// exposing a second, role-shaped state machine to the UI.
#[cfg(feature = "relay")]
#[derive(Clone, Debug)]
pub(crate) struct RelayDirectionSwitch {
    pub(crate) from: pw_graph_config::RelayMode,
    pub(crate) target: pw_graph_config::RelayMode,
    pub(crate) generation: u64,
    pub(crate) sessions: BTreeSet<u64>,
    pub(crate) resolved_sessions: BTreeSet<u64>,
    pub(crate) resolved: bool,
    pub(crate) peer: Option<RelayPeerInfo>,
    pub(crate) started_at: Instant,
}

pub(crate) struct Application {
    pub(crate) source: ApplicationDriver,
    pub(crate) commands: CommandStack,
    pub(crate) patchbay: Patchbay,
    pub(crate) patchbay_reconciler: PatchbayReconciler,
    pub(crate) patchbay_graph_generation: u64,
    /// Runtime-only desired-state suppression for an explicit user delete.
    /// This prevents a later topology sync from turning a manually removed
    /// edge back into a saved rule during the current session.
    pub(crate) manually_suppressed_patchbay: Vec<(PortKey, PortKey)>,
    pub(crate) patchbay_file: PathBuf,
    pub(crate) config: AppConfig,
    pub(crate) config_file: PathBuf,
    pub(crate) config_saved_snapshot: AppConfig,
    pub(crate) config_dirty_since: Option<Instant>,
    pub(crate) i18n: I18n,
    pub(crate) view: UiGraphState,
    pub(crate) snapshot: GraphSnapshot,
    pub(crate) status: String,
    pub(crate) toast_message: String,
    pub(crate) toast_until: Option<Instant>,
    pub(crate) toast_error: bool,
    pub(crate) pending_connection_pin: Option<i32>,
    /// Transient effect-gallery configuration. It is deliberately kept out
    /// of `AppConfig` until the user confirms creation.
    pub(crate) effect_draft_id: Option<String>,
    /// Stable descriptor identity projected into the ComboBox as an index.
    /// The index is never persisted or used as the effect identity.
    pub(crate) effect_selection_id: Option<String>,
    pub(crate) effect_draft_enabled: bool,
    pub(crate) effect_draft_parameters: BTreeMap<String, f32>,
    /// Submitted effect preparations. Closing the effects dialog only drops
    /// the unsubmitted draft; these records remain until a terminal backend
    /// event arrives or the user explicitly cancels one.
    pub(crate) pending_effect_tickets: BTreeMap<pw_graph_backend::EffectTicket, PendingEffectUi>,
    pub(crate) effect_debug_name: String,
    pub(crate) effect_debug_health: String,
    pub(crate) effect_debug_report: String,
    pub(crate) patchbay_debug_report: String,
    pub(crate) node_debug_name: String,
    pub(crate) node_debug_report: String,
    /// Recorder instances are runtime graph resources. Their lifecycle is
    /// intentionally not persisted as ordinary configuration.
    pub(crate) recorders:
        BTreeMap<pw_graph_backend::RecorderId, pw_graph_backend::RecorderInstance>,
    /// Recorder stops are finalized by the writer thread. These IDs await a
    /// completion event before the Save dialog is offered.
    pub(crate) pending_recorder_stops: BTreeSet<pw_graph_backend::RecorderId>,
    /// Crash-repairable `.wav.part` files discovered at startup. They stay
    /// outside the normal recorder graph because no live node owns them.
    pub(crate) recovered_recordings: Vec<pw_graph_backend::router::RecoveredRecording>,
    pub(crate) recovery_dialog_visible: bool,
    pub(crate) debug: bool,
    pub(crate) last_refresh: Instant,
    /// Last time the full model sync ran. The 50 ms pump only refreshes
    /// meters on most ticks; topology, selection, config and relay models
    /// rebuild here at a slower cadence or when something actually changed.
    pub(crate) last_full_sync: Instant,
    /// Fingerprint of the layout inputs the last `sync_config` consumed, so
    /// an idle pump can skip rebuilding every layout map.
    pub(crate) config_layout_fingerprint: u64,
    pub(crate) meters: BTreeMap<pw_graph_core::NodeId, MeterReading>,
    pub(crate) meter_error: Option<String>,
    /// Audio controls are live UI state only. They are intentionally not
    /// restored from a second Slint-specific file on startup.
    #[cfg(feature = "relay")]
    pub(crate) relay_levels: BTreeMap<u64, f32>,
    #[cfg(feature = "relay")]
    /// The connection attempt currently shown as "connecting", if any: the
    /// target address for display plus the attempt's session id, so a
    /// `SessionLost` for *this* attempt clears it and one for an unrelated
    /// session does not.
    pub(crate) relay_connecting: Option<RelayAttempt>,
    #[cfg(feature = "relay")]
    /// Whether relay discovery is currently running in the application.
    pub(crate) relay_discovery_active: bool,
    #[cfg(feature = "relay")]
    /// Current active USB tether state used by the hotplug watcher.
    pub(crate) relay_usb_present: bool,
    #[cfg(feature = "relay")]
    /// Last native interface enumeration performed by the fallback hotplug
    /// watcher. The UI pump runs much faster than a link can change.
    pub(crate) relay_usb_last_poll: Option<Instant>,
    #[cfg(feature = "relay")]
    /// Prevent a manual Stop from being undone until this USB appearance ends.
    pub(crate) relay_usb_auto_attempted: bool,
    #[cfg(feature = "relay")]
    /// Last trusted auto-connect attempt. A failed attempt must not spin every
    /// UI frame, but a transient cable/link failure should be retried even if
    /// discovery does not emit another `PeerDiscovered` event.
    pub(crate) relay_trusted_auto_attempt_at: Option<Instant>,
    #[cfg(feature = "relay")]
    /// `(peer_id,address)`-scoped backoff for discovered candidates. A
    /// spoofed stable ID must not make every other address for the same
    /// trusted peer unusable, nor couple unrelated peers sharing an address.
    pub(crate) relay_trusted_candidate_failures: BTreeMap<(String, String), (u8, Instant)>,
    #[cfg(feature = "relay")]
    /// Trusted-peer addresses that answered a dial with an active refusal or
    /// unreachable verdict. A stored address here is no longer re-injected as
    /// a reconnect candidate — without this, a phone that changed IP leaves
    /// the app retrying its old lease forever. Cleared when discovery
    /// re-announces the address or a session with it succeeds.
    pub(crate) relay_trusted_refused: BTreeSet<(String, String)>,
    #[cfg(feature = "relay")]
    pub(crate) relay_pending_enrollment: Option<PendingEnrollment>,
    #[cfg(feature = "relay")]
    pub(crate) relay_reconnect_pending: Option<ReconnectPending>,
    #[cfg(feature = "relay")]
    pub(crate) relay_direction_switch: Option<RelayDirectionSwitch>,
    #[cfg(feature = "relay")]
    /// A conflict winner can differ from the tab that initiated a switch.
    /// The next window-state read consumes this one-shot UI correction before
    /// interpreting the tab as a new user request.
    pub(crate) relay_direction_ui_sync: Option<pw_graph_config::AudioDirection>,
    #[cfg(feature = "relay")]
    /// Ensures persisted local source/sink selections are applied once after
    /// startup, without re-running PipeWire route reconciliation every frame.
    pub(crate) relay_route_preferences_applied: bool,
}

/// UI metadata for a submitted effect preparation. The ticket belongs to the
/// backend lifecycle; this record keeps the operation visible after its
/// configuration dialog has closed.
#[derive(Clone, Debug)]
pub(crate) struct PendingEffectUi {
    pub(crate) ticket: pw_graph_backend::EffectTicket,
    pub(crate) effect_id: String,
    pub(crate) effect_name: String,
    pub(crate) stage: pw_graph_backend::EffectLoadStage,
    pub(crate) started_at: Instant,
    pub(crate) cancellable: bool,
}

#[cfg(feature = "relay")]
#[derive(Clone, Debug)]
pub(crate) struct PendingEnrollment {
    pub(crate) transaction_id: u64,
    pub(crate) peer_id: String,
    pub(crate) peer_name: String,
    pub(crate) peer_addr: String,
}

#[cfg(feature = "relay")]
#[derive(Clone, Debug)]
pub(crate) struct ReconnectPending {
    pub(crate) peer_id: String,
    pub(crate) peer_name: String,
    pub(crate) peer_addr: String,
    pub(crate) next_retry: Instant,
}

const CONNECTION_TOAST_DURATION: Duration = Duration::from_secs(4);

/// Publish short-lived user feedback for any background or graph operation.
/// The compatibility-named wrapper below keeps the existing connection call
/// sites readable while effects and future operations use the generic path.
pub(crate) fn set_app_feedback(
    application: &mut Application,
    message: impl Into<String>,
    error: bool,
) {
    let message = message.into();
    application.status = message.clone();
    application.toast_message = message;
    application.toast_error = error;
    application.toast_until = Some(Instant::now() + CONNECTION_TOAST_DURATION);
}

pub(crate) fn set_connection_feedback(
    application: &mut Application,
    message: impl Into<String>,
    error: bool,
) {
    set_app_feedback(application, message, error);
}

pub(crate) fn toast_visible(application: &Application) -> bool {
    application
        .toast_until
        .is_some_and(|deadline| Instant::now() < deadline)
}

impl Application {
    pub(crate) fn mark_patchbay_graph_dirty(&mut self) {
        self.patchbay_graph_generation = self.patchbay_graph_generation.wrapping_add(1);
        if self.config.patchbay_activated {
            self.patchbay_reconciler.mark_dirty(Instant::now());
        }
    }

    pub(crate) fn reconcile_patchbay(&mut self) -> bool {
        if !self.config.patchbay_activated {
            return false;
        }
        match self.patchbay_reconciler.reconcile_if_due(
            &self.patchbay,
            &mut self.source,
            self.config.patchbay_exclusive,
            self.config.patchbay_auto_disconnect,
            Instant::now(),
            self.patchbay_graph_generation,
        ) {
            Ok(Some(report)) => {
                if report.connected > 0 || report.disconnected > 0 {
                    self.sync_patchbay_connections();
                    self.autosave_patchbay();
                }
                if let Some(warning) = report.warnings.first() {
                    self.status = self.tf(
                        "status.patchbay_reconcile_warning",
                        &[("warning", warning.clone())],
                    );
                }
                true
            }
            Ok(None) => false,
            Err(error) => {
                self.status = self.tf(
                    "status.patchbay_reconcile_failed",
                    &[("error", error.to_string())],
                );
                true
            }
        }
    }

    /// Keep durable rules synchronized with the live graph while preserving
    /// the original endpoints of inserted effects. Numeric IDs are only a
    /// cache; typed application/effect identity plus role/channel is the
    /// durable selector, with names as the compatibility fallback.
    pub(crate) fn live_connection_keys(&self) -> Vec<(PortKey, PortKey)> {
        self.source
            .graph()
            .links
            .values()
            .filter(|link| self.source.is_link_mutable(link.id))
            .filter_map(|link| {
                self.source
                    .graph()
                    .port_key(link.output_port)
                    .zip(self.source.graph().port_key(link.input_port))
            })
            .collect()
    }

    pub(crate) fn sync_patchbay_connections(&mut self) {
        for (output, input) in self.live_connection_keys() {
            if self
                .manually_suppressed_patchbay
                .iter()
                .any(|pair| stable_pair_matches(pair, &(output.clone(), input.clone())))
            {
                continue;
            }
            let Some(output_id) = self.source.graph().resolve_port_key(&output) else {
                continue;
            };
            let Some(input_id) = self.source.graph().resolve_port_key(&input) else {
                continue;
            };
            self.patchbay.add_graph_connection(
                self.source.graph(),
                output_id,
                input_id,
                self.config.patchbay_auto_pin,
            );
        }
    }

    /// Remove only rules corresponding to a deliberate user disconnect. A
    /// refresh must never discard unresolved saved intent: nodes can be
    /// temporarily absent while PipeWire or ALSA is starting.
    pub(crate) fn remove_patchbay_connections(&mut self, pairs: &[(PortKey, PortKey)]) {
        for (output, input) in pairs {
            self.patchbay.remove_stable_connection(output, input);
            if !self
                .manually_suppressed_patchbay
                .iter()
                .any(|pair| stable_pair_matches(pair, &(output.clone(), input.clone())))
            {
                self.manually_suppressed_patchbay
                    .push((output.clone(), input.clone()));
            }
        }
    }

    pub(crate) fn allow_patchbay_connections(&mut self, pairs: &[(PortKey, PortKey)]) {
        self.manually_suppressed_patchbay
            .retain(|saved| !pairs.iter().any(|pair| stable_pair_matches(saved, pair)));
    }

    pub(crate) fn autosave_patchbay(&mut self) {
        if let Err(error) = self.patchbay.save_to(&self.patchbay_file) {
            self.status = self.tf(
                "status.patchbay_save_failed",
                &[("error", error.to_string())],
            );
        }
    }

    pub(crate) fn t(&self, key: &str) -> String {
        self.i18n.text(key)
    }

    pub(crate) fn tf(&self, key: &str, values: &[(&str, String)]) -> String {
        self.i18n.format(key, values)
    }

    pub(crate) fn history(&self) -> (Vec<String>, Vec<String>) {
        (self.commands.undo_history(), self.commands.redo_history())
    }
}

fn stable_pair_matches(left: &(PortKey, PortKey), right: &(PortKey, PortKey)) -> bool {
    stable_selector_matches(&left.0.selector(), &right.0.selector())
        && stable_selector_matches(&left.1.selector(), &right.1.selector())
}

fn stable_selector_matches(
    left: &pw_graph_core::EndpointSelector,
    right: &pw_graph_core::EndpointSelector,
) -> bool {
    let app_channel_matches = !matches!(
        left.match_mode,
        pw_graph_core::EndpointMatchMode::NamePattern
    ) && !matches!(
        right.match_mode,
        pw_graph_core::EndpointMatchMode::NamePattern
    ) && left.channel.is_some()
        && left.channel == right.channel
        && (left.identity.application_id.is_some()
            || left.identity.process_binary.is_some()
            || left.identity.application_name.is_some());
    let port_name_matches = left.port_name == right.port_name || app_channel_matches;
    if left.node_type != right.node_type
        || !port_name_matches
        || left.channel != right.channel
        || left.direction != right.direction
        || left.port_type != right.port_type
    {
        return false;
    }
    let a = &left.identity;
    let b = &right.identity;
    if a.effect_instance_id.is_some() || b.effect_instance_id.is_some() {
        return a.effect_instance_id == b.effect_instance_id;
    }
    if a.application_id.is_some() || b.application_id.is_some() {
        return a.application_id.is_some() && a.application_id == b.application_id;
    }
    if a.process_binary.is_some() || b.process_binary.is_some() {
        return a.process_binary.is_some()
            && a.process_binary == b.process_binary
            && (a.application_name.is_none()
                || b.application_name.is_none()
                || a.application_name == b.application_name);
    }
    a.node_name == b.node_name
}
