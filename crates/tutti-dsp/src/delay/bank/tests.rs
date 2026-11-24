use tutti_buffers::ChannelTopology;
use tutti_buffers::{Mono, Planes, PlanesMut};
use tutti_fixtures::signal::impulse;
use tutti_music::{NoteDuration, NoteValue};
use tutti_units::Samples;
use tutti_units::{Bpm, Feedback, SampleRate, Seconds};

use super::*;
use crate::{ChannelTimes, FeedbackRouting, GAIN_RAMP, MaxDelay, NoteTimes};
use tutti_buffers::InputOutput;

// The tests other than the `f64` ones pin the `f32` instantiation, the session's audio
// format, so their tolerances are `f32`'s.

/// A rate at which a time of `n / 1024` seconds is exactly `n` frames, in `f32` and in `f64`.
const BINARY: SampleRate = SampleRate::of(1024.0);
const MAX: MaxDelay = MaxDelay::of(Seconds::new(1.0));

fn controls(time: f64, feedback: f64, cross: f64) -> DelayControls {
    DelayControls {
        sync: DelaySync::Free,
        time: Seconds::new(time),
        feedback: Feedback::new(feedback),
        cross_feedback: Feedback::new(cross),
    }
}

fn bus(width: u16) -> ChannelTopology {
    ChannelTopology::default_for(width.into()).expect("a test width is one bus")
}

fn mono(mode: InterpolationMode) -> DelayLayout {
    DelayLayout::new(ChannelTopology::MONO, MAX).interpolation(mode)
}

/// A stereo delay: the default swap routing, the channels linked in time.
fn stereo() -> DelayLayout {
    DelayLayout::new(ChannelTopology::STEREO, MAX)
}

/// A stereo delay with no routing between the channels and a time of its own for the right.
fn stereo_apart(right: f64) -> DelayLayout {
    DelayLayout::builder(ChannelTopology::STEREO, MAX)
        .routing(FeedbackRouting::none(ChannelTopology::STEREO))
        .times(ChannelTimes::try_from(&[Seconds::new(right)][..]).unwrap())
        .build()
        .unwrap()
}

/// A bank and its ramps, as a kernel holds them.
struct Rig {
    bank: DelayBank<f32>,
    ramps: DelayRamps<f32>,
}

impl Rig {
    /// `layout` at `rate`, at rest on `c`.
    fn at(layout: &DelayLayout, c: &DelayControls, rate: SampleRate) -> Self {
        let steps = u32::try_from(GAIN_RAMP.to_samples_ceil(rate).get()).unwrap();
        let mut rig = Self {
            bank: DelayBank::new(layout, rate),
            ramps: DelayRamps::new(steps),
        };
        rig.retarget(c);
        rig.ramps.jump_to_target();
        rig
    }

    fn retarget(&mut self, c: &DelayControls) {
        self.bank.retarget(&mut self.ramps, c, None);
    }

    /// `audio` (one slice per channel) through the bank in blocks of `block`, with the
    /// time and feedback control signals when given.
    fn run_in(
        &mut self,
        audio: &[Vec<f32>],
        signals: [Option<&[f32]>; 2],
        block: usize,
    ) -> Vec<Vec<f32>> {
        let n = audio[0].len();
        let mut out = vec![vec![0.0f32; n]; audio.len()];
        let mut at = 0;
        while at < n {
            let end = (at + block).min(n);
            let ins: Vec<&[f32]> = audio.iter().map(|c| &c[at..end]).collect();
            let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[at..end]).collect();
            let signals = DelaySignals {
                time: signals[0].map(|l| Mono::new(&l[at..end])),
                feedback: signals[1].map(|l| Mono::new(&l[at..end])),
            };
            self.bank
                .process(
                    &mut self.ramps,
                    signals,
                    &mut InputOutput::paired(
                        Planes::new(&ins).unwrap(),
                        PlanesMut::new(&mut outs).unwrap(),
                    )
                    .unwrap(),
                )
                .unwrap();
            at = end;
        }
        out
    }

    fn run(&mut self, audio: &[Vec<f32>]) -> Vec<Vec<f32>> {
        self.run_in(audio, [None, None], 64)
    }
}

/// A rig of `layout` at `c`, at [`BINARY`].
fn runner(layout: &DelayLayout, c: &DelayControls) -> Rig {
    Rig::at(layout, c, BINARY)
}

/// An impulse is heard exactly `d` frames later, in each interpolation mode, at every
/// whole delay: the line is `d` frames long and nothing else is heard.
#[test]
fn an_impulse_arrives_at_exactly_the_delay() {
    for mode in [
        InterpolationMode::None,
        InterpolationMode::Linear,
        InterpolationMode::CubicHermite,
    ] {
        for d in [1usize, 2, 3, 16, 100, 513] {
            let mut k = runner(&mono(mode), &controls(d as f64 / 1024.0, 0.0, 0.0));
            let y = k.run(&[impulse(Samples::new(d + 40))]).remove(0);
            for (i, &s) in y.iter().enumerate() {
                let want = f32::from(i == d);
                assert_eq!(s, want, "{mode:?}, delay {d}, frame {i}");
            }
        }
    }
}

/// A delay of `k + f` frames blends the two frames around it: `1 - f` of the impulse at
/// `k` and `f` of it at `k + 1`, for the linear read; and the half frame of a delay under
/// one frame blends the impulse with the silence before it.
#[test]
fn a_fractional_delay_blends_the_frames_around_it_by_the_fraction() {
    for (d, at, f) in [
        (10.25f64, 10usize, 0.25f32),
        (7.5, 7, 0.5),
        (3.875, 3, 0.875),
        (0.5, 0, 0.5),
    ] {
        let mut k = runner(
            &mono(InterpolationMode::Linear),
            &controls(d / 1024.0, 0.0, 0.0),
        );
        let y = k.run(&[impulse(Samples::new(40))]).remove(0);
        for (i, &s) in y.iter().enumerate() {
            let want = if i == at {
                1.0 - f
            } else if i == at + 1 {
                f
            } else {
                0.0
            };
            assert_eq!(s, want, "delay {d}, frame {i}");
        }
    }
}

/// With feedback `g` and a whole delay of `D` frames the impulse response is `g^k` at
/// `(k + 1) D` and silence between: each echo is the last one scaled by the feedback, a
/// period on.
#[test]
fn feedback_decays_by_the_gain_each_trip() {
    let (d, g) = (16usize, 0.6f64);
    let mut k = runner(
        &mono(InterpolationMode::Linear),
        &controls(d as f64 / 1024.0, g, 0.0),
    );
    let y = k.run(&[impulse(Samples::new(d * 12 + 5))]).remove(0);
    for (i, &s) in y.iter().enumerate() {
        let want = if i >= d && i % d == 0 {
            g.powi((i / d - 1) as i32)
        } else {
            0.0
        };
        // The samples are `f32`, which sets the tolerance.
        assert!(
            (f64::from(s) - want).abs() < 1e-6,
            "frame {i}: {s}, want {want}"
        );
    }
}

/// The stereo ping-pong from the definition. An impulse in the left of equal delays `D`
/// splits into the common mode, which recirculates with the gain `g + c`, and the
/// differential mode, with `g - c` (the swap routing adds the other channel's tap): so the
/// left is heard at `(n + 1) D` with `((g + c)^n + (g - c)^n) / 2` and the right with
/// `((g + c)^n - (g - c)^n) / 2`.
#[test]
fn a_ping_pong_splits_into_a_common_and_a_differential_mode() {
    let (d, g, c) = (8usize, 0.3f64, 0.4f64);
    let mut k = runner(&stereo(), &controls(d as f64 / 1024.0, g, c));
    let len = d * 10 + 3;
    let y = k.run(&[impulse(Samples::new(len)), vec![0.0; len]]);
    for n in 0..9usize {
        let (sum, diff) = ((g + c).powi(n as i32), (g - c).powi(n as i32));
        for (ch, want) in [(0, (sum + diff) / 2.0), (1, (sum - diff) / 2.0)] {
            let got = f64::from(y[ch][(n + 1) * d]);
            assert!(
                (got - want).abs() < 1e-6,
                "channel {ch}, echo {n}: {got}, want {want}"
            );
        }
    }
    for (ch, channel) in y.iter().enumerate() {
        for (i, &s) in channel.iter().enumerate() {
            if i % d != 0 || i == 0 {
                assert_eq!(
                    s, 0.0,
                    "channel {ch}, frame {i}: nothing between the echoes"
                );
            }
        }
    }
}

/// With no routing the cross-feedback is inert, and a channel's time past the first is
/// its own when the channels are not linked and channel 0's when they are: the impulse of
/// each channel comes back at its own delay.
#[test]
fn channel_times_are_linked_or_each_their_own() {
    let both = vec![impulse(Samples::new(60)), impulse(Samples::new(60))];
    let heard = |y: &[Vec<f32>]| {
        y.iter()
            .map(|c| c.iter().position(|&s| s != 0.0).unwrap())
            .collect::<Vec<_>>()
    };
    // Each its own: the right at 40 frames, the left at 10; the cross-feedback is inert.
    let mut k = runner(
        &stereo_apart(40.0 / 1024.0),
        &controls(10.0 / 1024.0, 0.0, 0.5),
    );
    assert_eq!(heard(&k.run(&both)), [10, 40]);
    // Linked: every channel follows the control.
    let linked = DelayLayout::builder(ChannelTopology::STEREO, MAX)
        .routing(FeedbackRouting::none(ChannelTopology::STEREO))
        .build()
        .unwrap();
    let mut k = runner(&linked, &controls(10.0 / 1024.0, 0.0, 0.0));
    assert_eq!(heard(&k.run(&both)), [10, 10]);
    // Six channels with no routing never recirculate across channels, whatever the
    // cross-feedback says: an impulse in channel 3 is heard on channel 3 alone.
    let wide = DelayLayout::new(bus(6), MAX);
    let mut k = runner(&wide, &controls(10.0 / 1024.0, 0.0, 0.9));
    let mut x = vec![vec![0.0; 60]; 6];
    x[3] = impulse(Samples::new(60));
    let y = k.run(&x);
    for (ch, c) in y.iter().enumerate() {
        assert_eq!(c.iter().any(|&s| s != 0.0), ch == 3, "channel {ch}");
    }
}

/// A delay moves no faster than [`DELAY_SLEW`] frames of delay per frame, however far its
/// time is moved, and arrives exactly on the time. A linear read of a ramp is exact, so
/// the delay in use at frame `n` is `n - y[n]`, read off the output: a time moved from 100
/// to 400 frames ramps for `300 / 0.125 = 2400` frames, never faster, and holds there.
#[test]
fn a_time_ramp_is_bounded_by_the_slew_and_lands_exactly() {
    let family = mono(InterpolationMode::Linear);
    let mut k = Rig::at(
        &family,
        &controls(100.0 / 1000.0, 0.0, 0.0),
        SampleRate::of(1000.0),
    );
    k.retarget(&controls(400.0 / 1000.0, 0.0, 0.0));
    let len = 4_000;
    let ramp: Vec<f32> = (0..len).map(|n| n as f32).collect();
    let y = k.run(&[ramp]).remove(0);
    let delay: Vec<f64> = (500..len).map(|n| n as f64 - f64::from(y[n])).collect();
    let worst = delay
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f64::max);
    assert!(
        worst <= DELAY_SLEW + 2e-3,
        "the delay moved {worst} frames in one"
    );
    assert!(
        delay.windows(2).all(|w| w[1] >= w[0] - 2e-3),
        "the ramp is monotone"
    );
    // At frame `n` the ramp has taken `n + 1` steps of the 2400: still short of 400 until
    // the last, and on it from then on.
    let at = |n: usize| delay[n - 500];
    assert!(
        (at(1000) - (100.0 + 300.0 * 1001.0 / 2400.0)).abs() < 2e-3,
        "{}",
        at(1000)
    );
    assert!(
        (at(2399) - 400.0).abs() < 2e-3,
        "arrived on the time: {}",
        at(2399)
    );
    assert!((at(3999) - 400.0).abs() < 2e-3, "and held: {}", at(3999));
}

/// A time control signal, in seconds, is the delay of **every frame**: a ramp through a delay swept
/// by a sine (a flanger's) is the ramp at the control signal's time, `y[n] = n - tau[n]`, read off
/// the output frame by frame; and a frame that is not finite, or not a time, is the
/// ramped delay (here 20 frames), and one past the longest delay is the longest.
#[test]
fn a_time_signal_is_the_delay_of_each_frame() {
    let family = mono(InterpolationMode::Linear);
    let len = 4_000;
    let tau = |n: usize| -> f32 {
        match n % 500 {
            100 => f32::NAN,
            200 => f32::INFINITY,
            300 => 1e9,
            _ => (40.0 + 25.0 * (n as f32 * 0.01).sin()) / 1024.0,
        }
    };
    let signal: Vec<f32> = (0..len).map(tau).collect();
    let ramp: Vec<f32> = (0..len).map(|n| n as f32).collect();
    let mut k = runner(&family, &controls(20.0 / 1024.0, 0.0, 0.0));
    let y = k.run_in(&[ramp], [Some(&signal), None], 64).remove(0);
    for n in 1_200..len {
        let want = match n % 500 {
            100 | 200 => 20.0,
            // Past the longest delay: the longest (1024 frames), which a ramp of fewer
            // frames has not reached: the line holds only the zeros from before the start.
            300 => 1024.0,
            _ => f64::from(signal[n]) * 1024.0,
        };
        let got = n as f64 - f64::from(y[n]);
        if n % 500 == 300 {
            continue;
        }
        assert!(
            (got - want).abs() < 2e-3,
            "frame {n}: delay {got}, want {want}"
        );
    }
    // The longest: an impulse swept to 1e9 s is heard 1024 frames on, not before and not
    // never.
    let mut x = impulse(Samples::new(1_300));
    x[0] = 1.0;
    let signal = vec![1e9f32; 1_300];
    let mut k = runner(&family, &controls(20.0 / 1024.0, 0.0, 0.0));
    let y = k.run_in(&[x], [Some(&signal), None], 64).remove(0);
    assert_eq!(y.iter().position(|&s| s != 0.0), Some(1024));
}

/// A feedback control signal is the feedback of every frame: held at `g` it is the echo train of a
/// cell at `g`; a frame that is not finite is the ramped cell's (here 0.5); and a value
/// past the stable range is the range's last (0.99).
#[test]
fn a_feedback_signal_is_the_feedback_of_each_frame() {
    let (d, len) = (16usize, 16 * 8 + 5);
    let family = mono(InterpolationMode::Linear);
    let train = |g: f32, k: usize| g.powi(k as i32);
    for (level, g) in [
        (0.8f32, 0.8f32),
        (f32::NAN, 0.5),
        (f32::INFINITY, 0.5),
        (5.0, 0.99),
        (-2.0, 0.0),
    ] {
        let signal = vec![level; len];
        let mut k = runner(&family, &controls(d as f64 / 1024.0, 0.5, 0.0));
        let y = k
            .run_in(&[impulse(Samples::new(len))], [None, Some(&signal)], 64)
            .remove(0);
        for (i, &s) in y.iter().enumerate() {
            let want = if i >= d && i % d == 0 && (i / d == 1 || g > 0.0) {
                train(g, i / d - 1)
            } else {
                0.0
            };
            assert!(
                (s - want).abs() < 1e-5,
                "signal gives {g}, frame {i}: {s}, want {want}"
            );
        }
    }
}

/// A feedback cell that moves ramps over [`GAIN_RAMP`](crate::GAIN_RAMP) (5 ms: 240
/// frames at 48 kHz), counted on the stream. The first echo has the feedback of its own
/// frame: an echo at `2 D` is the feedback `D + 1` steps into the ramp, so `D = 100` of
/// the 240 steps from 0.2 to 0.8 is `0.2 + 0.6 * 101 / 240`, and one past the ramp, `D = 300`, is
/// 0.8.
#[test]
fn a_feedback_ramps_over_five_milliseconds_counted_on_the_stream() {
    let rate = SampleRate::SR_48K;
    let family = mono(InterpolationMode::Linear);
    for (d, want) in [(100usize, 0.2 + 0.6 * 101.0 / 240.0), (300, 0.8)] {
        let c = |fb| controls(d as f64 / rate.get(), fb, 0.0);
        let mut k = Rig::at(&family, &c(0.2), rate);
        k.retarget(&c(0.8));
        let y = k.run(&[impulse(Samples::new(2 * d + 8))]).remove(0);
        let got = f64::from(y[2 * d]);
        assert!((got - want).abs() < 1e-5, "D {d}: {got}, want {want}");
    }
}

/// A time past the longest, or not a time, is a delay the line holds: it reads as the
/// longest, as none, and as none.
#[test]
fn a_hostile_time_is_a_delay_within_the_line() {
    for (time, want) in [
        (1e9, Some(1024)),
        (-5.0, Some(0)),
        (f64::NAN, Some(0)),
        (f64::INFINITY, Some(0)),
        (0.5, Some(512)),
    ] {
        let mut k = runner(&mono(InterpolationMode::Linear), &controls(time, 0.0, 0.0));
        let y = k.run(&[impulse(Samples::new(1_100))]).remove(0);
        assert_eq!(y.iter().position(|&s| s != 0.0), want, "{time}");
    }
}

/// The feedback is bounded as a pair where it is read: a feedback asked at 5 rings as the
/// greatest stable one, 0.99, and one that is not a number as none.
#[test]
fn a_feedback_past_the_stable_range_is_the_greatest_stable_one() {
    let d = 16;
    for (ask, g) in [(5.0f64, 0.99f64), (f64::NAN, 0.0), (-1.0, 0.0)] {
        let mut k = runner(
            &mono(InterpolationMode::Linear),
            &controls(d as f64 / 1024.0, ask, 0.0),
        );
        let y = k.run(&[impulse(Samples::new(d * 4 + 1))]).remove(0);
        let g = if g == 0.99 {
            Feedback::MAX_STABLE.get()
        } else {
            g
        };
        // The samples are `f32`, which sets the tolerance.
        assert!(
            (f64::from(y[2 * d]) - g).abs() < 1e-6,
            "asked {ask}: {}",
            y[2 * d]
        );
    }
}

/// The render is a function of the stream, not of where its blocks fall: a moving delay,
/// both feedbacks ramping and both control signals driving give the same bits in any split of the
/// stream, on the coupled loop and the uncoupled one.
#[test]
fn any_split_of_the_stream_renders_the_same_bits() {
    let len = 1_500;
    let x: Vec<Vec<f32>> = (0..2)
        .map(|c| {
            (0..len)
                .map(|n| ((n * (7 + c) + 3) % 100) as f32 / 50.0 - 1.0)
                .collect()
        })
        .collect();
    let time: Vec<f32> = (0..len)
        .map(|n| match n % 400 {
            100 => f32::NAN,
            _ => (30.0 + 20.0 * (n as f32 * 0.02).sin()) / 1024.0,
        })
        .collect();
    let feedback: Vec<f32> = (0..len)
        .map(|n| 0.3 + 0.4 * (n as f32 * 0.003).sin())
        .collect();
    for layout in [stereo(), stereo_apart(40.0 / 1024.0)] {
        for signals in [[None, None], [Some(&time[..]), Some(&feedback[..])]] {
            let render = |block: usize| {
                let mut k = Rig::at(&layout, &controls(10.0 / 1024.0, 0.2, 0.1), BINARY);
                k.retarget(&controls(300.0 / 1024.0, 0.7, 0.5));
                k.run_in(&x, signals, block)
            };
            let whole = render(len);
            for block in [1, 7, 16, 64, 333] {
                assert_eq!(render(block), whole, "blocks of {block}");
            }
            assert!(whole[0].iter().any(|&s| s != 0.0));
        }
    }
}

/// Reset is silence: after a loud stretch and a reset, silence in is exactly silence out,
/// the echoes in flight gone.
#[test]
fn reset_empties_every_line() {
    for layout in [stereo(), stereo_apart(40.0 / 1024.0)] {
        let mut k = runner(&layout, &controls(20.0 / 1024.0, 0.9, 0.5));
        let loud: Vec<Vec<f32>> = (0..2).map(|c| noise(c, 600)).collect();
        assert!(k.run(&loud)[0].iter().any(|&s| s != 0.0));
        k.bank.reset();
        let silent = k.run(&[vec![0.0; 600], vec![0.0; 600]]);
        assert!(silent.iter().flatten().all(|&s| s == 0.0));
    }
}

/// Deterministic input in `-1..1`.
fn noise(seed: usize, len: usize) -> Vec<f32> {
    (0..len)
        .map(|n| (((n + seed) * 2_654_435_761usize) % 1_000) as f32 / 500.0 - 1.0)
        .collect()
}

/// A clone keeps the lines' size and what is in them: an echo in flight in the original,
/// a full line's length away, is heard from the clone.
#[test]
fn a_clone_keeps_the_capacity_and_the_echoes_in_flight() {
    let mut k = runner(&mono(InterpolationMode::Linear), &controls(1e9, 0.0, 0.0));
    k.run(&[impulse(Samples::new(100))]);
    let mut copy = k.bank.clone();
    let silence = vec![0.0f32; 1_100];
    let mut out = vec![0.0f32; 1_100];
    copy.process(
        &mut k.ramps,
        DelaySignals::default(),
        &mut InputOutput::paired(
            Planes::new(&[&silence[..]]).unwrap(),
            PlanesMut::new(&mut [&mut out[..]]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(out.iter().position(|&s| s != 0.0), Some(1024 - 100));
}

/// An `f64` bank hears an impulse at the delay.
#[test]
fn an_f64_impulse_arrives_at_the_delay() {
    let mut bank = DelayBank::<f64>::new(&mono(InterpolationMode::Linear), BINARY);
    let mut ramps = DelayRamps::<f64>::new(1);
    bank.retarget(&mut ramps, &controls(20.0 / 1024.0, 0.0, 0.0), None);
    ramps.jump_to_target();
    let mut x = vec![0.0f64; 64];
    x[0] = 1.0;
    let mut y = vec![0.0f64; 64];
    let ins = [&x[..]];
    let mut outs = [&mut y[..]];
    bank.process(
        &mut ramps,
        DelaySignals::default(),
        &mut InputOutput::paired(
            Planes::new(&ins).unwrap(),
            PlanesMut::new(&mut outs).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(y.iter().position(|&s| s != 0.0), Some(20));
}

/// `x` (one slice per channel) through an `f64` bank of `layout` at `c`, in one block.
fn run_wide(layout: &DelayLayout, c: &DelayControls, x: &[Vec<f64>]) -> Vec<Vec<f64>> {
    let mut bank = DelayBank::<f64>::new(layout, BINARY);
    let mut ramps = DelayRamps::<f64>::new(1);
    bank.retarget(&mut ramps, c, None);
    ramps.jump_to_target();
    let ins: Vec<&[f64]> = x.iter().map(Vec::as_slice).collect();
    let mut out = vec![vec![0.0f64; x[0].len()]; x.len()];
    let mut outs: Vec<&mut [f64]> = out.iter_mut().map(Vec::as_mut_slice).collect();
    bank.process(
        &mut ramps,
        DelaySignals::default(),
        &mut InputOutput::paired(
            Planes::new(&ins).unwrap(),
            PlanesMut::new(&mut outs).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    out
}

/// An `f64` bank keeps its state in `f64`: at a whole delay `d` with no feedback a signal
/// of more than 24 significant bits returns `d` frames later exactly, and with feedback `g`
/// it is the loop `v[n] = x[n] + g v[n - d]` heard at `y[n] = v[n - d]`, to 2^-45; an `f32` state
/// is off by 1e-8.
#[test]
fn an_f64_bank_returns_a_wide_signal_d_frames_later_and_runs_the_loop_in_f64() {
    use tutti_fixtures::signal::{assert_close, wide_signal};
    let (d, len) = (16usize, 200usize);
    let x = wide_signal(len);
    let mono = mono(InterpolationMode::Linear);
    let time = d as f64 / 1024.0;
    let delayed = |v: &[f64]| -> Vec<f64> {
        (0..len)
            .map(|n| n.checked_sub(d).map_or(0.0, |k| v[k]))
            .collect()
    };
    let y = run_wide(&mono, &controls(time, 0.0, 0.0), std::slice::from_ref(&x));
    assert_close(&y[0], &delayed(&x), 0.0);

    let g = 0.6;
    let mut v = vec![0.0f64; len];
    for n in 0..len {
        v[n] = x[n] + g * n.checked_sub(d).map_or(0.0, |k| v[k]);
    }
    let y = run_wide(&mono, &controls(time, 0.6, 0.0), &[x]);
    assert_close(&y[0], &delayed(&v), 2.0f64.powi(-45));
}

/// The cross-fed loop of an `f64` bank is the two-channel recursion `v0[n] = x[n] + g v0[n -
/// d] + c v1[n - d]`, `v1[n] = g v1[n - d] + c v0[n - d]` of the swap routing, to 2^-45.
#[test]
fn an_f64_ping_pong_runs_the_coupled_loop_in_f64() {
    use tutti_fixtures::signal::{assert_close, wide_signal};
    let (d, len) = (8usize, 120usize);
    let x = wide_signal(len);
    let (g, c) = (0.3, 0.4);
    let (mut v0, mut v1) = (vec![0.0f64; len], vec![0.0f64; len]);
    for n in 0..len {
        let past = |v: &[f64]| n.checked_sub(d).map_or(0.0, |k| v[k]);
        let (p0, p1) = (past(&v0), past(&v1));
        v0[n] = x[n] + g * p0 + c * p1;
        v1[n] = g * p1 + c * p0;
    }
    let delayed = |v: &[f64]| -> Vec<f64> {
        (0..len)
            .map(|n| n.checked_sub(d).map_or(0.0, |k| v[k]))
            .collect()
    };
    let y = run_wide(
        &stereo(),
        &controls(d as f64 / 1024.0, 0.3, 0.4),
        &[x, vec![0.0; len]],
    );
    assert_close(&y[0], &delayed(&v0), 2.0f64.powi(-45));
    assert_close(&y[1], &delayed(&v1), 2.0f64.powi(-45));
}

/// A mono synced delay of `note` that holds up to four seconds.
fn synced(note: NoteDuration) -> DelayLayout {
    DelayLayout::builder(ChannelTopology::MONO, MaxDelay::of(Seconds::new(4.0)))
        .notes(NoteTimes::all(note))
        .build()
        .unwrap()
}

/// `time`, with no feedback, read while `sync`.
fn timed(sync: DelaySync, time: f64) -> DelayControls {
    DelayControls {
        sync,
        ..controls(time, 0.0, 0.0)
    }
}

/// A rig of `layout` at rest, synced, on `tempo`.
fn synced_at(layout: &DelayLayout, tempo: Bpm) -> Rig {
    let mut rig = Rig::at(layout, &DelayControls::DEFAULT, BINARY);
    rig.bank
        .retarget(&mut rig.ramps, &timed(DelaySync::Synced, 0.0), Some(tempo));
    rig.ramps.jump_to_target();
    rig
}

/// The time of a synced delay is `beats * 60 / bpm * rate` frames: a quarter is one beat, a
/// dotted note one and a half times its value and a triplet two thirds, at two tempos.
#[test]
fn a_synced_time_is_the_notes_beats_over_the_tempo_in_frames() {
    // (the note, its beats written out, not derived)
    let notes = [
        (NoteDuration::QUARTER, 1.0),
        (NoteDuration::new(NoteValue::QUARTER).dotted(), 1.5),
        (NoteDuration::new(NoteValue::QUARTER).triplet(), 2.0 / 3.0),
        (NoteDuration::new(NoteValue::EIGHTH).dotted(), 0.75),
        (NoteDuration::new(NoteValue::HALF), 2.0),
    ];
    for bpm in [120.0, 90.0] {
        for (note, beats) in notes {
            let rig = synced_at(&synced(note), Bpm::of(bpm));
            let want = beats * 60.0 / bpm * BINARY.get();
            let got = rig.ramps.times[0].target().span().get();
            assert!(
                (got - want).abs() < 1e-6,
                "{note:?} at {bpm} bpm: {got} frames, wanted {want}"
            );
        }
    }
}

/// A tempo that moves moves the time at `DELAY_SLEW` frames of delay per frame, not at
/// once. A quarter note at 120 bpm is 512 frames and at 60 bpm 1024: an echo of a ramp
/// input at frame `i` after the change reads `i + 1` steps of 1/8 frame along, so the output
/// is the input a delay of `512 + (i + 1) / 8` frames earlier. A jump would read 1024 at once.
#[test]
fn a_tempo_change_ramps_the_time_at_the_slew() {
    let mut rig = synced_at(&synced(NoteDuration::QUARTER), Bpm::of(120.0));
    let ramp =
        |from: usize, to: usize| -> Vec<f32> { (from..to).map(|g| g as f32 + 1.0).collect() };
    let settled = rig.run(&[ramp(0, 1024)]).remove(0);
    for (g, &y) in settled.iter().enumerate().skip(512) {
        assert!((y - (g as f32 + 1.0 - 512.0)).abs() < 1e-3, "frame {g}");
    }
    rig.bank.retarget(
        &mut rig.ramps,
        &timed(DelaySync::Synced, 0.0),
        Some(Bpm::of(60.0)),
    );
    assert_eq!(rig.ramps.times[0].target().span().get(), 1024.0);
    let moved = rig.run(&[ramp(1024, 2048)]).remove(0);
    for (i, &y) in moved.iter().enumerate() {
        let delay = 512.0 + (i as f32 + 1.0) / 8.0;
        let want = (1024 + i) as f32 + 1.0 - delay;
        assert!(
            (y - want).abs() < 1e-3,
            "frame {i} after the change: {y}, wanted {want}"
        );
    }
}

/// The source not in use moves nothing, and is kept. A quarter note at 120 bpm is 512 frames:
/// a time control of any value while synced leaves it, and a switch to free then aims at
/// that control (0.25 s, 256 frames). A tempo of any value while free leaves the 256, and a
/// switch to synced then aims at the note at that tempo (30 bpm: 2 s, 2048 frames).
#[test]
fn the_time_while_synced_and_the_tempo_while_free_move_nothing_and_are_kept() {
    let layout = synced(NoteDuration::QUARTER);
    let target = |rig: &Rig| rig.ramps.times[0].target().span().get();

    let mut rig = synced_at(&layout, Bpm::of(120.0));
    let tempo = Some(Bpm::of(120.0));
    rig.bank
        .retarget(&mut rig.ramps, &timed(DelaySync::Synced, 0.25), tempo);
    assert_eq!(target(&rig), 512.0);
    rig.bank
        .retarget(&mut rig.ramps, &timed(DelaySync::Free, 0.25), tempo);
    assert_eq!(target(&rig), 256.0);

    let tempo = Some(Bpm::of(30.0));
    rig.bank
        .retarget(&mut rig.ramps, &timed(DelaySync::Free, 0.25), tempo);
    assert_eq!(target(&rig), 256.0);
    rig.bank
        .retarget(&mut rig.ramps, &timed(DelaySync::Synced, 0.25), tempo);
    assert_eq!(target(&rig), 2048.0);
}

/// A synced delay with no tempo yet holds: its times stay aimed where they were (256 frames),
/// and the first tempo aims them at the note (a quarter at 120 bpm, 512 frames).
#[test]
fn a_synced_delay_with_no_tempo_yet_holds_its_times() {
    let mut rig = Rig::at(
        &synced(NoteDuration::QUARTER),
        &timed(DelaySync::Free, 0.25),
        BINARY,
    );
    rig.bank
        .retarget(&mut rig.ramps, &timed(DelaySync::Synced, 0.25), None);
    assert_eq!(rig.ramps.times[0].target().span().get(), 256.0);
    assert!(!rig.ramps.times[0].moving());
    rig.bank.retarget(
        &mut rig.ramps,
        &timed(DelaySync::Synced, 0.25),
        Some(Bpm::of(120.0)),
    );
    assert_eq!(rig.ramps.times[0].target().span().get(), 512.0);
}

/// A switch on a running delay keeps what the lines hold and moves each channel's time from
/// where it is to the other source's at `DELAY_SLEW` (1/8 frame of delay per frame), with no
/// jump, in both directions, for channels linked in seconds and for a channel with a time of
/// its own.
///
/// The input is the ramp `x[g] = g + 1` on both channels, which a fractional read reproduces
/// exactly, so the output at frame `g` is `g + 1 - delay(g)`: the delay each channel has at
/// that frame, read off the output. `i` frames after a switch a channel `dist` frames from
/// its new time is `min(i + 1, 8 * dist) / 8` frames along. The first frames after a switch
/// read input fed before it: the lines were not emptied.
///
/// At 1024 frames a second: free, channel 0 is 0.25 s (256 frames) and channel 1 follows it
/// or holds 0.75 s (768); synced at 120 bpm, channel 0 is a quarter (512) and channel 1 a
/// half (1024).
#[test]
fn a_switch_on_a_running_delay_keeps_the_lines_and_slews_every_channel_from_where_it_was() {
    let notes =
        NoteTimes::try_from(&[NoteDuration::QUARTER, NoteDuration::new(NoteValue::HALF)][..])
            .unwrap();
    let builder = || {
        DelayLayout::builder(ChannelTopology::STEREO, MaxDelay::of(Seconds::new(4.0))).notes(notes)
    };
    let own = ChannelTimes::try_from(&[Seconds::new(0.75)][..]).unwrap();
    let cases = [
        ("linked", builder().build().unwrap(), [256.0f64, 256.0]),
        (
            "a time of its own",
            builder().times(own).build().unwrap(),
            [256.0, 768.0],
        ),
    ];
    let synced_frames = [512.0f64, 1024.0];
    let tempo = Some(Bpm::of(120.0));
    // Longer than the longest move: 768 frames of delay at 1/8 a frame.
    const RUN: usize = 6400;
    for (name, layout, free_frames) in cases {
        let mut rig = Rig::at(&layout, &timed(DelaySync::Free, 0.25), BINARY);
        let mut g = 0usize;
        let mut run = |rig: &mut Rig, len: usize| -> (usize, Vec<Vec<f32>>) {
            let x: Vec<f32> = (g..g + len).map(|g| g as f32 + 1.0).collect();
            let first = g;
            g += len;
            (first, rig.run(&[x.clone(), x]))
        };
        // Settled, free: each channel reads its own seconds.
        let (first, y) = run(&mut rig, 2048);
        for c in 0..2 {
            let at = first + 2047;
            let want = at as f64 + 1.0 - free_frames[c];
            assert!(
                (f64::from(y[c][2047]) - want).abs() < 1e-2,
                "{name}, channel {c}, settled"
            );
        }
        let switches = [
            (DelaySync::Synced, free_frames, synced_frames),
            (DelaySync::Free, synced_frames, free_frames),
        ];
        for (sync, from, to) in switches {
            rig.bank.retarget(&mut rig.ramps, &timed(sync, 0.25), tempo);
            let (first, y) = run(&mut rig, RUN);
            for c in 0..2 {
                let dist = (to[c] - from[c]).abs();
                let sign = (to[c] - from[c]).signum();
                for (i, &v) in y[c].iter().enumerate() {
                    let along = ((i + 1) as f64 / 8.0).min(dist);
                    let delay = if dist == 0.0 {
                        from[c]
                    } else {
                        from[c] + sign * along
                    };
                    let want = (first + i) as f64 + 1.0 - delay;
                    assert!(
                        (f64::from(v) - want).abs() < 1e-2,
                        "{name}, to {sync:?}, channel {c}, frame {i}: {v}, wanted {want}"
                    );
                }
            }
        }
    }
}
