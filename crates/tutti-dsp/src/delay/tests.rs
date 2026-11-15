use super::*;

#[test]
fn test_delay_line_basic() {
    let mut dl = DelayLine::new(10);
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
        let mut dl = DelayLine::new(MAX);
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
    let mut dl = DelayLine::new(10);
    dl.push_sample(0.0);
    dl.push_sample(1.0);
    dl.push_sample(0.0);

    let val = dl.read_sample(1.5, InterpolationMode::Linear);
    assert!((val - 0.5).abs() < 0.001, "Expected 0.5, got {val}");
}

#[test]
fn test_delay_line_cubic_interpolation() {
    let mut dl = DelayLine::new(10);
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
    let mut dl = DelayLine::new(10);
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
    let mut line = DelayLine::new(LEN);
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
    let mut line = DelayLine::new(MAX);
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
