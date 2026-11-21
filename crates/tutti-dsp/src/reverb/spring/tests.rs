use tutti_buffers::{ChannelLayout, Planes, PlanesMut};
use tutti_units::{Feedback, Hz, Mix, SampleRate, Seconds, Tail};

use super::*;

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
    let k = STRETCH;
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
    let (m, g, k) = (6u16, 0.6, STRETCH);
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
    let mut spring = SpringReverb::<f64>::new(ChannelLayout::MONO, RATE, layout(5, 0.01));
    spring.retarget(&SpringControls {
        decay: Seconds::new(5000.0 / 48_000.0),
        tone: Hz::new(1.0e6),
        dispersion: Mix::DRY,
    });
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
    let spring = SpringReverb::<f64>::new(ChannelLayout::MONO, RATE, layout(8, 0.02));
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
                let mut s = SpringReverb::<f64>::new(ChannelLayout::MONO, RATE, layout(8, 0.02));
                s.retarget(&c);
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
    let mut spring = SpringReverb::<f64>::new(ChannelLayout::MONO, RATE, lay);
    spring.retarget(&c);
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
    let mut spring = SpringReverb::<f64>::new(ChannelLayout::STEREO, RATE, layout(8, 0.02));
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
    let mut spring = SpringReverb::<f64>::new(ChannelLayout::MONO, RATE, layout(5, 0.01));
    let c = SpringControls {
        decay: Seconds::new(5000.0 / 48_000.0),
        tone: Hz::new(1.0e6),
        dispersion: Mix::DRY,
    };
    spring.retarget(&c);
    let Tail::Finite(tail) = spring.tail_of(&c) else {
        panic!("a spring's tail is finite");
    };
    let tail = tail.get();
    assert!(tail > 20 + 9 * 500, "{tail}");
    let y = &render(&mut spring, &[&impulse(tail + 2000)])[0];
    assert!(y[tail..].iter().all(|v| v.abs() <= 1.0e-3 + 1e-9));
}
