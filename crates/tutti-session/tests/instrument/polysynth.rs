//! The instrument contract (`instrument_support`) against Poly.

contract!(crate::instrument_support::subjects::Poly, gaps = 0);

/// **A strip gain of 0.5 halves a held note's steady output.** A saw at
/// note 60 (261.63 Hz) at full sustain peaks at the same figure over any
/// window of several periods, within one sample step of the saw: the sampled
/// peak is at least `1 - d` of the true one, `d = 2 f / rate` the saw's step.
/// The ratio of the peak with the gain at 0.5 to the peak at unity is then
/// within `0.5 * d / (1 - d)` of one half.
#[test]
fn a_strip_gain_of_half_halves_the_steady_output_of_a_held_note() {
    use crate::instrument_support::cases::Subject;
    use crate::instrument_support::midi::note_on;
    use crate::instrument_support::subjects::Poly;
    use tutti_units::Amplitude;

    let mut rig = Poly::rig();
    rig.at(rig.t(1_037), note_on(0, 60, 100));
    // Past the attack, over 1000 frames (five periods).
    rig.render_to(rig.t(1_037) + 4_000);
    let unity = rig.peak(rig.frame() - 1_000..rig.frame());
    assert!(unity > 1e-3, "the note sounds: {unity}");

    rig.session
        .set(rig.handle.gain().to(Amplitude(0.5)))
        .unwrap();
    // The move ramps over one block; the window is past it.
    rig.render_to(rig.frame() + 4_000);
    let half = rig.peak(rig.frame() - 1_000..rig.frame());

    let step = 2.0 * 261.625_565 / 48_000.0;
    let bound = 0.5 * step / (1.0 - step);
    let ratio = f64::from(half) / f64::from(unity);
    assert!(
        (ratio - 0.5).abs() <= bound,
        "{half} / {unity} = {ratio}, not 0.5 within {bound}"
    );
}
