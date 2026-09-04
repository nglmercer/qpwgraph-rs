//! Minimal emit-mode client demo: sends a 440 Hz tone to a relay host,
//! simulating a phone microphone.
//!
//! Usage: `cargo run -p pw-graph-relay-sdk --example relay-emit -- <host:port> [pin]`

use pw_graph_relay_sdk::{RelayClientBuilder, RelayDirection};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(target) = args.get(1) else {
        eprintln!("usage: relay-emit <host:port> [pin]");
        std::process::exit(1);
    };
    let pin = args.get(2).cloned().unwrap_or_else(|| "123456".into());

    let client = RelayClientBuilder::new()
        .device_name("relay-emit-example")
        .direction(RelayDirection::MobileToDesktop)
        .build()
        .expect("valid client configuration")
        .connect(target, &pin)
        .expect("connects to host");

    println!(
        "connected to host {:?}, emitting a 440 Hz tone",
        client.host_name()
    );

    const FRAME: usize = 480; // 10 ms at 48 kHz mono, the default frame
    let mut phase = 0.0f32;
    let mut buffer = [0.0f32; FRAME];
    loop {
        for event in client.events() {
            println!("event: {event:?}");
        }
        for sample in buffer.iter_mut() {
            *sample = (phase * 2.0 * std::f32::consts::PI / 48_000.0 * 440.0).sin() * 0.25;
            phase += 1.0;
        }
        client.send_capture(&buffer);
        std::thread::sleep(Duration::from_millis(10));
    }
}
