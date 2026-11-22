use super::*;
use crate::{Inline, Tap};
use tutti_buffers::{InputOutput, Planes, PlanesMut};
use tutti_units::Gain;

fn tap(delay: f64, gain: f64, channel: u8) -> Tap {
    Tap {
        delay: SampleSpan::new(delay).unwrap(),
        gain: Gain::new(gain),
        channel,
    }
}

fn stage(taps: &[Tap]) -> TapSet {
    TapSet {
        taps: Inline::from_slice(taps).unwrap(),
    }
}

/// Runs `input` (one `Vec` a channel) through `er` in blocks of `block` frames into
/// `er.channels()` output channels.
fn run(er: &mut EarlyReflections<f64>, input: &[Vec<f64>], block: usize) -> Vec<Vec<f64>> {
    let frames = input[0].len();
    let mut out = vec![vec![0.0; frames]; er.channels()];
    for start in (0..frames).step_by(block) {
        let end = (start + block).min(frames);
        let x: Vec<Vec<f64>> = input.iter().map(|c| c[start..end].to_vec()).collect();
        let mut y: Vec<Vec<f64>> = vec![vec![0.0; end - start]; out.len()];
        er.process(
            &mut InputOutput::paired(
                Planes::from_vecs(&x).unwrap(),
                PlanesMut::from_vecs(&mut y).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        for (o, y) in out.iter_mut().zip(&y) {
            o[start..end].copy_from_slice(y);
        }
    }
    out
}

fn impulse(frames: usize) -> Vec<Vec<f64>> {
    let mut x = vec![0.0; frames];
    x[0] = 1.0;
    vec![x]
}

/// The impulse response of a stage is `h_c[n] = sum of the gains of the taps of channel c at
/// delay n`, whatever the block size: a unit impulse puts exactly each tap's gain at its delay,
/// two taps on one frame add, and a tap at delay 0 is the direct path.
#[test]
fn an_impulse_gives_each_taps_gain_at_its_delay_at_any_block_size() {
    let taps = [
        tap(0.0, 0.25, 0),
        tap(5.0, -0.5, 0),
        tap(5.0, 0.125, 0),
        tap(70.0, 0.75, 1),
        tap(129.0, -1.0, 1),
        tap(200.0, 0.5, 2),
    ];
    let mut want = vec![vec![0.0; 256]; 3];
    for t in &taps {
        want[usize::from(t.channel)][t.delay.get() as usize] += t.gain.get();
    }
    for block in [1, 7, 64, 65, 256] {
        let mut er = EarlyReflections::<f64>::try_from(&stage(&taps)).unwrap();
        assert_eq!(er.channels(), 3);
        assert_eq!(run(&mut er, &impulse(256), block), want, "block {block}");
    }
}

/// A delay `n + f` is the blend of the frames around it: an impulse puts `g (1 - f)` at `n`
/// and `g f` at `n + 1`, and the two weights sum to the tap's gain.
#[test]
fn a_fractional_delay_splits_the_gain_between_the_frames_around_it() {
    let mut er = EarlyReflections::<f64>::try_from(&stage(&[tap(3.25, -0.8, 0)])).unwrap();
    let y = &run(&mut er, &impulse(8), 8)[0];
    assert_eq!(y[3], -0.8 * 0.75);
    assert_eq!(y[4], -0.8 * 0.25);
    assert_eq!(y.iter().filter(|v| **v != 0.0).count(), 2);
}

/// The stage takes the mean of its input's channels: an impulse on one of `k` channels is
/// heard at `g / k`, and the same impulse on all `k` at `g`.
#[test]
fn the_input_channels_are_averaged() {
    let taps = stage(&[tap(2.0, 0.5, 0)]);
    let mut one = vec![vec![0.0; 4]; 4];
    one[1][0] = 1.0;
    let mut er = EarlyReflections::<f64>::try_from(&taps).unwrap();
    assert_eq!(run(&mut er, &one, 4)[0][2], 0.5 / 4.0);
    let all = vec![vec![1.0, 0.0, 0.0, 0.0]; 4];
    er.reset();
    assert_eq!(run(&mut er, &all, 4)[0][2], 0.5);
}

/// The tail is the last tap, rounded up to a frame; the output is one channel past the highest
/// a tap feeds; an empty stage is one silent channel with no tail.
#[test]
fn the_tail_is_the_last_tap_and_the_width_is_the_highest_channel() {
    let er = EarlyReflections::<f32>::try_from(&stage(&[
        tap(10.0, 1.0, 0),
        tap(99.5, 0.5, 3),
        tap(40.0, 0.5, 1),
    ]))
    .unwrap();
    assert_eq!((er.tail(), er.channels()), (Samples::new(100), 4));
    let empty = EarlyReflections::<f32>::try_from(&TapSet::default()).unwrap();
    assert_eq!((empty.tail(), empty.channels()), (Samples::ZERO, 1));
}

/// The realization's response is the model's: after the last tap the output is exactly silent,
/// and `reset` drops a tap still in flight.
#[test]
fn the_output_ends_at_the_tail_and_reset_drops_what_is_in_flight() {
    let mut er = EarlyReflections::<f64>::try_from(&stage(&[tap(6.0, 1.0, 0)])).unwrap();
    let y = &run(&mut er, &impulse(64), 16)[0];
    assert!(y[er.tail().get() + 1..].iter().all(|v| *v == 0.0));
    er.reset();
    let _ = run(&mut er, &impulse(3), 3);
    er.reset();
    let after = run(&mut er, &[vec![0.0; 16]], 16);
    assert!(after[0].iter().all(|v| *v == 0.0));
}

/// A tap that cannot be realized is refused with its place: a gain that is not finite, a channel
/// past the stage's last and a delay past `MAX_TAP_DELAY`.
#[test]
fn a_tap_that_cannot_run_is_refused_with_its_index() {
    let build = |t: Tap| EarlyReflections::<f32>::try_from(&stage(&[tap(1.0, 1.0, 0), t]));
    assert_eq!(
        build(tap(1.0, f64::NAN, 0)).unwrap_err(),
        EarlyReflectionsError::Gain { index: 1 }
    );
    assert_eq!(
        build(tap(1.0, 1.0, MAX_CHANNELS as u8)).unwrap_err(),
        EarlyReflectionsError::Channel {
            index: 1,
            channel: MAX_CHANNELS as u8
        }
    );
    let past = MAX_TAP_DELAY.get() as f64 + 1.0;
    assert!(matches!(
        build(tap(past, 1.0, 0)).unwrap_err(),
        EarlyReflectionsError::Delay { index: 1, .. }
    ));
}
