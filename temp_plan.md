# Task: Make window close hide to tray instead of exiting

Repository:

`https://github.com/nglmercer/qpwgraph-rs`

The desktop UI is implemented with Rust + Slint.

## Current behavior

Closing the main Slint window with the window **X** causes the application to terminate.

This is incorrect when the system tray is enabled.

The application already has tray support on both Linux and Windows.

Relevant files:

```text
crates/pw-graph-slint/src/bridge/mod.rs
crates/pw-graph-slint/src/tray.rs
crates/pw-graph-slint/src/tray_windows.rs
```

The tray implementations already expose:

```rust
Command::Show
Command::Hide
Command::Quit
```

`Command::Quit` already performs:

```rust
slint::quit_event_loop()
```

Therefore, **do not add another exit mechanism**.

## Desired behavior

When tray support initializes successfully:

```text
Window X / close button
    -> hide the main window
    -> keep the application running
    -> keep the tray icon running

Tray -> Show
    -> show and restore the main window

Tray -> Hide
    -> hide the main window

Tray -> Quit
    -> terminate the Slint event loop
    -> perform normal application cleanup
    -> remove the tray icon
    -> terminate the process
```

If tray initialization fails or tray support is unavailable, normal window-close behavior may terminate the application.

## Important Slint lifecycle issue

The application currently uses:

```rust
let result = self.window.run();
```

This is problematic for a custom native tray.

The Linux tray uses `ksni`, and the Windows tray uses a custom Win32 notification-area implementation. These are not Slint-owned tray objects, so they do not keep the Slint event loop alive after the last Slint window closes.

Closing the window can therefore make `MainWindow::run()` return, after which the application executes tray shutdown code and exits.

## Required implementation

When a tray exists, do not rely on:

```rust
self.window.run()
```

Instead, explicitly show the main window and run the Slint event loop until an explicit quit request.

Conceptually:

```rust
self.window.show()?;

let result = if tray.borrow().is_some() {
    slint::run_event_loop_until_quit()
} else {
    slint::run_event_loop()
};

let _ = self.window.hide();
```

Adapt this to the existing code structure and Slint 1.17.1 APIs.

Also explicitly intercept the main-window close request if appropriate:

```rust
self.window
    .window()
    .on_close_requested(|| slint::CloseRequestResponse::HideWindow);
```

The intent must be:

```text
close request != application quit
```

when tray support is active.

## Constraints

Do not:

* remove the existing tray `Quit` action
* create a second Exit/Quit implementation
* terminate audio/backend services when only hiding the window
* recreate the main window every time the tray Show action is used
* manipulate the Slint UI from the Windows tray thread
* break the current tray thread shutdown/cleanup logic
* leave a zombie tray thread or tray icon after explicit Quit

Preserve the existing architecture where the tray sends commands to the Slint event-loop thread.

## Expected lifecycle

```text
Application starts
        |
        v
Main window + tray created
        |
        +---- user clicks X
        |          |
        |          v
        |      window hidden
        |      app still running
        |      tray still active
        |
        +---- tray -> Show
        |          |
        |          v
        |      window shown/restored
        |
        +---- tray -> Quit
                   |
                   v
           quit_event_loop()
                   |
                   v
           normal cleanup
                   |
                   v
           tray shutdown
                   |
                   v
             process exits
```

## Acceptance criteria

The implementation is complete when:

1. Clicking the main window **X** with a functioning tray does not terminate the process.
2. The main window disappears after clicking X.
3. The tray icon remains available.
4. Clicking tray **Show** restores the same main window.
5. Clicking tray **Quit** exits the application completely.
6. Tray shutdown still removes the icon and joins/stops its worker correctly.
7. Linux and Windows follow the same lifecycle semantics.
8. Without a successfully initialized tray, closing the window can still exit normally.
9. Existing backend/audio state remains alive while the UI window is hidden.
10. `cargo check` / relevant tests still pass.

Please inspect the existing lifecycle code first and make the smallest maintainable change necessary.
