use super::*;
use crate::Complex64;

#[test]
fn detection_functions_respond_to_what_they_are_for() {
    let loud = vec![Complex64::new(1.0, 0.0); 16];
    let previous = vec![0.1f64; 16];

    // Flux counts growth only.
    assert!(spectral_flux(&previous, &loud) > 0.0);
    let falling = vec![Complex64::new(0.01, 0.0); 16];
    assert_eq!(spectral_flux(&previous, &falling), 0.0);

    // Spectral distance counts change in either direction.
    assert!(spectral_distance(&previous, &falling) > 0.0);

    // HFC weights high bins more than low ones.
    let mut low = vec![Complex64::default(); 16];
    low[1] = Complex64::new(1.0, 0.0);
    let mut high = vec![Complex64::default(); 16];
    high[15] = Complex64::new(1.0, 0.0);
    assert!(high_frequency_content(&high) > high_frequency_content(&low));

    assert!(spectral_energy(Mono::new(&[1.0f64; 4])) > spectral_energy(Mono::new(&[0.1f64; 4])));
}

/// The energy is the L2 norm: `sqrt(3^2 + 4^2) = sqrt(25) = 5`, exact because
/// the square root of a perfect square is exact in IEEE arithmetic.
#[test]
fn the_energy_of_three_and_four_is_five() {
    assert_eq!(spectral_energy(Mono::new(&[3.0f64, 4.0])), 5.0);
    assert_eq!(spectral_energy(Mono::new(&[0.0f64; 8])), 0.0);
}

/// Flux sums the positive growth of each bin's magnitude: 16 bins of modulus
/// 1 over a previous magnitude of 0.1 grow by 0.9 each, `16 * 0.9 = 14.4`. The
/// modulus `hypot(1, 0)` is exact; each term `1 - 0.1` is within one rounding
/// of 0.9 and the 16-term sum accumulates at most `15 u` of its value (`u` the
/// unit round-off), so `32 * 14.4 * EPSILON` bounds the error.
#[test]
fn the_flux_of_sixteen_bins_growing_by_point_nine_is_fourteen_point_four() {
    let bins = [Complex64::new(1.0, 0.0); 16];
    let flux = spectral_flux(&[0.1f64; 16], &bins);
    assert!(
        (flux - 16.0 * 0.9).abs() <= 32.0 * 14.4 * f64::EPSILON,
        "{flux}"
    );
}

/// The deviation is `sqrt(sum((|current| - previous)^2))`. Four bins of
/// modulus 0 over a previous magnitude of 1 differ by `-1` each, so the sum
/// of squares is 4 and the deviation `sqrt(4) = 2`, exact. A fall counts here,
/// where flux reads 0.
#[test]
fn the_deviation_of_four_bins_falling_by_one_is_two() {
    let bins = [Complex64::new(0.0, 0.0); 4];
    assert_eq!(spectral_distance(&[1.0f64; 4], &bins), 2.0);
    assert_eq!(spectral_flux(&[1.0f64; 4], &bins), 0.0);
}

/// Content is `sqrt(sum((k + 1) * |bin_k|^2))`, so a single unit bin at index
/// `k` reads `sqrt(k + 1)`: at k = 0, 3, 8 and 15 that is 1, 2, 3 and 4, each
/// a perfect square and so exact.
#[test]
fn the_content_of_a_unit_bin_at_k_is_the_root_of_k_plus_one() {
    for (k, expected) in [(0usize, 1.0), (3, 2.0), (8, 3.0), (15, 4.0)] {
        let mut bins = [Complex64::new(0.0, 0.0); 16];
        bins[k] = Complex64::new(1.0, 0.0);
        assert_eq!(high_frequency_content(&bins), expected, "bin {k}");
    }
}
