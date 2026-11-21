use tutti_buffers::MonoMut;
use tutti_units::{Amplitude, FrequencyRatio, Hz, SampleRate};

use super::*;
use crate::testing::lock_in;

const RATE: SampleRate = SampleRate::SR_48K;
/// Whole periods of every multiple of 10 Hz at 48 kHz.
const WINDOW: usize = 4800;
/// `f64` round-off over a window of sines: a few hundred ulps of 1.
const ROUND_OFF: f64 = 1e-12;

fn partial(ratio: f64, amplitude: f64) -> Partial {
    Partial::new(
        FrequencyRatio::new(ratio).unwrap(),
        Amplitude::new(amplitude),
    )
}

fn render(partials: &[Partial], controls: &AdditiveControls) -> Vec<f64> {
    let mut bank = AdditiveBank::<f64>::new(Partials::new(partials).unwrap(), RATE);
    bank.retarget(controls);
    let mut out = vec![0.0; WINDOW];
    bank.process(MonoMut::new(&mut out));
    out
}

fn controls(frequency: f64, level: f64) -> AdditiveControls {
    AdditiveControls {
        frequency: Hz::new(frequency),
        level: Amplitude::new(level),
    }
}

/// A partial at `ratio * f` and amplitude `a` is the line `a sin(2 pi ratio f t)`, which a
/// window of whole periods reads as `(0, -a)` exactly; a frequency nothing was put at reads 0.
/// The output level scales every amplitude.
#[test]
fn each_partial_appears_at_its_frequency_and_amplitude() {
    let partials = [partial(1.0, 0.5), partial(2.5, 0.25), partial(7.0, 0.125)];
    let out = render(&partials, &controls(480.0, 0.5));
    for (hz, amplitude) in [(480.0, 0.25), (1_200.0, 0.125), (3_360.0, 0.0625)] {
        let (re, im) = lock_in(&out, hz, RATE.get());
        assert!(re.abs() <= ROUND_OFF, "{hz} Hz: re {re}");
        assert!((im + amplitude).abs() <= ROUND_OFF, "{hz} Hz: im {im}");
    }
    for hz in [100.0, 960.0, 1_000.0, 2_000.0, 5_000.0] {
        let (re, im) = lock_in(&out, hz, RATE.get());
        assert!(re.hypot(im) <= ROUND_OFF, "{hz} Hz");
    }
}

/// A partial above the Nyquist is dropped: a partial at 25.2 kHz would fold to 22.8 kHz at its
/// full amplitude, and the bank has nothing there; the partial just below (23.52 kHz) is
/// heard at its exact amplitude. A sine at exactly the Nyquist is 0 at every frame from phase 0,
/// so it is the same silence either way.
#[test]
fn a_partial_above_the_nyquist_is_absent_and_the_one_below_is_heard() {
    let partials = [partial(1.0, 0.5), partial(49.0, 0.25), partial(52.5, 0.75)];
    let out = render(&partials, &controls(480.0, 1.0));
    let (_, below) = lock_in(&out, 23_520.0, RATE.get());
    assert!((below + 0.25).abs() <= ROUND_OFF);
    for hz in [25_200.0, 22_800.0] {
        let (re, im) = lock_in(&out, hz, RATE.get());
        assert!(re.hypot(im) <= ROUND_OFF, "{hz} Hz");
    }
    let (_, first) = lock_in(&out, 480.0, RATE.get());
    assert!((first + 0.5).abs() <= ROUND_OFF);
}

/// Raising the frequency drops the partials it pushes past the Nyquist: at 5 kHz the third
/// partial (15 kHz) is heard, at 9 kHz (27 kHz) it is not.
#[test]
fn a_frequency_that_pushes_a_partial_past_the_nyquist_drops_it() {
    let partials = [partial(1.0, 1.0), partial(3.0, 1.0)];
    let low = render(&partials, &controls(5_000.0, 1.0));
    let (_, im) = lock_in(&low, 15_000.0, RATE.get());
    assert!((im + 1.0).abs() <= ROUND_OFF);
    let high = render(&partials, &controls(9_000.0, 1.0));
    for hz in [27_000.0, 21_000.0] {
        let (re, im) = lock_in(&high, hz, RATE.get());
        assert!(re.hypot(im) <= ROUND_OFF, "{hz} Hz");
    }
}

/// A block's phases carry over to the next: two half-blocks are the one window.
#[test]
fn the_phase_runs_on_across_blocks() {
    let partials = [partial(1.0, 1.0)];
    let mut bank = AdditiveBank::<f64>::new(Partials::new(&partials).unwrap(), RATE);
    bank.retarget(&controls(480.0, 1.0));
    let mut out = vec![0.0; WINDOW];
    for half in out.chunks_mut(WINDOW / 2) {
        bank.process(MonoMut::new(half));
    }
    let (re, im) = lock_in(&out, 480.0, RATE.get());
    assert!(re.abs() <= ROUND_OFF && (im + 1.0).abs() <= ROUND_OFF);
}

#[test]
fn a_bank_has_between_one_and_256_partials() {
    let one = partial(1.0, 1.0);
    assert!(Partials::new(&[]).is_err());
    assert_eq!(Partials::new(&[one; 256]).unwrap().count().get(), 256);
    assert_eq!(Partials::new(&[one; 257]), Err(PartialCountError(257)));
}

/// A partial pushed past the Nyquist by a retarget leaves no tone at the Nyquist: the phase
/// step it would run at is clamped to half a cycle a frame, which at a phase of a quarter
/// cycle (100 frames at 15 kHz is 31.25 periods) is a full-amplitude alternating tone that the
/// bank must not play.
#[test]
fn a_partial_pushed_past_the_nyquist_mid_stream_leaves_no_nyquist_tone() {
    let mut bank = AdditiveBank::<f64>::new(
        Partials::new(&[partial(1.0, 0.5), partial(3.0, 1.0)]).unwrap(),
        RATE,
    );
    bank.retarget(&controls(5_000.0, 1.0));
    bank.process(MonoMut::new(&mut [0.0; 100]));
    bank.retarget(&controls(9_000.0, 1.0));
    let mut out = vec![0.0; WINDOW];
    bank.process(MonoMut::new(&mut out));
    let nyquist: f64 = out
        .iter()
        .enumerate()
        .map(|(n, y)| if n % 2 == 0 { *y } else { -*y })
        .sum::<f64>()
        / WINDOW as f64;
    assert!(nyquist.abs() <= ROUND_OFF, "{nyquist}");
}
