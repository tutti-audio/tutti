use tutti_buffers::{Planes, PlanesMut};
use tutti_fixtures::signal::impulse;
use tutti_units::{Db, Mix, Real, Samples};

use super::*;
use crate::TAIL_FLOOR;
use crate::reverb::tail_probe;
use tutti_buffers::{BlockSide, InputOutput};

// The tests pin the `f32` instantiation, the session's audio
// format, so their tolerances are `f32`'s.
const RATE: SampleRate = SampleRate::SR_44K1;

fn controls(room: f64, damp: f64) -> FreeverbControls {
    FreeverbControls {
        room: Mix::new(room),
        damp: Mix::new(damp),
    }
}

/// `x` through `bank` in blocks of `block`, `ramps` carried across them: `(left, right)`.
fn run(
    bank: &mut FreeverbBank<f32>,
    ramps: &mut FreeverbRamps<f32>,
    x: &[f32],
    block: usize,
) -> (Vec<f32>, Vec<f32>) {
    let mut out = (vec![0.0f32; x.len()], vec![0.0f32; x.len()]);
    let mut at = 0;
    while at < x.len() {
        let end = (at + block).min(x.len());
        let a = [&x[at..end]];
        let (l, r) = (&mut out.0[at..end], &mut out.1[at..end]);
        let mut outs: [&mut [f32]; 2] = [l, r];
        bank.process(
            ramps,
            &mut InputOutput::paired(Planes::new(&a).unwrap(), PlanesMut::new(&mut outs).unwrap())
                .unwrap(),
        )
        .unwrap();
        at = end;
    }
    out
}

/// A bank at `rate` with `c` set and at rest.
fn settled(rate: SampleRate, c: &FreeverbControls) -> (FreeverbBank<f32>, FreeverbRamps<f32>) {
    let bank = FreeverbBank::<f32>::new(rate);
    let mut ramps = bank.ramps();
    bank.retarget(&mut ramps, c);
    ramps.jump_to_target();
    (bank, ramps)
}

/// A comb's impulse response from its definition: the loop `v[n] = x[n] + g s[n]` with `s` the
/// one-pole `s[n] = (1 - d) y[n] + d s[n - 1]` of the output `y[n] = v[n - L]`. The first
/// echo is the impulse at `L`; echo `k >= 2` is at `kL` through `k - 1` passes of the
/// one-pole, each scaled by `g`: `g^(k-1) (1 - d)^(k-1) C(m + k - 2, m) d^m` at `kL + m`, the
/// `k - 1` fold convolution of the one-pole's `(1 - d) d^m` (a negative binomial). The echoes
/// overlap once `m` reaches `L`, and add.
fn comb_response(len: usize, l: usize, g: f64, d: f64) -> Vec<f64> {
    let mut h = vec![0.0; len];
    for k in 1..=len / l {
        for m in 0..len.saturating_sub(k * l) {
            let at = k * l + m;
            h[at] += if k == 1 {
                if m == 0 { 1.0 } else { 0.0 }
            } else {
                let choose = (0..m).fold(1.0f64, |c, i| {
                    c * (k as f64 - 2.0 + (i + 1) as f64) / (i + 1) as f64
                });
                (g * (1.0 - d)).powi(k as i32 - 1) * choose * d.powi(m as i32)
            };
        }
    }
    h
}

/// Each comb is the feedback comb of its definition: taps at multiples of its length with the
/// damping low-pass applied in the loop. At damping 0 that is `g^(k-1)` at `kL` and nothing
/// between; at 0.2 and 0.4 it is the negative-binomial echoes of [`comb_response`].
#[test]
fn a_comb_is_the_feedback_comb_of_its_definition() {
    for (l, g, d) in [
        (100usize, 0.84f64, 0.0f64),
        (97, 0.9, 0.2),
        (64, 0.7, 0.4),
        (200, 0.98, 0.1),
    ] {
        let mut comb = CombFilter::<f32>::new(
            CombForm::Feedback,
            NonZeroUsize::new(l).unwrap(),
            Feedback::NONE,
        );
        comb.retune(g as f32, d as f32);
        let len = 6 * l + 3;
        let x = impulse(Samples::new(len));
        let mut y = vec![0.0f32; len];
        for (y, &x) in y.iter_mut().zip(&x) {
            *y = comb.tick(x);
        }
        let want = comb_response(len, l, g, d);
        for (n, (&got, &w)) in y.iter().zip(&want).enumerate() {
            assert!(
                (f64::from(got) - w).abs() < 1e-5,
                "L {l}, g {g}, d {d}, frame {n}: {got} vs {w}"
            );
        }
        if d == 0.0 {
            let loud = |n: usize| n.is_multiple_of(l) && n >= l;
            assert!((0..len).filter(|&n| !loud(n)).all(|n| y[n] == 0.0), "L {l}");
        }
    }
}

/// An all-pass is Freeverb's, `H(z) = ((1 + g) z^-D - 1) / (1 - g z^-D)`, not the
/// unit-magnitude lattice. Its impulse response, from the transfer function, is -1 at frame 0
/// and `g^(k-1)` at frame `k D` for `k >= 1`, and zero elsewhere; its gain is 1 at DC for
/// `g = 0.5` and `(2 + g) / (1 + g)` where `D w = pi`.
#[test]
fn an_all_pass_is_freeverbs_form() {
    let (d, g) = (8usize, 0.5f32);
    let mut ap =
        AllPassFilter::<f32>::new(NonZeroUsize::new(d).unwrap(), Feedback::new(f64::from(g)))
            .form(AllPassForm::Freeverb);
    let mut h = impulse(Samples::new(18 * d + 1));
    ap.process(&mut h);
    assert!((h[0] + 1.0).abs() < 1e-6, "h[0] = {}", h[0]);
    for k in 1..=17usize {
        assert!(
            (h[k * d] - g.powi(k as i32 - 1)).abs() < 1e-5,
            "h[{}] = {}",
            k * d,
            h[k * d]
        );
    }
    assert!((0..h.len()).filter(|n| n % d != 0).all(|n| h[n] == 0.0));
    let gain_at = |omega: f64| {
        let (re, im) = h
            .iter()
            .enumerate()
            .fold((0.0f64, 0.0f64), |(re, im), (n, &x)| {
                let a = omega * n as f64;
                (
                    re + f64::from(x) * libm::cos(a),
                    im - f64::from(x) * libm::sin(a),
                )
            });
        re.hypot(im)
    };
    assert!((gain_at(0.0) - 1.0).abs() < 1e-3, "DC {}", gain_at(0.0));
    let peak = gain_at(std::f64::consts::PI / d as f64);
    assert!(
        (peak - (2.0 + 0.5) / (1.0 + 0.5)).abs() < 1e-3,
        "peak {peak}"
    );
}

const COMBS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALL_PASSES: [usize; 4] = [556, 441, 341, 225];

/// The tunings and the stereo spread are Freeverb's (combs 1116, 1188, 1277, 1356, 1422,
/// 1491, 1557, 1617 and all-passes 556, 441, 341, 225 at 44.1 kHz), the right channel's
/// lines 23 frames longer, and at another rate each is `round(rate / 44100 * tuning)` of
/// the rate as given, fractional included.
#[test]
fn the_tunings_and_the_stereo_spread_are_freeverbs() {
    let bank = FreeverbBank::<f32>::new(RATE);
    let lens = |side: usize| -> (Vec<usize>, Vec<usize>) {
        (
            bank.combs[side].iter().map(|c| c.delay().get()).collect(),
            bank.all_passes[side]
                .iter()
                .map(|a| a.delay().get())
                .collect(),
        )
    };
    let (left, aps) = (COMBS, ALL_PASSES);
    assert_eq!(lens(0), (left.to_vec(), aps.to_vec()));
    assert_eq!(
        lens(1),
        (
            left.iter().map(|l| l + 23).collect(),
            aps.iter().map(|l| l + 23).collect()
        )
    );
    for hz in [
        22_050.0f64,
        35_000.4,
        44_100.4,
        48_000.0,
        96_000.0,
        192_000.0,
    ] {
        let bank = FreeverbBank::<f32>::new(SampleRate::of(hz));
        let scale = |t: usize| (hz / 44_100.0 * t as f64).round() as usize;
        for (side, spread) in [(0, 0), (1, 23)] {
            assert_eq!(
                bank.combs[side]
                    .iter()
                    .map(|c| c.delay().get())
                    .collect::<Vec<_>>(),
                left.iter().map(|&l| scale(l + spread)).collect::<Vec<_>>(),
                "{hz} Hz, side {side}"
            );
            assert_eq!(
                bank.all_passes[side]
                    .iter()
                    .map(|a| a.delay().get())
                    .collect::<Vec<_>>(),
                aps.iter().map(|&l| scale(l + spread)).collect::<Vec<_>>(),
                "{hz} Hz, side {side}"
            );
        }
    }
}

/// The spread is heard: an impulse comes out of the left at the shortest comb, 1116 frames at
/// 44.1 kHz, and out of the right 23 frames later, and nothing before.
#[test]
fn the_right_channel_sounds_23_frames_after_the_left() {
    let (mut bank, mut ramps) = settled(RATE, &FreeverbControls::GENERAL_MIDI);
    let (l, r) = run(&mut bank, &mut ramps, &impulse(Samples::new(1_300)), 64);
    assert!(l[..1116].iter().all(|&s| s == 0.0));
    assert!(l[1116].abs() > 0.01);
    assert!(r[..1139].iter().all(|&s| s == 0.0));
    assert!(r[1139].abs() > 0.01);
}

/// The wet signal is the input through the 8 combs summed and the four all-passes; with the
/// input scaled by 0.015 the first echo at the shortest comb is `0.015` through the
/// all-passes' direct paths, `(-1)^4`.
#[test]
fn the_input_is_scaled_by_the_fixed_gain() {
    let (mut bank, mut ramps) = settled(RATE, &FreeverbControls::GENERAL_MIDI);
    let (l, _) = run(&mut bank, &mut ramps, &impulse(Samples::new(1_200)), 64);
    assert!((l[1116] - 0.015).abs() < 1e-6, "{}", l[1116]);
    assert_eq!(FreeverbBank::<f32>::FIXED_GAIN, Gain::new(0.015));
}

/// The decay is Schroeder's: a comb of `L` frames at loop gain `g` falls by 60 dB after
/// `RT60 = -3 L / (rate log10 g)` seconds, `L ln(1e-3) / ln g` frames. The comb's own
/// response at damping 0 crosses 1e-3 of its first echo at that trip, read off its echoes,
/// at several room sizes (the longest comb, 1640 frames at the right).
#[test]
fn a_comb_falls_sixty_decibels_in_the_schroeder_rt60() {
    let l = 1_640usize;
    for room in [0.0f64, 0.5, 0.9, 1.0] {
        let feedback = controls(room, 0.0).feedback();
        let g = feedback.get();
        let mut comb =
            CombFilter::<f32>::new(CombForm::Feedback, NonZeroUsize::new(l).unwrap(), feedback);
        let trips = (-3.0 / g.log10()).ceil() as usize + 2;
        let len = (trips + 1) * l;
        let x = impulse(Samples::new(len));
        let mut y = x;
        comb.process(&mut y);
        let first_below = (1..)
            .find(|&k| y[k * l].abs() < 1e-3)
            .expect("the comb decays");
        // The first echo is the impulse itself: `first_below - 1` trips of the loop gain.
        let rt60 = (first_below - 1) as f64 * l as f64 / RATE.get();
        let formula = -3.0 * l as f64 / (RATE.get() * g.log10());
        assert!(
            (rt60 - formula).abs() <= l as f64 / RATE.get(),
            "room {room}: measured {rt60} s, Schroeder {formula} s"
        );
    }
    // The General MIDI room: 1640 frames at 0.84 is 1.473 s.
    let gm = -3.0 * 1640.0 / (44_100.0 * libm::log10(0.84));
    assert!((gm - 1.4734).abs() < 1e-3, "{gm}");
}

/// The tail covers the ring-out and is not absurdly long: the declared tail of the General
/// MIDI room is past the Schroeder RT60 of its longest comb and the all-pass chain's
/// echoes, and the render has fallen 60 dB under its peak by then and not long before.
#[test]
fn the_tail_is_the_rt60_of_the_longest_comb_plus_the_chain() {
    let gm = FreeverbControls::GENERAL_MIDI;
    let (mut bank, mut ramps) = settled(RATE, &gm);
    let Tail::Finite(tail) = bank.tail_of(&gm) else {
        panic!("a finite tail")
    };
    let g = gm.feedback().get();
    let trips = (TAIL_FLOOR.get() / 20.0 * std::f64::consts::LN_10 / g.ln()).ceil();
    let comb = (1.0 + trips) * (1640.0 + 2.0);
    let chain: f64 = [579.0f64, 464.0, 364.0, 248.0]
        .iter()
        .map(|d| {
            (1.0 + (-60.0f64 / 20.0 * std::f64::consts::LN_10 / 0.5f64.ln()).ceil()) * (d + 2.0)
        })
        .sum();
    // The figure is seconds at the rate, ceiled to frames: the sum of whole frames, to the
    // frame the division rounds to.
    let want = (comb + chain).ceil() as usize;
    assert!(
        tail.get().abs_diff(want) <= 1,
        "{} against {want}",
        tail.get()
    );
    assert!(tail.get() as f64 > 1.5 * RATE.get(), "past one RT60");
    let len = tail.get() + 20_000;
    let (l, r) = run(&mut bank, &mut ramps, &impulse(Samples::new(len)), 512);
    let peak = l.iter().chain(&r).fold(0.0f32, |m, s| m.max(s.abs()));
    let last = |from: usize| {
        l[from..]
            .iter()
            .chain(&r[from..])
            .fold(0.0f32, |m, s| m.max(s.abs()))
    };
    assert!(last(tail.get()) < 1e-3 * peak, "under -60 dB past the tail");
    assert!(last(tail.get() / 4) > 1e-3 * peak, "not absurdly loose");
    // A bigger room rings longer, a smaller one shorter; the damping does not change it.
    let at = |room: f64, damp: f64| match bank.tail_of(&controls(room, damp)) {
        Tail::Finite(n) => n.get(),
        other => panic!("{other:?}"),
    };
    assert!(at(0.9, 0.5) > at(0.5, 0.5) && at(0.5, 0.5) > at(0.1, 0.5));
    assert_eq!(at(0.5, 0.0), at(0.5, 1.0));
    assert_eq!(
        bank.tail(),
        Tail::Finite(Samples::new(at(1.0, 0.0))),
        "the ceiling is the room at its largest"
    );
    assert_eq!(TAIL_FLOOR, Db::new(-60.0));
}

/// The reported tail covers the ring-out of the worst finite inputs: for the corners of both
/// controls and the General MIDI room, at 48 kHz and 44.1 kHz, an impulse, a full-scale noise
/// burst and a full-scale step (a loop longer than the longest comb, so every line is full)
/// stop, and from the reported tail past the last input frame no frame of either channel is
/// above `TAIL_FLOOR` of the render's own peak (see [`tail_probe`]). The floor is relative to
/// the response's peak because a bank scales its input and the loop builds a step far past it.
#[test]
fn the_reported_tail_covers_the_ring_out_of_the_worst_inputs() {
    let settings = [
        (0.0, 0.0),
        (0.0, 1.0),
        (1.0, 0.0),
        (1.0, 1.0),
        (0.5, 0.5), // FreeverbControls::GENERAL_MIDI
    ];
    assert_eq!(controls(0.5, 0.5), FreeverbControls::GENERAL_MIDI);
    for rate in [SampleRate::SR_48K, SampleRate::SR_44K1] {
        // The right channel's longest comb: past it every line has been round once.
        let period = (1640.0 * rate.get() / 44_100.0).ceil() as usize;
        for (room, damp) in settings {
            let c = controls(room, damp);
            let (mut bank, mut ramps) = settled(rate, &c);
            let Tail::Finite(tail) = bank.tail_of(&c) else {
                panic!("room {room}, damping {damp}: a finite tail")
            };
            for (name, input) in tail_probe::stimuli::<f32>(4 * period) {
                bank.reset();
                ramps.jump_to_target();
                let frames = tail_probe::run_length(input.len(), tail.get(), period);
                let x = tail_probe::padded(&input, frames);
                let (l, r) = run(&mut bank, &mut ramps, &x, 480);
                let what = format!("{rate:?} room {room} damping {damp}, {name}");
                tail_probe::assert_tail_covers(&what, input.len(), tail.get(), &[&l, &r]);
            }
        }
    }
}

/// A room raised to its largest while the first one rings out is covered by the tail a node
/// reports after the move (the larger of what is owed and the new room's tail, from the last
/// input frame, under the ceiling): from it past the last input frame no frame of either channel
/// is above `TAIL_FLOOR` of the render's own peak, after an impulse, a noise burst and a step
/// at the smallest room, the move landing right after the input and a third of the way into its tail. The energy in the
/// combs at the move decays at the new, slower rate from the move on.
#[test]
fn a_room_raised_during_the_ring_out_is_covered_by_the_tail_from_the_last_input() {
    let (short, long) = (controls(0.0, 0.0), controls(1.0, 0.0));
    let period = (1640.0 * RATE.get() / 44_100.0).ceil() as usize;
    let (mut bank, mut ramps) = settled(RATE, &short);
    let (Tail::Finite(was), Tail::Finite(now), Tail::Finite(ceiling)) =
        (bank.tail_of(&short), bank.tail_of(&long), bank.tail())
    else {
        panic!("finite tails")
    };
    for (moved_at, (name, input)) in [0, was.get() / 3].into_iter().flat_map(|at| {
        tail_probe::stimuli::<f32>(4 * period)
            .into_iter()
            .map(move |s| (at, s))
    }) {
        bank.reset();
        ramps.jump_to_target();
        let (head, owed) = (input.len() + moved_at, was.get() - moved_at);
        let reported = tail_probe::raised_tail(owed, now.get(), ceiling.get());
        let (l1, r1) = run(
            &mut bank,
            &mut ramps,
            &tail_probe::padded(&input, head),
            480,
        );
        bank.retarget(&mut ramps, &long);
        let rest = tail_probe::run_length(input.len(), reported, period) - head;
        let (l2, r2) = run(&mut bank, &mut ramps, &vec![0.0; rest], 480);
        let what = format!("{name}, room 0 to 1 at {moved_at} frames after the input");
        tail_probe::assert_raised_tail_covers(
            &what,
            input.len(),
            reported,
            &[&l1, &r1],
            &[&l2, &r2],
        );
        bank.retarget(&mut ramps, &short);
    }
}

/// A room size and a damping that move mid-stream move the combs' feedback and damping over
/// the 20 ms ramp, a chunk at a time, and land on the new figures exactly: each chunk's
/// change is the distance over the steps (20 ms at 48 kHz in chunks of 32 frames is 30), the
/// first is far from the target, and the figure is the target at the last step and not
/// before.
#[test]
fn a_moving_room_ramps_its_combs_a_chunk_at_a_time() {
    let rate = SampleRate::SR_48K;
    let (from_c, to_c) = (controls(0.2, 0.1), controls(0.9, 0.9));
    let (mut bank, mut ramps) = settled(rate, &from_c);
    let x = vec![0.25f32; FreeverbBank::<f32>::CHUNK];
    let chunk = |bank: &mut FreeverbBank<f32>, ramps: &mut FreeverbRamps<f32>| {
        run(bank, ramps, &x, FreeverbBank::<f32>::CHUNK);
        let c = &bank.combs[0][0];
        (c.gain(), c.damping())
    };
    let (from, from_damp) = chunk(&mut bank, &mut ramps);
    assert_eq!(from, f32::from_unit(from_c.feedback()));
    bank.retarget(&mut ramps, &to_c);
    let (to, to_damp) = (f32::from_unit(to_c.feedback()), to_c.damping() as f32);
    let steps = 30;
    let mut last = from;
    for k in 1..=steps {
        let (f, dmp) = chunk(&mut bank, &mut ramps);
        assert!(
            ((f - last) - (to - from) / steps as f32).abs() < 1e-5,
            "step {k}: {last} -> {f}"
        );
        if k == 1 {
            assert!(
                (to - f).abs() > 0.9 * (to - from).abs(),
                "the first chunk is far from the target: {f} of {from} -> {to}"
            );
        }
        if k < steps {
            assert!(f != to && dmp != to_damp, "arrived early at {k}");
        }
        last = f;
    }
    assert_eq!(chunk(&mut bank, &mut ramps), (to, to_damp));
    assert!(from != to && from_damp != to_damp);
}

/// The chunks are counted from the last reset, not from a call's edge: any split of the
/// stream gives the bits of one call, with the room moving through it.
#[test]
fn any_block_size_gives_the_same_bits() {
    let x: Vec<f32> = (0..3_000)
        .map(|n| ((n * 13 + 5) % 100) as f32 / 50.0 - 1.0)
        .collect();
    let render = |block: usize| {
        let (mut bank, mut ramps) = settled(RATE, &controls(0.2, 0.1));
        bank.retarget(&mut ramps, &controls(0.9, 0.7));
        run(&mut bank, &mut ramps, &x, block)
    };
    let whole = render(x.len());
    assert!(whole.0.iter().any(|&s| s != 0.0));
    for block in [1, 7, 31, 32, 33, 64, 100, 255] {
        assert_eq!(render(block), whole, "blocks of {block}");
    }
}

/// A reset empties the lines and restarts the chunks: the response to an impulse is a fresh
/// bank's, bit for bit, though the reset came off a chunk's edge; and a clone continues
/// identically and shares no line.
#[test]
fn a_reset_bank_is_a_fresh_one_and_a_clone_shares_nothing() {
    let c = controls(0.7, 0.3);
    let (mut used, mut ramps) = settled(RATE, &c);
    run(&mut used, &mut ramps, &vec![1.0; 5_017], 100);
    used.reset();
    let (mut fresh, mut fresh_ramps) = settled(RATE, &c);
    let mut ramps = used.ramps();
    used.retarget(&mut ramps, &c);
    ramps.jump_to_target();
    assert_eq!(
        run(&mut used, &mut ramps, &impulse(Samples::new(4_000)), 64),
        run(
            &mut fresh,
            &mut fresh_ramps,
            &impulse(Samples::new(4_000)),
            64
        )
    );
    let mut copy = used.clone();
    let mut copy_ramps = ramps;
    let more = run(&mut used, &mut ramps, &impulse(Samples::new(2_000)), 64);
    assert_eq!(
        run(
            &mut copy,
            &mut copy_ramps,
            &impulse(Samples::new(2_000)),
            64
        ),
        more
    );
    copy.reset();
    assert!(
        run(&mut used, &mut ramps, &vec![0.0; 100], 64)
            .0
            .iter()
            .any(|&s| s != 0.0),
        "the original still rings"
    );
}

/// An input of two channels is their average, so the same signal twice is itself; a block of
/// one output plane is refused, as the wet signal is a pair.
#[test]
fn two_input_channels_are_averaged_and_one_output_plane_is_refused() {
    let x: Vec<f32> = (0..2_000)
        .map(|n| ((n * 7) % 19) as f32 / 9.0 - 1.0)
        .collect();
    let setup = || {
        let (bank, mut ramps) = settled(RATE, &controls(0.3, 0.2));
        bank.retarget(&mut ramps, &controls(0.8, 0.5));
        (bank, ramps)
    };
    let (mut bank, mut ramps) = setup();
    let want = run(&mut bank, &mut ramps, &x, 128);
    // Two channels are averaged: the same signal twice is itself.
    let (mut bank, mut ramps) = setup();
    let twice = [x.as_slice(), x.as_slice()];
    let (mut l2, mut r2) = (vec![0.0f32; x.len()], vec![0.0f32; x.len()]);
    let mut outs: [&mut [f32]; 2] = [&mut l2, &mut r2];
    bank.process(
        &mut ramps,
        &mut InputOutput::paired(
            Planes::new(&twice).unwrap(),
            PlanesMut::new(&mut outs).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!((l2, r2), want);
    // One output plane is refused and left as it was.
    let (mut bank, mut ramps) = setup();
    let one_in = [x.as_slice()];
    let mut left = vec![7.0f32; x.len()];
    let mut outs: [&mut [f32]; 1] = [&mut left];
    let refused = bank.process(
        &mut ramps,
        &mut InputOutput::paired(
            Planes::new(&one_in).unwrap(),
            PlanesMut::new(&mut outs).unwrap(),
        )
        .unwrap(),
    );
    assert_eq!(
        refused,
        Err(BlockMismatch::Width {
            side: BlockSide::Output,
            expected: 2,
            found: 1
        })
    );
    assert!(left.iter().all(|&y| y == 7.0));
}

/// A tail that has run twice its declared length is under -100 dB of the impulse (the
/// declared tail is the fall to -60 dB, and the decay is exponential), and a hostile input
/// (a huge value) stays finite at the extreme room.
#[test]
fn the_tail_falls_far_below_its_floor_and_a_huge_input_stays_finite() {
    let c = controls(1.0, 0.0);
    let (mut bank, mut ramps) = settled(RATE, &c);
    let Tail::Finite(tail) = bank.tail_of(&c) else {
        panic!("finite")
    };
    let (l, r) = run(
        &mut bank,
        &mut ramps,
        &impulse(Samples::new(tail.get() * 3)),
        512,
    );
    assert!(
        l[tail.get() * 2..]
            .iter()
            .chain(&r[tail.get() * 2..])
            .all(|&s| s.abs() < 1e-5)
    );
    let (mut bank, mut ramps) = settled(RATE, &c);
    let loud = vec![1e30f32; 4_000];
    let (l, r) = run(&mut bank, &mut ramps, &loud, 64);
    assert!(l.iter().chain(&r).all(|s| s.is_finite()));
}

/// A rate so low a tuning rounds to no frames (the shortest all-pass, 225, is under half a frame
/// below 98 Hz) still has a line of one frame, so a render returns and is finite; and a rate no
/// device has saturates the line instead of allocating tens of millions of frames.
#[test]
fn an_absurd_rate_neither_hangs_nor_allocates_without_bound() {
    for hz in [1.0f64, 50.0, 97.0] {
        let (mut bank, mut ramps) = settled(SampleRate::of(hz), &FreeverbControls::GENERAL_MIDI);
        let whole = hz.round();
        let scale = |t: usize| ((whole / 44_100.0 * t as f64).round() as usize).max(1);
        for (side, spread) in [(0, 0), (1, 23)] {
            for (c, t) in bank.combs[side].iter().zip(COMBS) {
                assert_eq!(c.delay().get(), scale(t + spread), "{hz} Hz comb");
            }
            for (a, t) in bank.all_passes[side].iter().zip(ALL_PASSES) {
                assert_eq!(a.delay().get(), scale(t + spread), "{hz} Hz all-pass");
            }
        }
        let (l, r) = run(&mut bank, &mut ramps, &impulse(Samples::new(2_000)), 64);
        assert!(l.iter().chain(&r).all(|s| s.is_finite()), "{hz} Hz");
        assert!(l.iter().chain(&r).any(|&s| s != 0.0), "{hz} Hz sounds");
    }
    let bank = FreeverbBank::<f32>::new(SampleRate::of(1.0e9));
    let longest = bank.combs.iter().flatten().map(|c| c.delay().get()).max();
    assert_eq!(longest, Some(1 << 16));
}

/// The Schroeder integral of the render of the General MIDI room at damping 0 falls at the
/// loop decay of its combs: the sum of eight parallel exponentials, each with the RT60
/// `-3 L / (rate log10 g)` of its own length `L`, so the render's RT60 (the -20 to -50 dB
/// fall, doubled) lies between that of the shortest comb (1116 frames) and that of the
/// longest of the right channel (1617 + 23 = 1640), and nothing flushed it short: the curve
/// runs to -90 dB and below. The 5% slack on each side covers the two-point read of the
/// -20 to -50 dB fall on a sum of exponentials.
#[test]
fn the_render_schroeder_rt60_lies_between_the_shortest_and_longest_comb_t60() {
    let c = controls(0.5, 0.0);
    let g = c.feedback().get();
    let comb_t60 = |l: f64| -3.0 * l / (RATE.get() * libm::log10(g));
    let (mut bank, mut ramps) = settled(RATE, &c);
    let frames = (4.0 * comb_t60(1640.0) * RATE.get()) as usize;
    let (l, r) = run(&mut bank, &mut ramps, &impulse(Samples::new(frames)), 512);
    let mut energy = vec![0.0f64; frames];
    let mut total = 0.0;
    for i in (0..frames).rev() {
        total += f64::from(l[i]).powi(2) + f64::from(r[i]).powi(2);
        energy[i] = total;
    }
    let db: Vec<f64> = energy
        .iter()
        .map(|e| 10.0 * libm::log10(e / energy[0]))
        .collect();
    let at = |level: f64| {
        db.iter()
            .position(|&x| x <= level)
            .expect("the curve falls")
    };
    let rt60 = 2.0 * (at(-50.0) - at(-20.0)) as f64 / RATE.get();
    let (shortest, longest) = (comb_t60(1116.0), comb_t60(1640.0));
    assert!(
        rt60 > 0.95 * shortest && rt60 < 1.05 * longest,
        "{rt60} s outside {shortest}..{longest} s"
    );
    assert!(at(-90.0) > at(-50.0), "the decay runs on below -90 dB");
}

/// `x` through an `f64` bank in one call: `(left, right)`.
fn run_wide(
    bank: &mut FreeverbBank<f64>,
    ramps: &mut FreeverbRamps<f64>,
    x: &[f64],
) -> [Vec<f64>; 2] {
    let (mut l, mut r) = (vec![0.0f64; x.len()], vec![0.0f64; x.len()]);
    bank.process(
        ramps,
        &mut InputOutput::paired(
            Planes::new(&[x]).unwrap(),
            PlanesMut::new(&mut [&mut l[..], &mut r[..]]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    [l, r]
}

/// An `f64` comb is its difference equation on the wide signal: `y[n] = v[n - L]`, `s[n] =
/// d2 y[n] + d1 s[n - 1]`, `v[n] = x[n] g_in + g s[n]`, with coefficients `f32` cannot hold,
/// to 2^-45; an `f32` state is off by 1e-8.
#[test]
fn an_f64_comb_is_its_difference_equation() {
    use tutti_fixtures::signal::{assert_close, wide_signal};
    let (l, len) = (7usize, 300usize);
    let (g, d1, g_in) = (
        0.123_456_789_012_345_f64,
        0.234_567_890_123_456_f64,
        0.987_654_321_f64,
    );
    let x = wide_signal(len);
    let (mut v, mut y, mut s) = (vec![0.0f64; len], vec![0.0f64; len], 0.0f64);
    for n in 0..len {
        y[n] = n.checked_sub(l).map_or(0.0, |k| v[k]);
        s = y[n] * (1.0 - d1) + s * d1;
        v[n] = x[n] * g_in + s * g;
    }
    let mut comb = CombFilter::<f64>::new(
        CombForm::Feedback,
        NonZeroUsize::new(l).unwrap(),
        Feedback::NONE,
    );
    comb.retune(g, d1);
    let got: Vec<f64> = x.iter().map(|&x| comb.tick(x * g_in)).collect();
    assert_close(&got, &y, 2.0f64.powi(-45));
}

/// An `f64` Freeverb all-pass is its difference equation on the wide signal: `y[n] = b[n - D]
/// - x[n]`, `b[n] = x[n] + a b[n - D]`, with an `a` `f32` cannot hold, to 2^-45.
#[test]
fn an_f64_all_pass_is_its_difference_equation() {
    use tutti_fixtures::signal::{assert_close, wide_signal};
    let (d, len) = (5usize, 300usize);
    let a = 0.618_033_988_749_894_9_f64;
    let x = wide_signal(len);
    let (mut b, mut y) = (vec![0.0f64; len], vec![0.0f64; len]);
    for n in 0..len {
        let out = n.checked_sub(d).map_or(0.0, |k| b[k]);
        y[n] = out - x[n];
        b[n] = x[n] + a * out;
    }
    let mut ap = AllPassFilter::<f64>::new(NonZeroUsize::new(d).unwrap(), Feedback::new(a))
        .form(AllPassForm::Freeverb);
    let mut got = x.clone();
    ap.process(&mut got);
    assert_close(&got, &y, 2.0f64.powi(-45));
}

/// At 44.1 kHz a unit impulse of the wide signal's first value `x0` is silent until the
/// shortest comb (1116 frames) has gone round, then each of the combs of 1116, 1188 and 1277
/// frames returns `x0 · FIXED_GAIN` before any all-pass echo (the first, at `1116 + 225`) and
/// before the next comb: each of the four all-passes passes a first sample as `-x`, so the
/// four make `+x`. `FIXED_GAIN` is the `f32` unit widened; the product is exact in `f64`.
#[test]
fn an_f64_freeverb_returns_the_fixed_gain_times_the_input_at_each_comb_length() {
    use tutti_fixtures::signal::{assert_close, wide_signal};
    let x0 = wide_signal(1)[0];
    let g = FreeverbBank::<f64>::FIXED_GAIN.get();
    let mut x = vec![0.0f64; 1_300];
    x[0] = x0;
    let bank = FreeverbBank::<f64>::new(SampleRate::SR_44K1);
    let mut ramps = bank.ramps();
    let mut bank = bank;
    bank.retarget(&mut ramps, &controls(0.5, 0.5));
    ramps.jump_to_target();
    let [left, _] = run_wide(&mut bank, &mut ramps, &x);
    let mut want = vec![0.0f64; 1_300];
    for at in [1_116, 1_188, 1_277] {
        want[at] = x0 * g;
    }
    assert_close(&left, &want, 0.0);
}

/// An `f64` Freeverb is linear: `R(a x + b y) = a R(x) + b R(y)` to 2^-40 of the peak, with
/// `a` and `b` and two wide signals that an `f32` state cannot carry (it is off by 2^-24).
#[test]
fn an_f64_freeverb_is_linear() {
    use tutti_fixtures::signal::wide_signal;
    let len = 6_000;
    let x = wide_signal(len);
    let y: Vec<f64> = x.iter().rev().map(|v| v - 0.3).collect();
    let (a, b) = (1.0 / 3.0, -std::f64::consts::PI / 7.0);
    let mix: Vec<f64> = x.iter().zip(&y).map(|(x, y)| a * x + b * y).collect();
    let render = |input: &[f64]| {
        let bank = FreeverbBank::<f64>::new(SampleRate::SR_44K1);
        let mut ramps = bank.ramps();
        let mut bank = bank;
        bank.retarget(&mut ramps, &controls(0.7, 0.3));
        ramps.jump_to_target();
        run_wide(&mut bank, &mut ramps, input)
    };
    let (rx, ry, rm) = (render(&x), render(&y), render(&mix));
    for c in 0..2 {
        let peak = rm[c].iter().fold(0.0f64, |p, v| p.max(v.abs()));
        assert!(peak > 1e-4, "the reverb sounds");
        for n in 0..len {
            let want = a * rx[c][n] + b * ry[c][n];
            assert!(
                (rm[c][n] - want).abs() <= peak * 2.0f64.powi(-40),
                "channel {c}, frame {n}: {} against {want}",
                rm[c][n]
            );
        }
    }
}
