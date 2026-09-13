//! Direct KS inspection of the exact QPWGraph development devnode.
//! This binary never selects physical/default devices or changes boot settings.
#[cfg(windows)]
mod probe;

#[cfg(windows)]
fn main() {
    if let Err(error) = probe::run() {
        eprintln!("KS probe failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("The KS probe requires Windows.");
    std::process::exit(2);
}
