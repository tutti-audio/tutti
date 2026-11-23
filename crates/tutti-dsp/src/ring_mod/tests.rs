use super::*;
use core::f64::consts::TAU;
use tutti_buffers::{InputOutput, Planes, PlanesMut};
use tutti_fixtures::signal::amplitude_at;
use tutti_units::Hz;
use tutti_units::SampleRate;
use tutti_units::{Amplitude, Samples};

const RATE: SampleRate = SampleRate::SR_48K;

fn modulator(frequency: f64) -> RingModulator {
    let mut m = RingModulator::new(RATE);
    m.retarget(&RingModControls {
        frequency: Hz::new(frequency),
    });
    m
}

fn run(m: &mut RingModulator, input: &[f32], block: usize) -> Vec<f32> {
    let mut out = Vec::new();
    for chunk in input.chunks(block) {
        let ins = vec![chunk.to_vec()];
        let mut outs = vec![vec![0.0f32; chunk.len()]];
        m.process(
            &mut InputOutput::paired(
                Planes::from_vecs(&ins).unwrap(),
                PlanesMut::from_vecs(&mut outs).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        out.extend_from_slice(&outs[0]);
    }
    out
}

/// `frames` samples of a unit sine at `hz`, from phase 0.
fn sine(hz: f64, frames: usize) -> Vec<f32> {
    tutti_fixtures::signal::sine(Hz::new(hz), Amplitude::new(1.0), RATE, Samples::new(frames))
}

/// A constant input of 1 gives the carrier itself, `sin(2 pi fc n / fs)`, whatever the blocks.
#[test]
fn a_constant_input_gives_the_carrier() {
    for block in [1, 13, 4_800] {
        let out = run(&mut modulator(1_234.5), &[1.0; 4_800], block);
        for (n, &y) in out.iter().enumerate() {
            let want = (TAU * 1_234.5 * n as f64 / RATE.get()).sin();
            assert!(
                (f64::from(y) - want).abs() < 1e-5,
                "block {block}, frame {n}"
            );
        }
    }
}

/// For a 1 kHz sine the product `sin a sin b = (cos(a - b) - cos(a + b)) / 2`
/// holds 700 Hz and 1300 Hz at amplitude 1/2 each and nothing at 1000 Hz or at the 300 Hz
/// carrier (a second of frames is a whole number of cycles of each).
#[test]
fn a_sine_input_gives_the_sum_and_difference_and_no_input() {
    let out = run(&mut modulator(300.0), &sine(1_000.0, 48_000), 512);
    let at = |hz| amplitude_at(&out, Hz::new(hz), RATE);
    assert!((at(700.0) - 0.5).abs() < 1e-4, "{}", at(700.0));
    assert!((at(1_300.0) - 0.5).abs() < 1e-4, "{}", at(1_300.0));
    assert!(at(1_000.0) < 1e-4, "{}", at(1_000.0));
    assert!(at(300.0) < 1e-4, "{}", at(300.0));
}

/// A frequency change moves the phase's slope, not the phase: 100 frames at 440 Hz and then
/// 100 at 1 kHz give `sin(2 pi phi)` with `phi` the running sum of `f / fs`.
#[test]
fn a_frequency_change_never_jumps_the_phase() {
    let mut m = modulator(440.0);
    let mut out = run(&mut m, &[1.0; 100], 100);
    m.retarget(&RingModControls {
        frequency: Hz::new(1_000.0),
    });
    out.extend(run(&mut m, &[1.0; 100], 100));
    for (n, &y) in out.iter().enumerate() {
        let phase = if n < 100 {
            n as f64 * 440.0
        } else {
            100.0 * 440.0 + (n - 100) as f64 * 1_000.0
        } / RATE.get();
        assert!(
            (f64::from(y) - (TAU * phase).sin()).abs() < 1e-5,
            "frame {n}"
        );
    }
}
