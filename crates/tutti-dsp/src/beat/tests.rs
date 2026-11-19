use super::*;

/// With an odd count of maxima the threshold is half the middle score:
/// scores 12, 10, 2 have median 10, so the last beat is the one scoring 10.
#[test]
fn the_tail_threshold_is_half_the_median_of_an_odd_count_of_maxima() {
    let cumulative = [0.0, 12.0, 0.0, 10.0, 0.0, 2.0, 0.0];
    assert_eq!(last_beat(&cumulative), 3);
}

/// With an even count the median is the mean of the middle two: scores 2,
/// 10, 12, 4 have median (4 + 10) / 2 = 7, a threshold of 3.5, which the
/// last maximum (4) reaches. The upper middle score (10) would put it at 5.
#[test]
fn the_tail_threshold_is_half_the_mean_median_of_an_even_count_of_maxima() {
    let cumulative = [0.0, 2.0, 0.0, 10.0, 0.0, 12.0, 0.0, 4.0, 0.0];
    assert_eq!(last_beat(&cumulative), 7);
}

/// A period far past the curve sizes no table past the curve.
#[test]
fn tables_never_outgrow_the_curve() {
    let (lo, hi) = gap_bounds(1e12, 1000);
    assert!(hi <= 1000 && lo > hi, "{lo} {hi}");
    assert_eq!(kernel_half(1e11, 1000), 1000);
    assert_eq!(gap_bounds(20.0, 1000), (10, 40));
    assert_eq!(kernel_half(2.0, 1000), 6);
    // And a period too long for any gap tracks without a beat chain.
    let mut curve = vec![0.0f64; 1000];
    curve[10] = 1.0;
    assert!(track_beat_frames(&curve, 1e12, 100.0).len() <= 1);
}
