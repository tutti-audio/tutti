use super::*;
use crate::DecayTime;
use crate::{Lti as _, System as _};

/// The pre-delay is whole frames, rounded: 1 ms at 48 kHz is 48, more than half a
/// frame rounds up, and a negative or NaN span is none.
#[test]
fn the_pre_delay_is_rounded_to_whole_frames() {
    let rate = SampleRate::SR_48K;
    assert_eq!(
        pre_delay_frames(Seconds::new(0.001), rate),
        Samples::new(48)
    );
    assert_eq!(
        pre_delay_frames(Seconds::new(0.6 / 48_000.0), rate),
        Samples::new(1)
    );
    assert_eq!(
        pre_delay_frames(Seconds::new(0.4 / 48_000.0), rate),
        Samples::ZERO
    );
    assert_eq!(pre_delay_frames(Seconds::new(-1.0), rate), Samples::ZERO);
    assert_eq!(
        pre_delay_frames(Seconds::new(f64::NAN), rate),
        Samples::ZERO
    );
}

/// A `Network` is a plain value built on the control side: the whole value
/// stays under 113 KiB: `MAX_STAGES` (8) of the largest stage (14 448 bytes, each of its 64
/// lines holding the 8 bytes of its absorption's stored peak) is 115 584 bytes, the rate,
/// pre-delay, width and wet gain add 40, and 115 624 bytes round up to 113 KiB.
#[test]
fn a_network_stays_under_113_kib() {
    let bytes = std::mem::size_of::<Network>();
    assert!(bytes < 113 * 1024, "{bytes}");
}

/// An empty network recirculates nothing: no decay, no tail, stable.
#[test]
fn an_empty_network_is_silent_and_stable() {
    let n = Network::new(SampleRate::SR_44K1);
    assert_eq!(n.rt60(Band::Mid), Seconds::ZERO);
    assert_eq!(n.tail(Db::new(-60.0)), DecayTime::Finite(Seconds::ZERO));
    assert!(n.is_stable() && !n.is_lossless());
}

/// The two forms differ in their direct term: a lattice has `-g` (none at
/// `g = 0`, a plain delay), Freeverb's always `-1`; only the lattice has a
/// finite low-frequency group delay for the loop analysis.
#[test]
fn the_forms_differ_in_their_direct_term_and_group_delay() {
    let ap = |form, gain| AllPass {
        delay: SampleSpan::of(100.0),
        gain: Gain::new(gain),
        modulation: LineModulation::NONE,
        form,
    };
    assert!(ap(AllPassForm::Lattice, 0.5).has_direct_term());
    assert!(ap(AllPassForm::Lattice, -0.5).has_direct_term());
    assert!(!ap(AllPassForm::Lattice, 0.0).has_direct_term());
    assert!(ap(AllPassForm::Freeverb, 0.0).has_direct_term());
    assert!(ap(AllPassForm::Freeverb, 0.5).has_direct_term());
    assert_eq!(
        ap(AllPassForm::Lattice, 0.5).low_frequency_delay(),
        Some(SampleSpan::of(300.0))
    );
    assert_eq!(ap(AllPassForm::Freeverb, 0.5).low_frequency_delay(), None);
}

/// The lattice form is unit magnitude at every frequency, from its
/// recurrence `y[n] = -g x[n] + x[n-D] + g y[n-D]`; with the same `g` the
/// Freeverb form (`((1+g) z^-D - 1) / (1 - g z^-D)`) is not. Both
/// impulse responses are run from their difference equations.
#[test]
fn the_lattice_is_unit_magnitude_and_the_freeverb_form_is_not() {
    let (d, g) = (8usize, 0.5f64);
    let len = 40 * d;
    let mut lattice = vec![0.0f64; len];
    let mut freeverb = vec![0.0f64; len];
    // Freeverb: buf[n] = x[n] + g buf[n-D]; y[n] = buf[n-D] - x[n].
    let mut buf = vec![0.0f64; len];
    for n in 0..len {
        let x = if n == 0 { 1.0 } else { 0.0 };
        let back = |v: &[f64]| if n >= d { v[n - d] } else { 0.0 };
        lattice[n] = -g * x + (if n == d { 1.0 } else { 0.0 }) + g * back(&lattice);
        buf[n] = x + g * back(&buf);
        freeverb[n] = back(&buf) - x;
    }
    let mag = |h: &[f64], w: f64| {
        let (re, im) = h.iter().enumerate().fold((0.0, 0.0), |(re, im), (n, &x)| {
            (
                re + x * libm::cos(w * n as f64),
                im - x * libm::sin(w * n as f64),
            )
        });
        f64::hypot(re, im)
    };
    let omegas = [0.0, 0.1, 0.4, std::f64::consts::PI / d as f64, 2.0, 3.0];
    assert!(
        omegas
            .iter()
            .all(|&w| (mag(&lattice, w) - 1.0).abs() < 1e-6)
    );
    assert!(
        omegas
            .iter()
            .any(|&w| (mag(&freeverb, w) - 1.0).abs() > 0.5)
    );
}
