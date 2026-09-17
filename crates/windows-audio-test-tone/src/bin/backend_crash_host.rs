//! Test-support process that hosts an automatic application route until killed.
//!
//! The backend-crash integration test spawns this binary, waits for it to
//! report an applied route, verifies the stream is audible, and then
//! terminates it without cleanup (the OS equivalent of `kill -9`). A fresh
//! backend must reconcile over the stuck persisted route afterwards. This
//! binary intentionally skips `CoUninitialize`: it is designed to die
//! mid-stream, and process death releases COM.

#[cfg(target_os = "windows")]
mod imp {
    use pw_graph_backend::{
        AppRoutePolicy, AudioFlow, AudioRole, GraphDriver, ProcessIdentity,
        VerifiedAudioPolicyConfig, WindowsAudioDriver,
    };
    use pw_graph_config::WindowsApplicationRoute;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    struct Args {
        helper_pid: u32,
        physical_id: String,
        app_render_id: String,
        ready_file: PathBuf,
    }

    fn usage() -> String {
        "usage: qpwgraph-backend-crash-host --helper-pid PID --physical-id ID --app-render-id ID --ready-file PATH"
            .to_owned()
    }

    fn parse_args(mut raw: impl Iterator<Item = String>) -> Result<Args, String> {
        let mut helper_pid = None;
        let mut physical_id = None;
        let mut app_render_id = None;
        let mut ready_file = None;
        while let Some(flag) = raw.next() {
            let value = raw.next().ok_or_else(usage)?;
            match flag.as_str() {
                "--helper-pid" => {
                    helper_pid = Some(value.parse::<u32>().map_err(|_| usage())?);
                }
                "--physical-id" => physical_id = Some(value),
                "--app-render-id" => app_render_id = Some(value),
                "--ready-file" => ready_file = Some(PathBuf::from(value)),
                _ => return Err(usage()),
            }
        }
        Ok(Args {
            helper_pid: helper_pid.ok_or_else(usage)?,
            physical_id: physical_id.ok_or_else(usage)?,
            app_render_id: app_render_id.ok_or_else(usage)?,
            ready_file: ready_file.ok_or_else(usage)?,
        })
    }

    fn applied_everywhere(process: &ProcessIdentity, app_render_id: &str) -> bool {
        // A fresh handle per check: a too-early read fails closed, and must
        // not demote the handle the driver itself uses.
        let policy = VerifiedAudioPolicyConfig::new(true);
        [
            AudioRole::Console,
            AudioRole::Multimedia,
            AudioRole::Communications,
        ]
        .iter()
        .all(|role| {
            policy
                .get_persisted_endpoint(process, AudioFlow::Render, *role)
                .ok()
                .flatten()
                .is_some_and(|id| id.eq_ignore_ascii_case(app_render_id))
        })
    }

    pub fn run() -> Result<(), String> {
        let args = parse_args(std::env::args().skip(1))?;
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_err() {
            return Err(format!("could not initialize COM: {initialized:?}"));
        }
        let mut driver =
            WindowsAudioDriver::new().map_err(|error| format!("create driver: {error}"))?;
        driver.set_experimental_app_routing(true);
        let process = ProcessIdentity::from_pid(args.helper_pid)
            .map_err(|error| format!("read helper identity: {error}"))?;
        let route = WindowsApplicationRoute {
            application: process.application_selector(),
            destination_mmdevice_id: Some(args.physical_id.clone()),
            virtualization_required: true,
            gain: 1.0,
            enabled: true,
            ..WindowsApplicationRoute::default()
        };
        driver
            .reconcile_application_routes(vec![route])
            .map_err(|error| format!("install application route: {error}"))?;

        let deadline = Instant::now() + Duration::from_secs(20);
        while !applied_everywhere(&process, &args.app_render_id) {
            if Instant::now() >= deadline {
                return Err("route was not applied within 20 s".to_owned());
            }
            driver
                .refresh()
                .map_err(|error| format!("refresh driver: {error}"))?;
            std::thread::sleep(Duration::from_millis(100));
        }
        std::fs::write(&args.ready_file, "ready\n")
            .map_err(|error| format!("write ready file: {error}"))?;

        // Park as a live backend: keep pumping refreshes until killed.
        loop {
            let _ = driver.refresh();
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(target_os = "windows")]
fn main() {
    if let Err(error) = imp::run() {
        eprintln!("qpwgraph-backend-crash-host: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("qpwgraph-backend-crash-host is Windows-only");
    std::process::exit(3);
}
