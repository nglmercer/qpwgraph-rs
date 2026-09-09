//! Manual PipeWire probe: create a patchable Hush node and print live status.
//! Run with --release --features pipewire; connect input/output using pw-link.
#[cfg(all(target_os = "linux", feature = "pipewire"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use pw_graph_backend::{EffectDriver, EffectNodeRequest, PipewireDriver};
    let mut driver = PipewireDriver::new()?;
    driver.create_effect_node(EffectNodeRequest {
        instance_id: "hush-probe".into(),
        effect_id: pw_graph_effects::HUSH_NOISE_SUPPRESSOR_ID.into(),
        module_path: None,
        enabled: true,
        parameters: Default::default(),
        position: [0.0, 0.0],
    })?;
    for second in 0..30 {
        if second % 5 == 0 && second < 20 {
            let db = (second / 5 * 20) as f32;
            driver.set_effect_parameter("hush-probe", "reduction-db", db)?;
            println!("Reduction: {db} dB");
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        for instance in driver.effect_instances() {
            println!("{}", instance.diagnostics.unwrap_or_default());
        }
    }
    driver.remove_effect("hush-probe")?;
    Ok(())
}
#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
fn main() {
    eprintln!("This probe requires Linux and the pipewire feature");
}
