mod app;
mod args;
mod backend;
#[cfg(all(target_os = "linux", feature = "tray"))]
mod tray;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    app::run(args::parse_args())
}
