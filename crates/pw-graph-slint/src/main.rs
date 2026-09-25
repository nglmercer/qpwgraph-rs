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

/// Contract: the app ships every feature by default, so a plain
/// `cargo xtask run -p pw-graph-app` has full functionality. If a feature
/// must leave the default set, update this test and the docs together.
#[cfg(test)]
mod default_features {
    // `cfg!` is constant by design here: the test pins the default feature
    // set, so a constant assertion is exactly the check.
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn default_build_enables_every_feature() {
        assert!(cfg!(feature = "pipewire"), "default must enable pipewire");
        assert!(cfg!(feature = "alsa"), "default must enable alsa");
        assert!(cfg!(feature = "relay"), "default must enable relay");
        assert!(cfg!(feature = "tray"), "default must enable tray");
        assert!(
            cfg!(feature = "screencast"),
            "default must enable screencast"
        );
        assert!(cfg!(feature = "wasm"), "default must enable wasm");
    }
}
