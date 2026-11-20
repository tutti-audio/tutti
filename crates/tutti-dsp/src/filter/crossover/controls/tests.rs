use super::*;

/// `CrossoverControls::try_from` a slice and a rate is the design's own check: it refuses what a
/// crossover cannot run, and nothing else.
#[test]
fn controls_refuse_what_a_crossover_cannot_run() {
    let rate = SampleRate::SR_48K;
    assert!(CrossoverControls::try_from((&[Hz::new(100.0), Hz::new(1_000.0)][..], rate)).is_ok());
    assert!(CrossoverControls::try_from((&[Hz::new(1_000.0), Hz::new(100.0)][..], rate)).is_err());
    assert!(CrossoverControls::try_from((&[Hz::new(f64::NAN)][..], rate)).is_err());
    assert!(CrossoverControls::try_from((&[Hz::new(30_000.0)][..], rate)).is_err());
    assert!(CrossoverControls::try_from((&[][..], rate)).is_err());
}

/// `CrossoverControls::try_from` a slice is the conversion without a rate: it refuses the same
/// structure and admits a split a rate's Nyquist excludes, which the conversion at that rate
/// refuses and the design clamps at the rate it runs at.
#[test]
fn ordered_controls_refuse_structure_and_leave_nyquist_to_the_rate() {
    assert!(CrossoverControls::try_from(&[Hz::new(100.0), Hz::new(1_000.0)][..]).is_ok());
    assert!(CrossoverControls::try_from(&[Hz::new(1_000.0), Hz::new(100.0)][..]).is_err());
    assert!(CrossoverControls::try_from(&[Hz::new(f64::NAN)][..]).is_err());
    assert!(CrossoverControls::try_from(&[Hz::ZERO][..]).is_err());
    assert!(CrossoverControls::try_from(&[][..]).is_err());
    let high = [Hz::new(30_000.0)];
    assert!(CrossoverControls::try_from((&high[..], SampleRate::SR_48K)).is_err());
    assert!(CrossoverControls::try_from((&high[..], SampleRate::SR_96K)).is_ok());
    assert_eq!(
        CrossoverControls::try_from(&high[..]),
        CrossoverControls::try_from((&high[..], SampleRate::SR_96K))
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
