//! Windows notification-area integration.
//!
//! The same interface the Linux StatusNotifier tray presents — `start`,
//! `poll`, `shutdown` — so the bridge does not know which one it is talking
//! to. Like that one, the tray owns no application state: it sends show,
//! hide, and quit intents back to the Slint event loop and lets the normal
//! window lifecycle apply them.
//!
//! A tray icon on Windows needs a window to deliver its callback message to,
//! and a message loop to receive it. Slint owns the main loop, so rather than
//! fight it this runs its own hidden window on its own thread and posts
//! intents over a channel. Nothing here touches the Slint window from that
//! thread; the UI is only ever driven from [`support::poll`], on the event
//! loop. §14 of the Windows parity roadmap asks for exactly that separation.

pub(crate) mod support {
    use slint::ComponentHandle;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::Mutex;
    use std::thread::JoinHandle;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    };
    use windows::Win32::UI::WindowsAndMessaging as Win;

    use crate::bridge::MainWindow;

    pub(crate) enum Command {
        Show,
        Hide,
        Quit,
    }

    /// The message Windows sends the hidden window when the icon is clicked.
    /// Anything from `WM_APP` upwards is the application's to define.
    const WM_TRAY_ICON: u32 = Win::WM_APP + 1;
    /// Asks the tray thread to take the icon down and stop.
    const WM_TRAY_QUIT: u32 = Win::WM_APP + 2;
    /// Distinguishes this icon from any other the process might add later.
    const TRAY_ICON_ID: u32 = 1;

    const MENU_SHOW: usize = 1;
    const MENU_HIDE: usize = 2;
    const MENU_QUIT: usize = 3;

    /// What the window procedure needs, kept on the heap for the window's
    /// lifetime and reached through `GWLP_USERDATA`.
    struct TrayContext {
        sender: Sender<Command>,
        show_label: Vec<u16>,
        hide_label: Vec<u16>,
        quit_label: Vec<u16>,
    }

    pub(crate) struct State {
        receiver: Receiver<Command>,
        /// The tray window as a raw value. `HWND` is not `Send`, and this
        /// crosses to the UI thread only to be posted to.
        window: isize,
        worker: Mutex<Option<JoinHandle<()>>>,
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub(crate) fn start(
        show_label: String,
        hide_label: String,
        quit_label: String,
    ) -> Option<State> {
        let (sender, receiver) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("qpwgraph-tray".into())
            .spawn(move || {
                let context = Box::new(TrayContext {
                    sender,
                    show_label: wide(&show_label),
                    hide_label: wide(&hide_label),
                    quit_label: wide(&quit_label),
                });
                run(context, ready_tx);
            })
            .ok()?;

        // A tray icon is optional decoration, not a reason to fail startup, so
        // a window that never appeared simply means no tray.
        match ready_rx.recv() {
            Ok(Some(window)) => Some(State {
                receiver,
                window,
                worker: Mutex::new(Some(worker)),
            }),
            _ => {
                let _ = worker.join();
                None
            }
        }
    }

    /// Create the hidden window, add the icon, and pump messages until asked
    /// to stop. Everything here happens on the tray thread.
    fn run(context: Box<TrayContext>, ready: Sender<Option<isize>>) {
        let class_name = wide("qpwgraph-rs-tray");
        let Ok(instance) = (unsafe { GetModuleHandleW(None) }) else {
            let _ = ready.send(None);
            return;
        };
        let class = Win::WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        // Registering a class twice fails the second time, which is harmless:
        // the first registration is the one that matters.
        unsafe { Win::RegisterClassW(&class) };

        let context = Box::into_raw(context);
        // The window is never shown. It exists to receive the icon's callback
        // message and to own the popup menu.
        let window = unsafe {
            Win::CreateWindowExW(
                Win::WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(class_name.as_ptr()),
                Win::WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance.into()),
                Some(context.cast()),
            )
        };
        let Ok(window) = window else {
            // Safety: the window was never created, so nothing else holds
            // this pointer.
            drop(unsafe { Box::from_raw(context) });
            let _ = ready.send(None);
            return;
        };

        let mut icon = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: window,
            uID: TRAY_ICON_ID,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY_ICON,
            hIcon: unsafe { Win::LoadIconW(None, Win::IDI_APPLICATION) }.unwrap_or_default(),
            ..Default::default()
        };
        let tip = wide("qpwgraph-rs");
        icon.szTip[..tip.len()].copy_from_slice(&tip);
        if !unsafe { Shell_NotifyIconW(NIM_ADD, &icon) }.as_bool() {
            unsafe {
                let _ = Win::DestroyWindow(window);
            }
            let _ = ready.send(None);
            return;
        }
        let _ = ready.send(Some(window.0 as isize));

        let mut message = Win::MSG::default();
        while unsafe { Win::GetMessageW(&mut message, None, 0, 0) }.as_bool() {
            unsafe {
                let _ = Win::TranslateMessage(&message);
                Win::DispatchMessageW(&message);
            }
        }

        // Removing the icon before the thread exits keeps a ghost out of the
        // notification area, which Windows otherwise leaves behind until the
        // user happens to hover over it.
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &icon);
        }
    }

    unsafe extern "system" fn window_proc(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            Win::WM_NCCREATE => {
                // The context pointer arrives as the creation parameter and is
                // parked where every later message can find it.
                let create = lparam.0 as *const Win::CREATESTRUCTW;
                if !create.is_null() {
                    let context = unsafe { (*create).lpCreateParams } as isize;
                    unsafe { Win::SetWindowLongPtrW(window, Win::GWLP_USERDATA, context) };
                }
                unsafe { Win::DefWindowProcW(window, message, wparam, lparam) }
            }
            WM_TRAY_ICON => {
                let Some(context) = (unsafe { context_of(window) }) else {
                    return LRESULT(0);
                };
                match lparam.0 as u32 {
                    // A left click is the obvious "give me the window back".
                    Win::WM_LBUTTONUP => {
                        let _ = context.sender.send(Command::Show);
                    }
                    Win::WM_RBUTTONUP | Win::WM_CONTEXTMENU => unsafe {
                        show_menu(window, context);
                    },
                    _ => {}
                }
                LRESULT(0)
            }
            WM_TRAY_QUIT => {
                unsafe {
                    let _ = Win::DestroyWindow(window);
                }
                LRESULT(0)
            }
            Win::WM_DESTROY => {
                // Reclaim the context now that no further message can reach
                // it.
                let context = unsafe { Win::SetWindowLongPtrW(window, Win::GWLP_USERDATA, 0) }
                    as *mut TrayContext;
                if !context.is_null() {
                    drop(unsafe { Box::from_raw(context) });
                }
                unsafe { Win::PostQuitMessage(0) };
                LRESULT(0)
            }
            _ => unsafe { Win::DefWindowProcW(window, message, wparam, lparam) },
        }
    }

    /// Safety: only called from the window procedure, where the pointer was
    /// installed by `WM_NCCREATE` and has not yet been reclaimed by
    /// `WM_DESTROY`.
    unsafe fn context_of<'a>(window: HWND) -> Option<&'a TrayContext> {
        let pointer = unsafe { Win::GetWindowLongPtrW(window, Win::GWLP_USERDATA) };
        (pointer != 0).then(|| unsafe { &*(pointer as *const TrayContext) })
    }

    /// Show the tray menu at the cursor and send whatever was chosen.
    ///
    /// `TPM_RETURNCMD` makes this synchronous, so the choice comes back as a
    /// return value instead of as another message to route.
    unsafe fn show_menu(window: HWND, context: &TrayContext) {
        let Ok(menu) = (unsafe { Win::CreatePopupMenu() }) else {
            return;
        };
        unsafe {
            let _ = Win::AppendMenuW(
                menu,
                Win::MF_STRING,
                MENU_SHOW,
                PCWSTR(context.show_label.as_ptr()),
            );
            let _ = Win::AppendMenuW(
                menu,
                Win::MF_STRING,
                MENU_HIDE,
                PCWSTR(context.hide_label.as_ptr()),
            );
            let _ = Win::AppendMenuW(menu, Win::MF_SEPARATOR, 0, PCWSTR::null());
            let _ = Win::AppendMenuW(
                menu,
                Win::MF_STRING,
                MENU_QUIT,
                PCWSTR(context.quit_label.as_ptr()),
            );
        }

        let mut cursor = POINT::default();
        unsafe {
            let _ = Win::GetCursorPos(&mut cursor);
            // The documented dance: a tray menu will not dismiss on an outside
            // click unless its owner is foreground first, and needs a nudge
            // afterwards so it closes when focus moves away.
            let _ = Win::SetForegroundWindow(window);
        }
        let choice = unsafe {
            Win::TrackPopupMenu(
                menu,
                Win::TPM_RETURNCMD | Win::TPM_RIGHTBUTTON,
                cursor.x,
                cursor.y,
                Some(0),
                window,
                None,
            )
        };
        unsafe {
            let _ = Win::PostMessageW(Some(window), Win::WM_NULL, WPARAM(0), LPARAM(0));
            let _ = Win::DestroyMenu(menu);
        }

        let command = match choice.0 as usize {
            MENU_SHOW => Command::Show,
            MENU_HIDE => Command::Hide,
            MENU_QUIT => Command::Quit,
            // Zero means the menu was dismissed without a choice.
            _ => return,
        };
        let _ = context.sender.send(command);
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
        /// Take the icon down and wait for its thread.
        ///
        /// Waiting matters: §14 asks for no zombie background process, and a
        /// tray thread outliving the UI is exactly that.
        pub(crate) fn shutdown(&self) {
            unsafe {
                let _ = Win::PostMessageW(
                    Some(HWND(self.window as *mut _)),
                    WM_TRAY_QUIT,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
            if let Ok(mut worker) = self.worker.lock() {
                if let Some(worker) = worker.take() {
                    let _ = worker.join();
                }
            }
        }

        /// Whether the tray thread has finished. Only meaningful after
        /// [`State::shutdown`].
        #[cfg(test)]
        fn stopped(&self) -> bool {
            self.worker
                .lock()
                .map(|worker| worker.is_none())
                .unwrap_or(false)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// The lifecycle contract §14 asks for: the tray comes up, and asking
        /// it to stop actually stops it rather than leaving a thread and an
        /// icon behind.
        ///
        /// A session with no notification area -- a service account, a CI
        /// runner -- cannot add an icon, and reports that by returning `None`
        /// rather than by failing. Both outcomes are correct; what would not
        /// be is `shutdown` hanging.
        #[test]
        fn a_tray_that_starts_can_be_stopped() {
            let Some(state) = start("Show".into(), "Hide".into(), "Quit".into()) else {
                // Said out loud so a skip is not mistaken for a pass.
                eprintln!("no notification area in this session; tray skipped");
                return;
            };
            state.shutdown();
            assert!(state.stopped(), "the tray thread was joined");
            // Idempotent: the bridge shuts the tray down on the way out, and a
            // second call must not block on an already-joined thread.
            state.shutdown();
        }

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

        #[test]
        fn labels_cross_the_boundary_as_terminated_utf16() {
            let wide = wide("Quit");
            // The menu API reads until a NUL; without one it would run off the
            // end of the buffer.
            assert_eq!(wide.last(), Some(&0));
            assert_eq!(wide.len(), "Quit".len() + 1);
        }
    }
}
