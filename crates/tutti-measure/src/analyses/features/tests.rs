use super::*;
use crate::test_rng::uniform;
use crate::{SampleRange, StftConfig, stft_magnitude};
use tutti_buffers::Mono;
use tutti_rt::SplitMix64;
use tutti_units::Samples;
use tutti_units::{Normalized, SampleRate};

const RATE: f64 = 48_000.0;
const WINDOW: usize = 2048;

fn magnitudes_of(samples: &[f64]) -> StftMagnitude {
    let config = StftConfig::new(Samples::new(WINDOW), Samples::new(WINDOW / 2));
    stft_magnitude(
        &config,
        Mono::new(samples),
        SampleRange::All,
        SampleRate::of(RATE),
    )
    .unwrap()
}

fn bin_width() -> Hz {
    Hz::new(RATE / WINDOW as f64)
}

fn sine(hz: f64, len: usize) -> Vec<f64> {
    tutti_fixtures::signal::sine(
        tutti_units::Hz::new(hz),
        tutti_units::Amplitude::new(1.0),
        tutti_units::SampleRate::of(RATE),
        tutti_units::Samples::new(len),
    )
}

/// A pure sine's centroid is its frequency, to within a bin.
#[test]
fn a_sine_centroid_is_its_frequency_within_a_bin() {
    for hz in [1000.0f64, 3517.0, 9000.0] {
        let stft = magnitudes_of(&sine(hz, 16_384));
        let features = spectral_features(Normalized::of(0.85), &stft);
        assert!(!features.is_empty());
        for f in &features {
            assert!(
                (f.centroid.get() - hz).abs() < bin_width().get(),
                "{hz}: {}",
                f.centroid
            );
        }
    }
}

/// White noise is flat and a sine is not.
#[test]
fn noise_is_flat_a_sine_is_not() {
    let mut rng = SplitMix64::new(42);
    let noise: Vec<f64> = (0..65_536).map(|_| uniform(&mut rng, -1.0, 1.0)).collect();
    let flat = spectral_features(Normalized::of(0.85), &magnitudes_of(&noise));
    let mean: f64 = flat.iter().map(|f| f.flatness).sum::<f64>() / flat.len() as f64;
    // The geometric mean of an exponential power distribution sits at
    // exp(-gamma) = 0.56 of its mean.
    assert!((0.5..0.62).contains(&mean), "noise flatness {mean}");

    let tone = spectral_features(Normalized::of(0.85), &magnitudes_of(&sine(1000.0, 16_384)));
    assert!(tone.iter().all(|f| f.flatness < 1e-3), "{tone:?}");
}

/// The grid helper takes its bin width from the transform: its output is
/// the per-row kernels' on the transform's own rows.
#[test]
fn the_grid_helper_applies_the_kernels_row_by_row() {
    let stft = magnitudes_of(&sine(2000.0, 8192));
    let width = stft.geometry().bin_frequency(BinIndex(1));
    let grid = stft.magnitudes().grid();
    let features = spectral_features(Normalized::of(0.5), &stft);
    assert_eq!(features.len(), grid.frames().get());
    for (frame, f) in grid.frames().indices().zip(&features) {
        let row = grid.row(frame);
        assert_eq!(f.centroid, spectral_centroid(row, width));
        assert_eq!(f.bandwidth, spectral_bandwidth(row, width));
        assert_eq!(f.flatness, spectral_flatness(row));
        assert_eq!(f.crest, spectral_crest(row));
        assert_eq!(f.slope, spectral_slope(row, width));
        assert_eq!(f.rolloff, spectral_rolloff(row, width, Normalized::of(0.5)));
    }
    assert_eq!(width.get(), 23.4375);
}
