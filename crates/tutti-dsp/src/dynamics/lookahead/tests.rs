use super::fold_peak;

/// The peak fold ignores a NaN (`max` returns the other operand), so a NaN sample never
/// reaches the detector, and takes the magnitude of a negative one.
#[test]
fn a_nan_fed_to_the_peak_fold_is_ignored() {
    assert_eq!(fold_peak(0.5, f32::NAN), 0.5);
    assert_eq!(fold_peak(0.0, f32::NAN), 0.0);
    assert_eq!(fold_peak(0.5, -0.75), 0.75);
}

/// A ring of `L` frames of lookahead delays an `f64` signal of more than 24 bits by exactly
/// `L` frames, scaled by the frame's gain: `out[n] = x[n - L] g[n]`, with silence before.
#[test]
fn an_f64_ring_delays_by_exactly_its_lookahead_and_scales_by_the_gain() {
    use super::LookaheadRing;
    use tutti_fixtures::signal::{assert_close, wide_signal};
    let (lookahead, len) = (7usize, 40usize);
    let x = wide_signal(len);
    let gains: Vec<f64> = (0..len).map(|n| 0.5 + n as f64 / 128.0).collect();
    let mut ring = LookaheadRing::<f64>::new(1, lookahead, lookahead + 1);
    let mut y = vec![0.0f64; len];
    ring.delay_scaled(0, &x, &gains, &mut y);
    let want: Vec<f64> = (0..len)
        .map(|n| n.checked_sub(lookahead).map_or(0.0, |k| x[k] * gains[n]))
        .collect();
    assert_close(&y, &want, 0.0);
}

/// The sliding minimum of `f64` gains is the minimum of the last `window` values, exactly.
#[test]
fn an_f64_ring_pushes_the_window_minimum_of_its_gains_exactly() {
    use super::LookaheadRing;
    use tutti_fixtures::signal::wide_signal;
    let gains = wide_signal(30);
    let window = 4;
    let mut ring = LookaheadRing::<f64>::new(1, 5, window);
    for (n, &g) in gains.iter().enumerate() {
        // The gains rise, so the minimum of the window is its oldest.
        let oldest = gains[n.saturating_sub(window - 1)];
        assert_eq!(ring.push_gain(g), oldest, "frame {n}");
    }
}
