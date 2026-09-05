mod args;
mod bridge;
mod canvas;
mod diagnostics;
mod model;
mod names;
mod shortcuts;
mod source;
// One name, two implementations. The bridge calls `tray::support::*` without
// knowing which platform answered.
#[cfg(all(target_os = "linux", feature = "tray"))]
mod tray;
#[cfg(all(target_os = "windows", feature = "tray"))]
#[path = "tray_windows.rs"]
mod tray;

fn main() -> Result<(), slint::PlatformError> {
    bridge::UiBridge::new(args::parse_args())?.run()
}
