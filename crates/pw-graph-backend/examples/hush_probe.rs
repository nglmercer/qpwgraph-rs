//! Manual PipeWire probe: create a patchable Hush node and print live status.
//! Run with --release --features pipewire; connect input/output using pw-link.
#[cfg(all(target_os = "linux", feature = "pipewire"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use pw_graph_backend::{EffectDriver, EffectNodeRequest, PipewireDriver};
    let mut seconds = 30u64;
    let mut fixed_reduction = None;
    let mut channels = 2u16;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => {
                seconds = args.next().ok_or("--seconds needs a value")?.parse()?;
            }
            "--reduction" => {
                fixed_reduction = Some(
                    args.next()
                        .ok_or("--reduction needs a value")?
                        .parse::<f32>()?
                        .clamp(0.0, 60.0),
                );
            }
            "--channels" => {
                channels = args
                    .next()
                    .ok_or("--channels needs a value")?
                    .parse::<u16>()?;
                if !matches!(channels, 1 | 2) {
                    return Err("--channels must be 1 or 2".into());
                }
            }
            "--help" | "-h" => {
                println!("usage: hush_probe [--seconds N] [--reduction DB] [--channels 1|2]");
                return Ok(());
            }
            unknown => return Err(format!("unknown argument: {unknown}").into()),
        }
    }
    let mut driver = PipewireDriver::new()?;
    driver.create_effect_node(EffectNodeRequest {
        instance_id: "hush-probe".into(),
        effect_id: pw_graph_effects::HUSH_NOISE_SUPPRESSOR_ID.into(),
        module_path: None,
        enabled: true,
        parameters: Default::default(),
        channels: Some(channels),
        position: [0.0, 0.0],
    })?;
    for second in 0..seconds {
        if let Some(db) = fixed_reduction
            .or_else(|| (second % 5 == 0 && second < 20).then_some((second / 5 * 20) as f32))
        {
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
