use super::*;
use core::f64::consts::TAU;
use core::num::NonZeroU16;
use tutti_fixtures::signal::{hann_image_leak, sine_frequency};
use tutti_graph::Node;
use tutti_graph::contract::{BlockRig, assert_param_fork, prepared};
use tutti_units::{Hz, Latency, Tail, UnitParam};

const RATE: SampleRate = SampleRate::SR_48K;
/// The default geometry's window and hop.
const WINDOW: usize = 2048;
const HOP: usize = WINDOW / 4;

/// The latency of the fixed-rate vocoder the node runs, at `hop` and `width`.
fn vocoder_latency(width: u16, hop: usize) -> Samples {
    let geometry = StftGeometry::new(
        RATE,
        WindowLength::new(Samples::new(4 * hop)).unwrap(),
        HopSize::new(Samples::new(hop)).unwrap(),
    );
    PhaseVocoder::<f32>::fixed_rate(
        ChannelLayout::from_count(width),
        ColaGeometry::try_from(geometry).unwrap(),
    )
    .latency()
}

/// `amplitude * sin(2 pi f n / RATE)` for `len` samples.
fn sine<F: FftFloat>(f: f64, amplitude: f64, len: usize) -> Vec<F> {
    (0..len)
        .map(|n| F::from_design(amplitude * (TAU * f * n as f64 / RATE.get()).sin()))
        .collect()
}

/// `source` through a node of `config` in blocks of `block` frames (the source is padded with
/// silence to a whole number of blocks), the pitch set to `pitch(i)` cents before block `i`.
fn stream<F: FftFloat>(
    config: PitchShifterConfig,
    source: &[Vec<F>],
    block: usize,
    pitch: impl Fn(usize) -> f64,
) -> Vec<Vec<F>> {
    let node = PitchShifterNode::<F>::from(config);
    let (mut rig, controls) = BlockRig::<F>::with_format(node, RATE, block);
    let blocks = source[0].len().div_ceil(block);
    let mut out = vec![Vec::new(); source.len()];
    for i in 0..blocks {
        controls.set(UnitParam::Pitch, pitch(i));
        for (c, mut plane) in rig.inputs_mut().iter_mut().enumerate() {
            let slot = plane.samples_mut();
            slot.fill(F::ZERO);
            let from = (i * block).min(source[c].len());
            let to = ((i + 1) * block).min(source[c].len());
            slot[..to - from].copy_from_slice(&source[c][from..to]);
        }
        rig.block();
        for (c, o) in out.iter_mut().enumerate() {
            o.extend_from_slice(rig.output().plane(c).samples());
        }
    }
    out
}

fn f64s<F: FftFloat>(x: &[F]) -> Vec<f64> {
    x.iter().map(|&v| v.into()).collect()
}

/// An FFT roundtrip's relative error in `f32`: each transform grows rounding like
/// `eps * log2(window)`, and a frame is one forward and one inverse.
fn f32_transform_error() -> f64 {
    2.0 * (WINDOW as f64).log2() * f64::from(f32::EPSILON)
}

/// A fork starts from the shift last set and shares no cell.
#[test]
fn a_fork_shares_no_cell() {
    assert_param_fork(PitchShifterNode::from(
        PitchShifterConfig::new(NonZeroU16::new(2).unwrap()).pitch(Cents::new(300.0)),
    ));
}

/// The shape is the width in and out, declares the shift as `Pitch` starting as configured,
/// reports the fixed-rate vocoder's latency, and a finite tail.
#[test]
fn the_shape_declares_the_shift_the_latency_and_a_finite_tail() {
    let hop = HopSize::new(Samples::new(256)).unwrap();
    let n = prepared(
        PitchShifterNode::<f32>::from(
            PitchShifterConfig::new(NonZeroU16::new(3).unwrap())
                .hop(hop)
                .pitch(Cents::new(-250.0)),
        ),
        RATE,
        64,
    );
    let shape = n.shape();
    assert_eq!(
        (shape.audio_in().count(), shape.audio_out().count()),
        (3, 3)
    );
    assert_eq!(shape.latency, Latency::new(vocoder_latency(3, 256)));
    assert!(matches!(shape.tail, Tail::Finite(_)));
    assert_eq!(shape.params.as_slice(), &[UnitParam::Pitch][..]);
    assert_eq!(n.param_base(0), Some(-250.0));
    let mono = PitchShifterNode::<f32>::from(PitchShifterConfig::new(NonZeroU16::MIN));
    assert_eq!(mono.shape().audio_in().count(), 1);
}

/// An impulse rings for the window read at the shift's ratio: a frame's window is `WINDOW`
/// frames of the stretched time and the cubic read crosses it at `r = 2^(cents/1200)` per
/// output frame, so the last sound is at most `WINDOW / r` frames after the impulse's own
/// arrival at the latency. The declared tail is past that, so the node's cut at the tail
/// (which would end the sound in the block it reaches) never falls on sound.
#[test]
fn an_impulse_rings_for_a_window_at_the_ratio_and_the_declared_tail_is_past_it() {
    for cents in [-2400.0, -1200.0, 0.0, 700.0, 2400.0] {
        let config = PitchShifterConfig::new(NonZeroU16::MIN).pitch(Cents::new(cents));
        let n = prepared(PitchShifterNode::<f32>::from(config), RATE, 64);
        let shape = n.shape();
        let latency = shape.latency.samples().get();
        let Tail::Finite(tail) = shape.tail else {
            panic!("the tail is finite")
        };
        let mut impulse = vec![0.0f32; 60_000];
        impulse[4000] = 1.0;
        let out = stream(config, &[impulse], 64, |_| cents);
        let last = out[0].iter().rposition(|&x| x != 0.0).unwrap();
        let ring = last - (4000 + latency);
        let ratio = (cents / 1200.0f64).exp2();
        assert!(
            (ring as f64) <= WINDOW as f64 / ratio,
            "{cents} cents: rings {ring} frames"
        );
        assert!(
            ring + 64 < tail.get(),
            "{cents} cents: rings {ring} of a tail of {}",
            tail.get()
        );
    }
}

/// The node's latency is the fixed-rate vocoder's, whatever the width and the hop.
#[test]
fn the_reported_latency_is_the_fixed_rate_vocoders() {
    for (width, hop) in [(1u16, 512usize), (2, 512), (6, 512), (2, 256)] {
        let config = PitchShifterConfig::new(NonZeroU16::new(width).unwrap())
            .hop(HopSize::new(Samples::new(hop)).unwrap());
        let n = prepared(PitchShifterNode::<f32>::from(config), RATE, 64);
        assert_eq!(
            n.shape().latency,
            Latency::new(vocoder_latency(width, hop)),
            "width {width}, hop {hop}"
        );
    }
}

/// An impulse at 0 cents comes out as an impulse at exactly the reported latency later, and
/// nowhere else, in blocks of any size, those past a hop included: four overlapped frames, each
/// in error by the transforms' rounding.
#[test]
fn an_impulse_comes_out_at_the_reported_latency_in_any_block_size() {
    for block in [97, 480, 2048, 5000] {
        let mut impulse = vec![0.0f32; 20_000];
        impulse[4000] = 1.0;
        let n = prepared(
            PitchShifterNode::<f32>::from(PitchShifterConfig::new(NonZeroU16::MIN)),
            RATE,
            block,
        );
        let latency = n.shape().latency.samples().get();
        assert_eq!(latency, vocoder_latency(1, HOP).get());
        let out = stream(
            PitchShifterConfig::new(NonZeroU16::MIN),
            &[impulse],
            block,
            |_| 0.0,
        );
        let bound = 4.0 * f32_transform_error();
        for (i, &y) in out[0].iter().enumerate() {
            let want = if i == 4000 + latency { 1.0 } else { 0.0 };
            assert!(
                (f64::from(y) - want).abs() < bound,
                "block {block}, sample {i}: {y}"
            );
        }
    }
}

/// At 0 cents the node is the input delayed by the latency: a Hann-squared COLA frame sum of
/// untouched spectra reproduces the signal, so `out[n + latency] = in[n]` to the transforms'
/// rounding, at `f64` here (`1e-12` is `log2(2048) * 2^-53` with four orders of margin).
#[test]
fn at_zero_cents_the_output_is_the_input_delayed_by_the_latency() {
    let input = sine::<f64>(1234.5, 0.5, 40_000);
    let delay = vocoder_latency(1, HOP).get();
    for block in [300, 4096] {
        let out = stream(
            PitchShifterConfig::new(NonZeroU16::MIN),
            std::slice::from_ref(&input),
            block,
            |_| 0.0,
        );
        for n in 0..30_000 {
            assert!(
                (out[0][n + delay] - input[n]).abs() < 1e-12,
                "block {block}, sample {n}"
            );
        }
    }
}

/// A sine at `f` through `cents` comes out at `f 2^(cents/1200)`, by the phase advance of its
/// exact-bin DFT between two segments 4096 frames apart, measured after the latency. The
/// estimator's only systematic error is the window's leakage of the tone's negative-frequency
/// image, once in the measurement and once in the vocoder's own frames, each a phase error of
/// at most that many radians on two frames: `2 eps` rad over the step, as a frequency
/// `2 eps fs / (2 pi step)`.
#[test]
fn a_shift_lands_at_the_input_times_two_to_the_cents_over_1200() {
    const MEASURE: usize = 16_384;
    let leak = |f: f64, length: usize| hann_image_leak(Hz::new(f), Samples::new(length), RATE);
    for (f, cents) in [
        (1000.0, 700.0),
        (1000.0, -700.0),
        (1000.0, 1200.0),
        (440.0, 50.0),
    ] {
        let out = stream(
            PitchShifterConfig::new(NonZeroU16::MIN),
            &[sine::<f64>(f, 0.5, 70_000)],
            480,
            |_| cents,
        );
        let want = f * (cents / 1200.0f64).exp2();
        let from = vocoder_latency(1, HOP).get() + 12_288;
        let got = sine_frequency(
            &f64s(&out[0])[from..],
            Hz::new(want),
            RATE,
            Samples::new(MEASURE),
            Samples::new(4096),
        )
        .get();
        let bound = 2.0
            * (leak(f, WINDOW) * RATE.get() / (TAU * HOP as f64)
                + leak(want, MEASURE) * RATE.get() / (TAU * 4096.0));
        assert!(
            (got - want).abs() < bound,
            "{f} Hz {cents} cents: {got} for {want} (bound {bound})"
        );
    }
}

/// The blocks of 128 frames (a whole number of periods of 1500 Hz, 375 Hz and 6000 Hz) stay
/// within 1 dB of the steady sine's level across a live change of the pitch, however the
/// graph's blocks fall against the hop. 1 dB is two overlapped frames' phase mismatch of
/// `acos(10^(-1/20)) = 0.48` rad. A block that came up short of input would be partly silent.
#[test]
fn a_live_pitch_change_keeps_every_block_within_one_db_on_a_steady_sine() {
    let steady = 0.5 / 2.0f64.sqrt();
    for (first, second) in [
        (0.0, 700.0),
        (-2400.0, 2400.0),
        (2400.0, -2400.0),
        (-700.0, 300.0),
    ] {
        for block in [64, 480, 1024] {
            let out = stream(
                PitchShifterConfig::new(NonZeroU16::MIN).pitch(Cents::new(first)),
                &[sine::<f32>(1500.0, 0.5, 60_000)],
                block,
                |i| if i * block < 30_000 { first } else { second },
            );
            for (b, chunk) in out[0][12_000..].as_chunks::<128>().0.iter().enumerate() {
                let rms = (chunk.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / 128.0).sqrt();
                let db = 20.0 * (rms / steady).log10();
                assert!(
                    db.abs() < 1.0,
                    "{first} -> {second} cents, block {block}: chunk {b} is {db} dB"
                );
            }
        }
    }
}

/// Equal channels come out equal bit for bit, through a live change of the pitch: one phase
/// solution serves the width.
#[test]
fn equal_channels_stay_equal_bit_for_bit() {
    let one = sine::<f32>(1234.5, 0.5, 30_000);
    let other = sine::<f32>(2468.0, 0.25, 30_000);
    let mix: Vec<f32> = one.iter().zip(&other).map(|(a, b)| a + b).collect();
    let out = stream(
        PitchShifterConfig::new(NonZeroU16::new(2).unwrap()),
        &[mix.clone(), mix],
        480,
        |i| if i < 30 { 300.0 } else { -500.0 },
    );
    assert!(out[0].iter().any(|&x| x != 0.0));
    assert_eq!(out[0], out[1]);
}
