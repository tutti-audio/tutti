use super::*;
use crate::Complex64;
use tutti_buffers::{InputOutput, Planes, PlanesMut};

const LN_1000: f64 = 3.0 * core::f64::consts::LN_10;

/// A rate and a frequency whose loop is a whole number of frames, `P = 98` (a rail of
/// `M = 48`, the all-pass's delay exactly 1 and its coefficient exactly 0).
const RATE: f64 = 49_000.0;
const F0: f64 = 500.0;
const PERIOD: f64 = 98.0;
const RAIL: usize = 48;
const T60: f64 = 0.5;
/// A decay whose round-trip gain is above the comb's 0.99 bound: `exp(-ln(1000) 98 / (49000 5))`.
const LONG_T60: f64 = 5.0;

fn string(excitation: f64, pickup: f64) -> WaveguideString<f64> {
    string_at(excitation, pickup, T60)
}

fn string_at(excitation: f64, pickup: f64, t60: f64) -> WaveguideString<f64> {
    let mut s = WaveguideString::new(
        WaveguidePositions {
            excitation: Normalized::of(excitation),
            pickup: Normalized::of(pickup),
        },
        LowestPitch::new(Hz::new(50.0)).unwrap(),
        SampleRate::of(RATE),
    );
    s.retarget(&WaveguideControls {
        frequency: Hz::new(F0),
        decay: Seconds::new(t60),
    });
    s
}

fn impulse(s: &mut WaveguideString<f64>, len: usize) -> Vec<f64> {
    (0..len)
        .map(|n| s.tick(if n == 0 { 1.0 } else { 0.0 }))
        .collect()
}

/// The round trip's gain: `exp(-ln(1000) P / (fs T60))`.
fn rho() -> f64 {
    rho_at(T60)
}

fn rho_at(t60: f64) -> f64 {
    (-LN_1000 * PERIOD / (RATE * t60)).exp()
}

/// The DTFT of `h` at `w`.
fn dtft(h: &[f64], w: f64) -> Complex64 {
    h.iter()
        .enumerate()
        .fold(Complex64::new(0.0, 0.0), |acc, (n, &v)| {
            acc + Complex64::new(v, 0.0) * (Complex64::new(0.0, -w) * n as f64).exp()
        })
}

/// The transfer function from the input to the pickup at `z^-1 = e^(-jw)`, from the rails'
/// equations (`R = Rn z^-M L + z^a_q / 2`, `L = Rb z^-M R + z^(M - a_q) / 2`, with
/// `Rn = -g z^-1 (1 + z^-1) / 2` at the nut, `Rb = -g (1 + z^-1) / 2` at the bridge and the
/// all-pass `z^-1`), read at `a_p` frames from the nut.
fn transfer(w: f64, a_q: usize, a_p: usize) -> Complex64 {
    let zi = Complex64::new(0.0, -w).exp();
    let z = |k: i64| zi.powi(k as i32);
    let g = rho().sqrt();
    let rb = (zi + 1.0) * 0.5 * (-g);
    let rn = rb * zi;
    let m = RAIL as i64;
    let (a_q, a_p) = (a_q as i64, a_p as i64);
    let denom = Complex64::new(1.0, 0.0) - rn * rb * z(2 * m);
    let r = (z(-a_q) + rn * z(a_q)) * 0.5 / denom;
    let l = (z(a_q - m) + rb * z(m - a_q)) * 0.5 / denom;
    // A wave injected downstream of the pickup (on its own rail) is read there at a frame before
    // the input: the model's `z^(a_q - a_p)` and `z^(a_p - a_q)` direct terms, whichever is
    // acausal, are not in the response.
    let acausal = if a_p < a_q {
        z(a_p - a_q) * 0.5
    } else if a_p > a_q {
        z(a_q - a_p) * 0.5
    } else {
        Complex64::new(0.0, 0.0)
    };
    r * z(a_p) + l * z(m - a_p) - acausal
}

/// At `d = 1` the all-pass is one frame of delay, so with the ends' average `[1/2, 1/2]` and
/// the end gains `g g = rho` the pickup's response obeys the round trip exactly:
/// `h[n] = rho (h[n - 2M - 1] + 2 h[n - 2M - 2] + h[n - 2M - 3]) / 4` once the first
/// injection has been through (`n >= 2M + 3`). Every wave on the rails repeats at the round
/// trip, so the pickup's does.
#[test]
fn the_pickup_repeats_at_the_round_trip_with_the_decay_and_the_ends_taps() {
    for t60 in [T60, LONG_T60] {
        check_round_trip(t60);
    }
}

fn check_round_trip(t60: f64) {
    let mut s = string_at(0.2, 0.33, t60);
    assert_eq!(s.tuning().0.get(), RAIL);
    assert!(
        t60 < 1.0 || rho_at(t60) > 0.99,
        "the long decay is above the comb's bound"
    );
    let h = impulse(&mut s, 3000);
    let m = 2 * RAIL;
    let worst = (m + 3..h.len())
        .map(|n| {
            let want = rho_at(t60) * (h[n - m - 1] + 2.0 * h[n - m - 2] + h[n - m - 3]) / 4.0;
            (h[n] - want).abs()
        })
        .fold(0.0, f64::max);
    assert!(worst < 1e-14, "{worst}");
    assert!(
        h.iter().any(|v| v.abs() > 0.1),
        "the response is not silent"
    );
}

/// The simulated response's DTFT is the transfer function's at the modes `j fs / P` (the response
/// is cut where `rho^k` is 1e-24), and a pickup at the middle of the string, a node of every even
/// mode, reads them under 5% of their odd neighbours (the closed form gives 4.6% for mode 2 against mode 1): a node's residue is the ends' loss `1 - g cos(w / 2)` (0.016) where an antinode's is
/// `1 + g cos(w / 2)` (1.98), and the excitation at a fifth of the string and the shared
/// resonance weigh the modes by under 5. A pickup a third of the way along reads mode 2 above
/// 30% of mode 1.
#[test]
fn a_pickup_at_a_node_of_a_mode_leaves_that_mode_out() {
    let len = 200_000;
    let mode = |j: f64| core::f64::consts::TAU * j / PERIOD;
    let centre = {
        let mut s = string(0.2, 0.5);
        assert_eq!(s.taps().1.get(), RAIL / 2);
        impulse(&mut s, len)
    };
    let third = impulse(&mut string(0.2, 1.0 / 3.0), len);
    let (a_q, a_third) = (10, 16);
    for j in 1..=4 {
        let w = mode(f64::from(j));
        for (h, a_p) in [(&centre, RAIL / 2), (&third, a_third)] {
            let got = dtft(h, w);
            let want = transfer(w, a_q, a_p);
            assert!(
                (got - want).norm() < 1e-9 * want.norm().max(1.0),
                "mode {j}, pickup {a_p}: {got} against {want}"
            );
        }
    }
    let at = |h: &[f64], j: f64| dtft(h, mode(j)).norm();
    // Even modes vanish at the centre; odd modes do not; a third of the way, mode 2 does not.
    assert!(
        at(&centre, 2.0) < 0.05 * at(&centre, 1.0),
        "{} {}",
        at(&centre, 2.0),
        at(&centre, 1.0)
    );
    assert!(at(&centre, 4.0) < 0.05 * at(&centre, 3.0));
    assert!(at(&third, 2.0) > 0.3 * at(&third, 1.0));
}

/// The tail covers the ring-out: nothing past it is above `TAIL_FLOOR` (-60 dB) of the input.
#[test]
fn the_declared_tail_covers_the_ring_out() {
    let controls = WaveguideControls {
        frequency: Hz::new(F0),
        decay: Seconds::new(T60),
    };
    let mut s = string(0.2, 0.33);
    let Tail::Finite(tail) = s.tail(&controls) else {
        panic!("a finite tail")
    };
    let h = impulse(&mut s, tail.get() + 20_000);
    let last = h.iter().rposition(|y| y.abs() > 1e-3).unwrap();
    assert!(
        last < tail.get(),
        "rings to {last}, declared {}",
        tail.get()
    );
    assert!(
        last > tail.get() / 20,
        "the declared tail is not absurdly long"
    );
}

/// A position is held inside the rails (`1..M`) and a frequency past `fs / 8` is held at it.
#[test]
fn positions_and_frequencies_are_held_inside_the_string() {
    let ends = WaveguideString::<f64>::new(
        WaveguidePositions {
            excitation: Normalized::of(0.0),
            pickup: Normalized::of(1.0),
        },
        LowestPitch::new(Hz::new(50.0)).unwrap(),
        SampleRate::of(RATE),
    );
    assert_eq!(
        ends.taps(),
        (Samples::new(1), Samples::new(ends.tuning().0.get() - 1))
    );
    let mut high = string(0.2, 0.33);
    high.retarget(&WaveguideControls {
        frequency: Hz::new(1.0e6),
        decay: Seconds::new(1.0),
    });
    assert_eq!(high.tuning().0.get(), 3);
}

/// Every channel of a width-3 bank is a string: an impulse on each channel rings on each of
/// them.
#[test]
fn every_channel_of_a_three_wide_bank_rings() {
    let mut bank = WaveguideBank::<f64>::new(
        WaveguidePositions {
            excitation: Normalized::of(0.2),
            pickup: Normalized::of(0.7),
        },
        LowestPitch::new(Hz::new(50.0)).unwrap(),
        SampleRate::of(RATE),
        ChannelLayout::from_count(3),
    );
    let mut x = vec![0.0; 1024];
    x[0] = 1.0;
    let mut y = vec![vec![0.0; 1024]; 3];
    let [y0, y1, y2] = &mut y[..] else {
        unreachable!()
    };
    bank.process(
        &mut InputOutput::paired(
            Planes::new(&[&x[..], &x[..], &x[..]]).unwrap(),
            PlanesMut::new(&mut [&mut y0[..], &mut y1[..], &mut y2[..]]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    for (c, ch) in y.iter().enumerate() {
        assert!(ch.iter().any(|v| *v != 0.0), "channel {c} is silent");
    }
}
