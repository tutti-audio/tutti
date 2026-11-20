//! A stretching voice plays what the phase vocoder's spec says it does, through
//! the real voice: the transport seats the read, the vocoder takes the source
//! frames it asks for, and the sampler's mix carries its output.
//!
//! The figures are the vocoder's own (`tutti_dsp::PhaseVocoder`): a stereo image
//! kept bit for bit and to the transforms' rounding, a duration of `N` times the
//! ratio to within a hop, a shift of `f 2^(cents / 1200)`, a level that holds
//! across a step, and a latency the voice reports.

use std::f64::consts::{PI, TAU};
use std::sync::Arc;

use tutti_buffers::{ChannelLayout, Interleaved, Planes, Wave};
use tutti_dsp::{ColaGeometry, HopSize, PhaseVocoder, StftGeometry, WindowLength};
use tutti_graph::{Node, contract};
use tutti_sampler::testing::MockTransport;
use tutti_sampler::{
    MemorySource, MemorySourceConfig, Playback, SlotId, Voice, VoiceNode, VoicePool,
    VoicePoolHandle, VoiceSource, VoiceWindow,
};
use tutti_units::{Beat, Bpm, Cents, SampleRate, Samples, StretchFactor};

const SR: f64 = 48_000.0;
const BLOCK: usize = 64;
/// The vocoder's window and hop, the sampler's: 2048 frames, hop 512.
const WINDOW: usize = 2048;
const HOP: usize = WINDOW / 4;

/// A wave of `frames` frames: channel `c` of frame `i` is `channels[c](i)`.
fn wave(frames: usize, channels: &[&dyn Fn(usize) -> f64]) -> Arc<Wave> {
    let mut wave = Wave::new(channels.len(), SampleRate::of(SR));
    for i in 0..frames {
        let frame: Vec<f32> = channels.iter().map(|c| c(i) as f32).collect();
        wave.append(Interleaved::new(&frame, wave.layout()).unwrap())
            .unwrap();
    }
    Arc::new(wave)
}

fn sine(f: f64, amplitude: f64, phase: f64) -> impl Fn(usize) -> f64 {
    move |i| amplitude * (TAU * f * i as f64 / SR + phase).sin()
}

fn source(wave: &Arc<Wave>) -> MemorySource<f32> {
    MemorySource::with_config(
        Arc::clone(wave),
        MemorySourceConfig {
            placement: Some(VoiceWindow::from(Beat::new(0.0))),
            channels: wave.layout(),
            ..Default::default()
        },
    )
    .expect("a placeable wave")
}

/// `wave` placed at beat 0 in a pool, stretched by `stretch` and shifted by
/// `cents`, rendered `frames` frames in blocks of [`BLOCK`]; `each` runs before
/// every block with its index and the pool's handle.
fn render(
    wave: &Arc<Wave>,
    stretch: f64,
    cents: f64,
    frames: usize,
    mut each: impl FnMut(usize, &VoicePoolHandle<f32>),
) -> Vec<Vec<f32>> {
    let (pool, handle) = VoicePool::with_channels(wave.layout())
        .expect("a width the sampler reads")
        .with_handle();
    let mut pool = contract::prepared(pool, SampleRate::of(SR), BLOCK);
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    handle
        .add(
            SlotId(1),
            source(wave),
            Playback {
                stretch: StretchFactor::new(stretch),
                pitch: Cents::new(cents),
                ..Default::default()
            },
        )
        .expect("the command queue has room in a test");
    let mut out = vec![Vec::new(); usize::from(wave.layout().count())];
    for index in 0..frames / BLOCK {
        each(index, &handle);
        let block = contract::drive_input(
            &mut pool,
            &transport.env(BLOCK, SampleRate::of(SR)),
            Planes::default(),
            &[],
            &[],
        )
        .audio;
        for (channel, plane) in out.iter_mut().zip(&block) {
            channel.extend_from_slice(plane);
        }
        transport.advance(BLOCK as i64, SampleRate::of(SR));
    }
    out
}

fn f64s(x: &[f32]) -> Vec<f64> {
    x.iter().map(|&v| f64::from(v)).collect()
}

/// The Hann-windowed DFT of `x` at exactly `f` Hz.
fn tone(x: &[f64], f: f64) -> (f64, f64) {
    let m = x.len();
    let (mut re, mut im) = (0.0, 0.0);
    for (n, &v) in x.iter().enumerate() {
        let w = 0.5 - 0.5 * (TAU * n as f64 / m as f64).cos();
        let a = -TAU * f * n as f64 / SR;
        re += v * w * a.cos();
        im += v * w * a.sin();
    }
    (re, im)
}

/// The phase of `a` over `b` (`a * conj(b)`), within `-pi..pi`.
fn phase_over((are, aim): (f64, f64), (bre, bim): (f64, f64)) -> f64 {
    (aim * bre - are * bim).atan2(are * bre + aim * bim)
}

/// `(angle + pi) mod tau - pi`.
fn wrapped(angle: f64) -> f64 {
    (angle + PI).rem_euclid(TAU) - PI
}

/// The leakage of a Hann window of `length` samples at the negative-frequency
/// image of a tone at `f`, relative to the window's gain at the tone, bounded by
/// `1 / (pi v (v^2 - 1))` for `v = 2 f length / rate` bins.
fn hann_image(f: f64, length: usize) -> f64 {
    let v = 2.0 * f * length as f64 / SR;
    1.0 / (PI * v * (v * v - 1.0))
}

/// What an `f32` transform pair costs: each grows its rounding by `eps log2(window)`.
fn f32_transform_error() -> f64 {
    2.0 * (WINDOW as f64).log2() * f64::from(f32::EPSILON)
}

/// Equal channels stay equal through the voice, bit for bit: the vocoder keeps one
/// phase solution for both, where a solution per channel decorrelates them (a
/// source of correlation 0.69 came out at 0.003 at stretch 1.5).
#[test]
fn equal_channels_stay_equal_bit_for_bit_through_a_stretch_and_a_shift() {
    let (a, b) = (sine(1234.5, 0.5, 0.0), sine(2468.0, 0.25, 1.0));
    let mix = |i: usize| a(i) + b(i);
    let source = wave(60_000, &[&mix, &mix]);
    for (stretch, cents) in [(1.5, 0.0), (0.8, 300.0), (1.0, -500.0)] {
        let out = render(&source, stretch, cents, 40_000, |_, _| {});
        assert!(
            out[0].iter().any(|&x| x != 0.0),
            "{stretch}, {cents}: silent"
        );
        assert_eq!(out[0], out[1], "L = R at stretch {stretch}, {cents} cents");
    }
}

/// A fixed inter-channel phase offset survives a 1.5 stretch: the rotation is
/// common to the channels, so only rounding is left, and the exact-bin DFT's
/// leakage of the tone's image is `hann_image`-small (under `1e-9`), so the bound
/// is the `f32` transforms'.
#[test]
fn a_stretch_keeps_the_inter_channel_phase_offset() {
    const MEASURE: usize = 16_384;
    let (f, offset) = (1000.3, 0.6);
    let source = wave(80_000, &[&sine(f, 0.5, 0.0), &sine(f, 0.5, offset)]);
    let out = render(&source, 1.5, 0.0, 60_000, |_, _| {});
    let (left, right) = (f64s(&out[0]), f64s(&out[1]));
    let at = 30_000;
    let measured = phase_over(
        tone(&right[at..at + MEASURE], f),
        tone(&left[at..at + MEASURE], f),
    );
    let error = wrapped(measured - offset);
    assert!(
        hann_image(f, MEASURE) < 1e-9 && error.abs() < f32_transform_error(),
        "{f} Hz: the right channel leads by {measured} rad, want {offset} (error {error})"
    );
}

/// `N` source frames play over `N` times the ratio of output frames, to within a
/// hop: at 1.5, a tone burst of 12 000 frames sounds for 18 000.
///
/// The burst's extent is where the RMS of 128-frame blocks is above half its
/// steady value; the edge is smeared symmetrically, so its midpoint is where the
/// edge is, to within the hop a frame steps by and the block the RMS is taken in.
#[test]
fn a_non_integer_ratio_plays_n_frames_over_n_times_the_ratio() {
    const N: usize = 12_000;
    const RES: usize = 128;
    let tone_burst = |i: usize| {
        if i < N {
            sine(1000.0, 0.5, 0.0)(i)
        } else {
            0.0
        }
    };
    let source = wave(60_000, &[&tone_burst, &tone_burst]);
    for ratio in [1.5, 0.8] {
        let out = render(&source, ratio, 0.0, 40_000, |_, _| {});
        let half = 0.5 / 2.0f64.sqrt() / 2.0;
        let loud: Vec<usize> = out[0]
            .as_chunks::<RES>()
            .0
            .iter()
            .enumerate()
            .filter(|(_, block)| {
                let rms =
                    (block.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / RES as f64).sqrt();
                rms >= half
            })
            .map(|(b, _)| b)
            .collect();
        let extent = (loud.last().unwrap() + 1 - loud[0]) * RES;
        let want = N as f64 * ratio;
        assert!(
            (extent as f64 - want).abs() <= (HOP + RES) as f64,
            "ratio {ratio}: {N} frames sound for {extent}, want {want}"
        );
    }
}

/// A sine through a shift of +700 cents comes out at `f 2^(7/12)`, by the phase
/// advance of its exact-bin DFT between two segments 4096 frames apart. The
/// estimator's systematic error is the window's leakage of the tone's image, once
/// in the measurement and once in the vocoder's own frames: each a phase error of
/// at most `hann_image` radians on two frames, `2 eps` rad over the step, as a
/// frequency `2 eps fs / (2 pi step)`.
#[test]
fn a_shift_of_700_cents_lands_at_the_input_times_two_to_the_seven_twelfths() {
    const MEASURE: usize = 16_384;
    const STEP: usize = 4096;
    let f = 440.0;
    let source = wave(80_000, &[&sine(f, 0.5, 0.0), &sine(f, 0.5, 0.0)]);
    let out = f64s(&render(&source, 1.0, 700.0, 60_000, |_, _| {})[0]);
    let want = f * (700.0f64 / 1200.0).exp2();
    let at = 14_336;
    let (a, b) = (&out[at..at + MEASURE], &out[at + STEP..at + STEP + MEASURE]);
    let advance = phase_over(tone(b, want), tone(a, want));
    let deviation = wrapped(advance - TAU * want * STEP as f64 / SR);
    let got = want + deviation * SR / (TAU * STEP as f64);
    let bound = 2.0
        * (hann_image(f, WINDOW) * SR / (TAU * HOP as f64)
            + hann_image(want, MEASURE) * SR / (TAU * STEP as f64));
    assert!(
        (got - want).abs() < bound,
        "{f} Hz up 700 cents: {got} for {want} (bound {bound})"
    );
}

/// A step of the ratio mid-play leaves no hole: the blocks of 128 frames (four
/// periods of 1500 Hz, so a block's RMS is exact) after the vocoder has filled stay
/// within 1 dB of the sine's, which is two overlapped frames' phase mismatch of
/// `acos(10^(-1/20)) = 0.48` rad. The change of intake that flushed the stream
/// cost 11.6 dB.
#[test]
fn a_ratio_step_mid_play_keeps_every_block_within_a_decibel() {
    let source = wave(120_000, &[&sine(1500.0, 0.5, 0.0), &sine(1500.0, 0.5, 0.0)]);
    for (first, second) in [(0.8, 1.0667), (1.0667, 0.8), (1.0, 1.05)] {
        let step_at = 320; // a block, 20 480 frames in
        let out = render(&source, first, 0.0, 60_000, |index, handle| {
            if index == step_at {
                handle
                    .set_stretch(SlotId(1), StretchFactor::new(second), Cents::new(0.0))
                    .expect("the command queue has room in a test");
            }
        });
        let steady = 0.5 / 2.0f64.sqrt();
        for (b, block) in out[0][6000..].as_chunks::<128>().0.iter().enumerate() {
            let rms = (block.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / 128.0).sqrt();
            let db = 20.0 * (rms / steady).log10();
            assert!(
                db.abs() < 1.0,
                "{first} to {second}: block {b} after 6000 is {db} dB"
            );
        }
    }
}

/// The geometry the sampler's vocoder runs on, built here: its latency is the
/// oracle for the voice's.
fn vocoder_latency() -> Samples {
    let geometry = StftGeometry::new(
        SampleRate::of(SR),
        WindowLength::new(Samples::new(WINDOW)).unwrap(),
        HopSize::new(Samples::new(HOP)).unwrap(),
    );
    PhaseVocoder::<f32>::new(
        ChannelLayout::STEREO,
        ColaGeometry::try_from(geometry).unwrap(),
    )
    .latency()
}

/// A memory voice reads its source the vocoder's latency ahead, so it sounds on time
/// and declares no latency, whatever its stretch and shift: the pooled and the
/// standalone voice agree.
#[test]
fn a_stretching_memory_voice_declares_no_latency() {
    let clip = wave(4800, &[&sine(440.0, 0.5, 0.0), &sine(440.0, 0.5, 0.0)]);
    for (stretch, cents) in [
        (0.25, 0.0),
        (0.8, 0.0),
        (1.5, 0.0),
        (4.0, 0.0),
        (1.0, 700.0),
        (1.5, -1200.0),
    ] {
        let node = VoiceNode::new(Voice {
            source: VoiceSource::Memory(source(&clip)),
            play: Playback {
                stretch: StretchFactor::new(stretch),
                pitch: Cents::new(cents),
                ..Default::default()
            },
        });
        assert_eq!(
            node.shape().latency.samples(),
            Samples::new(0),
            "stretch {stretch}, {cents} cents"
        );
    }
    assert!(
        vocoder_latency().get() > 0,
        "a vocoder with a latency to read ahead of"
    );
}

/// An impulse scheduled at timeline frame 4000 comes out at frame 4000 through a pool,
/// within the transforms' rounding: a vocoder at unity (a voice whose stretch was turned
/// back to 1.0, which keeps it) delays its input by exactly `latency()`, and the voice
/// reads that far ahead. The bound is the kernel's: four overlapped frames, each in error
/// by the `f32` transforms' rounding.
#[test]
fn an_onset_lands_at_its_timeline_frame_through_a_pool() {
    const AT: usize = 4000;
    let impulse = |i: usize| if i == AT { 1.0 } else { 0.0 };
    let source = wave(20_000, &[&impulse, &impulse]);
    let out = render(&source, 1.5, 0.0, 12_000, |index, handle| {
        if index == 0 {
            handle
                .set_stretch(SlotId(1), StretchFactor::UNITY, Cents::new(0.0))
                .expect("the command queue has room in a test");
        }
    });
    let bound = 4.0 * f32_transform_error();
    for (n, &y) in out[0].iter().enumerate() {
        let want = if n == AT { 1.0 } else { 0.0 };
        assert!(
            (f64::from(y) - want).abs() < bound,
            "frame {n}: {y}, the impulse is due at {AT}"
        );
    }
}

/// The same through a standalone node at a stretch of 1.5: a tone that starts at source
/// frame 4000 starts at output frame 4000 x 1.5 = 6000, where the edge is: its half-level
/// crossing within the hop a frame steps by and the 128 frames the RMS is taken in.
#[test]
fn an_onset_lands_at_its_timeline_frame_through_a_node() {
    const AT: usize = 4000;
    const RES: usize = 128;
    let tone_from = |i: usize| {
        if i >= AT {
            sine(1000.0, 0.5, 0.0)(i)
        } else {
            0.0
        }
    };
    let clip = wave(30_000, &[&tone_from, &tone_from]);
    let node = VoiceNode::new(Voice {
        source: VoiceSource::Memory(source(&clip)),
        play: Playback {
            stretch: StretchFactor::new(1.5),
            ..Default::default()
        },
    });
    let mut node = contract::prepared(node, SampleRate::of(SR), BLOCK);
    let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut out = Vec::new();
    for _ in 0..16_000 / BLOCK {
        let block = contract::drive_input(
            &mut node,
            &transport.env(BLOCK, SampleRate::of(SR)),
            Planes::default(),
            &[],
            &[],
        )
        .audio;
        out.extend_from_slice(&block[0]);
        transport.advance(BLOCK as i64, SampleRate::of(SR));
    }
    let half = 0.5 / 2.0f64.sqrt() / 2.0;
    let first = out
        .as_chunks::<RES>()
        .0
        .iter()
        .position(|block| {
            (block.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / RES as f64).sqrt() >= half
        })
        .expect("the tone sounds")
        * RES;
    let want = AT as f64 * 1.5;
    assert!(
        (first as f64 - want).abs() <= (HOP + RES) as f64,
        "the tone starts at {first}, want {want}"
    );
}

/// A voice wider than a stereo pair stretches every channel: six channels of one
/// tone, each its own amplitude, at 1.5 each come out at that amplitude's level,
/// within 1 dB (the vocoder's level holds across a ratio).
#[test]
fn a_six_channel_voice_stretches_every_channel() {
    let amplitudes = [0.1, 0.15, 0.2, 0.25, 0.3, 0.35];
    let channels: Vec<Box<dyn Fn(usize) -> f64>> = amplitudes
        .iter()
        .map(|&a| Box::new(sine(1500.0, a, 0.0)) as Box<dyn Fn(usize) -> f64>)
        .collect();
    let refs: Vec<&dyn Fn(usize) -> f64> = channels.iter().map(|c| &**c).collect();
    let source = wave(60_000, &refs);
    let out = render(&source, 1.5, 0.0, 40_000, |_, _| {});
    assert_eq!(out.len(), 6);
    for (c, a) in amplitudes.iter().enumerate() {
        let steady = a / 2.0f64.sqrt();
        let x = f64s(&out[c][10_000..30_000]);
        let rms = (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt();
        let db = 20.0 * (rms / steady).log10();
        assert!(
            db.abs() < 1.0,
            "channel {c}: {db} dB from its amplitude {a}"
        );
    }
}

/// A non-finite source sample never latches: the output is finite again once the
/// read has left the sample behind, with no seek or reset between.
///
/// The NaN is at source frame 5000, which the vocoder sounds from output frame 7500
/// (at 1.5) for its latency and a window (2049 + 2048) at most: clear by 11 700,
/// under the 16 000 the check starts at.
#[test]
fn a_non_finite_source_sample_does_not_latch() {
    let tone = sine(1000.0, 0.5, 0.0);
    let poisoned = |i: usize| if i == 5000 { f64::NAN } else { tone(i) };
    let source = wave(60_000, &[&poisoned, &poisoned]);
    let out = render(&source, 1.5, 0.0, 40_000, |_, _| {});
    assert!(
        out[0].iter().any(|x| !x.is_finite()),
        "the sample reaches the output (a vacuous test otherwise)"
    );
    let clear = 16_000;
    let first_bad_after = out[0][clear..].iter().position(|x| !x.is_finite());
    assert_eq!(
        first_bad_after, None,
        "non-finite output past frame {clear}"
    );
    assert!(
        out[0][clear..].iter().any(|&x| x.abs() > 0.1),
        "and the voice sounds again"
    );
}
