use tutti_buffers::{Planes, PlanesMut};
use tutti_fixtures::signal::impulse;
use tutti_units::Samples;
use tutti_units::{Bipolar, Hz, Mix, SampleRate, Seconds, Tail};

use super::*;
use crate::reverb::network::NetworkStage;
use crate::tail_probe;
use tutti_buffers::InputOutput;

// The tests other than the `f64` ones pin the `f32` instantiation, the session's audio
// format, so their tolerances are `f32`'s.

/// Controls with no modulation and no damping: the tank is a pure loop of unit-magnitude
/// all-passes and delays.
fn still(decay: f64) -> PlateControls {
    PlateControls {
        decay: Seconds::new(decay),
        damping: Mix::new(0.0),
        modulation: Bipolar::of(0.0),
        ..PlateControls::DEFAULT
    }
}

/// A bank at `rate`, at rest on `c`.
fn at_rest(rate: SampleRate, c: &PlateControls) -> (PlateBank<f32>, PlateRamps<f32>) {
    let mut bank = PlateBank::<f32>::new(rate);
    let mut ramps = bank.ramps();
    bank.retarget(&mut ramps, c);
    ramps.jump_to_target();
    (bank, ramps)
}

/// `x` through the bank in calls of `block` frames; the two output channels.
fn run(
    bank: &mut PlateBank<f32>,
    ramps: &mut PlateRamps<f32>,
    x: &[f32],
    block: usize,
) -> [Vec<f32>; 2] {
    let (mut l, mut r) = (vec![0.0f32; x.len()], vec![0.0f32; x.len()]);
    let mut at = 0;
    for piece in x.chunks(block) {
        let end = at + piece.len();
        bank.process(
            ramps,
            &mut InputOutput::paired(
                Planes::new(&[piece]).unwrap(),
                PlanesMut::new(&mut [&mut l[at..end], &mut r[at..end]]).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        at = end;
    }
    [l, r]
}

/// The impulse response of `c` at `rate`, `frames` long.
fn response(rate: SampleRate, c: &PlateControls, frames: usize) -> [Vec<f32>; 2] {
    let (mut bank, mut ramps) = at_rest(rate, c);
    run(&mut bank, &mut ramps, &impulse(Samples::new(frames)), 480)
}

/// The Schroeder backward integral of `h` squared, in dB below its total.
fn schroeder_db(h: &[f32]) -> Vec<f64> {
    let mut total = 0.0;
    let mut e = vec![0.0; h.len()];
    for (i, &x) in h.iter().enumerate().rev() {
        total += f64::from(x) * f64::from(x);
        e[i] = total;
    }
    let all = e[0];
    e.iter().map(|x| 10.0 * (x / all).log10()).collect()
}

/// The RT60 of a decay curve in dB at `rate`: the least-squares line through the curve from
/// -5 dB to -35 dB (a T30 fit), its slope extended to 60 dB.
fn rt60_t30(curve: &[f64], rate: SampleRate) -> f64 {
    let from = curve.iter().position(|&x| x <= -5.0).unwrap();
    let to = curve.iter().position(|&x| x <= -35.0).unwrap();
    let n = (to - from + 1) as f64;
    let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
    for (i, &y) in curve.iter().enumerate().take(to + 1).skip(from) {
        let x = i as f64 / rate.get();
        sx += x;
        sy += y;
        sxx += x * x;
        sxy += x * y;
    }
    let slope = (n * sxy - sx * sy) / (n * sxx - sx * sx);
    -60.0 / slope
}

/// Every lattice a plain delay, no modulation, no damping.
fn pure(decay: f64) -> PlateControls {
    PlateControls {
        diffusion: Mix::new(0.0),
        ..still(decay)
    }
}

/// With the diffusion at 0, no damping and no modulation every lattice is a plain delay, the
/// loop is `T` frames at the gain `g`, and the Schroeder-integrated RT60 (a T30 fit) of the
/// rendered impulse response is `-3 T / log10 g` within 1%, at two rates and two decays. (With
/// damping each half's one-pole adds `p / (1 - p)` frames, and with diffusion the decay is a
/// mixture over frequency: the closed form is exact only under these conditions.)
///
/// The 1% is the fit's own error: the response is an impulse train, and the integral and the
/// least-squares line over 30 dB of it can differ from the underlying slope by the part of a
/// trip the fit starts and ends in. Measured: 0.26% at every rate and decay.
#[test]
fn at_diffusion_zero_the_rendered_rt60_is_minus_three_loop_over_log10_g() {
    for rate in [SampleRate::SR_44K1, SampleRate::SR_96K] {
        for decay in [2.0, 6.0] {
            let c = pure(decay);
            let want = decay; // the design maps it to the closed form's `c`
            let frames = (rate.get() * decay * 1.6) as usize;
            let [l, _] = response(rate, &c, frames);
            let got = rt60_t30(&schroeder_db(&l), rate);
            assert!(
                (got - want).abs() <= 0.01 * want,
                "{rate:?} decay {decay}: measured {got}, closed form {want}"
            );
        }
    }
}

/// The two outputs are decorrelated: the normalized cross-correlation of the two channels of
/// the impulse response, over every lag within the longest tap offset, peaks under 0.3 at 44.1
/// and 48 kHz.
///
/// Both outputs are sums of seven equal taps of the same tank, so where one tap of the left
/// lines up with one of the right, `1/7` of the energy correlates (0.143); the offsets of table 2
/// are distinct, so each pair of taps has its own lag. The taps also read buffers whose signals
/// are delayed copies of one another round the loop, which lines up further pairs at one lag:
/// that raises the measurement above one pair (0.216 at 48 kHz, 0.20 at 44.1 kHz). 0.3 is a
/// stated limit (two pairs are 0.286, the rest adds noise of `1/sqrt(window)` = 0.006), not a
/// derived figure.
#[test]
fn the_two_outputs_cross_correlation_peak_is_under_0_3() {
    let window = 24_000;
    for rate in [SampleRate::SR_44K1, SampleRate::SR_48K] {
        let [l, r] = response(rate, &still(3.0), window + 8_000);
        let (l, r): (Vec<f64>, Vec<f64>) = (
            l.iter().map(|&x| f64::from(x)).collect(),
            r.iter().map(|&x| f64::from(x)).collect(),
        );
        let energy = |x: &[f64]| x[..window].iter().map(|v| v * v).sum::<f64>();
        let norm = (energy(&l) * energy(&r)).sqrt();
        let reach = (3627.0 * rate.get() / 29_761.0) as isize;
        let mut peak = 0.0f64;
        for lag in -reach..=reach {
            let sum: f64 = (0..window as isize)
                .filter(|&i| i + lag >= 0)
                .map(|i| l[i as usize] * r[(i + lag) as usize])
                .sum();
            peak = peak.max((sum / norm).abs());
        }
        assert!(peak < 0.3, "{rate:?}: cross-correlation peak {peak}");
    }
}

/// Nothing sounds before the pre-delay and the earliest output tap (266 frames at 29761 Hz)
/// have gone by, and the first sound is then: the sound enters the tank through a lattice
/// with a direct term.
#[test]
fn the_first_sound_is_the_pre_delay_plus_the_earliest_tap() {
    let rate = SampleRate::SR_48K;
    let c = PlateControls {
        pre_delay: Seconds::new(0.01),
        ..still(2.0)
    };
    let [l, r] = response(rate, &c, 4_000);
    let first = |y: &[f32]| y.iter().position(|&x| x != 0.0).unwrap();
    let tap = (266.0f64 * 48_000.0 / 29_761.0).round() as usize;
    assert_eq!(first(&l), 480 + tap);
    assert!(first(&r) > 480 + tap);
}

/// The reported tail covers the impulse response: past it nothing is above the floor below the
/// response's peak, and the tail never exceeds the ceiling, at 48 and 44.1 kHz. The sets include
/// the worst corners of what the tail adds to the network's own: damping 0.999 (each half's
/// one-pole adds `p / (1 - p)` = 999 frames to the loop, more than the bound's slack at diffusion
/// 0) and bandwidth 10 Hz (the input's one-pole rings on for thousands of frames).
#[test]
fn the_tail_covers_the_response_falling_under_the_floor() {
    let floor = 10f64.powf(crate::TAIL_FLOOR.get() / 20.0);
    for rate in [SampleRate::SR_48K, SampleRate::SR_44K1] {
        // (decay, damping, bandwidth, diffusion, pre-delay)
        for (decay, damping, bandwidth, diffusion, pre) in [
            (1.0, 0.0, 20_000.0, 1.0, 0.0),
            (4.0, 0.3, 20_000.0, 1.0, 0.2),
            (8.0, 0.0005, 20_000.0, 1.0, 0.2),
            (3.0, 0.999, 10.0, 0.0, 0.0),
            (3.0, 0.999, 10.0, 1.0, 0.0),
            (8.0, 0.999, 10.0, 0.0, 0.0),
            (8.0, 0.5, 20_000.0, 0.0, 0.0),
        ] {
            let c = PlateControls {
                damping: Mix::new(damping),
                bandwidth: Hz::new(bandwidth),
                diffusion: Mix::new(diffusion),
                pre_delay: Seconds::new(pre),
                ..still(decay)
            };
            let (mut bank, mut ramps) = at_rest(rate, &c);
            let (Tail::Finite(tail), Tail::Finite(ceiling)) =
                (bank.tail_of(&ramps, &c), bank.tail())
            else {
                panic!("finite tails")
            };
            assert!(tail <= ceiling);
            let [l, r] = run(
                &mut bank,
                &mut ramps,
                &impulse(Samples::new(tail.get() + 48_000)),
                512,
            );
            let peak = l.iter().chain(&r).fold(0.0f32, |m, x| m.max(x.abs()));
            let late = (l[tail.get()..].iter().chain(&r[tail.get()..]))
                .fold(0.0f32, |m, x| m.max(x.abs()));
            assert!(
                f64::from(late) <= floor * f64::from(peak),
                "{c:?}: {late} past the tail {tail:?}, peak {peak}"
            );
        }
    }
}

/// The reported tail covers the ring-out of the worst finite inputs: for the shortest, the
/// default and the longest decay, damping from none to its largest pole, the narrowest bandwidth
/// and the default, diffusion 0 and 1, no pre-delay and the longest, and no modulation and the
/// deepest, at 8 kHz (the default also at 48 kHz and 44.1 kHz), an impulse, a full-scale noise
/// burst and a full-scale step (twice the tank's loop, so every line is full) stop, and from the
/// reported tail past the last input frame no frame of either channel is above `TAIL_FLOOR` of
/// the render's own peak (see [`tail_probe`]). The floor is relative to the response's peak: the
/// output taps sum the tank's lines, and a step is built up by the loop far past the input.
#[test]
fn the_reported_tail_covers_the_ring_out_of_the_worst_inputs() {
    use crate::MIN_DECAY;
    use crate::reverb::fdn::design::MAX_PRE_DELAY;
    use crate::reverb::plate::design::MAX_PLATE_DECAY;
    // The longest decay a control reaches runs once; the rest of the long
    // settings are 0.5 s, the same tank at a sixteenth of the render; the 8 s one runs at
    // 22.05 kHz only (the tank's lines scale with the rate).
    let long = PlateControls {
        decay: Seconds::new(0.5),
        damping: Mix::new(0.0),
        pre_delay: MAX_PRE_DELAY,
        modulation: Bipolar::of(0.5),
        ..PlateControls::DEFAULT
    };
    let eight = PlateControls {
        decay: MAX_PLATE_DECAY,
        ..long
    };
    let cases: [(&str, PlateControls); 9] = [
        ("default", PlateControls::DEFAULT),
        (
            "shortest",
            PlateControls {
                decay: MIN_DECAY,
                ..PlateControls::DEFAULT
            },
        ),
        ("longest", long),
        (
            "longest, plain delays",
            PlateControls {
                diffusion: Mix::new(0.0),
                modulation: Bipolar::of(0.0),
                pre_delay: Seconds::new(0.0),
                ..long
            },
        ),
        (
            "longest, damping 0.999, bandwidth 10 Hz",
            PlateControls {
                damping: Mix::new(0.999),
                bandwidth: Hz::new(10.0),
                ..long
            },
        ),
        (
            "longest, deepest modulation",
            PlateControls {
                modulation: Bipolar::of(1.0),
                pre_delay: Seconds::new(0.0),
                ..long
            },
        ),
        (
            "plain delays, damping 0.999",
            PlateControls {
                damping: Mix::new(0.999),
                diffusion: Mix::new(0.0),
                modulation: Bipolar::of(0.0),
                ..long
            },
        ),
        (
            "shortest, bandwidth 10 Hz",
            PlateControls {
                decay: MIN_DECAY,
                bandwidth: Hz::new(10.0),
                ..PlateControls::DEFAULT
            },
        ),
        (
            "shortest, longest pre-delay",
            PlateControls {
                decay: MIN_DECAY,
                pre_delay: MAX_PRE_DELAY,
                ..PlateControls::DEFAULT
            },
        ),
    ];
    let check = |name: &str, c: &PlateControls, rate: SampleRate| {
        let burst = 2 * crate::reverb::plate::design::loop_frames(rate).ceil() as usize;
        let (mut bank, mut ramps) = at_rest(rate, c);
        let Tail::Finite(tail) = bank.tail_of(&ramps, c) else {
            panic!("{name}: a finite tail")
        };
        for (input_name, input) in tail_probe::stimuli::<f32>(burst) {
            bank.reset();
            let frames = tail_probe::run_length(input.len(), tail.get(), burst / 2);
            let y = run(
                &mut bank,
                &mut ramps,
                &tail_probe::padded(&input, frames),
                512,
            );
            let what = format!("{rate:?} {name}, {input_name}");
            tail_probe::assert_tail_covers(&what, input.len(), tail.get(), &[&y[0], &y[1]]);
        }
    };
    // The whole grid runs at 8 kHz (the tank's lines scale with the rate); the default also runs
    // at 48 kHz and 44.1 kHz, where the lengths round differently.
    for (name, c) in cases {
        check(name, &c, SampleRate::of(8_000.0));
    }
    check("8 s", &eight, SampleRate::of(8_000.0));
    for rate in [SampleRate::SR_48K, SampleRate::SR_44K1] {
        check("default", &PlateControls::DEFAULT, rate);
    }
}

/// A decay raised to its longest while the short one rings out is covered by the tail a node
/// reports after the move (the larger of what is owed and the long tail, from the last input
/// frame, under the ceiling): from it past the last input frame no frame of either channel is
/// above `TAIL_FLOOR` of the render's own peak, after an impulse, a noise burst and a step, the
/// move landing right after the input and a third of the way into the short tail. The energy in
/// the tank at the move decays at the new, slower rate from the move on.
#[test]
fn a_decay_raised_during_the_ring_out_is_covered_by_the_tail_from_the_last_input() {
    use crate::reverb::plate::design::MAX_PLATE_DECAY;
    let rate = SampleRate::SR_48K;
    let short = PlateControls {
        decay: Seconds::new(0.3),
        ..PlateControls::DEFAULT
    };
    let long = PlateControls {
        decay: MAX_PLATE_DECAY,
        ..short
    };
    let burst = 2 * crate::reverb::plate::design::loop_frames(rate).ceil() as usize;
    let (mut bank, mut ramps) = at_rest(rate, &short);
    let (Tail::Finite(was), Tail::Finite(now), Tail::Finite(ceiling)) = (
        bank.tail_of(&ramps, &short),
        bank.tail_of(&ramps, &long),
        bank.tail(),
    ) else {
        panic!("finite tails")
    };
    for (moved_at, (name, input)) in [0, was.get() / 3].into_iter().flat_map(|at| {
        tail_probe::stimuli::<f32>(burst)
            .into_iter()
            .map(move |s| (at, s))
    }) {
        bank.reset();
        bank.retarget(&mut ramps, &short);
        ramps.jump_to_target();
        let head = input.len() + moved_at;
        let reported = tail_probe::raised_tail(was.get() - moved_at, now.get(), ceiling.get());
        let first = run(
            &mut bank,
            &mut ramps,
            &tail_probe::padded(&input, head),
            512,
        );
        bank.retarget(&mut ramps, &long);
        let rest = tail_probe::run_length(input.len(), reported, burst / 2) - head;
        let second = run(&mut bank, &mut ramps, &vec![0.0; rest], 512);
        tail_probe::assert_raised_tail_covers(
            &format!("{name}, decay 0.3 s to 8 s at {moved_at} frames after the input"),
            input.len(),
            reported,
            &[&first[0], &first[1]],
            &[&second[0], &second[1]],
        );
    }
}

/// A lattice's impulse response: its direct term is `-g` and its energy is 1 (it is an
/// all-pass), for the coefficients and delays the design gives the first input diffuser (input
/// diffusion 1, `+0.75`) and the first decay diffusion 1 lattice (`-0.70`): pins the sign
/// convention, `H(z) = (z^-D - g) / (1 - g z^-D)`.
#[test]
fn a_lattice_has_the_direct_term_minus_g_and_unit_energy() {
    let rate = SampleRate::SR_48K;
    let mut network = Box::new(Network::new(rate));
    PlateControls::DEFAULT.design_into(rate, &mut network);
    let mut stages = network.stages.iter();
    let (Some(NetworkStage::AllPass(input)), Some(NetworkStage::Fdn(tank))) = (
        stages.next(),
        network
            .stages
            .iter()
            .find(|s| matches!(s, NetworkStage::Fdn(_))),
    ) else {
        panic!("an input diffuser and the tank")
    };
    let decay1 = tank.lines[0].diffuser.unwrap();
    for (what, design, direct) in [("input 1", input[0], -0.75), ("decay 1", decay1, 0.70)] {
        // The lattice under test runs in `f32`: the coefficient narrows once.
        let (d, g) = (design.delay.get() as usize, design.gain.get() as f32);
        let mut ring = line(Samples::new(d));
        let y: Vec<f32> = (0..80 * d)
            .map(|n| {
                let y = lattice(
                    &mut ring,
                    if n == 0 { 1.0 } else { 0.0 },
                    g,
                    ReadPosition::from_frames(Samples::new(d)),
                );
                ring.advance();
                y
            })
            .collect();
        let energy: f64 = y.iter().map(|&v| f64::from(v).powi(2)).sum();
        assert!((y[0] - direct).abs() < 1e-6, "{what}: direct term {}", y[0]);
        assert!((energy - 1.0).abs() < 1e-5, "{what}: energy {energy}");
    }
}

/// The modulated delays sweep as the paper's excursion says. At diffusion 0 the tank's first
/// lattices are plain delays read at `D + m(t)`, so an impulse put in at frame `T` arrives at
/// `T + (input delays) + D + m(t) + tap` and the arrival's centroid gives `m(t)`. At depth 1,
/// 1 Hz, it is `A sin(2 pi t)` for the left half (heard on the right output, earliest tap 353)
/// and `A cos(2 pi t)` for the right (left output, tap 266), `A = 16 rate / 29761` = 26 frames,
/// within 0.1 frame at eight times across the period: a zero sweep, another rate or the other
/// quarter turn misses by many frames.
#[test]
fn the_modulated_delays_sweep_the_papers_excursion_in_quadrature() {
    let rate = SampleRate::SR_48K;
    let fs = rate.get();
    let a = 16.0 * fs / 29_761.0;
    let scaled = |n: u32| (f64::from(n) * fs / 29_761.0).round();
    let c = PlateControls {
        diffusion: Mix::new(0.0),
        modulation: Bipolar::of(1.0),
        bandwidth: Hz::new(1.0e9),
        ..still(2.0)
    };
    // The bandwidth one-pole spreads an impulse: its mean delay is `p / (1 - p)`.
    let pole = (-std::f64::consts::TAU * c.bandwidth_at(rate).get() / fs).exp();
    let smear = pole / (1.0 - pole);
    let input: f64 = [142, 107, 379, 277].into_iter().map(scaled).sum();
    // (output channel, the half it hears, that half's first lattice, the tap, the quadrature)
    let halves: [(usize, f64, f64, fn(f64) -> f64); 2] = [
        (1, scaled(672), scaled(353), f64::sin),
        (0, scaled(908), scaled(266), f64::cos),
    ];
    for k in 0..8 {
        let start = k * 6_000;
        let (mut bank, mut ramps) = at_rest(rate, &c);
        let mut x = vec![0.0f32; start + 4_000];
        x[start] = 1.0;
        let out = run(&mut bank, &mut ramps, &x, 480);
        for (channel, delay, tap, wave) in halves {
            let y = &out[channel];
            let n0 = y.iter().position(|&v| v != 0.0).unwrap();
            let (sum, moment) = (0..8).fold((0.0f64, 0.0f64), |(s, m), i| {
                let v = f64::from(y[n0 + i]);
                (s + v, m + v * i as f64)
            });
            let measured = n0 as f64 + moment / sum - smear - start as f64 - input - delay - tap;
            // The read happens at `start + input + delay + m`: find `m` there.
            let mut m = 0.0;
            for _ in 0..3 {
                let t = (start as f64 + input + delay + m) / fs;
                m = a * wave(std::f64::consts::TAU * t);
            }
            assert!(
                (measured - m).abs() < 0.1,
                "channel {channel}, frame {start}: swept {measured}, paper {m}"
            );
        }
    }
}

/// A bank that was never set renders silence.
#[test]
fn a_bank_never_retargeted_renders_silence() {
    let mut bank = PlateBank::<f32>::new(SampleRate::SR_48K);
    let mut ramps = bank.ramps();
    let out = run(&mut bank, &mut ramps, &impulse(Samples::new(2_000)), 256);
    assert!(out.iter().flatten().all(|&x| x == 0.0));
}

/// `x` through an `f64` bank in one call: the two output channels.
fn run_wide(bank: &mut PlateBank<f64>, ramps: &mut PlateRamps<f64>, x: &[f64]) -> [Vec<f64>; 2] {
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

/// An `f64` lattice of integer delay `D` and coefficient `g` is the all-pass equation `v[n] =
/// x[n] + g v[n - D]`, `y[n] = v[n - D] - g v[n]` on the wide signal, with a `g` `f32` cannot
/// hold, to 2^-45; an `f32` state is off by 1e-8.
#[test]
fn an_f64_lattice_is_the_all_pass_equation() {
    use tutti_fixtures::signal::{assert_close, wide_signal};
    let (d, len) = (11usize, 300usize);
    let g = 0.618_033_988_749_894_9_f64;
    let x = wide_signal(len);
    let (mut v, mut want) = (vec![0.0f64; len], vec![0.0f64; len]);
    for n in 0..len {
        let z = n.checked_sub(d).map_or(0.0, |k| v[k]);
        v[n] = x[n] + g * z;
        want[n] = z - g * v[n];
    }
    let mut ring = DelayRing::<f64>::new(d + SLACK);
    let got: Vec<f64> = x
        .iter()
        .map(|&x| {
            let y = lattice(&mut ring, x, g, ReadPosition::from_frames(Samples::new(d)));
            ring.advance();
            y
        })
        .collect();
    assert_close(&got, &want, 2.0f64.powi(-45));
}

/// An `f64` plate with modulation off is linear: `R(a x + b y) = a R(x) + b R(y)` to 2^-40
/// of the peak, with `a`, `b` and two wide signals that an `f32` state cannot carry.
#[test]
fn an_f64_plate_is_linear() {
    use tutti_fixtures::signal::wide_signal;
    let len = 6_000;
    let x = wide_signal(len);
    let y: Vec<f64> = x.iter().rev().map(|v| v - 0.3).collect();
    let (a, b) = (1.0 / 3.0, -std::f64::consts::PI / 7.0);
    let mix: Vec<f64> = x.iter().zip(&y).map(|(x, y)| a * x + b * y).collect();
    let controls = PlateControls {
        damping: Mix::new(0.3),
        ..still(1.5)
    };
    let render = |input: &[f64]| {
        let mut bank = PlateBank::<f64>::new(SampleRate::SR_48K);
        let mut ramps = bank.ramps();
        bank.retarget(&mut ramps, &controls);
        ramps.jump_to_target();
        run_wide(&mut bank, &mut ramps, input)
    };
    let (rx, ry, rm) = (render(&x), render(&y), render(&mix));
    for c in 0..2 {
        let peak = rm[c].iter().fold(0.0f64, |p, v| p.max(v.abs()));
        assert!(peak > 1e-4, "the plate sounds");
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

/// The longest decay at the highest rate, in `f32`, against the closed form. At diffusion
/// 0, no damping and no modulation the impulse response repeats every `T` frames (the loop of
/// the tank's eight delays) at the loop gain `g = c^4`, `c` the decay gain, so the energies of
/// the successive loop-long windows past the first fall by `g^2`, and the measured RT60
/// `-3 T / (fs log10 g)` is the control's 8 s.
///
/// The bound is the rounding of `f32` over the `K` trips the windows span, in `ln g` a trip: the
/// decay gain `c` rounds once (at most `u`, `u = 2^-24`) and each of the four products a trip
/// rounds (at most `u`), `8 u` in all, coherent; plus the noise of the two windows' energies
/// compared, each a sum over a window of samples that carry at most `4 K + 14` roundings (the
/// products along their echo and the tap sum), so a sample's relative error is `sigma_s =
/// sqrt(4 K + 14) u / sqrt(3)` and a window's energy's `2 sigma_s`; the pair differ by `1.5 sqrt(2
/// ln N)` of `sqrt(2)` of that (`N = K + 1` windows), in `ln g` a trip over the `2 K` trips of
/// the amplitude between them. An RT60 is `-3 T / (fs log10 g)`, so its relative error is the
/// bound over `|ln g|`.
#[test]
fn the_longest_decay_at_192_khz_holds_its_closed_form_rt60_in_f32() {
    use crate::reverb::plate::design::loop_frames;
    let rate = SampleRate::SR_192K;
    let decay = crate::MAX_PLATE_DECAY.get();
    let c = pure(decay);
    let t = loop_frames(rate) as usize;
    let k = (decay * rate.get() / t as f64) as usize - 1;
    let [left, _] = response(rate, &c, (k + 2) * t);

    let window = |j: usize| -> f64 {
        left[t * (1 + j)..t * (2 + j)]
            .iter()
            .map(|&s| f64::from(s) * f64::from(s))
            .sum()
    };
    let (first, last) = (window(0), window(k));
    let g_measured = libm::pow(last / first, 1.0 / (2.0 * k as f64));
    let measured = -3.0 * t as f64 / (rate.get() * g_measured.log10());

    let u = f64::from(f32::EPSILON) / 2.0;
    let n = (k + 1) as f64;
    let sigma = (4.0 * k as f64 + 14.0).sqrt() * u / 3.0f64.sqrt();
    let noise = 1.5 * (2.0 * n.ln()).sqrt() * 2.0f64.sqrt() * 2.0 * sigma / (2.0 * k as f64);
    let per_trip = 8.0 * u + noise;
    let ln_g = -libm::log(10.0) * 3.0 * t as f64 / (decay * rate.get());
    let bound = per_trip / ln_g.abs();
    let error = (measured / decay - 1.0).abs();
    assert!(
        error <= bound,
        "RT60 {measured} s against {decay} s: error {error:e}, bound {bound:e} ({k} trips of {t} frames)"
    );
}

/// The bandwidth one-pole stores its complement `c = 1 - exp(-2 pi fc / fs)`, designed in `f64`,
/// so it holds `f32`'s relative precision (at most `u = 2^-24`) at the lowest bandwidth (10 Hz at
/// 192 kHz: `c = 3.3e-4`). The pole narrowed first and `1 - p` formed in `f32` is off by up to
/// `u / (2 c)` of `c` (4.6e-5 at 10 Hz), over a sweep of corners clearly worse.
#[test]
fn the_bandwidth_complement_keeps_its_relative_precision_where_one_minus_the_pole_does_not() {
    let rate = SampleRate::SR_192K;
    let u = f64::from(f32::EPSILON) / 2.0;
    let (mut stored, mut formed) = (0.0f64, 0.0f64);
    for hz in (10..=100).map(f64::from) {
        let controls = PlateControls {
            bandwidth: Hz::new(hz),
            ..PlateControls::DEFAULT
        };
        let (_, ramps) = at_rest(rate, &controls);
        let exact = -libm::expm1(-std::f64::consts::TAU * hz / rate.get());
        let held = f64::from(ramps.bandwidth.now());
        stored = stored.max((held / exact - 1.0).abs());
        let pole = libm::exp(-std::f64::consts::TAU * hz / rate.get()) as f32;
        let old = f64::from(1.0f32 - pole);
        formed = formed.max((old / exact - 1.0).abs());
    }
    assert!(
        stored <= u,
        "the complement's error {stored:e} against u = {u:e}"
    );
    assert!(
        formed > 100.0 * u,
        "`1 - p` in f32 is off by {formed:e}, not clearly worse than {u:e}"
    );
}

/// The damping one-pole stores its complement `1 - damp`, designed in `f64`: at the largest
/// damping (0.999: `c = 1e-3`) the pole rounded to `f32` first is off by up to `u / c`, 6e-5 of
/// `c`, where the complement is within `u`.
#[test]
fn the_damping_complement_keeps_its_relative_precision_near_the_largest_pole() {
    let u = f64::from(f32::EPSILON) / 2.0;
    let (mut stored, mut formed) = (0.0f64, 0.0f64);
    for step in 0..=90 {
        let damp = 0.99 + 0.0001 * f64::from(step);
        let controls = PlateControls {
            damping: Mix::new(damp),
            ..PlateControls::DEFAULT
        };
        let (_, ramps) = at_rest(SampleRate::SR_48K, &controls);
        let exact = 1.0 - damp;
        stored = stored.max((f64::from(ramps.damping.now()) / exact - 1.0).abs());
        formed = formed.max((f64::from(1.0f32 - damp as f32) / exact - 1.0).abs());
    }
    assert!(
        stored <= u,
        "the complement's error {stored:e} against u = {u:e}"
    );
    assert!(
        formed > 100.0 * u,
        "`1 - p` in f32 is off by {formed:e}, not clearly worse than {u:e}"
    );
}
