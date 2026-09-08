#![cfg(feature = "hush")]

use std::env;

use nnnoiseless::{HushDenoiser, HushModel, HUSH_FRAME_SIZE, HUSH_SAMPLE_RATE};

fn model_path() -> Option<String> {
    env::var("HUSH_MODEL").ok().filter(|path| !path.is_empty())
}

fn tone_frame(frame_index: usize, frequency: f32, amplitude: f32) -> [f32; HUSH_FRAME_SIZE] {
    let rate = HUSH_SAMPLE_RATE as f32;
    let mut frame = [0.0f32; HUSH_FRAME_SIZE];
    for (sample_index, sample) in frame.iter_mut().enumerate() {
        let index = frame_index * HUSH_FRAME_SIZE + sample_index;
        *sample = (2.0 * std::f32::consts::PI * frequency * index as f32 / rate).sin() * amplitude;
    }
    frame
}

fn process_frames(denoiser: &mut HushDenoiser, frames: &[[f32; HUSH_FRAME_SIZE]]) -> Vec<f32> {
    let mut output = Vec::with_capacity(frames.len() * HUSH_FRAME_SIZE);
    let mut frame_out = [0.0f32; HUSH_FRAME_SIZE];
    for frame in frames {
        denoiser
            .process_frame(&mut frame_out, frame)
            .expect("Hush frame should process");
        output.extend_from_slice(&frame_out);
    }
    output
}

#[test]
fn released_hush_bundle_processes_streaming_frames() {
    let Some(path) = model_path() else {
        eprintln!("skipping Hush integration test: set HUSH_MODEL to the ONNX bundle");
        return;
    };

    let model = HushModel::from_path(path).expect("Hush model should load");
    let mut denoiser = model.denoiser().expect("Hush runtime should initialize");
    assert_eq!(denoiser.sample_rate(), HUSH_SAMPLE_RATE);
    assert_eq!(denoiser.frame_size(), HUSH_FRAME_SIZE);

    let mut phase = 0.0f32;
    let mut output = [0.0f32; HUSH_FRAME_SIZE];
    let mut finite_lsnr = false;
    let mut input_rms = 0.0f32;
    let mut output_rms = 0.0f32;
    for frame in 0..120 {
        let mut input = [0.0f32; HUSH_FRAME_SIZE];
        for sample in &mut input {
            phase += 2.0 * std::f32::consts::PI * 180.0 / HUSH_SAMPLE_RATE as f32;
            if phase > 2.0 * std::f32::consts::PI {
                phase -= 2.0 * std::f32::consts::PI;
            }
            *sample = phase.sin() * 0.12;
        }
        let lsnr = denoiser
            .process_frame(&mut output, &input)
            .expect("Hush frame should process");
        finite_lsnr |= lsnr.is_finite();
        if frame > 10 {
            input_rms += input.iter().map(|x| x * x).sum::<f32>();
            output_rms += output.iter().map(|x| x * x).sum::<f32>();
        }
        assert!(output.iter().all(|x| x.is_finite()));
    }

    assert!(finite_lsnr);
    assert!(input_rms > 0.0);
    assert!(output_rms > 0.0, "Hush output was silent after warmup");
    denoiser.reset().expect("Hush reset should work");
}

#[test]
fn reset_matches_a_fresh_runtime() {
    let Some(path) = model_path() else {
        eprintln!("skipping Hush reset regression test: set HUSH_MODEL to the ONNX bundle");
        return;
    };

    let model = HushModel::from_path(path).expect("Hush model should load");
    let mut dirty = model.denoiser().expect("Hush runtime should initialize");
    let mut fresh = model.denoiser().expect("Hush runtime should initialize");

    for frame_index in 0..120 {
        let input = tone_frame(frame_index, 180.0, 0.12);
        let mut output = [0.0f32; HUSH_FRAME_SIZE];
        dirty
            .process_frame(&mut output, &input)
            .expect("dirty Hush frame should process");
    }

    dirty.reset().expect("Hush reset should work");
    let sequence: Vec<_> = (0..32)
        .map(|frame_index| tone_frame(frame_index, 260.0, 0.08))
        .collect();
    let dirty_output = process_frames(&mut dirty, &sequence);
    let fresh_output = process_frames(&mut fresh, &sequence);
    let max_error = dirty_output
        .iter()
        .zip(&fresh_output)
        .map(|(dirty, fresh)| (dirty - fresh).abs())
        .fold(0.0f32, f32::max);

    assert!(
        fresh_output.iter().any(|sample| sample.abs() > 1e-6),
        "reset regression sequence produced no output"
    );
    assert!(
        max_error < 1e-5,
        "reset did not restore a fresh Hush runtime (max error {max_error})"
    );
}
