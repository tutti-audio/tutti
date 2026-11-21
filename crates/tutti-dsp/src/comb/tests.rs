use super::*;
use core::f64::consts::TAU;

/// The magnitude of the DTFT `sum h[n] e^{-j w n}` of an impulse response at the angular
/// frequency `w`, in radians a frame.
fn magnitude(h: &[f64], w: f64) -> f64 {
    let (re, im) = h.iter().enumerate().fold((0.0, 0.0), |(re, im), (n, &x)| {
        let a = w * n as f64;
        (re + x * a.cos(), im - x * a.sin())
    });
    re.hypot(im)
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

/// A feedforward comb `1 + g z^-D` has its notches at the odd multiples of `fs / 2D`, where
/// `|H| = 1 - g`, and its peaks at the multiples of `fs / D`, where `|H| = 1 + g`.
#[test]
fn a_feedforward_comb_notches_at_the_odd_multiples_of_fs_over_two_d() {
    let (d, g) = (10usize, 0.9);
    let h = impulse_response(CombForm::Feedforward, d, g, d + 1);
    for k in 0..=4 {
        let notch = magnitude(&h, TAU * (2 * k + 1) as f64 / (2 * d) as f64);
        assert!((notch - (1.0 - g)).abs() < 1e-12, "notch {k}: {notch}");
        let peak = magnitude(&h, TAU * k as f64 / d as f64);
        assert!((peak - (1.0 + g)).abs() < 1e-12, "peak {k}: {peak}");
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
        Planes::from_vecs(&x).unwrap(),
        PlanesMut::from_vecs(&mut y).unwrap(),
    );
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
