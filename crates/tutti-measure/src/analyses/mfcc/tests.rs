use super::*;
use crate::{SampleRange, StftConfig, stft_magnitude};
use core::num::NonZeroUsize;
use tutti_buffers::Mono;
use tutti_units::{SampleRate, Samples};

const RATE: f64 = 16_000.0;
const WINDOW: usize = 512;

fn transform(samples: &[f64]) -> StftMagnitude {
    let config = StftConfig::new(Samples::new(WINDOW), Samples::new(WINDOW / 2));
    stft_magnitude(
        &config,
        Mono::new(samples),
        SampleRange::All,
        SampleRate::of(RATE),
    )
    .unwrap()
}

fn noise(len: usize, seed: u64) -> Vec<f64> {
    tutti_fixtures::uniform_noise(seed, len)
}

fn config(coefficients: u16) -> MfccConfig {
    MfccConfig::new(
        MelBandCount::of(26),
        NonZeroUsize::new(coefficients.into()).unwrap(),
    )
}

/// The output is one row per frame and one column per coefficient.
#[test]
fn there_is_a_row_per_frame_and_a_column_per_coefficient() {
    let t = transform(&noise(8192, 1));
    let g = mfcc(&config(13), &t).unwrap();
    assert_eq!(g.frames(), t.frames());
    assert_eq!(g.bins(), BinCount(13));
}

/// Doubling the signal doubles every magnitude, which moves `c_0` by `sqrt(26) ln 4` and
/// leaves `c_1..` where they were, in every frame.
#[test]
fn doubling_a_signal_moves_only_c0_by_sqrt_n_ln_4_in_every_frame() {
    let x = noise(8192, 2);
    let doubled: Vec<f64> = x.iter().map(|v| v * 2.0).collect();
    let (a, b) = (
        mfcc(&config(13), &transform(&x)).unwrap(),
        mfcc(&config(13), &transform(&doubled)).unwrap(),
    );
    let shift = 26f64.sqrt() * 4f64.ln();
    for frame in a.frames().indices() {
        let (ra, rb) = (a.row(frame), b.row(frame));
        assert!((rb[0] - ra[0] - shift).abs() < 1e-9, "frame {frame}");
        for k in 1..13 {
            assert!((rb[k] - ra[k]).abs() < 1e-9, "frame {frame} c_{k}");
        }
    }
}

/// A silent signal has no cepstrum: every coefficient is zero.
#[test]
fn a_silent_signal_has_zero_coefficients() {
    let g = mfcc(&config(13), &transform(&vec![0.0; 4096])).unwrap();
    assert!(g.as_slice().iter().all(|&c| c == 0.0));
}

/// A range above Nyquist (8 kHz here) is refused with the bank's own error, and a bank of
/// more bands than the 257 bins can hold is refused as an empty band.
#[test]
fn a_bank_that_does_not_fit_the_transform_is_refused() {
    let t = transform(&noise(4096, 3));
    let above = config(13).range(Hz::new(0.0), Hz::new(9000.0));
    assert!(matches!(
        mfcc(&above, &t),
        Err(MfccError::Mel(tutti_dsp::MelError::AboveNyquist { .. }))
    ));
    let dense = MfccConfig::new(MelBandCount::of(512), NonZeroUsize::new(13).unwrap());
    assert!(matches!(
        mfcc(&dense, &t),
        Err(MfccError::Mel(tutti_dsp::MelError::EmptyBand { .. }))
    ));
}

/// `c_1` is the projection of the log band energies on `cos(pi (2n + 1) / 2N)`, which falls
/// from positive in the low bands to negative in the high ones, so a 300 Hz tone (energy in
/// the low bands) has a larger `c_1` than a 3 kHz tone.
#[test]
fn a_low_tone_has_a_larger_c1_than_a_high_tone() {
    let tone = |hz: f64| -> Vec<f64> {
        (0..8192)
            .map(|n| (2.0 * core::f64::consts::PI * hz * n as f64 / RATE).sin())
            .collect()
    };
    let c1 = |hz: f64| {
        mfcc(&config(13), &transform(&tone(hz)))
            .unwrap()
            .row(tutti_dsp::FrameIndex(3))[1]
    };
    assert!(c1(300.0) > c1(3000.0), "{} vs {}", c1(300.0), c1(3000.0));
}
