use super::*;
use core::f64::consts::TAU;
use tutti_buffers::{InputOutput, Planes, PlanesMut};
use tutti_units::Seconds;

/// The magnitude of the DTFT `sum h[n] e^{-j w n}` of an impulse response at the angular
/// frequency `w`, in radians a frame.
fn magnitude(h: &[f64], w: f64) -> f64 {
    tutti_fixtures::signal::dtft_magnitude(h, w)
}

fn impulse_response(form: CombForm, delay: usize, gain: f64, len: usize) -> Vec<f64> {
    let mut comb =
        CombFilter::<f64>::new(form, NonZeroUsize::new(delay).unwrap(), Feedback::new(gain));
    let mut h = vec![0.0; len];
    h[0] = 1.0;
    comb.process(&mut h);
    h
}

/// A feedback comb of `D` frames and gain `g` peaks at `k fs / D` with magnitude `1 / (1 - g)`
/// and bottoms out between them, at `(k + 1/2) fs / D`, at `1 / (1 + g)`: `H(z) = z^-D / (1 - g
/// z^-D)` has `|H| = 1 / |1 - g e^{-j D w}|`. The response is cut where `g^k` is below 1e-30.
#[test]
fn a_feedback_comb_peaks_at_the_multiples_of_fs_over_d_at_one_over_one_minus_g() {
    let (d, g) = (12usize, 0.8);
    let h = impulse_response(CombForm::Feedback, d, g, 400 * d);
    for k in 0..=6 {
        let peak = magnitude(&h, TAU * k as f64 / d as f64);
        assert!((peak - 1.0 / (1.0 - g)).abs() < 1e-9, "peak {k}: {peak}");
        let trough = magnitude(&h, TAU * (k as f64 + 0.5) / d as f64);
        assert!(
            (trough - 1.0 / (1.0 + g)).abs() < 1e-9,
            "trough {k}: {trough}"
        );
    }
}

/// A feedforward comb `1 + g z^-D` has `|H| = 1 + g` at the multiples of `fs / D` and `1 - g` at
/// the odd multiples of `fs / 2D`: for `g > 0` the former are its peaks and the latter its
/// notches, and for `g = -0.5` they swap, `|H|` is 0.5 at the former and 1.5 at the latter.
#[test]
fn a_feedforward_comb_notches_at_the_odd_multiples_of_fs_over_two_d() {
    let d = 10usize;
    for g in [0.9, -0.5] {
        let h = impulse_response(CombForm::Feedforward, d, g, d + 1);
        for k in 0..=4 {
            let odd = magnitude(&h, TAU * (2 * k + 1) as f64 / (2 * d) as f64);
            assert!((odd - (1.0 - g)).abs() < 1e-12, "g {g}, odd {k}: {odd}");
            let even = magnitude(&h, TAU * k as f64 / d as f64);
            assert!((even - (1.0 + g)).abs() < 1e-12, "g {g}, even {k}: {even}");
        }
    }
}

/// The feedforward comb's impulse response is 1 at frame 0 and `g` at frame `D`; the feedback
/// comb's is `g^(k-1)` at `k D` for `k >= 1` and zero elsewhere.
#[test]
fn the_impulse_responses_are_the_difference_equations_taps() {
    let ff = impulse_response(CombForm::Feedforward, 5, 0.5, 40);
    for (n, &y) in ff.iter().enumerate() {
        let want = match n {
            0 => 1.0,
            5 => 0.5,
            _ => 0.0,
        };
        assert_eq!(y, want, "feedforward frame {n}");
    }
    let fb = impulse_response(CombForm::Feedback, 5, 0.5, 40);
    for (n, &y) in fb.iter().enumerate() {
        let want = if n >= 5 && n % 5 == 0 {
            0.5f64.powi((n / 5) as i32 - 1)
        } else {
            0.0
        };
        assert_eq!(y, want, "feedback frame {n}");
    }
}

/// A bank runs one comb per channel and takes a new coefficient on every channel: the
/// second echo of a feedback comb is the coefficient set last.
#[test]
fn a_bank_retargets_every_channel() {
    let mut bank = CombBank::<f32>::new(
        CombForm::Feedback,
        NonZeroUsize::new(4).unwrap(),
        Feedback::new(0.5),
        ChannelLayout::from_count(2),
    );
    assert_eq!(bank.delay(), Samples::new(4));
    bank.retarget(&CombControls {
        gain: Feedback::new(0.25),
    });
    let mut x = vec![vec![0.0f32; 16]; 2];
    x[0][0] = 1.0;
    x[1][0] = 1.0;
    let mut y = vec![vec![0.0f32; 16]; 2];
    bank.process(
        &mut InputOutput::paired(
            Planes::from_vecs(&x).unwrap(),
            PlanesMut::from_vecs(&mut y).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    for ch in &y {
        assert_eq!((ch[4], ch[8], ch[12]), (1.0, 0.25, 0.0625));
    }
}

/// A feedback comb's declared tail covers its ring-out to the floor: the echo `k` is `g^(k-1)`,
/// under 1e-3 of the first by the declared tail; a feedforward comb's is its delay.
#[test]
fn the_tail_covers_the_ring_out_to_the_floor() {
    let rate = SampleRate::SR_48K;
    let (d, g) = (100usize, 0.9);
    let bank = CombBank::<f64>::new(
        CombForm::Feedback,
        NonZeroUsize::new(d).unwrap(),
        Feedback::new(g),
        ChannelLayout::MONO,
    );
    let Tail::Finite(tail) = bank.tail(Feedback::new(g), rate) else {
        panic!("finite");
    };
    let h = impulse_response(CombForm::Feedback, d, g, tail.get() + d);
    let last = h.iter().rposition(|&y| y.abs() >= 1e-3).unwrap();
    assert!(last < tail.get(), "{last} vs {}", tail.get());
    let ff = CombBank::<f64>::new(
        CombForm::Feedforward,
        NonZeroUsize::new(d).unwrap(),
        Feedback::new(g),
        ChannelLayout::MONO,
    );
    assert_eq!(
        ff.tail(Feedback::new(g), rate),
        Tail::Finite(Samples::new(d))
    );
}

/// The delay is the frames asked for: one frame is one frame (a `NonZeroUsize` has no zero
/// to turn into one), and a one-frame feedback comb is `z^-1 / (1 - g z^-1)`: `h[0] = 0`, `h[n] = g^(n-1)`.
#[test]
fn a_one_frame_comb_is_one_frame_and_the_response_has_its_closed_form() {
    let g = 0.5;
    let comb = CombFilter::<f64>::new(CombForm::Feedback, NonZeroUsize::MIN, Feedback::new(g));
    assert_eq!(comb.delay(), Samples::new(1));
    let h = impulse_response(CombForm::Feedback, 1, g, 5);
    for (n, got) in h.iter().enumerate() {
        let want = if n == 0 { 0.0 } else { g.powi(n as i32 - 1) };
        assert!((got - want).abs() < 1e-15, "h[{n}] = {got}, want {want}");
    }
}

/// **A comb delay is above zero and at most 10 s**: zero, a negative time, a non-finite one
/// and one past the cap are refused rather than raised to a frame, and the cap is accepted.
/// It rounds up to whole frames at the rate: 4 ms at 1 kHz is 4 frames, 4.1 ms is 5, and one
/// far under a frame is one.
#[test]
fn a_comb_delay_takes_exactly_the_finite_times_above_zero_to_the_cap() {
    tutti_fixtures::check_range_contract(&tutti_fixtures::RangeContract {
        new: CombDelay::new,
        get: CombDelay::get,
        of: Some(CombDelay::of),
        valid: &[Seconds::new(1e-9), Seconds::new(0.004), CombDelay::CAP],
        invalid: &[
            Seconds::ZERO,
            Seconds::new(-0.004),
            Seconds::new(f64::NAN),
            Seconds::new(f64::INFINITY),
            Seconds::new(10.000_001),
        ],
    });
    let rate = SampleRate::of(1_000.0);
    let frames = |s: f64| CombDelay::of(Seconds::new(s)).frames(rate).get();
    assert_eq!((frames(0.004), frames(0.004_1), frames(1e-9)), (4, 5, 1));
}

/// A driven coefficient is held by the comb's form: not finite is 0, a feedforward comb's is
/// within `-1..=1`, a feedback comb's is within `0..=MAX_STABLE`; a coefficient inside is
/// unchanged, the bounds included.
#[test]
fn a_driven_coefficient_is_held_by_the_form() {
    let held = |form: CombForm, g: f64| form.held(Feedback::new(g)).get();
    for form in [CombForm::Feedforward, CombForm::Feedback] {
        assert_eq!(held(form, f64::NAN), 0.0);
        assert_eq!(held(form, f64::INFINITY), 0.0);
        assert_eq!(held(form, 0.0), 0.0);
        assert_eq!(held(form, 0.5), 0.5);
        assert_eq!(held(form, 0.99), 0.99);
    }
    // Feedforward holds `-1..=1`; feedback holds `0..=MAX_STABLE`.
    for (g, ff) in [(-0.5, -0.5), (-1.0, -1.0), (-3.0, -1.0), (3.0, 1.0)] {
        assert_eq!(held(CombForm::Feedforward, g), ff, "feedforward {g}");
    }
    assert_eq!(held(CombForm::Feedback, -0.5), 0.0);
    assert_eq!(held(CombForm::Feedback, 3.0), Feedback::MAX_STABLE.get());
}
