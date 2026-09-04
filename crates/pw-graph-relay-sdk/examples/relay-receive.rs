//! Minimal receive-mode client demo: connects to a relay host and prints a
//! level meter of whatever audio the host broadcasts (simulating a phone
//! speaker without opening an audio device).
//!
//! Usage: `cargo run -p pw-graph-relay-sdk --example relay-receive -- <host:port> [pin]`

use pw_graph_relay_sdk::{RelayClientBuilder, RelayDirection};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(target) = args.get(1) else {
        eprintln!("usage: relay-receive <host:port> [pin]");
        std::process::exit(1);
    };
    let pin = args.get(2).cloned().unwrap_or_else(|| "123456".into());

    let client = RelayClientBuilder::new()
        .device_name("relay-receive-example")
        .direction(RelayDirection::DesktopToMobile)
        .build()
        .expect("valid client configuration")
        .connect(target, &pin)
        .expect("connects to host");

    println!(
        "connected to host {:?}, receiving audio",
        client.host_name()
    );

    let mut buffer = [0.0f32; 960];
    loop {
        for event in client.events() {
            println!("event: {event:?}");
        }
        let samples = client.pull_playback(&mut buffer);
        if samples > 0 {
            let rms =
                (buffer[..samples].iter().map(|s| s * s).sum::<f32>() / samples as f32).sqrt();
            let bars = (rms * 200.0).min(40.0) as usize;
            println!("|{}{}", "#".repeat(bars), " ".repeat(40 - bars));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
