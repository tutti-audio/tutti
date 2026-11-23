use tutti_buffers::{Planes, PlanesMut};
use tutti_units::{Db, SampleRate, Seconds};

use super::*;
use crate::testing::noise;
use tutti_buffers::InputOutput;

// The banks under test are `f32` (the session's audio format), so their tolerances are `f32`'s;
// the `f64` banks have their own tests below.

const RATE: SampleRate = SampleRate::SR_48K;
const FAST: Seconds = Seconds::new(0.001);
const SLOW: Seconds = Seconds::new(0.02);
const LEVEL: f64 = 0.5;

/// A shaper at `RATE` at rest on the amounts `attack` and `sustain` dB.
fn shaper(attack: f64, sustain: f64) -> TransientBank<f32> {
    let mut bank = TransientBank::<f32>::new(RATE, FAST, SLOW);
    bank.retarget(&TransientControls {
        attack: Db::new(attack),
        sustain: Db::new(sustain),
    });
    bank.jump_to_target();
    bank
}

/// `audio` (one slice per channel) through `bank` in calls of at most `block` frames.
fn run(bank: &mut TransientBank<f32>, audio: &[&[f32]], block: usize) -> Vec<Vec<f32>> {
    let n = audio[0].len();
    let mut out = vec![vec![0.0f32; n]; audio.len()];
    let mut at = 0;
    while at < n {
        let end = (at + block).min(n);
        let a: Vec<&[f32]> = audio.iter().map(|c| &c[at..end]).collect();
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        bank.process(
            &mut InputOutput::paired(Planes::new(&a).unwrap(), PlanesMut::new(&mut outs).unwrap())
                .unwrap(),
        )
        .unwrap();
        at = end;
    }
    out
}

/// The one-pole pole `a = exp(-1 / (tau * rate))` of a follower of time constant `tau`.
fn pole(tau: Seconds) -> f64 {
    (-1.0 / (tau.get() * RATE.get())).exp()
}

/// The amplitude of `db`, from the definition.
fn amplitude(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// A level of `LEVEL` held for `n` frames: a square wave, whose magnitude is steady.
fn steady(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            if i % 2 == 0 {
                LEVEL as f32
            } else {
                -LEVEL as f32
            }
        })
        .collect()
}

/// With the fast and slow envelopes at one level (a steady magnitude), the gain is 0 dB
/// whatever the amounts are: the closed form's `f = s` branch.
#[test]
fn a_steady_level_is_left_alone_at_any_amounts() {
    for (attack, sustain) in [(0.0, 0.0), (12.0, -12.0), (-24.0, 24.0), (24.0, 24.0)] {
        let mut bank = shaper(attack, sustain);
        let x = steady(48_000);
        let y = run(&mut bank, &[&x], 480).remove(0);
        // A settled steady level: the fast envelope is at the level to `f32`'s rounding, and
        // the slow one stalls within `6e-8 / c` of it (`c` = 1 - exp(-1 / (20 ms * 48 kHz)),
        // about 1e-3, so 6e-5 relative): the gain is 24 dB * 6e-5 = 1.4e-3 dB at most, an
        // output error of 0.5 * 0.115 * 1.4e-3 = 8e-5, under the 1e-4 allowed here.
        for (i, (a, b)) in x.iter().zip(&y).enumerate().skip(40_000) {
            assert!(
                (f64::from(*b) - f64::from(*a)).abs() < 1e-4,
                "frame {i} at ({attack}, {sustain}) dB: {a} -> {b}"
            );
        }
    }
}

/// From silence into a constant level `L`, the envelopes are `L (1 - a^n)` after `n` frames,
/// so the gain in dB at frame `n - 1` is `attack * (1 - (1 - as^n) / (1 - af^n))`.
///
/// The 2e-5 absolute tolerance on an output of level 0.5: the two `f32` followers carry a
/// relative rounding of at most about 1e-6 after 2000 recurrences, which moves the gain by
/// under `12 dB * 1e-6` and the output by about 1e-6; the closed form moves the output by
/// 0.1 to 0.5, so a wrong branch or time constant is far outside the band.
#[test]
fn the_gain_on_a_step_onset_follows_the_two_followers_step_responses() {
    let attack: f64 = 12.0;
    let mut bank = shaper(attack, -12.0);
    let x = vec![LEVEL as f32; 2_000];
    let y = run(&mut bank, &[&x], 64).remove(0);
    let (af, asl) = (pole(FAST), pole(SLOW));
    let mut peak = 0.0f64;
    for (i, got) in y.iter().enumerate() {
        let n = (i + 1) as i32;
        let g = attack * (1.0 - (1.0 - asl.powi(n)) / (1.0 - af.powi(n)));
        peak = peak.max(g);
        let want = LEVEL * amplitude(g);
        assert!(
            (f64::from(*got) - want).abs() < 2e-5,
            "frame {i}: gain {got} vs {want} ({g} dB)"
        );
    }
    assert!(
        peak > 0.5 * attack,
        "the onset reaches into the attack amount: {peak} dB"
    );
}

/// After a steady `L`, a drop to a constant `L2` moves the envelopes to
/// `L2 + (L - L2) a^n`, and the fast one is under the slow one: the gain in dB is
/// `sustain * (1 - f / s)`.
#[test]
fn the_gain_on_a_decay_follows_the_two_followers_step_responses() {
    let sustain: f64 = 9.0;
    let mut bank = shaper(-6.0, sustain);
    let low = LEVEL / 4.0;
    let mut x = vec![LEVEL as f32; 48_000];
    x.extend(std::iter::repeat_n(low as f32, 2_000));
    let y = run(&mut bank, &[&x], 100).remove(0);
    let (af, asl) = (pole(FAST), pole(SLOW));
    let mut deepest = 0.0f64;
    for i in 0..2_000 {
        let n = (i + 1) as i32;
        let f = low + (LEVEL - low) * af.powi(n);
        let s = low + (LEVEL - low) * asl.powi(n);
        let g = sustain * (1.0 - f / s);
        deepest = deepest.max(g);
        let want = low * amplitude(g);
        let got = f64::from(y[48_000 + i]);
        assert!(
            (got - want).abs() < 2e-5,
            "decay frame {i}: {got} vs {want} ({g} dB)"
        );
    }
    assert!(
        deepest > 0.5 * sustain,
        "the decay reaches into the sustain amount: {deepest} dB"
    );
}

/// A shaper at 0 dB on both amounts passes any signal bit for bit.
#[test]
fn zero_amounts_are_the_identity() {
    let mut bank = shaper(0.0, 0.0);
    let x = noise(3, 4_000, 0.9);
    let y = run(&mut bank, &[&x], 256).remove(0);
    assert_eq!(x, y);
}

/// One gain serves every channel, read from the louder one: the gain on both channels is the
/// closed-form decay of the loudest level (`0.5` falling to `0.25`), which a detector per
/// channel would not give (the quieter channel's own level never falls).
#[test]
fn the_channels_share_one_gain_from_the_loudest() {
    let sustain: f64 = 9.0;
    let mut bank = shaper(0.0, sustain);
    let (hi, lo) = (0.5f64, 0.25f64);
    let settled = 48_000;
    let after = 1_500;
    // The loud channel steps from 0.5 to 0.1 and the other from 0.1 to 0.25: the loudest
    // level falls from 0.5 to 0.25 while the second channel's own level rises.
    let mut l = vec![hi as f32; settled];
    l.extend(std::iter::repeat_n(0.1f32, after));
    let mut r = vec![0.1f32; settled];
    r.extend(std::iter::repeat_n(lo as f32, after));
    let y = run(&mut bank, &[&l, &r], 77);
    let (af, asl) = (pole(FAST), pole(SLOW));
    for i in 0..after {
        let n = (i + 1) as i32;
        let f = lo + (hi - lo) * af.powi(n);
        let s = lo + (hi - lo) * asl.powi(n);
        let want = amplitude(sustain * (1.0 - f / s));
        for (c, x) in [(0usize, &l), (1usize, &r)] {
            let got = f64::from(y[c][settled + i]) / f64::from(x[settled + i]);
            assert!(
                (got - want).abs() < 1e-4,
                "channel {c}, frame {i}: gain {got} vs {want}"
            );
        }
    }
}

/// A request past `MAX_AMOUNT` is the bound, which the step-onset closed form then follows,
/// and a non-finite amount is 0 dB (the identity).
#[test]
fn the_amounts_are_held_within_the_range_and_a_non_finite_one_is_zero() {
    let x = vec![LEVEL as f32; 600];
    let (af, asl) = (pole(FAST), pole(SLOW));
    for (asked, bound) in [(1_000.0, 24.0), (-1_000.0, -24.0)] {
        let y = run(&mut shaper(asked, 0.0), &[&x], 600).remove(0);
        for (i, got) in y.iter().enumerate() {
            let n = (i + 1) as i32;
            let g = bound * (1.0 - (1.0 - asl.powi(n)) / (1.0 - af.powi(n)));
            let want = LEVEL * amplitude(g);
            assert!(
                (f64::from(*got) - want).abs() < 2e-5,
                "asked {asked}, frame {i}"
            );
        }
    }
    for asked in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(run(&mut shaper(asked, asked), &[&x], 600).remove(0), x);
    }
}

/// A mid-stream `retarget` after `jump_to_target` ramps the amount along
/// `lerp(a0, a1, k / steps)` over the `GAIN_RAMP` steps (`ceil(5 ms * rate)`), frame `j` after
/// the `retarget` reading step `j + 1`, and the rendered gain is that amount in the onset's
/// closed form. Tolerance as the onset test's.
#[test]
fn a_retargeted_amount_ramps_along_the_linear_ramp() {
    let (a0, a1): (f64, f64) = (6.0, 18.0);
    let steps = (GAIN_RAMP.get() * RATE.get()).ceil() as usize;
    let cut = 500;
    let mut bank = shaper(a0, 0.0);
    let x = vec![LEVEL as f32; 1_200];
    let mut y = run(&mut bank, &[&x[..cut]], 64).remove(0);
    bank.retarget(&TransientControls {
        attack: Db::new(a1),
        sustain: Db::UNITY,
    });
    y.extend(run(&mut bank, &[&x[cut..]], 64).remove(0));
    let (af, asl) = (pole(FAST), pole(SLOW));
    for (i, got) in y.iter().enumerate() {
        let n = (i + 1) as i32;
        let amount = if i < cut {
            a0
        } else {
            let k = (i - cut + 1).min(steps);
            a0 + (a1 - a0) * k as f64 / steps as f64
        };
        let g = amount * (1.0 - (1.0 - asl.powi(n)) / (1.0 - af.powi(n)));
        let want = LEVEL * amplitude(g);
        assert!(
            (f64::from(*got) - want).abs() < 2e-5,
            "frame {i}: {got} vs {want} (amount {amount} dB)"
        );
    }
}

/// The render does not depend on where blocks fall, with a `retarget` between two calls: the
/// frames before and after it come out bit for bit as one block of 1 200 gives them.
#[test]
fn the_render_does_not_depend_on_where_blocks_fall() {
    let x = noise(5, 1_200, 0.8);
    let cut = 500;
    let render = |block: usize| {
        let mut bank = shaper(6.0, -3.0);
        let mut y = run(&mut bank, &[&x[..cut]], block).remove(0);
        bank.retarget(&TransientControls {
            attack: Db::new(18.0),
            sustain: Db::new(9.0),
        });
        y.extend(run(&mut bank, &[&x[cut..]], block).remove(0));
        y
    };
    let reference = render(1_200);
    assert!(
        x.iter().zip(&reference).any(|(a, b)| a != b),
        "the shaper moved the signal"
    );
    for block in [1, 7, 64, 480] {
        assert_eq!(reference, render(block), "blocks of {block}");
    }
}

/// At 0 dB attack and 0 dB sustain the gain is `10^0 = 1` on both branches, so an `f64`
/// shaper returns a signal of more than 24 bits unchanged, rising (the fast envelope above the
/// slow) and falling (below it): nothing in the path narrows it.
#[test]
fn an_f64_shaper_at_zero_amounts_is_the_identity() {
    use tutti_fixtures::signal::{assert_close, wide_signal};
    let rising = wide_signal(2_000);
    let x: Vec<f64> = rising.iter().chain(rising.iter().rev()).copied().collect();
    let mut bank = TransientBank::<f64>::new(RATE, FAST, SLOW);
    bank.retarget(&TransientControls::DEFAULT);
    bank.jump_to_target();
    let mut out = vec![0.0f64; x.len()];
    bank.process(
        &mut InputOutput::paired(
            Planes::new(&[&x[..]]).unwrap(),
            PlanesMut::new(&mut [&mut out[..]]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_close(&out, &x, 0.0);
}
