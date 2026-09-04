//! Minimal relay host demo.
//!
//! Usage: `cargo run -p pw-graph-relay-sdk --example relay-host -- [pin] [port]`
//!
//! Print incoming events and drain the phone-to-desktop audio queue.

use pw_graph_relay_sdk::{RelayDirection, RelayHostBuilder};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let pin = args.get(1).cloned().unwrap_or_else(|| "123456".into());
    let port: u16 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(0);

    let host = RelayHostBuilder::new()
        .device_name("relay-host-example")
        .pin(pin.clone())
        .port(port)
        .direction(RelayDirection::MobileToDesktop)
        .build()
        .expect("valid host configuration")
        .start()
        .expect("host starts");

    println!(
        "relay host listening on TCP port {} (PIN supplied by caller)",
        host.port()
    );
    println!("press Ctrl-C to stop");

    loop {
        for event in host.events() {
            println!("event: {event:?}");
        }
        let status = host.status();
        if !status.sessions.is_empty() {
            // Drain incoming peer audio so the queue never overflows.
            let mut buffer = [0.0f32; 960];
            let samples = host.pull_playback(&mut buffer);
            if samples > 0 {
                print!(".");
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
