use super::*;

const RATE: SampleRate = SampleRate::SR_48K;

/// A 1 s loop (48 000 frames) at RT60 1 s loses 60 dB per trip: the gain is
/// 10^-3 exactly. At RT60 2 s it loses 30 dB: 10^-1.5.
#[test]
fn a_loop_that_decays_sixty_decibels_per_trip_has_a_thousandth_gain() {
    assert!(
        (loop_gain(SampleSpan::of(48_000.0), Seconds::new(1.0), RATE).get() - 1e-3).abs() < 1e-17
    );
    let g = loop_gain(SampleSpan::of(48_000.0), Seconds::new(2.0), RATE);
    assert!((g.get() - 10f64.powf(-1.5)).abs() < 1e-15, "{g:?}");
}

/// The two functions are inverses, over delays, decays and rates.
#[test]
fn rt60_from_gain_inverts_loop_gain() {
    for rate in [44_100.0, 48_000.0, 96_000.0] {
        for delay in [1.0f64, 37.5, 1116.0, 40_000.0] {
            for rt60 in [0.05f64, 0.5, 2.0, 8.0] {
                let g = loop_gain(
                    SampleSpan::of(delay),
                    Seconds::new(rt60),
                    SampleRate::of(rate),
                );
                let back = rt60_from_gain(g, SampleSpan::of(delay), SampleRate::of(rate)).get();
                assert!((back - rt60).abs() <= 1e-9 * rt60, "{back} vs {rt60}");
            }
        }
    }
}

/// The RT60 is the formula in `f64`, returned exact: delay 100 at gain 0.5
/// and 48 kHz is `-3 * 100 / (48000 * log10 0.5)` = 0.020763936...  s, to the
/// bit. An `f32` rounding of the result differs.
#[test]
fn rt60_from_gain_is_exact() {
    let want = -3.0 * 100.0 / (48_000.0 * libm::log10(0.5));
    assert_eq!(
        rt60_from_gain(
            Amplitude::new(0.5),
            SampleSpan::of(100.0),
            SampleRate::SR_48K
        ),
        Seconds::new(want)
    );
}

/// The edges: no memory is gain 0, never decaying is gain 1 and the inverse
/// reads those back, a gain above 1 or NaN is unbounded, a zero delay at
/// gain below 1 decays at once.
#[test]
fn the_edges_of_the_formula() {
    assert_eq!(
        loop_gain(SampleSpan::of(100.0), Seconds::ZERO, RATE),
        Amplitude::SILENT
    );
    assert_eq!(
        loop_gain(SampleSpan::of(100.0), Seconds::new(f64::NAN), RATE),
        Amplitude::SILENT
    );
    assert_eq!(
        loop_gain(SampleSpan::of(100.0), Seconds::new(f64::INFINITY), RATE),
        Amplitude::UNITY
    );
    assert_eq!(
        rt60_from_gain(Amplitude::new(0.0), SampleSpan::of(100.0), RATE).get(),
        0.0
    );
    assert_eq!(
        rt60_from_gain(Amplitude::new(1.0), SampleSpan::of(100.0), RATE).get(),
        f64::INFINITY
    );
    assert_eq!(
        rt60_from_gain(Amplitude::new(1.2), SampleSpan::of(100.0), RATE).get(),
        f64::INFINITY
    );
    assert_eq!(
        rt60_from_gain(Amplitude::new(f64::NAN), SampleSpan::of(100.0), RATE).get(),
        f64::INFINITY
    );
    assert_eq!(
        rt60_from_gain(Amplitude::new(-0.5), SampleSpan::of(100.0), RATE).get(),
        f64::INFINITY
    );
    assert_eq!(
        rt60_from_gain(Amplitude::new(0.5), SampleSpan::of(0.0), RATE).get(),
        0.0
    );
}

/// An RT60 whose frame count overflows `f64` never decays, like an infinite one: gain 1,
/// not the silence of a loop with no memory.
#[test]
fn an_rt60_too_long_to_count_in_frames_never_decays() {
    assert_eq!(
        loop_gain(
            SampleSpan::of(100.0),
            Seconds::new(1e300),
            SampleRate::of(1e10)
        ),
        Amplitude::UNITY
    );
    assert_eq!(
        loop_gain(SampleSpan::of(100.0), Seconds::new(1e300), RATE),
        Amplitude::UNITY
    );
}
