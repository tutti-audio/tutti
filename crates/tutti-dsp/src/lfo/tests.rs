//! The LFO's figures: the waves at eighths of a cycle, the delay and the fade-in at named
//! frames, and the phase after a chunk against the same frames one at a time. Every rate here
//! is 480 Hz at 48 kHz, 0.01 of a cycle a frame, so a frame count is a phase to the
//! `2^-64` quantum of the step.

use super::*;

const RATE: f64 = 48_000.0;

fn lfo(wave: LfoWave) -> Lfo {
    Lfo::new(wave, Hz::new(480.0), SampleRate::of(RATE))
}

fn frames(n: usize) -> Samples {
    Samples::new(n)
}

fn run(mut lfo: Lfo, n: usize) -> f64 {
    lfo.advance(frames(n));
    lfo.value().get()
}

/// The sine is `sin(2 pi p)` and the triangle `0, 1/2, 1, 1/2, 0, -1/2, -1, -1/2` at the
/// eighths of a cycle (SF2 2.04 section 8.1.2: it starts at zero and rises). Both are exact
/// at an eighth, to the sine's rounding.
#[test]
fn the_waves_are_their_closed_forms_at_the_eighths_of_a_cycle() {
    let triangle = [0.0, 0.5, 1.0, 0.5, 0.0, -0.5, -1.0, -0.5];
    for (k, want) in triangle.into_iter().enumerate() {
        let phase = RunningPhase::at(Phase::new(k as f64 / 8.0));
        let sine = LfoWave::Sine.value_at(phase).get();
        let sine_want = (core::f64::consts::TAU * k as f64 / 8.0).sin();
        assert!((sine - sine_want).abs() < 4e-16, "sine at {k}/8: {sine}");
        let got = LfoWave::Triangle.value_at(phase).get();
        assert!((got - want).abs() < 1e-15, "triangle at {k}/8: {got}");
    }
}

/// The delay holds the start value, zero, for exactly its frames, and the first cycle begins
/// where it ends: at 100 frames the triangle is 0, 25 frames later (a quarter cycle) it is 1,
/// and a delay of 100.5 frames puts the phase half a frame, 0.005 of a cycle, behind.
#[test]
fn the_delay_holds_the_start_value_for_exactly_its_frames() {
    let delayed = lfo(LfoWave::Triangle).delay(Seconds::new(100.0 / RATE));
    for n in [0, 1, 99, 100] {
        assert!(run(delayed, n).abs() < 1e-15, "{n} frames in the delay");
    }
    assert!((run(delayed, 101) - 0.04).abs() < 1e-15);
    assert!((run(delayed, 125) - 1.0).abs() < 1e-15);
    let half = lfo(LfoWave::Triangle).delay(Seconds::new(100.5 / RATE));
    assert!(run(half, 100).abs() < 1e-15);
    assert!((run(half, 101) - 0.02).abs() < 1e-15);
}

/// A fade-in is a gain of `n / T` on the wave `n` frames after the delay, up to 1 at `T`: with
/// a fade of 200 frames, the triangle at 25 frames (value 1) is 1/8, and past the fade it is the
/// wave itself.
#[test]
fn a_fade_in_is_linear_from_the_end_of_the_delay_over_its_time() {
    let faded = lfo(LfoWave::Triangle)
        .delay(Seconds::new(50.0 / RATE))
        .fade_in(Seconds::new(200.0 / RATE));
    assert!((run(faded, 50 + 25) - 1.0 / 8.0).abs() < 1e-15);
    // 10 frames in: the wave is 0.04 * 10 = 0.4, the gain 10 / 200.
    assert!((run(faded, 50 + 10) - 0.4 * 10.0 / 200.0).abs() < 1e-15);
    // 225 frames: the phase is 2.25 cycles, the wave 1, the gain 1.
    assert!((run(faded, 50 + 225) - 1.0).abs() < 1e-15);
}

/// A chunk is the same frames: the phase adds `n` steps as one wrapping product, so any
/// chunking reads the value of the same phase count, bit for bit.
#[test]
fn a_chunk_reads_the_same_value_as_the_frames_one_at_a_time() {
    let start = lfo(LfoWave::Sine).delay(Seconds::new(7.3 / RATE));
    for total in [1, 8, 37, 1000, 4801] {
        let mut ones = start;
        (0..total).for_each(|_| ones.advance(frames(1)));
        assert_eq!(run(start, total), ones.value().get());
    }
}

/// A rate change keeps the phase: a quarter cycle at 480 Hz, then 25 frames at 960 Hz (half a
/// cycle) is three quarters, where the triangle is -1.
#[test]
fn a_retarget_keeps_the_phase_and_changes_the_step() {
    let mut lfo = lfo(LfoWave::Triangle);
    lfo.advance(frames(25));
    lfo.retarget(Hz::new(960.0), SampleRate::of(RATE));
    lfo.advance(frames(25));
    assert!((lfo.value().get() + 1.0).abs() < 1e-15);
}

/// The phase is a wrapping count, so it does not drift: after 10^9 frames at 0.01 of a cycle,
/// 10^7 whole cycles, the triangle is back at zero to the count's error, `n 2^-64` cycles of
/// quantum per frame, far under 1e-8.
#[test]
fn the_phase_does_not_drift_over_a_billion_frames() {
    let mut lfo = lfo(LfoWave::Triangle);
    for _ in 0..1000 {
        lfo.advance(frames(1_000_000));
    }
    assert!(lfo.value().get().abs() < 1e-8, "{}", lfo.value().get());
}

/// A reset starts the delay again.
#[test]
fn a_reset_returns_to_the_delay() {
    let mut lfo = lfo(LfoWave::Triangle).delay(Seconds::new(100.0 / RATE));
    lfo.advance(frames(125));
    lfo.reset();
    lfo.advance(frames(99));
    assert_eq!(lfo.value(), LfoWave::START);
}

/// The clock position in cycles is the rate times the frames over the sample rate, exact in
/// whole cycles, whether the frames came as one chunk or one at a time, running forward or
/// backward.
#[test]
fn the_clock_position_counts_whole_cycles_forward_and_backward() {
    let rate = SampleRate::of(100.0);
    for hz in [1.0, 2.5, -1.0, -7.25] {
        let mut chunk = Lfo::new(LfoWave::Sine, Hz::new(hz), rate);
        let mut single = chunk;
        chunk.advance(Samples::new(1_000));
        for _ in 0..1_000 {
            single.advance(Samples::new(1));
        }
        let want = hz * 10.0;
        assert!(
            (chunk.cycles() - want).abs() < 1e-9,
            "{hz}: {}",
            chunk.cycles()
        );
        assert_eq!(chunk.cycles(), single.cycles(), "{hz}");
    }
}

/// A rate of half a cycle a frame or more is held under half a cycle: it never reads as a
/// step back, so the position keeps moving forward.
#[test]
fn a_rate_past_the_nyquist_is_held_under_half_a_cycle_a_frame() {
    let mut lfo = Lfo::new(LfoWave::Sine, Hz::new(90.0), SampleRate::of(100.0));
    lfo.advance(Samples::new(10));
    assert!(lfo.cycles() > 4.9 && lfo.cycles() < 5.0, "{}", lfo.cycles());
}

/// A located clock reads the cycles it was put at, forward and backward, and advances from
/// there as a clock that ran to it would: 2.75 and -0.25 are dyadic, so the phase holds them
/// exactly, and 25 frames at 0.01 of a cycle a frame later it is a quarter cycle on.
#[test]
fn a_located_clock_reads_its_cycles_and_advances_from_them() {
    for at in [2.75, -0.25, 0.0, -3.5] {
        let mut clock = lfo(LfoWave::Sine);
        clock.advance(frames(37));
        clock.locate(at);
        assert_eq!(clock.cycles(), at);
        clock.advance(frames(25));
        assert!((clock.cycles() - (at + 0.25)).abs() < 1e-15, "{at}");
    }
}

/// A position that is not finite leaves the clock where it is.
#[test]
fn a_located_clock_ignores_a_position_that_is_not_finite() {
    let mut clock = lfo(LfoWave::Sine);
    clock.advance(frames(10));
    let before = clock.cycles();
    for at in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        clock.locate(at);
        assert_eq!(clock.cycles(), before);
    }
}
