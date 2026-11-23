use super::*;
use crate::TAIL_FLOOR;

const RATE: SampleRate = SampleRate::SR_48K;

/// A bell's controls past their ranges are read as the end of each: the center within 20 Hz
/// and 0.99 of the Nyquist, the quality factor within 0.1 and 30, the gain within 24 dB; a
/// control inside its range is read as it is.
#[test]
fn a_bell_is_held_within_its_ranges() {
    let held = |frequency: f64, q: f64, gain: f64| {
        BellFilterControls {
            frequency: Hz::new(frequency),
            q: Q::new(q),
            gain: Db::new(gain),
        }
        .held(RATE)
    };
    let low = held(1.0, 0.01, -90.0);
    assert_eq!(
        (low.frequency, low.q, low.gain),
        (MIN_EQ_CENTER, Q::new(0.1), Db::new(-24.0))
    );
    let high = held(90_000.0, 90.0, 90.0);
    assert_eq!(
        (high.frequency, high.q, high.gain),
        (RATE.nyquist_scaled(0.99), Q::new(30.0), Db::new(24.0))
    );
    let inside = held(300.0, 2.0, -6.0);
    assert_eq!(
        (inside.frequency, inside.q, inside.gain),
        (Hz::new(300.0), Q::new(2.0), Db::new(-6.0))
    );
}

/// A tilt's pivot and gain are held within the bell's center and gain ranges.
#[test]
fn a_tilt_is_held_within_its_ranges() {
    let held = |frequency: f64, gain: f64| {
        TiltFilterControls {
            frequency: Hz::new(frequency),
            gain: Db::new(gain),
        }
        .held(RATE)
    };
    assert_eq!(held(1.0, -90.0).frequency, MIN_EQ_CENTER);
    assert_eq!(held(1.0, -90.0).gain, -MAX_EQ_GAIN);
    assert_eq!(held(90_000.0, 90.0).frequency, RATE.nyquist_scaled(0.99));
    assert_eq!(held(90_000.0, 90.0).gain, MAX_EQ_GAIN);
}

/// Every graphic band's gain is held within 24 dB, each on its own.
#[test]
fn a_graphic_equalizer_holds_each_gain_within_24_db() {
    let mut controls = GraphicEqControls::FLAT;
    controls.gains[0] = Db::new(90.0);
    controls.gains[1] = Db::new(-90.0);
    controls.gains[2] = Db::new(6.0);
    let held = controls.held();
    assert_eq!(
        &held.gains[..4],
        &[MAX_EQ_GAIN, -MAX_EQ_GAIN, Db::new(6.0), Db::UNITY]
    );
}

/// The bell's ceiling is unbounded, and the tilt's is finite and covers the ring-out of a
/// tilt at its slowest control, the lowest pivot at the largest gain.
#[test]
fn the_bell_declares_no_ceiling_and_the_tilt_one_over_its_ranges() {
    assert_eq!(
        BellFilterControls::decay_ceiling(RATE, TAIL_FLOOR),
        Ok(DecayTime::Unbounded)
    );
    let DecayTime::Finite(ceiling) = TiltFilterControls::decay_ceiling(RATE, TAIL_FLOOR).unwrap()
    else {
        panic!("a tilt's ceiling is finite");
    };
    let slowest = TiltFilterControls {
        frequency: MIN_EQ_CENTER,
        gain: MAX_EQ_GAIN,
    };
    let DecayTime::Finite(at_slowest) = slowest.decay_time(RATE, TAIL_FLOOR).unwrap() else {
        panic!("a tilt's ring-out is finite");
    };
    assert!(ceiling.get() >= at_slowest.get());
}
