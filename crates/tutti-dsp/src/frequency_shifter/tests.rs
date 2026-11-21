use super::*;
use crate::Complex64;
use crate::testing::amplitude_at;
use core::f64::consts::{FRAC_PI_2, PI, TAU};

const RATE: SampleRate = SampleRate::SR_48K;

/// A chain's `H(e^{jw}) = prod (z^-2 - a^2) / (1 - a^2 z^-2)`, the closed form of the sections
/// (the numerator's sign is the same in both chains, so it cancels in their difference).
fn chain(a: &[f64; 4], w: f64) -> Complex64 {
    let z2 = Complex64::from_polar(1.0, -2.0 * w);
    a.iter()
        .map(|a| (z2 - a * a) / (1.0 - z2 * (a * a)))
        .product()
}

/// The frequencies of `lo..=hi` of the Nyquist frequency, 20,000 of them spaced evenly on the
/// logarithm, as radians a frame.
fn grid(lo: f64, hi: f64) -> impl Iterator<Item = f64> {
    (0..=20_000).map(move |k| PI * lo * (hi / lo).powf(f64::from(k) / 20_000.0))
}

/// Both chains have unit magnitude at every frequency, and the second leads the first (on the
/// input one frame late) by 90 degrees to within [`QUADRATURE_ERROR`] over the stated band. The
/// bound is also tight: the worst error over the band is within 1% of it, so a stale figure
/// fails.
#[test]
fn the_chains_are_in_quadrature_within_the_stated_error_over_the_stated_band() {
    let mut worst = 0.0f64;
    for w in grid(0.002, 0.998) {
        let (a, b) = (chain(&FIRST, w), chain(&SECOND, w));
        assert!((a.norm() - 1.0).abs() < 1e-12 && (b.norm() - 1.0).abs() < 1e-12);
        // The first chain is fed one frame late: `e^{-jw}`.
        let lead = (a * Complex64::from_polar(1.0, -w) / b).arg() + FRAC_PI_2;
        worst = worst.max(lead.abs());
        assert!(lead.abs() <= QUADRATURE_ERROR.get(), "w = {w}: {lead} rad");
    }
    assert!(worst > 0.99 * QUADRATURE_ERROR.get(), "worst {worst}");
    let (low, high) = quadrature_band(RATE);
    assert_eq!((low.get(), high.get()), (48.0, 23_952.0));
}

/// The two exact figures of a shifted sine: `|y|` has `cos(e / 2)` of the amplitude at the
/// wanted frequency and `sin(e / 2)` at the image, for the quadrature error `e` at the input's
/// frequency, which is at most [`QUADRATURE_ERROR`].
fn shifted(input: f64, shift: f64) -> (Vec<f64>, usize) {
    let mut bank = FrequencyShifterBank::<f64>::new(ChannelLayout::MONO, RATE);
    bank.retarget(&FrequencyShifterControls {
        shift: Hz::new(shift),
    });
    let frames = 48_000;
    let x: Vec<f64> = (0..frames)
        .map(|n| (TAU * input * n as f64 / RATE.get()).sin())
        .collect();
    let mut y = vec![0.0; frames];
    bank.process(
        Planes::new(&[&x[..]]).unwrap(),
        PlanesMut::new(&mut [&mut y[..]]).unwrap(),
    );
    (y, frames / 2)
}

/// A sine at `f` comes out at `f + s` for a positive shift `s` and `f - s` for a negative one,
/// at `cos(e / 2)` of its amplitude, with the image at `s - f` (resp. `f + s`) under `sin(e /
/// 2)` of it and nothing left at `f`, measured over whole periods, at a frequency in the middle
/// of the band and one at its low edge.
#[test]
fn a_sine_shifts_by_the_shift_and_the_image_is_under_the_quadrature_error() {
    let half = QUADRATURE_ERROR.get() / 2.0;
    for (input, shift) in [
        (1_000.0, 300.0),
        (1_000.0, -300.0),
        (60.0, 1_000.0),
        (60.0, -20.0),
    ] {
        let (y, from) = shifted(input, shift);
        let y = &y[from..];
        let (wanted, image) = if shift > 0.0 {
            (input + shift, shift - input)
        } else {
            ((input + shift).abs(), input - shift)
        };
        let image = image.abs();
        let w = amplitude_at(y, wanted, RATE.get());
        let i = amplitude_at(y, image, RATE.get());
        assert!(
            w >= half.cos() - 1e-9 && w <= 1.0 + 1e-9,
            "{input} {shift:+}: wanted {w}"
        );
        assert!(i <= half.sin(), "{input} {shift:+}: image {i}");
        assert!(
            amplitude_at(y, input, RATE.get()) <= half.sin(),
            "{input} {shift:+}: the input frequency is gone"
        );
    }
}

/// A shift of zero is the first chain alone: a sine keeps its amplitude to 1e-9 (a unit-magnitude
/// all-pass) at its own frequency.
#[test]
fn no_shift_is_a_unit_gain_phase_network() {
    let (y, from) = shifted(1_000.0, 0.0);
    assert!((amplitude_at(&y[from..], 1_000.0, RATE.get()) - 1.0).abs() < 1e-9);
}

/// The carrier's phase runs across blocks: any cut of the input gives the render of one call,
/// to the bit.
#[test]
fn any_block_size_gives_the_same_bits() {
    let x: Vec<f64> = (0..500).map(|n| (0.3 * n as f64).sin() + 0.1).collect();
    let run = |block: usize| {
        let mut bank = FrequencyShifterBank::<f64>::new(ChannelLayout::MONO, RATE);
        bank.retarget(&FrequencyShifterControls {
            shift: Hz::new(-123.4),
        });
        let mut out = Vec::new();
        for chunk in x.chunks(block) {
            let mut y = vec![0.0; chunk.len()];
            bank.process(
                Planes::new(&[chunk]).unwrap(),
                PlanesMut::new(&mut [&mut y[..]]).unwrap(),
            );
            out.extend(y);
        }
        out
    };
    let whole = run(500);
    for block in [1, 7, 64] {
        assert_eq!(run(block), whole, "block {block}");
    }
}

/// The impulse response is under [`TAIL_FLOOR`] of its peak after the declared tail.
#[test]
fn the_tail_covers_the_impulse_response_down_to_the_floor() {
    let mut bank = FrequencyShifterBank::<f64>::new(ChannelLayout::MONO, RATE);
    let Tail::Finite(tail) = bank.tail() else {
        panic!("a finite tail");
    };
    let frames = 4 * tail.get();
    let mut x = vec![0.0; frames];
    x[0] = 1.0;
    let mut y = vec![0.0; frames];
    bank.process(
        Planes::new(&[&x[..]]).unwrap(),
        PlanesMut::new(&mut [&mut y[..]]).unwrap(),
    );
    let peak = y.iter().fold(0.0f64, |m, v| m.max(v.abs()));
    let last = y.iter().rposition(|v| v.abs() > peak * 1e-3).unwrap();
    assert!(
        last < tail.get(),
        "rings to {last}, declared {}",
        tail.get()
    );
}

/// Every channel of a width-3 bank is a shifter: a sine on each channel comes out on each of
/// them (the Hilbert pair's response to a sine is a sine of the same amplitude, `cos(e / 2)`).
#[test]
fn every_channel_of_a_three_wide_bank_shifts() {
    let mut bank = FrequencyShifterBank::<f64>::new(ChannelLayout::from_count(3), RATE);
    bank.retarget(&FrequencyShifterControls {
        shift: Hz::new(100.0),
    });
    let x: Vec<f64> = (0..4_800)
        .map(|n| (TAU * 1_000.0 * n as f64 / RATE.get()).sin())
        .collect();
    let mut y = vec![vec![0.0; x.len()]; 3];
    let [y0, y1, y2] = &mut y[..] else {
        unreachable!()
    };
    bank.process(
        Planes::new(&[&x[..], &x[..], &x[..]]).unwrap(),
        PlanesMut::new(&mut [&mut y0[..], &mut y1[..], &mut y2[..]]).unwrap(),
    );
    for (c, ch) in y.iter().enumerate() {
        let peak = ch[2_400..].iter().fold(0.0f64, |m, v| m.max(v.abs()));
        assert!(peak > 0.9, "channel {c} peaks at {peak}");
    }
}
