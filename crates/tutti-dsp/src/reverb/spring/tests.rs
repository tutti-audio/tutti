use core::num::NonZeroU16;
use tutti_buffers::{Planes, PlanesMut};
use tutti_rt::SplitMix64;
use tutti_units::{Feedback, Hz, Mix, SampleRate, Seconds, Tail};

use super::*;

const NZ2: NonZeroU16 = NonZeroU16::new(2).unwrap();

const RATE: SampleRate = SampleRate::SR_48K;

/// `x` through a spring of `width` channels, in one call; the channels out.
fn render(spring: &mut SpringReverb<f64>, x: &[&[f64]]) -> Vec<Vec<f64>> {
    let frames = x[0].len();
    let mut out = vec![vec![0.0; frames]; x.len()];
    {
        let mut planes: Vec<&mut [f64]> = out.iter_mut().map(Vec::as_mut_slice).collect();
        spring.process(
            Planes::new(x).unwrap(),
            PlanesMut::new(planes.as_mut_slice()).unwrap(),
        );
    }
    out
}

/// A unit impulse followed by silence, `frames` long.
fn impulse(frames: usize) -> Vec<f64> {
    let mut x = vec![0.0; frames];
    x[0] = 1.0;
    x
}

fn layout(stages: u16, length: f64) -> SpringLayout {
    SpringLayout {
        stages: SpringStages::of(stages),
        length: SpringLength::of(Seconds::new(length)),
    }
}

/// The group delay of `h` at `w` radians per frame, `Re(sum n h[n] e^-jwn / sum h[n] e^-jwn)`:
/// `-d arg H / d w` for a causal `h`, without unwrapping a phase.
fn group_delay(h: &[f64], w: f64) -> f64 {
    let (mut re, mut im, mut nre, mut nim) = (0.0, 0.0, 0.0, 0.0);
    for (n, &x) in h.iter().enumerate() {
        let (s, c) = (-w * n as f64).sin_cos();
        re += x * c;
        im += x * s;
        nre += n as f64 * x * c;
        nim += n as f64 * x * s;
    }
    (nre * re + nim * im) / (re * re + im * im)
}

/// The group delay of one stretched all-pass of coefficient `g` and stretch `k`:
/// `k (1 - g^2) / (1 + g^2 - 2 g cos(w k))` frames.
fn stage_delay(g: f64, k: usize, w: f64) -> f64 {
    let k = k as f64;
    k * (1.0 - g * g) / (1.0 + g * g - 2.0 * g * (w * k).cos())
}

/// A cascade of `m` stretched all-passes of coefficient `g`: its group delay at `w` is the sum
/// of the first-order all-passes' (`m` times one stage's), from the phase of
/// `(z^-K - g) / (1 - g z^-K)`. The impulse response is cut at 8000 frames, where it is under
/// `g^(8000/K)` ~ 1e-440 of its peak, so the sums carry only rounding.
#[test]
fn the_cascades_group_delay_is_the_sum_of_the_stages() {
    let k = STRETCH.get();
    for (m, g) in [(6u16, 0.6), (9, 0.3), (1, 0.0)] {
        let mut stages = cascade::<f64>(SpringStages::of(m), Feedback::new(g));
        let mut h = impulse(8000);
        for stage in &mut stages {
            stage.process(&mut h);
        }
        for w in [0.0, 0.05, 0.4, core::f64::consts::PI / k as f64, 2.0, 3.1] {
            let want = f64::from(m) * stage_delay(g, k, w);
            let got = group_delay(&h, w);
            assert!(
                (got - want).abs() <= 1e-9 * want.max(1.0),
                "m {m} g {g} w {w}: {got} against {want}"
            );
        }
    }
}

/// The chirp: for `g > 0` the delay is greatest at DC, `m k (1 + g) / (1 - g)`, and least where
/// `w k = pi`, `m k (1 - g) / (1 + g)`, so a low frequency arrives after a high one.
#[test]
fn a_low_frequency_arrives_after_a_high_one() {
    let (m, g, k) = (6u16, 0.6, STRETCH.get());
    let mut stages = cascade::<f64>(SpringStages::of(m), Feedback::new(g));
    let mut h = impulse(8000);
    for stage in &mut stages {
        stage.process(&mut h);
    }
    let mk = f64::from(m) * k as f64;
    let dc = group_delay(&h, 0.0);
    let high = group_delay(&h, core::f64::consts::PI / k as f64);
    assert!((dc - mk * (1.0 + g) / (1.0 - g)).abs() < 1e-8, "{dc}");
    assert!((high - mk * (1.0 - g) / (1.0 + g)).abs() < 1e-8, "{high}");
    assert!(dc > 3.0 * high);
}

/// With no dispersion and an open tone the spring is a pure loop of `N + M K` frames and the
/// gain `g`: the response is `g^k` at frame `M K + k (N + M K)` and 0 elsewhere. At a decay
/// of ten trips, `g^10 = 10^-3`: 60 dB down in the decay time. Here `N = 480`, `M K = 20`, a
/// trip of 500 frames and a decay of 5000 frames.
#[test]
fn at_zero_dispersion_the_echoes_fall_sixty_decibels_in_the_decay_time() {
    let mut spring = SpringReverb::<f64>::new(NonZeroU16::MIN, RATE, layout(5, 0.01));
    spring.retarget(&SpringControls {
        decay: Seconds::new(5000.0 / 48_000.0),
        tone: Hz::new(1.0e6),
        dispersion: Mix::DRY,
    });
    spring.jump_to_target();
    let y = &render(&mut spring, &[&impulse(5600)])[0];
    let first = 20;
    let trip = 500;
    let ratio = y[first + trip];
    assert!((y[first] - 1.0).abs() < 1e-12);
    for (n, &got) in y.iter().enumerate() {
        let echo = n >= first && (n - first) % trip == 0;
        let want = if echo {
            ratio.powi(((n - first) / trip) as i32)
        } else {
            0.0
        };
        assert!(
            (got - want).abs() < 1e-12,
            "frame {n}: {got} against {want}"
        );
    }
    assert!(
        (y[first + 10 * trip] - 1.0e-3).abs() < 1e-9,
        "{}",
        y[first + 10 * trip]
    );
}

/// Every response is bounded by `sum g^k`: the loop is `sum_k (g T z^-N)^k P^(k+1)` and each
/// term is at most `g^k` in L2 for the unit impulse (`|P| <= 1`, `|T| <= 1`), so the
/// energy is at most `1 / (1 - g)^2`, over decays, tones and dispersions; the gain is under
/// the [`Feedback`] ceiling for every one, the longest decay included.
#[test]
fn the_loop_gain_is_below_one_and_the_response_is_bounded_for_every_config() {
    let spring = SpringReverb::<f64>::new(NonZeroU16::MIN, RATE, layout(8, 0.02));
    for decay in [0.1, 0.5, 2.0, 60.0] {
        for tone in [20.0, 200.0, 4000.0, 1.0e6] {
            for dispersion in [0.0, 0.5, 1.0] {
                let c = SpringControls {
                    decay: Seconds::new(decay),
                    tone: Hz::new(tone),
                    dispersion: Mix::new(dispersion),
                };
                let g = spring.loop_gain(&c).get();
                assert!(g > 0.0 && g <= 0.99, "{c:?}: {g}");
                if decay > 2.0 {
                    continue;
                }
                let mut s = SpringReverb::<f64>::new(NonZeroU16::MIN, RATE, layout(8, 0.02));
                s.retarget(&c);
                s.jump_to_target();
                let frames = (4.0 * decay * 48_000.0) as usize;
                let y = &render(&mut s, &[&impulse(frames)])[0];
                let energy: f64 = y.iter().map(|v| v * v).sum();
                let bound = 1.0 / ((1.0 - g) * (1.0 - g));
                assert!(energy <= bound, "{c:?}: {energy} over {bound}");
            }
        }
    }
}

/// The reported tail covers the ring-out: past it the impulse response is under `TAIL_FLOOR`
/// (-60 dB) of its peak, and it is no more than the decay and three trips long, the trip
/// `N + M K (1 + g) / (1 - g) + p / (1 - p)` of the low cascade at full dispersion.
#[test]
fn the_tail_covers_the_ring_out_and_is_no_longer_than_the_decay_and_three_trips() {
    let lay = layout(40, 0.04);
    let c = SpringControls::DEFAULT;
    let mut spring = SpringReverb::<f64>::new(NonZeroU16::MIN, RATE, lay);
    spring.retarget(&c);
    spring.jump_to_target();
    let Tail::Finite(tail) = spring.tail_of(&c) else {
        panic!("a spring's tail is finite");
    };
    let tail = tail.get();
    let y = &render(&mut spring, &[&impulse(tail + 8000)])[0];
    let peak = y.iter().fold(0.0f64, |p, v| p.max(v.abs()));
    let late = y[tail..].iter().fold(0.0f64, |p, v| p.max(v.abs()));
    assert!(late <= 1.0e-3 * peak, "{late} past the tail, peak {peak}");
    let pole = (-core::f64::consts::TAU * 4000.0 / 48_000.0).exp();
    let g = LOW_COEFFICIENT;
    let trip = 1920.0 + 160.0 * (1.0 + g) / (1.0 - g) + pole / (1.0 - pole);
    assert!(tail as f64 <= 2.0 * 48_000.0 + 3.0 * trip, "{tail}");
}

/// Each channel is its own spring: input on one channel leaves the other exactly silent.
#[test]
fn a_channel_with_no_input_stays_silent() {
    let mut spring = SpringReverb::<f64>::new(NZ2, RATE, layout(8, 0.02));
    let silent = vec![0.0; 4000];
    let y = render(&mut spring, &[&impulse(4000), &silent]);
    assert!(y[0].iter().any(|v| *v != 0.0));
    assert!(y[1].iter().all(|v| *v == 0.0));
}

/// In the pure loop (no dispersion, open tone) the echoes are `10^(-0.3 k)` at frame
/// `20 + 500 k`, so the last one above the floor (-60 dB, 1e-3) is `k = 9`, at frame 4520: the
/// tail is past it, and past it every frame is at or under the floor.
#[test]
fn the_tail_of_a_pure_loop_is_past_the_last_echo_over_the_floor() {
    let mut spring = SpringReverb::<f64>::new(NonZeroU16::MIN, RATE, layout(5, 0.01));
    let c = SpringControls {
        decay: Seconds::new(5000.0 / 48_000.0),
        tone: Hz::new(1.0e6),
        dispersion: Mix::DRY,
    };
    spring.retarget(&c);
    spring.jump_to_target();
    let Tail::Finite(tail) = spring.tail_of(&c) else {
        panic!("a spring's tail is finite");
    };
    let tail = tail.get();
    assert!(tail > 20 + 9 * 500, "{tail}");
    let y = &render(&mut spring, &[&impulse(tail + 2000)])[0];
    assert!(y[tail..].iter().all(|v| v.abs() <= 1.0e-3 + 1e-9));
}

/// The pure loop at a trip of 260 frames (a 240-frame loop and a cascade of 20) with the decay
/// stepped from 0.1 s to 0.2 s at frame 1024, a chunk start. The loop gain is
/// `g(t) = 10^(-3 * 260 / (48000 t))` and glides over the 30 chunks of 20 ms, step `s` of the
/// chunks after the move (the first reads step 1): `g = g0 + (g1 - g0) min(s, 30) / 30`. The
/// echo `k` is the one before it times the gain of the chunk its trip starts in, `260 k / 32`,
/// so the echoes follow the products of the glided gains, not of either end.
#[test]
fn a_decay_step_glides_the_loop_gain_over_thirty_chunks() {
    let mut spring = SpringReverb::<f64>::new(NonZeroU16::MIN, RATE, layout(5, 0.005));
    let controls = |decay: f64| SpringControls {
        decay: Seconds::new(decay),
        tone: Hz::new(1.0e6),
        dispersion: Mix::DRY,
    };
    spring.retarget(&controls(0.1));
    spring.jump_to_target();
    let x = impulse(2200);
    let mut y = render(&mut spring, &[&x[..1024]]).remove(0);
    spring.retarget(&controls(0.2));
    y.extend(render(&mut spring, &[&x[1024..]]).remove(0));
    let gain = |decay: f64| 10f64.powf(-3.0 * 260.0 / (48_000.0 * decay));
    let (g0, g1) = (gain(0.1), gain(0.2));
    let mut want = 1.0;
    for k in 1..=7usize {
        let chunk = 260 * k / 32;
        let steps = (chunk as i64 - 31).clamp(0, 30) as f64;
        want *= g0 + (g1 - g0) * steps / 30.0;
        let got = y[20 + 260 * k];
        assert!((got - want).abs() < 1e-12, "echo {k}: {got} against {want}");
    }
}

/// The glide is counted in chunks from the last reset, not in calls: one stream rendered in
/// calls of 7, 32, 50, 96 and 4096 frames, with a design move at frame 1024 in each, is the same
/// stream to the bit, over noise and the default dispersion, tone and decay.
#[test]
fn the_render_is_the_same_at_every_block_split() {
    let mut rng = SplitMix64::new(7);
    let x: Vec<f64> = (0..4096)
        .map(|_| f64::from_fraction_bits(rng.next_u64()) - 0.5)
        .collect();
    let moved = SpringControls {
        decay: Seconds::new(0.7),
        tone: Hz::new(900.0),
        dispersion: Mix::new(0.3),
    };
    let run = |block: usize| {
        let mut spring = SpringReverb::<f64>::new(NonZeroU16::MIN, RATE, layout(8, 0.02));
        spring.retarget(&SpringControls::DEFAULT);
        spring.jump_to_target();
        let mut y = Vec::new();
        for (from, to) in [(0, 1024), (1024, 4096)] {
            if from > 0 {
                spring.retarget(&moved);
            }
            for piece in x[from..to].chunks(block) {
                y.extend(render(&mut spring, &[piece]).remove(0));
            }
        }
        y
    };
    let whole = run(4096);
    assert_eq!(whole, run(32));
    assert_eq!(whole, run(96));
    assert_eq!(whole, run(50));
    assert_eq!(whole, run(7));
}

/// Every channel of a three-wide spring is a spring: an impulse on each of three channels rings
/// on each of them.
#[test]
fn every_channel_of_a_three_wide_spring_rings() {
    let mut spring = SpringReverb::<f64>::new(NonZeroU16::new(3).unwrap(), RATE, layout(8, 0.02));
    let x = impulse(4000);
    let y = render(&mut spring, &[&x, &x, &x]);
    for (c, ch) in y.iter().enumerate() {
        assert!(ch.iter().any(|v| *v != 0.0), "channel {c} is silent");
    }
}
