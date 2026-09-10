use crate::args::Args;
use crate::canvas::CanvasGeometry;
use crate::model::{restore_node_positions, GraphSnapshot, UiGraphState};
use crate::source::ApplicationDriver;
use pw_graph_backend::MeterPolicy;
use pw_graph_config::{config_path, AppConfig};
use pw_graph_i18n::I18n;
use pw_graph_patchbay::Patchbay;
use slint::{ComponentHandle, ModelRc, PhysicalSize, SharedString, Timer, TimerMode, VecModel};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

mod actions;
mod app;
mod bootstrap;
mod callbacks;
mod config;
mod connections;
mod effects;
mod effects_restore;
mod events;
mod icons;
#[cfg(test)]
mod keyboard;
mod meters;
mod models;
mod patchbay;
mod relay;
mod utils;
use app::*;
use callbacks::*;
use config::*;
use effects::*;
use events::*;
use meters::*;
use models::*;
use patchbay::*;
use relay::*;
use utils::*;

slint::include_modules!();

mod window_state;

#[cfg(test)]
mod tests;

use bootstrap::bootstrap_application;
use window_state::{apply_window_state, install_i18n};

pub(crate) struct UiBridge {
    window: MainWindow,
    app: Rc<RefCell<Application>>,
    nodes: Rc<VecModel<NodeRow>>,
    links: Rc<VecModel<LinkRow>>,
    minimap_nodes: Rc<VecModel<MinimapNode>>,
    shortcuts: Rc<VecModel<ShortcutRow>>,
    events: Rc<RefCell<Vec<UiEvent>>>,
    geometry: Rc<RefCell<CanvasGeometry>>,
    geometry_version: Rc<Cell<i32>>,
}

impl UiBridge {
    pub(crate) fn new(args: Args) -> Result<Self, slint::PlatformError> {
        let (app, meter_policy) = bootstrap_application(&args);

        let window = MainWindow::new()?;
        install_i18n(&window, &app);
        apply_window_state(&window, &app, &args, meter_policy);

        let nodes = Rc::new(VecModel::default());
        let links = Rc::new(VecModel::default());
        let minimap_nodes = Rc::new(VecModel::default());
        let shortcuts = Rc::new(VecModel::from(shortcut_rows(&app.borrow().i18n, "")));
        window.set_nodes(ModelRc::from(nodes.clone()));
        window.set_links(ModelRc::from(links.clone()));
        window.set_minimap_nodes(ModelRc::from(minimap_nodes.clone()));
        window.set_shortcuts(ModelRc::from(shortcuts.clone()));
        {
            let application = app.borrow();
            window.set_rules(ModelRc::from(Rc::new(VecModel::from(rule_rows(
                &application.patchbay,
                application.patchbay_reconciler.last_report(),
            )))));
        }
        {
            let application = app.borrow();
            window.set_effects(ModelRc::from(Rc::new(VecModel::from(effect_rows(
                &application.source,
                &application.i18n,
            )))));
            window.set_relay_rows(ModelRc::from(Rc::new(VecModel::from(relay_rows(
                &application,
                &application.i18n,
            )))));
            window.set_effect_options(ModelRc::from(Rc::new(VecModel::from(effect_options(
                &application.source,
            )))));
        }

        let events = Rc::new(RefCell::new(Vec::new()));
        let bridge = Self {
            window,
            app,
            nodes,
            links,
            minimap_nodes,
            shortcuts,
            events,
            geometry: Rc::new(RefCell::new(CanvasGeometry::default())),
            geometry_version: Rc::new(Cell::new(0)),
        };
        bridge.install_callbacks();
        bridge.refresh_meters();
        bridge.sync_models();
        Ok(bridge)
    }

    fn install_callbacks(&self) {
        let events = self.events.clone();
        self.window.on_action(move |action| {
            events
                .borrow_mut()
                .push(UiEvent::Action(action.to_string()));
        });
        let events = self.events.clone();
        self.window.on_effect_selected(move |index| {
            events.borrow_mut().push(UiEvent::EffectSelected(index))
        });
        let events = self.events.clone();
        self.window.on_effect_create_requested(move || {
            events.borrow_mut().push(UiEvent::EffectCreateRequested)
        });
        let events = self.events.clone();
        self.window
            .on_effect_config_back(move || events.borrow_mut().push(UiEvent::EffectConfigBack));
        let events = self.events.clone();
        self.window.on_effect_toggle(move |instance_id| {
            events.borrow_mut().push(UiEvent::EffectToggle {
                instance_id: instance_id.to_string(),
            })
        });
        let events = self.events.clone();
        self.window.on_effect_remove(move |instance_id| {
            events.borrow_mut().push(UiEvent::EffectRemove {
                instance_id: instance_id.to_string(),
            })
        });
        let events = self.events.clone();
        self.window.on_effect_inspect(move |instance_id| {
            events.borrow_mut().push(UiEvent::EffectInspect {
                instance_id: instance_id.to_string(),
            })
        });
        let events = self.events.clone();
        self.window.on_effect_debug(move |instance_id| {
            events.borrow_mut().push(UiEvent::EffectDebug {
                instance_id: instance_id.to_string(),
            })
        });
        let events = self.events.clone();
        self.window
            .on_effect_debug_close(move || events.borrow_mut().push(UiEvent::EffectDebugClose));
        let events = self.events.clone();
        self.window
            .on_effect_debug_copy(move || events.borrow_mut().push(UiEvent::EffectDebugCopy));
        let events = self.events.clone();
        self.window.on_effect_cancel(move |ticket| {
            events.borrow_mut().push(UiEvent::EffectCancel {
                ticket: ticket.max(0) as u64,
            })
        });
        let events = self.events.clone();
        self.window
            .on_effect_parameter_changed(move |instance_id, parameter_id, value| {
                events.borrow_mut().push(UiEvent::EffectParameterChanged {
                    instance_id: instance_id.to_string(),
                    parameter_id: parameter_id.to_string(),
                    value,
                })
            });
        let events = self.events.clone();
        self.window
            .on_effect_draft_parameter_changed(move |parameter_id, value| {
                events
                    .borrow_mut()
                    .push(UiEvent::EffectDraftParameterChanged {
                        parameter_id: parameter_id.to_string(),
                        value,
                    })
            });
        let events = self.events.clone();
        self.window.on_effect_draft_enabled_changed(move |enabled| {
            events
                .borrow_mut()
                .push(UiEvent::EffectDraftEnabledChanged(enabled))
        });
        install_canvas_callbacks(
            &self.window,
            &self.nodes,
            &self.links,
            &self.geometry,
            &self.events,
        );
    }

    pub(crate) fn run(self) -> Result<(), slint::PlatformError> {
        #[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "tray"))]
        let tray = {
            let application = self.app.borrow();
            Rc::new(RefCell::new(crate::tray::support::start(
                application.t("tray.show"),
                application.t("tray.hide"),
                application.t("tray.quit"),
            )))
        };
        let timer = Timer::default();
        let weak_window = self.window.as_weak();
        let app = self.app.clone();
        let nodes = self.nodes.clone();
        let links = self.links.clone();
        let minimap_nodes = self.minimap_nodes.clone();
        let shortcuts = self.shortcuts.clone();
        let events = self.events.clone();
        let geometry = self.geometry.clone();
        let geometry_version = self.geometry_version.clone();
        #[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "tray"))]
        let tray_for_timer = tray.clone();
        timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
            let Some(window) = weak_window.upgrade() else {
                return;
            };
            #[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "tray"))]
            crate::tray::support::poll(&window, &tray_for_timer);
            pump(
                &window,
                &app,
                &nodes,
                &links,
                &minimap_nodes,
                &shortcuts,
                &events,
                &geometry,
                &geometry_version,
            );
        });
        let result = self.window.run();
        {
            let mut application = self.app.borrow_mut();
            cancel_pending_effects(&mut application);
            read_window_state(&self.window, &mut application);
            application.sync_patchbay_connections();
            application.autosave_patchbay();
            save_config(&mut application, false);
            application.source.reset_meters();
            #[cfg(feature = "relay")]
            {
                if application.source.relay_status().host_active {
                    let _ = application.source.relay_stop_host();
                }
                application.source.relay_discovery_stop();
            }
        }
        #[cfg(all(any(target_os = "linux", target_os = "windows"), feature = "tray"))]
        if let Some(state) = tray.borrow().as_ref() {
            state.shutdown();
        }
        result
    }

    fn sync_models(&self) {
        let mut app = self.app.borrow_mut();
        sync_models(
            &self.window,
            &mut app,
            &self.nodes,
            &self.links,
            &self.minimap_nodes,
            &self.geometry,
            &self.geometry_version,
        );
    }

    fn refresh_meters(&self) {
        let mut app = self.app.borrow_mut();
        refresh_meters(&self.window, &mut app);
    }
}
