use super::*;
use crate::FftScratch;
use rand::{RngExt, SeedableRng, rngs::StdRng};

const RATE: f64 = 48_000.0;

/// A perfectly flat row is flat: `exp(mean ln p - ln mean p)` of equal powers
/// is 1, to the round trip of `ln` and `exp`.
#[test]
fn a_flat_row_has_flatness_one() {
    assert!((spectral_flatness(&[3.0; 100]) - 1.0).abs() < 1e-12);
}

/// Two equal bins at 10 and 30 put the centroid midway, exactly.
#[test]
fn the_centroid_of_two_equal_bins_is_their_midpoint() {
    let mut row = vec![0.0f64; 65];
    row[10] = 2.0;
    row[30] = 2.0;
    let c = spectral_centroid(&row, Hz(10.0));
    assert!((c.get() - 200.0).abs() < 1e-3, "{c}");
}

/// A row flat over bins 0..100 rolls off at the energy share's bin, and at
/// a full share at the band edge.
///
/// 100 equal bins hold one hundredth of the energy each. Half is reached at
/// bin 49 (50 bins, exactly 50 of 100, so the comparison is `>=`). A share of
/// 85.5 % lies between 85 and 86 bins, so it takes the 86th, bin 85. The share
/// is not 85 % so that the test does not sit on the boundary, where the answer
/// is a rounding of the share.
#[test]
fn rolloff_sits_at_the_energy_share_and_the_band_edge() {
    let mut row = vec![0.0f64; 513];
    row[..100].fill(1.0);
    let width = Hz(10.0);
    assert_eq!(spectral_rolloff(&row, width, 0.5), Hz(490.0));
    assert_eq!(spectral_rolloff(&row, width, 0.855), Hz(850.0));
    assert_eq!(spectral_rolloff(&row, width, 1.0), Hz(990.0));
    assert_eq!(spectral_rolloff(&row, width, 0.0), Hz(0.0));
}

/// Band-limited noise (unit magnitude below an edge, random phase) rolls
/// off at its band edge at 99 %.
#[test]
fn band_limited_noise_rolls_off_at_the_edge() {
    let mut rng = StdRng::seed_from_u64(9);
    let mut fft = FftScratch::new();
    let size = 16_384;
    let edge_bin = 1000usize; // 5859 Hz at 48 kHz / 16384
    let mut bins = vec![crate::Complex64::default(); size / 2 + 1];
    for b in bins.iter_mut().take(edge_bin).skip(1) {
        *b = crate::Complex64::from_polar(1.0, rng.random_range(0.0..core::f64::consts::TAU));
    }
    let mut x = vec![0.0f64; size];
    fft.inverse(&bins, tutti_buffers::MonoMut::new(&mut x));
    let width = RATE / size as f64;
    let row = {
        let mut out = vec![crate::Complex64::default(); size / 2 + 1];
        fft.forward_unwindowed(tutti_buffers::Mono::new(&x), &mut out);
        out.iter().map(|b| b.norm()).collect::<Vec<_>>()
    };
    let r = spectral_rolloff(&row, Hz(width), 0.99);
    let edge = edge_bin as f64 * width;
    assert!(r.get() <= edge && r.get() > 0.98 * edge, "{r} vs {edge}");
}

/// Flatness depends on the spectrum's shape, not its level, and zero bins in
/// a tonal frame do not make it depend on the bin count.
#[test]
fn flatness_is_level_independent_and_a_zero_bin_tonal_frame_is_near_zero() {
    let mut rng = StdRng::seed_from_u64(1);
    let row: Vec<f64> = (0..1025).map(|_| rng.random_range(0.2..1.0)).collect();
    let quiet: Vec<f64> = row.iter().map(|m| m * 1e-12).collect();
    let (loud, soft) = (spectral_flatness(&row), spectral_flatness(&quiet));
    assert!(loud > 0.5 && (loud - soft).abs() < 1e-4, "{loud} vs {soft}");

    for bins in [65usize, 1025, 16_385] {
        let mut tonal = vec![0.0f64; bins];
        tonal[bins / 3] = 1.0;
        let f = spectral_flatness(&tonal);
        assert!(f < 1e-9, "{bins} bins: {f}");
    }
}

/// A NaN or infinite magnitude gives no result rather than a NaN, and a
/// rolloff share outside `0..=1` acts as its nearer end.
#[test]
fn non_finite_magnitudes_give_no_result_and_an_out_of_range_share_acts_as_its_nearer_end() {
    for bad in [f64::NAN, f64::INFINITY] {
        let mut row = vec![1.0f64; 32];
        row[5] = bad;
        assert_eq!(spectral_centroid(&row, Hz(10.0)), Hz(0.0));
        assert_eq!(spectral_flatness(&row), 0.0);
        assert_eq!(spectral_rolloff(&row, Hz(10.0), 0.85), Hz(0.0));
    }
    let mut row = vec![0.0f64; 40];
    row[..20].fill(1.0);
    assert_eq!(spectral_rolloff(&row, Hz(10.0), 7.0), Hz(190.0));
    assert_eq!(spectral_rolloff(&row, Hz(10.0), -3.0), Hz(0.0));
    assert_eq!(spectral_rolloff(&row, Hz(10.0), f64::NAN), Hz(0.0));
}

/// A silent spectrum reads zero on all three, and an empty one does not
/// panic.
#[test]
fn silence_has_no_shape() {
    let zeros = [0.0f64; 33];
    assert_eq!(spectral_centroid(&zeros, Hz(5.0)), Hz(0.0));
    assert_eq!(spectral_flatness(&zeros), 0.0);
    assert_eq!(spectral_rolloff(&zeros, Hz(5.0), 0.85), Hz(0.0));
    assert_eq!(spectral_flatness::<f64>(&[]), 0.0);
    assert_eq!(spectral_centroid::<f64>(&[], Hz(5.0)), Hz(0.0));
}

/// The kernels read a spectrum of either float width: a single bin `k` of a
/// spectrum in `f32` has its centroid at `k` bin widths and its rolloff at that
/// bin, and its flatness is near zero (a tone is not flat).
#[test]
fn the_kernels_read_an_f32_spectrum_at_the_same_figures() {
    let mut row = vec![0.0f32; 64];
    row[10] = 2.0;
    let width = Hz(100.0);
    assert_eq!(spectral_centroid(&row, width), Hz(1000.0));
    assert_eq!(spectral_rolloff(&row, width, 1.0), Hz(1000.0));
    assert!(spectral_flatness(&row) < 1e-9);
}
