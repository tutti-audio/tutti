use super::*;

/// `CrossoverControls::new` is the design's own check: it refuses what a
/// crossover cannot run, and nothing else.
#[test]
fn controls_refuse_what_a_crossover_cannot_run() {
    let rate = SampleRate::SR_48K;
    assert!(CrossoverControls::new(&[Hz(100.0), Hz(1_000.0)], rate).is_ok());
    assert!(CrossoverControls::new(&[Hz(1_000.0), Hz(100.0)], rate).is_err());
    assert!(CrossoverControls::new(&[Hz(f64::NAN)], rate).is_err());
    assert!(CrossoverControls::new(&[Hz(30_000.0)], rate).is_err());
    assert!(CrossoverControls::new(&[], rate).is_err());
}

/// `CrossoverControls::ordered` is `new` without a rate: it refuses the same structure and
/// admits a split a rate's Nyquist excludes, which `new` at that rate refuses and the
/// design clamps at the rate it runs at.
#[test]
fn ordered_controls_refuse_structure_and_leave_nyquist_to_the_rate() {
    assert!(CrossoverControls::ordered(&[Hz(100.0), Hz(1_000.0)]).is_ok());
    assert!(CrossoverControls::ordered(&[Hz(1_000.0), Hz(100.0)]).is_err());
    assert!(CrossoverControls::ordered(&[Hz(f64::NAN)]).is_err());
    assert!(CrossoverControls::ordered(&[Hz::ZERO]).is_err());
    assert!(CrossoverControls::ordered(&[]).is_err());
    let high = [Hz(30_000.0)];
    assert!(CrossoverControls::new(&high, SampleRate::SR_48K).is_err());
    assert!(CrossoverControls::new(&high, SampleRate::SR_96K).is_ok());
    assert_eq!(
        CrossoverControls::ordered(&high),
        CrossoverControls::new(&high, SampleRate::SR_96K)
    );
}

/// The blank set is increasing and under Nyquist at every rate, five splits wide.
#[test]
fn the_octave_set_is_increasing_and_under_nyquist_at_every_rate() {
    for rate in [SampleRate::SR_44K1, SampleRate::SR_48K, SampleRate::SR_96K] {
        let set = CrossoverControls::octaves(rate).frequencies;
        assert!(set.windows(2).all(|w| w[0] < w[1]), "{set:?}");
        assert!(set.iter().all(|&f| f > Hz::ZERO && f < rate.nyquist()));
    }
}
