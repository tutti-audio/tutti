use super::*;

#[test]
fn test_delay_line_basic() {
    let mut dl = DelayLine::new(Samples(10));
    dl.push_sample(1.0);
    dl.push_sample(0.0);
    dl.push_sample(0.0);

    let val = dl.read_sample(2.0, InterpolationMode::None);
    assert!((val - 1.0).abs() < 0.001, "Expected 1.0, got {val}");
}

/// An integer read is a delay of exactly that many frames, at every
/// delay the line holds and every cursor position: an impulse pushed at
/// frame `k` reads back at `d` only on frame `k + d`.
#[test]
fn an_integer_read_delays_by_exactly_that_many_frames() {
    const MAX: usize = 6;
    for at in 0..3 * (MAX + 1) {
        let mut dl = DelayLine::new(Samples(MAX));
        for frame in 0..at + MAX + 2 {
            dl.push_sample(if frame == at { 1.0 } else { 0.0 });
            for d in 0..=MAX {
                let want = if frame == at + d { 1.0 } else { 0.0 };
                let got = dl.read_sample(d as f32, InterpolationMode::None);
                assert_eq!(got, want, "impulse at {at}, frame {frame}, delay {d}");
            }
        }
    }
}

#[test]
fn test_delay_line_linear_interpolation() {
    let mut dl = DelayLine::new(Samples(10));
    dl.push_sample(0.0);
    dl.push_sample(1.0);
    dl.push_sample(0.0);

    let val = dl.read_sample(1.5, InterpolationMode::Linear);
    assert!((val - 0.5).abs() < 0.001, "Expected 0.5, got {val}");
}

#[test]
fn test_delay_line_cubic_interpolation() {
    let mut dl = DelayLine::new(Samples(10));
    for i in 0..5 {
        dl.push_sample(i as f32);
    }
    let val = dl.read_sample(1.5, InterpolationMode::CubicHermite);
    // Between samples 3 and 2 (delay 1 = sample 3, delay 2 = sample 2)
    assert!(
        val > 2.0 && val < 4.0,
        "Cubic value {val} should be between recent samples"
    );
}

#[test]
fn test_delay_line_reset() {
    let mut dl = DelayLine::new(Samples(10));
    dl.push_sample(1.0);
    dl.push_sample(1.0);
    dl.reset();
    let val = dl.read_sample(1.0, InterpolationMode::None);
    assert!((val).abs() < 0.001, "After reset, should read 0.0");
}

/// The whole-frame reader is the fixed delay: pushing `x` and reading `len` back returns
/// each sample `len` pushes later, and silence before the first one arrives, across every
/// wrap and after a reset.
#[test]
fn a_whole_read_returns_each_sample_len_pushes_later() {
    const LEN: usize = 4;
    let mut line = DelayLine::new(Samples(LEN));
    for round in 0..2 {
        for k in 0..5 * LEN {
            line.push_sample((k + 1) as f32);
            let want = if k >= LEN { (k - LEN + 1) as f32 } else { 0.0 };
            assert_eq!(line.read_whole(LEN), want, "round {round}, push {k}");
        }
        line.reset();
    }
}

/// The two readers share one ring: at a whole delay the interpolated read of every mode is
/// the whole-frame read, and a read past the capacity is the oldest sample held.
#[test]
fn the_two_readers_agree_on_a_whole_delay() {
    const MAX: usize = 7;
    let mut line = DelayLine::new(Samples(MAX));
    for k in 0..3 * MAX {
        line.push_sample(((k * 37) % 11) as f32 - 5.0);
        for d in 0..=MAX {
            let whole = line.read_whole(d);
            for mode in [
                InterpolationMode::None,
                InterpolationMode::Linear,
                InterpolationMode::CubicHermite,
            ] {
                assert_eq!(line.read_sample(d as f32, mode), whole, "{mode:?} at {d}");
            }
        }
        assert_eq!(line.read_whole(MAX + 5), line.read_whole(MAX));
    }
}

/// The trips an echo makes before it is `floor` under the first: `ceil(ln(10^(floor/20)) / ln q)`.
/// At `q = 1/2` each trip is 6.0206 dB down, so -60 dB takes 9.97 trips (10), -90 dB 14.95 (15)
/// and -120 dB 19.93 (20); a loop gain of 0 never echoes.
#[test]
fn trips_count_the_echoes_to_the_floor_in_decibels() {
    use tutti_units::Db;
    assert_eq!(taps::trips(0.5, Db(-60.0)), 10.0);
    assert_eq!(taps::trips(0.5, Db(-90.0)), 15.0);
    assert_eq!(taps::trips(0.5, Db(-120.0)), 20.0);
    assert_eq!(taps::trips(0.0, Db(-60.0)), 0.0);
}

/// A line in `f64` returns the 29-bit signal `d` frames later exactly at every mode and
/// every whole delay, across the wrap.
#[test]
fn an_f64_line_returns_a_wide_signal_d_frames_later_exactly() {
    use crate::testing::{assert_close_f64, wide_signal};
    const MAX: usize = 6;
    let signal = wide_signal(4 * MAX + 1);
    for mode in [
        InterpolationMode::None,
        InterpolationMode::Linear,
        InterpolationMode::CubicHermite,
    ] {
        for d in 0..=MAX {
            let mut line = DelayLine::<f64>::new(Samples(MAX));
            let got: Vec<f64> = signal
                .iter()
                .map(|&x| {
                    line.push_sample(x);
                    line.read_sample(d as f32, mode)
                })
                .collect();
            let want: Vec<f64> = (0..signal.len())
                .map(|i| if i >= d { signal[i - d] } else { 0.0 })
                .collect();
            assert_close_f64(&got, &want, 0.0);
        }
    }
}

/// A feedback-free `delay_step` of `d` frames in `f64` returns the wide input `d` frames
/// later exactly.
#[test]
fn an_f64_delay_step_without_feedback_returns_the_input_d_frames_later() {
    use crate::testing::{assert_close_f64, wide_signal};
    const D: usize = 4;
    let signal = wide_signal(20);
    let mut line = DelayLine::<f64>::new(Samples(8));
    let got: Vec<f64> = signal
        .iter()
        .map(|&x| delay_step(&mut line, x, D as f32, 0.0, InterpolationMode::Linear))
        .collect();
    let want: Vec<f64> = (0..signal.len())
        .map(|i| if i >= D { signal[i - D] } else { 0.0 })
        .collect();
    assert_close_f64(&got, &want, 0.0);
}
