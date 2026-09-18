//! Tray integration.
//!
//! Two implementations behind one interface: StatusNotifier on Linux and
//! `Shell_NotifyIcon` on Windows. Both present the same `start`, `poll`, and
//! `shutdown`, so the bridge does not know which one it is talking to.
//!
//! The tray owns no application state. It only sends show, hide, and quit
//! intents back to the Slint event loop, where the normal window lifecycle is
//! responsible for applying them. §14 of the Windows parity roadmap asks for
//! that separation specifically: tray lifecycle must not be coupled to
//! anything the audio path depends on.

#[cfg(all(target_os = "linux", feature = "tray"))]
pub(crate) mod support {
    use ksni::blocking::TrayMethods;
    use ksni::menu::StandardItem;
    use slint::ComponentHandle;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::mpsc::{self, Receiver, Sender};

    use crate::bridge::MainWindow;

    pub(crate) enum Command {
        Show,
        Hide,
        Quit,
    }

    struct TrayMenu {
        sender: Sender<Command>,
        show_label: String,
        hide_label: String,
        quit_label: String,
    }

    impl ksni::Tray for TrayMenu {
        fn id(&self) -> String {
            "qpwgraph-rs".into()
        }

        fn title(&self) -> String {
            "qpwgraph-rs".into()
        }

        fn icon_name(&self) -> String {
            "audio-card".into()
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            let _ = self.sender.send(Command::Show);
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            let show_sender = self.sender.clone();
            let hide_sender = self.sender.clone();
            let quit_sender = self.sender.clone();
            vec![
                StandardItem {
                    label: self.show_label.clone(),
                    activate: Box::new(move |_| {
                        let _ = show_sender.send(Command::Show);
                    }),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: self.hide_label.clone(),
                    activate: Box::new(move |_| {
                        let _ = hide_sender.send(Command::Hide);
                    }),
                    ..Default::default()
                }
                .into(),
                ksni::MenuItem::Separator,
                StandardItem {
                    label: self.quit_label.clone(),
                    activate: Box::new(move |_| {
                        let _ = quit_sender.send(Command::Quit);
                    }),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    pub(crate) struct State {
        receiver: Receiver<Command>,
        handle: ksni::blocking::Handle<TrayMenu>,
    }

    pub(crate) fn start(
        show_label: String,
        hide_label: String,
        quit_label: String,
    ) -> Option<State> {
        let (sender, receiver) = mpsc::channel();
        let tray = TrayMenu {
            sender,
            show_label,
            hide_label,
            quit_label,
        };
        let handle = tray.spawn().ok()?;
        Some(State { receiver, handle })
    }

    pub(crate) fn poll(window: &MainWindow, tray: &Rc<RefCell<Option<State>>>) {
        loop {
            let command = tray
                .borrow()
                .as_ref()
                .and_then(|state| state.receiver.try_recv().ok());
            let Some(command) = command else {
                break;
            };
            apply_command(window, command);
        }
    }

    fn apply_command(window: &MainWindow, command: Command) {
        match command {
            Command::Show => {
                let _ = window.show();
                window.window().set_minimized(false);
            }
            Command::Hide => {
                let _ = window.hide();
            }
            Command::Quit => {
                let _ = slint::quit_event_loop();
            }
        }
    }

    impl State {
        pub(crate) fn shutdown(&self) {
            self.handle.shutdown().wait();
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Hide-to-tray contract: Hide hides the window (it does not merely
        /// minimize it), and Show restores the same window.
        #[test]
        fn tray_hide_hides_and_show_restores_the_window() {
            i_slint_backend_testing::init_no_event_loop();
            let window = MainWindow::new().expect("test window");
            window.show().expect("show test window");
            assert!(window.window().is_visible());

            apply_command(&window, Command::Hide);
            assert!(!window.window().is_visible(), "tray Hide hides the window");

            apply_command(&window, Command::Show);
            assert!(
                window.window().is_visible(),
                "tray Show restores the window"
            );
        }
    }
}
