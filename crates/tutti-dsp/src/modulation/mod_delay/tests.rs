use tutti_buffers::{Planes, PlanesMut};
use tutti_types::BusTopology;
use tutti_units::{Feedback, Gain, Hz, PhaseIncrement, SampleRate, SampleSpan, Seconds};

use super::*;
use crate::{DELAY_SLEW, Steps};

const RATE: SampleRate = SampleRate::SR_48K;

fn bus(width: u16) -> BusTopology {
    BusTopology::assumed(width.into()).expect("a test width is one bus")
}

fn controls(rate: f32, depth: f64, feedback: f32) -> ModDelayControls {
    ModDelayControls {
        rate: Hz(rate),
        depth: Seconds(depth),
        feedback: Feedback(feedback),
    }
}

/// A flanger's numbers (1 ms base, a 20 ms line) with a depth that may be zero: the real
/// flanger's least depth is 0.1 ms, so it is never frozen.
fn short_base() -> Sweep {
    Sweep::of(
        Seconds(0.001),
        MaxDelay::of(Seconds(0.02)),
        Seconds(0.0),
        Seconds(0.01),
        Feedback::MAX_STABLE,
    )
}

/// A mono swept delay of a base of `frames` at `rate`, in a 20 ms line.
fn at_frames(frames: f64, rate: SampleRate) -> ModDelayBank {
    let sweep = Sweep::of(
        Seconds(frames / rate.get()),
        MaxDelay::of(Seconds(0.02)),
        Seconds(0.0),
        Seconds(0.01),
        Feedback::MAX_STABLE,
    );
    let layout = ModDelayLayout::new(bus(1), sweep, Stagger::NONE).unwrap();
    ModDelayBank::new(&layout, rate)
}

/// The delay at the sweep's centre, in frames, floored at the frame a read takes at least.
fn centre(bank: &ModDelayBank) -> f32 {
    bank.swept(Gain::SILENT, SampleSpan::ZERO).get().max(1.0) as f32
}

/// `audio` (one slice per channel) through `bank` in blocks of `block`, `delays` and
/// `feedback` carried across them.
fn run(
    bank: &mut ModDelayBank,
    delays: &mut [Glide<f32>],
    feedback: &mut Glide<f32>,
    audio: &[Vec<f32>],
    block: usize,
) -> Vec<Vec<f32>> {
    let n = audio[0].len();
    let mut out = vec![vec![0.0f32; n]; audio.len()];
    let mut at = 0;
    while at < n {
        let end = (at + block).min(n);
        let a: Vec<&[f32]> = audio.iter().map(|c| &c[at..end]).collect();
        let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
        bank.process(
            delays,
            feedback,
            Planes::new(&a).unwrap(),
            PlanesMut::new(&mut outs).unwrap(),
        );
        at = end;
    }
    out
}

/// The response of a frozen bank (a delay of `d` frames, feedback `g`) to an impulse, `len`
/// frames long.
fn impulse_response(d: f64, g: f32, len: usize, rate: SampleRate) -> Vec<f32> {
    let mut bank = at_frames(d, rate);
    let mut delays = [Glide::landed(centre(&bank))];
    let mut feedback = Glide::landed(g);
    let mut x = vec![0.0f32; len];
    x[0] = 1.0;
    run(&mut bank, &mut delays, &mut feedback, &[x], 64).remove(0)
}

/// The delays a sweep reaches are `[max(1, base - depth), base + depth]` frames, an analytic
/// figure of the definition: a 10 ms chorus 5 ms deep at 48 kHz is `240..720`, and a depth
/// past the base **flat-bottoms** at one frame (a delay is at least a frame). It follows the
/// rate, and the depth is clamped into the sweep's own range first.
#[test]
fn a_sweep_reaches_base_plus_and_minus_the_depth_floored_at_a_frame() {
    let reach = |bank: &ModDelayBank, depth: f64| {
        let d = bank.design(&controls(1.0, depth, 0.0)).depth;
        (
            bank.swept(Gain(-1.0), d).get().max(1.0),
            bank.swept(Gain(1.0), d).get(),
        )
    };
    let chorus = ModDelayBank::new(&ModDelayLayout::chorus(bus(1)), RATE);
    assert_eq!(reach(&chorus, 0.005), (240.0, 720.0));
    let chorus96 = ModDelayBank::new(&ModDelayLayout::chorus(bus(1)), SampleRate::SR_96K);
    assert_eq!(reach(&chorus96, 0.005), (480.0, 1_440.0));
    // A depth past the base: 1 ms base, 10 ms deep: from one frame to 11 ms.
    let flanger = ModDelayBank::new(
        &ModDelayLayout::new(bus(1), short_base(), Stagger::NONE).unwrap(),
        RATE,
    );
    let (lo, hi) = reach(&flanger, 0.01);
    assert_eq!(lo, 1.0);
    assert!((hi - 0.011 * 48_000.0).abs() < 1e-6, "{hi}");
    // The depth is the sweep's: 1 s asks for the greatest (40 ms, 1,920 frames), and a
    // depth of none on the real flanger is its least, 0.1 ms.
    assert_eq!(chorus.design(&controls(1.0, 1.0, 0.0)).depth.get(), 1_920.0);
    assert_eq!(reach(&chorus, 1.0), (1.0, 2_400.0));
    let real = ModDelayBank::new(&ModDelayLayout::flanger(bus(1)), RATE);
    let least = real.design(&controls(1.0, 0.0, 0.0)).depth.get();
    assert!((least - 0.0001 * 48_000.0).abs() < 1e-3, "{least}");
    // The centre is the base, and the delay is held inside the line.
    assert_eq!(centre(&chorus), 480.0);
    assert_eq!(
        chorus.swept(Gain(1.0), SampleSpan::of(1e9)).get(),
        2_400.0,
        "no delay is past the line"
    );
}

/// What the controls become: a rate below 0.01 Hz is the floor, a depth is clamped into the
/// sweep's range and a feedback into `0..=feedback_max`, and a control that is not finite is
/// the default's.
#[test]
fn the_controls_are_made_safe() {
    let chorus = ModDelayBank::new(&ModDelayLayout::chorus(bus(1)), RATE);
    let step = |hz: f32| PhaseIncrement::per_sample(Hz(hz), RATE);
    for (hz, effective) in [
        (-3.0f32, 0.01),
        (0.0, 0.01),
        (0.005, 0.01),
        (0.01, 0.01),
        (0.5, 0.5),
        (f32::NAN, 1.0),
        (f32::NEG_INFINITY, 1.0),
        (f32::INFINITY, 1.0),
    ] {
        assert_eq!(
            chorus.design(&controls(hz, 0.005, 0.3)).inc,
            step(effective),
            "rate {hz}"
        );
    }
    for (fb, want) in [
        (0.3f32, 0.3),
        (5.0, 0.99),
        (-1.0, 0.0),
        (f32::NAN, 0.0),
        (f32::INFINITY, 0.0),
    ] {
        assert_eq!(
            chorus.design(&controls(1.0, 0.005, fb)).feedback,
            Feedback(want),
            "feedback {fb}"
        );
    }
    assert_eq!(
        chorus.design(&controls(1.0, -2.0, 0.0)).depth,
        SampleSpan::ZERO
    );
    assert_eq!(
        chorus.design(&controls(1.0, f64::NAN, 0.0)).depth,
        SampleSpan::ZERO,
        "the default's depth"
    );
}

/// A sweep with no feedback at all (the General MIDI chorus) is a pure delay: whatever its
/// feedback control is set to, the bank has none, and a frozen sweep answers an impulse with
/// one echo of unit size at the delay.
#[test]
fn a_sweep_with_no_feedback_ignores_the_control_and_is_a_pure_delay() {
    let layout = ModDelayLayout::general_midi_chorus(bus(1));
    let mut bank = ModDelayBank::new(&layout, RATE);
    let design = bank.design(&controls(0.4, 0.0, 0.9));
    assert_eq!(design.feedback, Feedback::NONE);
    // The centre is 2 ms, 96 frames.
    let d = centre(&bank);
    assert_eq!(d, 96.0);
    let mut delays = [Glide::landed(d)];
    let mut feedback = Glide::landed(design.feedback.get());
    let mut x = vec![0.0f32; 400];
    x[0] = 1.0;
    let y = run(&mut bank, &mut delays, &mut feedback, &[x], 64).remove(0);
    for (n, &v) in y.iter().enumerate() {
        assert_eq!(v, if n == 96 { 1.0 } else { 0.0 }, "frame {n}");
    }
}

/// The loop's period is the delay, as the spec's comb `1 / (1 - g z^-d)`: a frozen sweep of
/// a whole `d` frames and a feedback `g` answers an impulse with echoes at frames `d, 2d,
/// 3d, ...` of amplitudes `1, g, g^2, ...` and nothing between. At a fractional delay `d + f`
/// the read is linear, so one trip is `(1 - f) z^-d + f z^-(d + 1)` and the echoes are its
/// powers: the second, `g` times the square, is `g (1 - f)^2` at `2d`, `2 g f (1 - f)` at
/// `2d + 1` and `g f^2` at `2d + 2`, the first at `d` and `d + 1` of `1 - f` and `f`.
#[test]
fn the_feedback_loops_period_is_the_delay() {
    let rate = SampleRate::of(49_152.0);
    let g = 0.5f32;
    // A whole delay of 48 frames: exactly the comb.
    let y = impulse_response(48.0, g, 400, rate);
    for (n, &got) in y.iter().enumerate() {
        let want = if n >= 48 && n % 48 == 0 {
            g.powi((n / 48 - 1) as i32)
        } else {
            0.0
        };
        assert_eq!(got, want, "frame {n}");
    }
    // A fractional delay of 48.5 frames, through the linear read.
    let y = impulse_response(48.5, g, 400, rate);
    let h = 0.5f32;
    let want = [
        (48, h),
        (49, h),
        (96, g * h * h),
        (97, g * 2.0 * h * h),
        (98, g * h * h),
    ];
    for (n, &got) in y.iter().enumerate().take(99) {
        let w = want.iter().find(|t| t.0 == n).map_or(0.0, |t| t.1);
        assert!((got - w).abs() < 1e-4, "frame {n}: {got} vs {w}");
    }
}

/// A flanger's notches from the definition: the dry and the wet at equal weight, with no
/// feedback and a delay of `D` whole frames, are `1 + z^-D`, `|H| = 2 |cos(w D / 2)|`, with
/// its zeros at `(2k + 1) / (2 D)` of the sample rate and spacing `1 / D`; with feedback `g`
/// the wet's loop is `g z^-D` and the comb **peaks** where `z^-D` is 1, every `1 / D` of the
/// rate, at `1 / (1 - g)`. Read off the impulse response.
#[test]
fn a_frozen_flanger_has_notches_at_odd_multiples_of_half_over_the_delay() {
    let rate = SampleRate::of(49_152.0);
    let d = 48usize;
    let response = |h: &[f32], f: f64| {
        let (re, im) = h
            .iter()
            .enumerate()
            .fold((0.0f64, 0.0f64), |(re, im), (n, &x)| {
                let a = std::f64::consts::TAU * f / rate.get() * n as f64;
                (re + f64::from(x) * a.cos(), im - f64::from(x) * a.sin())
            });
        (re, im)
    };
    let wet = impulse_response(d as f64, 0.0, 200, rate);
    let sum_with_dry = |f: f64| {
        let (re, im) = response(&wet, f);
        (1.0 + re).hypot(im)
    };
    for k in 0..6 {
        let notch = (2.0 * f64::from(k) + 1.0) / (2.0 * d as f64) * rate.get();
        assert!(sum_with_dry(notch) < 1e-3, "notch {k} at {notch} Hz");
        let peak = f64::from(k + 1) / d as f64 * rate.get();
        assert!((sum_with_dry(peak) - 2.0).abs() < 1e-3, "peak at {peak} Hz");
    }
    // Feedback: the wet's own loop peaks every `1 / D`, at `1 / (1 - g)`.
    let g = 0.7f32;
    let looped = impulse_response(d as f64, g, 8_000, rate);
    let (re, im) = response(&looped, rate.get() / d as f64);
    assert!(
        (re.hypot(im) - 1.0 / f64::from(1.0 - g)).abs() < 1e-3,
        "{}",
        re.hypot(im)
    );
}

/// The delay glides, and a frame reads it in closed form: with the ramp `x[n] = n` through
/// the line a linear read is exact, so frame `i` of a glide aimed from 100 to 200 frames over
/// 400 steps is `n - at(i + 1)` (the first frame after an aim reads step 1), and it lands
/// exactly.
#[test]
fn a_delay_glide_is_read_in_closed_form_at_the_step_after_those_taken() {
    let rate = SampleRate::SR_48K;
    let mut bank = at_frames(100.0, rate);
    let mut delays = [Glide::landed(100.0f32)];
    let mut feedback = Glide::landed(0.0f32);
    let warm: Vec<f32> = (0..300).map(|n| n as f32).collect();
    run(&mut bank, &mut delays, &mut feedback, &[warm], 64);
    delays[0].aim_in(200.0, 400);
    let glide = delays[0];
    let x: Vec<f32> = (300..720).map(|n| n as f32).collect();
    let y = run(&mut bank, &mut delays, &mut feedback, &[x], 77).remove(0);
    for (i, &got) in y.iter().enumerate() {
        let want = (300 + i) as f32 - glide.at(i as u32 + 1);
        assert!((got - want).abs() < 2e-3, "frame {i}: {got} vs {want}");
    }
    assert_eq!(delays[0].now(), 200.0, "landed exactly");
}

/// Any split of the stream gives the bits of one call, with a delay glide and a feedback
/// glide in flight on every channel and a lane of varied input.
#[test]
fn any_block_size_gives_the_same_bits() {
    let layout = ModDelayLayout::chorus(bus(3));
    let len = 1_000;
    let x: Vec<Vec<f32>> = (0..3)
        .map(|c| {
            (0..len)
                .map(|n| ((n * (7 + c) + 3) % 101) as f32 / 50.0 - 1.0)
                .collect()
        })
        .collect();
    let render = |block: usize| {
        let mut bank = ModDelayBank::new(&layout, RATE);
        let mut delays = [Glide::landed(480.0f32); 3];
        for (c, g) in delays.iter_mut().enumerate() {
            g.aim_in(300.0 + 100.0 * c as f32, 700);
        }
        let mut feedback = Glide::landed(0.1f32);
        feedback.aim_in(0.8, 500);
        run(&mut bank, &mut delays, &mut feedback, &x, block)
    };
    let whole = render(len);
    assert!(whole[0].iter().any(|&s| s != 0.0));
    for block in [1, 7, 33, 64, 100, 255] {
        assert_eq!(render(block), whole, "blocks of {block}");
    }
}

/// A reset bank equals a fresh one bit for bit, and a clone continues identically and shares
/// no line.
#[test]
fn a_reset_bank_is_a_fresh_one_and_a_clone_shares_nothing() {
    let layout = ModDelayLayout::flanger(bus(2));
    let x: Vec<Vec<f32>> = (0..2)
        .map(|c| {
            (0..600)
                .map(|n| ((n * (5 + c)) % 31) as f32 - 15.0)
                .collect()
        })
        .collect();
    let glides = || {
        (
            [Glide::landed(48.0f32), Glide::landed(50.0)],
            Glide::landed(0.7f32),
        )
    };
    let (mut d, mut f) = glides();
    let mut used = ModDelayBank::new(&layout, RATE);
    run(&mut used, &mut d, &mut f, &x, 64);
    used.reset();
    let mut fresh = ModDelayBank::new(&layout, RATE);
    let (mut d1, mut f1) = glides();
    let (mut d2, mut f2) = glides();
    assert_eq!(
        run(&mut used, &mut d1, &mut f1, &x, 64),
        run(&mut fresh, &mut d2, &mut f2, &x, 64)
    );
    // A clone continues the same, and its lines are its own: resetting one leaves the
    // original and another clone ringing, and the reset one is not what they are.
    let mut a = used.clone();
    let mut b = used.clone();
    a.reset();
    let (mut d3, mut f3) = (d1, f1);
    let (mut d4, mut f4) = (d1, f1);
    let (mut d5, mut f5) = (d1, f1);
    let rung = run(&mut used, &mut d3, &mut f3, &x, 64);
    assert_eq!(run(&mut b, &mut d4, &mut f4, &x, 64), rung);
    assert_ne!(run(&mut a, &mut d5, &mut f5, &x, 64), rung);
}

/// A channel's line is its own: a silent channel stays silent while another sounds.
#[test]
fn a_silent_channel_stays_silent() {
    let mut bank = ModDelayBank::new(&ModDelayLayout::chorus(bus(3)), RATE);
    let mut delays = [
        Glide::landed(480.0f32),
        Glide::landed(300.0),
        Glide::landed(100.0),
    ];
    let mut feedback = Glide::landed(0.9f32);
    let noise: Vec<f32> = (0..2_000).map(|n| ((n * 37) % 23) as f32 - 11.0).collect();
    let y = run(
        &mut bank,
        &mut delays,
        &mut feedback,
        &[noise.clone(), vec![0.0; 2_000], noise],
        64,
    );
    assert!(y[1].iter().all(|&s| s == 0.0));
    assert!(y[0].iter().any(|&s| s != 0.0) && y[2].iter().any(|&s| s != 0.0));
}

/// A view of `f64` is the `f32` render converted, to the last bit, with glides in flight.
#[test]
fn an_f64_view_is_the_f32_render_converted() {
    let layout = ModDelayLayout::chorus(bus(2));
    let x: Vec<Vec<f32>> = (0..2)
        .map(|c| {
            (0..500)
                .map(|n| ((n * (3 + c)) % 17) as f32 / 8.0 - 1.0)
                .collect()
        })
        .collect();
    let glides = || {
        let mut d = [Glide::landed(480.0f32), Glide::landed(480.0)];
        d[0].aim_in(240.0, 300);
        let mut f = Glide::landed(0.3f32);
        f.aim_in(0.6, 200);
        (d, f)
    };
    let (mut d, mut f) = glides();
    let mut bank = ModDelayBank::new(&layout, RATE);
    let want = run(&mut bank, &mut d, &mut f, &x, 100);
    let (mut d, mut f) = glides();
    let mut bank = ModDelayBank::new(&layout, RATE);
    let wide: Vec<Vec<f64>> = x
        .iter()
        .map(|c| c.iter().map(|&s| f64::from(s)).collect())
        .collect();
    let mut got = vec![vec![0.0f64; 500]; 2];
    let ins: Vec<&[f64]> = wide.iter().map(Vec::as_slice).collect();
    let mut outs: Vec<&mut [f64]> = got.iter_mut().map(Vec::as_mut_slice).collect();
    bank.process(
        &mut d,
        &mut f,
        Planes::new(&ins).unwrap(),
        PlanesMut::new(&mut outs).unwrap(),
    );
    for (g, w) in got.iter().flatten().zip(want.iter().flatten()) {
        assert_eq!(*g, f64::from(*w));
    }
}

/// The slew is a span of frames: a depth glide aimed at `DELAY_SLEW` a frame takes
/// `ceil(distance / slew)` steps, so a jump of 1,680 frames takes 13,440.
#[test]
fn the_slew_is_frames_of_delay_per_frame() {
    assert_eq!(DELAY_SLEW, 0.125);
    let mut depth = Glide::landed(240.0f32);
    depth.aim_by(1_920.0, Steps::Rate(DELAY_SLEW));
    assert_eq!(depth.at(13_439), 240.0 + 1_680.0 * 13_439.0 / 13_440.0);
    assert_eq!(depth.at(13_440), 1_920.0);
}
