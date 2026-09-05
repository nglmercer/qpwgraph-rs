//! Deterministic WASAPI tone source used by Windows process-loopback tests.
//!
//! The helper is intentionally a small executable rather than a test-only
//! module: a test runner can launch it, read its PID, move that process to a
//! virtual endpoint, and then terminate it while a loopback route is active.
//! On non-Windows hosts the signal generator still compiles and is unit tested,
//! while the executable explains why the live smoke test cannot run.

#[cfg(any(target_os = "windows", test))]
use std::f32::consts::TAU;
#[cfg(any(target_os = "windows", test))]
use std::time::Duration;
#[cfg(target_os = "windows")]
use std::time::Instant;

#[cfg(any(target_os = "windows", test))]
const SAMPLE_RATE: u32 = 48_000;
#[cfg(any(target_os = "windows", test))]
const CHANNELS: u16 = 2;

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Copy, Debug)]
struct Options {
    duration: Duration,
    frequency: f32,
    amplitude: f32,
    sessions: usize,
}

#[cfg(any(target_os = "windows", test))]
impl Default for Options {
    fn default() -> Self {
        Self {
            duration: Duration::from_secs(10),
            frequency: 440.0,
            amplitude: 0.25,
            sessions: 1,
        }
    }
}

/// A phase-continuous interleaved sine generator. It allocates only the
/// caller-owned block and is deterministic across helper invocations.
#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Copy, Debug)]
struct Tone {
    phase: f32,
    phase_step: f32,
    amplitude: f32,
}

#[cfg(any(target_os = "windows", test))]
impl Tone {
    fn new(frequency: f32, amplitude: f32) -> Self {
        Self {
            phase: 0.0,
            phase_step: TAU * frequency / SAMPLE_RATE as f32,
            amplitude,
        }
    }

    fn fill(&mut self, output: &mut [f32]) {
        for frame in output.chunks_exact_mut(CHANNELS as usize) {
            let value = self.phase.sin() * self.amplitude;
            frame.fill(value);
            self.phase = (self.phase + self.phase_step).rem_euclid(TAU);
        }
    }
}

#[cfg(target_os = "windows")]
fn parse_options() -> Result<Options, String> {
    parse_arguments(std::env::args().skip(1))
}

#[cfg(any(target_os = "windows", test))]
fn parse_arguments<I, S>(arguments: I) -> Result<Options, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut options = Options::default();
    let mut args = arguments.into_iter().map(Into::into);
    while let Some(argument) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {argument}"))?;
        match argument.as_str() {
            "--duration-ms" => {
                let millis = value
                    .parse::<u64>()
                    .map_err(|_| "--duration-ms must be an integer".to_owned())?;
                options.duration = Duration::from_millis(millis);
            }
            "--frequency" => {
                options.frequency = value
                    .parse::<f32>()
                    .map_err(|_| "--frequency must be a number".to_owned())?;
            }
            "--amplitude" => {
                options.amplitude = value
                    .parse::<f32>()
                    .map_err(|_| "--amplitude must be a number".to_owned())?;
            }
            "--sessions" => {
                options.sessions = value
                    .parse::<usize>()
                    .map_err(|_| "--sessions must be an integer".to_owned())?;
            }
            _ => return Err(format!("unknown argument {argument}")),
        }
    }
    if !options.frequency.is_finite() || options.frequency <= 0.0 {
        return Err("frequency must be finite and positive".into());
    }
    if !options.amplitude.is_finite() || !(0.0..=1.0).contains(&options.amplitude) {
        return Err("amplitude must be finite and between 0 and 1".into());
    }
    if options.sessions == 0 {
        return Err("sessions must be at least one".into());
    }
    Ok(options)
}

#[cfg(target_os = "windows")]
fn main() {
    let options = match parse_options() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}\nusage: windows-audio-test-tone [--duration-ms N] [--frequency HZ] [--amplitude 0..1] [--sessions N]");
            std::process::exit(2);
        }
    };
    println!(
        "pid={} sample-rate={} channels={} frequency={} amplitude={} sessions={}",
        std::process::id(),
        SAMPLE_RATE,
        CHANNELS,
        options.frequency,
        options.amplitude,
        options.sessions
    );

    let mut workers = Vec::with_capacity(options.sessions);
    for index in 0..options.sessions {
        let worker_options = options;
        workers.push(
            std::thread::Builder::new()
                .name(format!("qpwgraph-test-tone-{index}"))
                .spawn(move || play(worker_options))
                .unwrap_or_else(|error| {
                    eprintln!("could not start tone session {index}: {error}");
                    std::process::exit(1);
                }),
        );
    }
    for worker in workers {
        if let Err(error) = worker.join() {
            eprintln!("tone session panicked: {error:?}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("windows-audio-test-tone requires Windows WASAPI");
    std::process::exit(2);
}

#[cfg(target_os = "windows")]
fn play(options: Options) {
    use windows::Win32::System::Com::{self, COINIT_MULTITHREADED};

    let initialized = unsafe { Com::CoInitializeEx(None, COINIT_MULTITHREADED) };
    if initialized.is_err() {
        eprintln!("could not initialize COM: {initialized:?}");
        return;
    }
    let result = play_wasapi(options);
    unsafe { Com::CoUninitialize() };
    if let Err(error) = result {
        eprintln!("tone session failed: {error}");
    }
}

#[cfg(target_os = "windows")]
fn play_wasapi(options: Options) -> windows::core::Result<()> {
    use windows::Win32::Media::Audio;
    use windows::Win32::System::Com::CLSCTX_ALL;

    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    let enumerator: Audio::IMMDeviceEnumerator = unsafe {
        windows::Win32::System::Com::CoCreateInstance(&Audio::MMDeviceEnumerator, None, CLSCTX_ALL)?
    };
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(Audio::eRender, Audio::eConsole)? };
    let client: Audio::IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let bits = 32u16;
    let block_align = CHANNELS * bits / 8;
    let format = Audio::WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: CHANNELS,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * u32::from(block_align),
        nBlockAlign: block_align,
        wBitsPerSample: bits,
        cbSize: 0,
    };
    let flags =
        Audio::AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | Audio::AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    unsafe {
        client.Initialize(
            Audio::AUDCLNT_SHAREMODE_SHARED,
            flags,
            400_000,
            0,
            &format,
            None,
        )?
    };
    let buffer_frames = unsafe { client.GetBufferSize()? } as usize;
    let render = unsafe { client.GetService::<Audio::IAudioRenderClient>()? };
    unsafe { client.Start()? };

    let deadline = Instant::now() + options.duration;
    let mut tone = Tone::new(options.frequency, options.amplitude);
    let mut block = vec![0.0f32; 480 * CHANNELS as usize];
    while Instant::now() < deadline {
        let padding = unsafe { client.GetCurrentPadding()? } as usize;
        let available = buffer_frames.saturating_sub(padding).min(480);
        if available == 0 {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        let samples = available * CHANNELS as usize;
        tone.fill(&mut block[..samples]);
        let data = unsafe { render.GetBuffer(available as u32)? };
        if data.is_null() {
            let _ = unsafe { render.ReleaseBuffer(available as u32, 0) };
            continue;
        }
        unsafe {
            std::slice::from_raw_parts_mut(data.cast::<f32>(), samples)
                .copy_from_slice(&block[..samples]);
            render.ReleaseBuffer(available as u32, 0)?;
        }
    }
    unsafe { client.Stop()? };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_is_deterministic_and_stereo() {
        let mut first = Tone::new(1_000.0, 0.5);
        let mut second = Tone::new(1_000.0, 0.5);
        let mut a = [0.0; 16];
        let mut b = [0.0; 16];
        first.fill(&mut a);
        second.fill(&mut b);
        assert_eq!(a, b);
        assert!(a.chunks_exact(2).all(|frame| frame[0] == frame[1]));
        assert!(a.iter().all(|sample| sample.abs() <= 0.5));
    }

    #[test]
    fn invalid_options_are_rejected() {
        assert!(parse_arguments(["--sessions", "0"]).is_err());
        assert!(parse_arguments(["--amplitude", "2"]).is_err());
        let options = parse_arguments(["--duration-ms", "25", "--frequency", "1000"]).unwrap();
        assert_eq!(options.duration, Duration::from_millis(25));
        assert_eq!(options.frequency, 1_000.0);
    }
}
