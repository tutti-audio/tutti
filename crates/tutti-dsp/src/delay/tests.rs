use super::*;

/// The position `frames` frames back, to the quantum.
fn pos(frames: f64) -> ReadPosition {
    ReadPosition::from_count((frames * ReadPosition::COUNTS_PER_FRAME).round() as u64)
}

/// `frames` back as a position of `line`.
fn within<F: tutti_buffers::Float>(line: &DelayLine<F>, frames: f64) -> LinePosition {
    line.position(pos(frames)).unwrap()
}

// The tests other than the `f64` ones pin the `f32` instantiation, the session's audio
// format, so their tolerances are `f32`'s.

#[test]
fn test_delay_line_basic() {
    let mut dl = DelayLine::new(Samples::new(10));
    dl.push_sample(1.0);
    dl.push_sample(0.0);
    dl.push_sample(0.0);

    let val = dl.read_sample(within(&dl, 2.0), InterpolationMode::None);
    assert_eq!(val, 1.0);
}

/// An integer read is a delay of exactly that many frames, at every
/// delay the line holds and every cursor position: an impulse pushed at
/// frame `k` reads back at `d` only on frame `k + d`.
#[test]
fn an_integer_read_delays_by_exactly_that_many_frames() {
    const MAX: usize = 6;
    for at in 0..3 * (MAX + 1) {
        let mut dl = DelayLine::new(Samples::new(MAX));
        for frame in 0..at + MAX + 2 {
            dl.push_sample(if frame == at { 1.0 } else { 0.0 });
            for d in 0..=MAX {
                let want = if frame == at + d { 1.0 } else { 0.0 };
                let got = dl.read_sample(within(&dl, d as f64), InterpolationMode::None);
                assert_eq!(got, want, "impulse at {at}, frame {frame}, delay {d}");
            }
        }
    }
}

#[test]
fn test_delay_line_linear_interpolation() {
    let mut dl = DelayLine::new(Samples::new(10));
    dl.push_sample(0.0);
    dl.push_sample(1.0);
    dl.push_sample(0.0);

    let val = dl.read_sample(within(&dl, 1.5), InterpolationMode::Linear);
    assert_eq!(val, 0.5);
}

#[test]
fn test_delay_line_cubic_interpolation() {
    let mut dl = DelayLine::new(Samples::new(10));
    for i in 0..5 {
        dl.push_sample(i as f32);
    }
    let val = dl.read_sample(within(&dl, 1.5), InterpolationMode::CubicHermite);
    // Between samples 3 and 2 (delay 1 = sample 3, delay 2 = sample 2)
    assert!(
        val > 2.0 && val < 4.0,
        "Cubic value {val} should be between recent samples"
    );
}

#[test]
fn test_delay_line_reset() {
    let mut dl = DelayLine::new(Samples::new(10));
    dl.push_sample(1.0);
    dl.push_sample(1.0);
    dl.reset();
    let val = dl.read_sample(within(&dl, 1.0), InterpolationMode::None);
    assert_eq!(val, 0.0, "after a reset the line reads silence");
}

/// The whole-frame reader is the fixed delay: pushing `x` and reading `len` back returns
/// each sample `len` pushes later, and silence before the first one arrives, across every
/// wrap and after a reset.
#[test]
fn a_whole_read_returns_each_sample_len_pushes_later() {
    const LEN: usize = 4;
    let mut line = DelayLine::new(Samples::new(LEN));
    for round in 0..2 {
        for k in 0..5 * LEN {
            line.push_sample((k + 1) as f32);
            let want = if k >= LEN { (k - LEN + 1) as f32 } else { 0.0 };
            assert_eq!(
                line.read_whole(within(&line, LEN as f64)),
                want,
                "round {round}, push {k}"
            );
        }
        line.reset();
    }
}

/// The two readers share one ring: at a whole delay the interpolated read of every mode is
/// the whole-frame read.
#[test]
fn the_two_readers_agree_on_a_whole_delay() {
    const MAX: usize = 7;
    let mut line = DelayLine::new(Samples::new(MAX));
    for k in 0..3 * MAX {
        line.push_sample(((k * 37) % 11) as f32 - 5.0);
        for d in 0..=MAX {
            let whole = line.read_whole(within(&line, d as f64));
            for mode in [
                InterpolationMode::None,
                InterpolationMode::Linear,
                InterpolationMode::CubicHermite,
            ] {
                assert_eq!(
                    line.read_sample(within(&line, d as f64), mode),
                    whole,
                    "{mode:?} at {d}"
                );
            }
        }
    }
}

/// The trips an echo makes before it is `floor` under the first: `ceil(ln(10^(floor/20)) / ln q)`.
/// At `q = 1/2` each trip is 6.0206 dB down, so -60 dB takes 9.97 trips (10), -90 dB 14.95 (15)
/// and -120 dB 19.93 (20); a loop gain of 0 never echoes.
#[test]
fn trips_count_the_echoes_to_the_floor_in_decibels() {
    use tutti_units::Db;
    assert_eq!(taps::trips(0.5, Db::new(-60.0)), 10.0);
    assert_eq!(taps::trips(0.5, Db::new(-90.0)), 15.0);
    assert_eq!(taps::trips(0.5, Db::new(-120.0)), 20.0);
    assert_eq!(taps::trips(0.0, Db::new(-60.0)), 0.0);
}

/// A line in `f64` returns the 29-bit signal `d` frames later exactly at every mode and
/// every whole delay, across the wrap.
#[test]
fn an_f64_line_returns_a_wide_signal_d_frames_later_exactly() {
    use tutti_fixtures::signal::{assert_close, wide_signal};
    const MAX: usize = 6;
    let signal = wide_signal(4 * MAX + 1);
    for mode in [
        InterpolationMode::None,
        InterpolationMode::Linear,
        InterpolationMode::CubicHermite,
    ] {
        for d in 0..=MAX {
            let mut line = DelayLine::<f64>::new(Samples::new(MAX));
            let got: Vec<f64> = signal
                .iter()
                .map(|&x| {
                    line.push_sample(x);
                    line.read_sample(within(&line, d as f64), mode)
                })
                .collect();
            let want: Vec<f64> = (0..signal.len())
                .map(|i| if i >= d { signal[i - d] } else { 0.0 })
                .collect();
            assert_close(&got, &want, 0.0);
        }
    }
}

/// A feedback-free `delay_step` of `d` frames in `f64` returns the wide input `d` frames
/// later exactly.
#[test]
fn an_f64_delay_step_without_feedback_returns_the_input_d_frames_later() {
    use tutti_fixtures::signal::{assert_close, wide_signal};
    const D: usize = 4;
    let signal = wide_signal(20);
    let mut line = DelayLine::<f64>::new(Samples::new(8));
    let got: Vec<f64> = signal
        .iter()
        .map(|&x| delay_step(&mut line, x, pos(D as f64), 0.0, InterpolationMode::Linear))
        .collect();
    let want: Vec<f64> = (0..signal.len())
        .map(|i| if i >= D { signal[i - D] } else { 0.0 })
        .collect();
    assert_close(&got, &want, 0.0);
}

/// A feedback `delay_step` in `f64` with `fb = 0.3` (not an `f32` value) follows the
/// recursion `w[n] = x[n] + fb * w[n - d]`, output `w[n - d]`, to `2^-50`: an `f32`
/// feedback misses it by about `1e-9`.
#[test]
fn an_f64_delay_step_with_feedback_follows_the_closed_form_recursion() {
    use tutti_fixtures::signal::{assert_close, wide_signal};
    const D: usize = 4;
    const FB: f64 = 0.3;
    let signal = wide_signal(40);
    let mut line = DelayLine::<f64>::new(Samples::new(8));
    let got: Vec<f64> = signal
        .iter()
        .map(|&x| delay_step(&mut line, x, pos(D as f64), FB, InterpolationMode::Linear))
        .collect();
    let mut w = vec![0.0_f64; signal.len()];
    for n in 0..signal.len() {
        w[n] = signal[n] + if n >= D { FB * w[n - D] } else { 0.0 };
    }
    let want: Vec<f64> = (0..signal.len())
        .map(|n| if n >= D { w[n - D] } else { 0.0 })
        .collect();
    assert_close(&got, &want, 2f64.powi(-50));
}

/// A linear read at `100000.3` frames on a long `f64` line is the lerp of its two
/// wide slots at the position's own fraction, to `2^-45`.
#[test]
fn an_f64_line_read_at_a_wide_position_is_the_lerp_of_the_wide_slots() {
    use tutti_fixtures::signal::wide_signal;
    const N: usize = 200_000;
    let signal = wide_signal(N);
    let mut line = DelayLine::<f64>::new(Samples::new(N));
    signal.iter().for_each(|&x| line.push_sample(x));
    let at = pos(100_000.3);
    let frac = (at.get() & 0xffff_ffff) as f64 / ReadPosition::COUNTS_PER_FRAME;
    let (a, b) = (signal[N - 1 - 100_000], signal[N - 1 - 100_001]);
    let want = a + frac * (b - a);
    let got = line.read_sample(line.position(at).unwrap(), InterpolationMode::Linear);
    assert!(
        (got - want).abs() <= 2f64.powi(-45),
        "got {got} want {want}"
    );
}

/// A fixed delay at the line's capacity is accepted and one a quantum past it is refused with
/// the capacity named; a held delay past the line is the line's end and one inside is
/// unchanged.
#[test]
fn a_position_past_the_line_is_refused_and_a_held_one_is_its_end() {
    const MAX: usize = 5;
    let line = DelayLine::<f32>::new(Samples::new(MAX));
    assert_eq!(line.position(pos(MAX as f64)), Ok(line.end()));
    let past = ReadPosition::from_count(pos(MAX as f64).get() + 1);
    assert_eq!(
        line.position(past),
        Err(DelayError::PastLine {
            requested: past,
            longest: MAX
        })
    );
    assert_eq!(line.held(past), line.end());
    assert_eq!(line.held(pos(9.0)), line.end());
    assert_eq!(line.held(pos(2.5)).get(), pos(2.5));
}
