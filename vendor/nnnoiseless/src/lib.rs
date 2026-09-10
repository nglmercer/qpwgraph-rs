#![deny(missing_docs)]
// The reference DSP kernels deliberately use indexed loops for fixed-size numerical buffers.
#![allow(clippy::needless_range_loop)]

//! `nnnoiseless` is a crate for removing noise from audio. The main entry point is
//! [`DenoiseState`].
//!
//! Denoising behaviour is configured with [`DenoiseParams`]; the defaults reproduce the
//! original RNNoise signal path exactly. [`MultiDenoiser`] denoises several channels with
//! linked gains, and [`Resampler`] converts other sample rates to the 48kHz the denoiser
//! requires.
//!
//! [`DenoiseState`]: struct.DenoiseState.html

mod util;

use std::sync::OnceLock;

mod denoise;
mod features;
#[cfg(feature = "hush")]
mod hush;
mod multi;
mod params;
mod pitch;
mod resample;
mod rnn;
mod simd;
#[cfg(feature = "wasm")]
mod wasm;

pub use denoise::{denoise_offline, DenoiseState};
pub use features::DenoiseFeatures;
#[cfg(feature = "hush")]
pub use hush::{
    denoise_hush_buffer, HushDenoiser, HushError, HushMaskMode, HushModel, HushMultiDenoiser,
    HUSH_ALGORITHMIC_LATENCY_SAMPLES, HUSH_FRAME_SIZE, HUSH_LATENCY_SAMPLES, HUSH_SAMPLE_RATE,
    HUSH_SYNTHESIS_DELAY_SAMPLES,
};
pub use multi::{ChannelLink, MultiDenoiser};
pub use params::DenoiseParams;
pub use resample::{FixedPolyphaseResampler, Resampler};
pub use rnn::{Activation, DenseLayer, GruLayer, RnnModel};
pub use simd::Isa;

#[doc(hidden)]
pub const FRAME_SIZE_SHIFT: usize = 2;
#[doc(hidden)]
pub const FRAME_SIZE: usize = 120 << FRAME_SIZE_SHIFT;
pub(crate) const WINDOW_SIZE: usize = 2 * FRAME_SIZE;
#[doc(hidden)]
pub const FREQ_SIZE: usize = FRAME_SIZE + 1;

pub(crate) const PITCH_MIN_PERIOD: usize = 60;
pub(crate) const PITCH_MAX_PERIOD: usize = 768;
pub(crate) const PITCH_FRAME_SIZE: usize = 960;
pub(crate) const PITCH_BUF_SIZE: usize = PITCH_MAX_PERIOD + PITCH_FRAME_SIZE;

#[doc(hidden)]
pub const NB_BANDS: usize = 22;
pub(crate) const CEPS_MEM: usize = 8;
const NB_DELTA_CEPS: usize = 6;
#[doc(hidden)]
pub const NB_FEATURES: usize = NB_BANDS + 3 * NB_DELTA_CEPS + 2;
#[doc(hidden)]
pub const EBAND_5MS: [usize; 22] = [
    // 0  200 400 600 800  1k 1.2 1.4 1.6  2k 2.4 2.8 3.2  4k 4.8 5.6 6.8  8k 9.6 12k 15.6 20k*/
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 14, 16, 20, 24, 28, 34, 40, 48, 60, 78, 100,
];

/// The number of frequency bins actually covered by the Bark-like bands. Bins above this
/// are outside the highest band and are always zeroed.
pub(crate) const NB_BINS: usize = EBAND_5MS[NB_BANDS - 1] << FRAME_SIZE_SHIFT;

pub(crate) type Complex = realfft::num_complex::Complex<f32>;

/// Per-bin interpolation weights for mapping between frequency bins and Bark-like bands.
///
/// Both `compute_band_corr` and `interp_band_gain` walk the same triangular overlapping
/// windows. Recomputing `j / band_size` per bin showed up in profiles, so the weights are
/// built once here.
pub(crate) struct BandTables {
    /// `frac[i]` is the weight bin `i` contributes to the *upper* of the two bands that
    /// overlap it; `1.0 - frac[i]` goes to the lower band.
    frac: [f32; NB_BINS],
}

impl BandTables {
    fn new() -> BandTables {
        let mut frac = [0.0; NB_BINS];
        for i in 0..(NB_BANDS - 1) {
            let start = EBAND_5MS[i] << FRAME_SIZE_SHIFT;
            let band_size = (EBAND_5MS[i + 1] - EBAND_5MS[i]) << FRAME_SIZE_SHIFT;
            for j in 0..band_size {
                frac[start + j] = j as f32 / band_size as f32;
            }
        }
        BandTables { frac }
    }

    /// The half-open bin range covered by band `i` and its upper neighbour.
    #[inline(always)]
    pub(crate) fn band_range(i: usize) -> std::ops::Range<usize> {
        (EBAND_5MS[i] << FRAME_SIZE_SHIFT)..(EBAND_5MS[i + 1] << FRAME_SIZE_SHIFT)
    }

    #[inline(always)]
    pub(crate) fn frac(&self) -> &[f32; NB_BINS] {
        &self.frac
    }
}

/// Computes the correlation between two frequency-domain signals, and aggregates the correlation
/// into bands.
///
/// `out` is the output (duh), and it has length `NB_BANDS`.
pub(crate) fn compute_band_corr(out: &mut [f32], x: &[Complex], p: &[Complex]) {
    let c = common();
    (c.kernels.band_corr)(out, x, p, &c.bands);
}

pub(crate) fn interp_band_gain(out: &mut [f32], band_e: &[f32]) {
    let c = common();
    let frac = c.bands.frac();

    for i in 0..(NB_BANDS - 1) {
        let r = BandTables::band_range(i);
        let (lo, hi) = (band_e[i], band_e[i + 1]);
        for (o, &f) in out[r.clone()].iter_mut().zip(&frac[r]) {
            *o = lo + f * (hi - lo);
        }
    }
    // Bins above the highest band centre carry no gain at all.
    for o in out[NB_BINS..].iter_mut() {
        *o = 0.0;
    }
}

pub(crate) struct CommonState {
    window: [f32; WINDOW_SIZE],
    /// The analysis window scaled so that the inverse FFT does not need a separate
    /// normalization pass.
    synthesis_window: [f32; WINDOW_SIZE],
    dct_table: [f32; NB_BANDS * NB_BANDS],
    wnorm: f32,
    pub(crate) bands: BandTables,
    pub(crate) kernels: simd::Kernels,
    /// Shared FFT plans.
    ///
    /// `easyfft`, which this crate used to call, looked its plan up in a global cache behind
    /// a lock on every single transform. Planning once here and handing out `Arc`s keeps that
    /// cost out of the per-frame path entirely.
    pub(crate) fft_fwd: std::sync::Arc<dyn realfft::RealToComplex<f32>>,
    pub(crate) fft_inv: std::sync::Arc<dyn realfft::ComplexToReal<f32>>,
}

static COMMON: OnceLock<CommonState> = OnceLock::new();

pub(crate) fn common() -> &'static CommonState {
    COMMON.get_or_init(|| {
        let pi = std::f64::consts::PI;
        let mut window = [0.0; WINDOW_SIZE];
        for i in 0..FRAME_SIZE {
            let sin = (0.5 * pi * (i as f64 + 0.5) / FRAME_SIZE as f64).sin();
            window[i] = (0.5 * pi * sin * sin).sin() as f32;
            window[WINDOW_SIZE - i - 1] = (0.5 * pi * sin * sin).sin() as f32;
        }
        let wnorm = 1_f32 / window.iter().map(|x| x * x).sum::<f32>();

        // The forward transform is scaled by `wnorm`; a round trip through an unnormalized
        // real FFT of length `WINDOW_SIZE` additionally needs `1 / WINDOW_SIZE`. Folding the
        // leftover factor into the synthesis window removes a pass over the whole buffer.
        let leftover = 1.0 / (wnorm * WINDOW_SIZE as f32);
        let mut synthesis_window = [0.0; WINDOW_SIZE];
        for (s, &w) in synthesis_window.iter_mut().zip(&window) {
            *s = w * leftover;
        }

        let mut dct_table = [0.0; NB_BANDS * NB_BANDS];
        for i in 0..NB_BANDS {
            for j in 0..NB_BANDS {
                dct_table[i * NB_BANDS + j] =
                    ((i as f64 + 0.5) * j as f64 * pi / NB_BANDS as f64).cos() as f32;
                if j == 0 {
                    dct_table[i * NB_BANDS + j] *= 0.5f32.sqrt();
                }
            }
        }

        let mut planner = realfft::RealFftPlanner::<f32>::new();
        let fft_fwd = planner.plan_fft_forward(WINDOW_SIZE);
        let fft_inv = planner.plan_fft_inverse(WINDOW_SIZE);

        CommonState {
            window,
            synthesis_window,
            dct_table,
            wnorm,
            bands: BandTables::new(),
            kernels: simd::Kernels::detect(),
            fft_fwd,
            fft_inv,
        }
    })
}

impl CommonState {
    pub(crate) fn wnorm(&self) -> f32 {
        self.wnorm
    }
}

/// A brute-force DCT (discrete cosine transform) of size NB_BANDS.
pub(crate) fn dct(out: &mut [f32], x: &[f32]) {
    let c = common();
    for i in 0..NB_BANDS {
        let mut sum = 0.0;
        for j in 0..NB_BANDS {
            sum += x[j] * c.dct_table[j * NB_BANDS + i];
        }
        out[i] = (sum as f64 * (2.0 / NB_BANDS as f64).sqrt()) as f32;
    }
}

fn apply_window(output: &mut [f32], input: &[f32]) {
    let c = common();
    for (x, &y, &w) in util::zip3(output, input, &c.window[..]) {
        *x = y * w;
    }
}

/// Applies the synthesis window, which also carries the inverse-FFT normalization.
fn apply_synthesis_window_in_place(xs: &mut [f32]) {
    let c = common();
    for (x, &w) in xs.iter_mut().zip(&c.synthesis_window[..]) {
        *x *= w;
    }
}

/// Reports which instruction set the hot kernels were compiled for on this machine.
///
/// This is decided once, at first use, from runtime CPU feature detection. Building with the
/// `reference` feature pins it to [`Isa::Scalar`].
pub fn active_isa() -> Isa {
    common().kernels.isa
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_and_constants_are_consistent() {
        let _model = RnnModel::default();
        assert_eq!(FRAME_SIZE, 480);
        assert_eq!(FREQ_SIZE, FRAME_SIZE + 1);
        assert_eq!(NB_BANDS, 22);
        assert_eq!(NB_FEATURES, 42);
        assert_eq!(EBAND_5MS.len(), NB_BANDS);
        assert_eq!(EBAND_5MS[0], 0);
        assert_eq!(EBAND_5MS[NB_BANDS - 1], 100);
        assert_eq!(NB_BINS, 400);
    }

    #[test]
    fn band_tables_match_the_naive_computation() {
        let c = common();
        for i in 0..(NB_BANDS - 1) {
            let band_size = (EBAND_5MS[i + 1] - EBAND_5MS[i]) << FRAME_SIZE_SHIFT;
            for j in 0..band_size {
                let idx = (EBAND_5MS[i] << FRAME_SIZE_SHIFT) + j;
                assert_eq!(c.bands.frac()[idx], j as f32 / band_size as f32);
            }
        }
    }

    #[test]
    fn interp_band_gain_is_flat_for_constant_bands() {
        let mut out = [7.0; FREQ_SIZE];
        interp_band_gain(&mut out, &[0.25; NB_BANDS]);
        for &x in &out[..NB_BINS] {
            assert!((x - 0.25).abs() < 1e-6, "{x}");
        }
        for &x in &out[NB_BINS..] {
            assert_eq!(x, 0.0);
        }
    }

    #[test]
    fn silent_frames_are_finite_and_report_no_vad() {
        let mut state = DenoiseState::new();
        let input = [0.0; FRAME_SIZE];
        let mut output = [0.0; FRAME_SIZE];
        for _ in 0..3 {
            let vad = state.process_frame(&mut output, &input);
            assert_eq!(vad, 0.0);
            assert!(output.iter().all(|x| x.is_finite()));
        }
    }

    #[test]
    fn non_silent_input_produces_finite_output() {
        let mut state = DenoiseState::new();
        let input: Vec<f32> = (0..FRAME_SIZE)
            .map(|i| (i as f32 * 0.17).sin() * 12_000.0)
            .collect();
        let mut output = [0.0; FRAME_SIZE];
        for _ in 0..4 {
            let vad = state.process_frame(&mut output, &input);
            assert!((0.0..=1.0).contains(&vad));
            assert!(output.iter().all(|x| x.is_finite()));
        }
    }

    #[test]
    fn malformed_models_are_rejected() {
        assert!(RnnModel::from_bytes(&[]).is_none());
        assert!(RnnModel::from_bytes(&[42, 42, 9]).is_none());
    }

    #[test]
    fn borrowed_and_owned_model_apis_process_audio() {
        let bytes: &'static [u8] = include_bytes!("weights.rnn");
        let borrowed_model = RnnModel::from_static_bytes(bytes).expect("built-in model is valid");
        let owned_model = RnnModel::from_bytes(bytes).expect("built-in model is valid");
        let input = [1_000.0; FRAME_SIZE];
        let mut output = [0.0; FRAME_SIZE];

        let mut borrowed_state = DenoiseState::with_model(&borrowed_model);
        let borrowed_vad = borrowed_state.process_frame(&mut output, &input);
        assert!((0.0..=1.0).contains(&borrowed_vad));

        let mut owned_state = DenoiseState::from_model(owned_model);
        let owned_vad = owned_state.process_frame(&mut output, &input);
        assert!((0.0..=1.0).contains(&owned_vad));
        assert!(output.iter().all(|x| x.is_finite()));
    }
}
