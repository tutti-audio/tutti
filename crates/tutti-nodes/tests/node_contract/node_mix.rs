//! **A node's blend with its input is the graph's mix**, not the node's: the
//! convolver, the delay, the chorus and flanger, the phaser and the two reverbs output their wet
//! signal alone, and `Editor::set_mix` blends it with the input delayed by the
//! node's declared latency.
//!
//! - at `Mix::WET` the graph's output is the node rendered alone, bit for bit;
//! - at `Mix::DRY` the output is the input delayed by the node's latency, bit
//!   for bit (the convolver's block; none for the rest), on the channels the
//!   input has (a reverb's mono input leaves its right channel silent: the graph's
//!   rule is `y * m` where there is no `x̂`);
//! - at a mix between, the output is `y * m + x̂ * (1 - m)`, with `y` the node's
//!   own output (rendered alone, at `WET`) and `x̂` the aligned input, bit for bit;
//! - a mix that moves ramps across the block it lands on, to the new mix.
//!
//! Each render is preceded by one block of silence, the block the mix's ramp from
//! the unit's `WET` start lands on, and only the frames after it are compared.

use tutti_buffers::MaxBlock;
use tutti_buffers::Planes;
use tutti_buffers::{BusTopology, ChannelLayout};
use tutti_fixtures::noise;
use tutti_fixtures::signal::bits;
use tutti_graph::contract::drive;
use tutti_graph::{IntoNode, Prepare, Standalone};
use tutti_nodes::{
    ChannelTimes, CompressorConfig, CompressorControls, CompressorNode, CrossoverConfig,
    CrossoverControls, CrossoverNode, DelayConfig, DelayControls, DelayLayout, DelayNode,
    FdnConfig, FdnNode, FreeverbConfig, FreeverbNode, GateConfig, GateControls, GateNode,
    InterpolationMode, KeySource, MaxDelay, ModDelayConfig, ModDelayNode, PhaserConfig, PhaserNode,
    PlateReverbConfig, PlateReverbNode, ShimmerConfig, ShimmerNode, Stages,
};
use tutti_units::{CompressionRatio, Db, Feedback, Hz, Mix, SampleRate, Samples, Seconds};

const RATE: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 64;
const FRAMES: usize = 1_024;

fn solo<N: IntoNode>(node: N) -> Standalone<N::Controls> {
    Standalone::new(node, Prepare::new(RATE, MaxBlock::of(Samples::new(BLOCK))))
}

fn set_mix<C>(solo: &mut Standalone<C>, mix: Mix) {
    let key = solo.key();
    solo.renderer_mut()
        .editor_mut()
        .set_mix(key, mix)
        .expect("the node is in the graph and the mix is in range");
}

/// `BLOCK` frames of silence, then `x`, on every channel.
fn after_silence(x: &[Vec<f32>]) -> Vec<Vec<f32>> {
    x.iter()
        .map(|c| {
            let mut v = vec![0.0; BLOCK];
            v.extend_from_slice(c);
            v
        })
        .collect()
}

/// What `make()` renders for `x` (after a silent block) at `mix` in a graph,
/// the silent block cut off.
fn rendered<N: IntoNode>(make: &dyn Fn() -> N, x: &[Vec<f32>], mix: Mix) -> Vec<Vec<f32>> {
    let mut s = solo(make());
    set_mix(&mut s, mix);
    let input = after_silence(x);
    let refs: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
    s.render_input(Planes::new(&refs).unwrap())
        .into_iter()
        .map(|c| c[BLOCK..].to_vec())
        .collect()
}

/// The input delayed by `latency` frames: what the graph's `x̂` is.
fn delayed(x: &[f32], latency: usize) -> Vec<f32> {
    let mut v = vec![0.0; latency];
    v.extend_from_slice(&x[..x.len() - latency]);
    v
}

/// The three properties above for the node `make` builds, of `latency` frames
/// declared and `width` channels in and out.
fn check<N: IntoNode>(name: &str, make: &dyn Fn() -> N, width: usize, latency: usize) {
    check_io(name, make, width, width, latency);
}

/// [`check`] for a node of `inputs` channels in and `outputs` out, whose main
/// buses are placed (a design's `Placement::Speakers`): a mono input is a centre,
/// which the graph's fold sends to every side of the output at gain 1.0, so each
/// output's `x̂` is that input; otherwise output `c`'s `x̂` is input `c`.
fn check_io<N: IntoNode>(
    name: &str,
    make: &dyn Fn() -> N,
    inputs: usize,
    outputs: usize,
    latency: usize,
) {
    let x: Vec<Vec<f32>> = (0..inputs).map(|c| noise(40 + c as u32, FRAMES)).collect();
    let aligned = |c: usize| match x.get(if inputs == 1 { 0 } else { c }) {
        Some(x) => delayed(x, latency),
        None => vec![0.0; FRAMES],
    };

    // DRY is the aligned input.
    let dry = rendered(make, &x, Mix::DRY);
    for (c, dry) in dry.iter().enumerate().take(outputs) {
        assert_eq!(
            bits(dry),
            bits(&aligned(c)),
            "{name}: channel {c} at DRY is the input delayed by {latency}"
        );
    }

    // WET is the node alone.
    let wet = rendered(make, &x, Mix::WET);
    let (mut alone, _) = make().into_node();
    alone = {
        alone.prepare(&Prepare::new(RATE, MaxBlock::of(Samples::new(BLOCK))));
        alone
    };
    let input = after_silence(&x);
    let mut solo_out: Vec<Vec<f32>> = vec![Vec::new(); outputs];
    for at in (0..input[0].len()).step_by(BLOCK) {
        let block: Vec<&[f32]> = input.iter().map(|c| &c[at..at + BLOCK]).collect();
        for (o, b) in solo_out.iter_mut().zip(drive(
            alone.as_mut(),
            RATE,
            Planes::new(&block).unwrap(),
            &[],
        )) {
            o.extend(b);
        }
    }
    for c in 0..outputs {
        assert_eq!(
            bits(&wet[c]),
            bits(&solo_out[c][BLOCK..]),
            "{name}: channel {c} at WET is the node alone"
        );
    }

    // A mix between is the formula.
    let m = 0.3_f64;
    let mixed = rendered(make, &x, Mix::new(m));
    // The blend runs in the `f32` samples' width.
    let m = m as f32;
    for c in 0..outputs {
        let want: Vec<f32> = if c < inputs || inputs == 1 {
            wet[c]
                .iter()
                .zip(aligned(c))
                .map(|(&y, x)| y * m + x * (1.0 - m))
                .collect()
        } else {
            wet[c].iter().map(|&y| y * m).collect()
        };
        assert_eq!(
            bits(&mixed[c]),
            bits(&want),
            "{name}: channel {c} at {m} is y*m + x*(1-m)"
        );
    }
}
#[test]
fn the_convolver_is_blended_by_the_graph() {
    use tutti_buffers::Mono;
    use tutti_fixtures::signal::room_ir;

    let ir = room_ir(0.5, Seconds::new(0.05), RATE);
    let ir2 = room_ir(0.8, Seconds::new(0.08), RATE);
    check(
        "convolver (dual mono, 64-frame block)",
        &|| per_channel_node(&[Mono::new(&ir), Mono::new(&ir2)], Samples::new(64)).unwrap(),
        2,
        64,
    );
    check(
        "convolver (true stereo, 64-frame block)",
        &|| {
            true_stereo_node(
                Mono::new(&ir),
                Mono::new(&ir2),
                Mono::new(&ir2),
                Mono::new(&ir),
                Samples::new(64),
            )
        },
        2,
        64,
    );
    check(
        "convolver (shared IR, six channels)",
        &|| shared_ir_node(6u16, Mono::new(&[1.0, 0.5]), Samples::new(128)).unwrap(),
        6,
        128,
    );
    check(
        "convolver (folded, stereo)",
        &|| folded_node(&[Mono::new(&ir), Mono::new(&ir2)], Samples::new(32)).unwrap(),
        2,
        32,
    );
}

#[test]
fn the_delay_is_blended_by_the_graph() {
    check(
        "delay (stereo, cross-fed)",
        &|| {
            let layout = DelayLayout::builder(
                BusTopology::STEREO,
                MaxDelay::of(Seconds::new(0.1)),
                InterpolationMode::Linear,
            )
            .times(ChannelTimes::try_from(&[Seconds::new(0.021)][..]).unwrap())
            .build()
            .unwrap();
            DelayNode::<f32>::from(DelayConfig::new(layout).controls(DelayControls {
                time: Seconds::new(0.013),
                feedback: Feedback::new(0.5),
                cross_feedback: Feedback::new(0.3),
            }))
        },
        2,
        0,
    );
    check(
        "delay (mono)",
        &|| {
            DelayNode::<f32>::from(
                DelayConfig::new(DelayLayout::new(
                    BusTopology::MONO,
                    MaxDelay::of(Seconds::new(0.1)),
                    InterpolationMode::Linear,
                ))
                .controls(DelayControls {
                    time: Seconds::new(0.01),
                    feedback: Feedback::new(0.4),
                    ..DelayControls::DEFAULT
                }),
            )
        },
        1,
        0,
    );
}

#[test]
fn the_chorus_flanger_and_phaser_are_blended_by_the_graph() {
    check(
        "chorus (stereo)",
        &|| ModDelayNode::<f32>::from(ModDelayConfig::chorus(BusTopology::STEREO)),
        2,
        0,
    );
    check(
        "flanger (mono)",
        &|| ModDelayNode::<f32>::from(ModDelayConfig::flanger(BusTopology::MONO)),
        1,
        0,
    );
    check(
        "phaser (stereo, 4 stages)",
        &|| {
            PhaserNode::<f32>::try_from(PhaserConfig::new(BusTopology::STEREO, Stages::of(4)))
                .unwrap()
        },
        2,
        0,
    );
}

/// The Freeverb and the feedback delay network (mono in) and the plate (stereo in), all stereo out, are
/// blended by the graph like the rest: WET is the node alone, DRY the aligned
/// input on **both** sides (a mono source is a centre, which the fold sends to
/// left and right at gain 1.0: the design places its channels,
/// `Placement::Speakers`), and between them `y * m + x̂ * (1 - m)`, bit for bit.
#[test]
fn the_reverbs_are_blended_by_the_graph() {
    check_io(
        "freeverb (General MIDI room)",
        &|| FreeverbNode::<f32>::from(FreeverbConfig::general_midi()),
        1,
        2,
        0,
    );
    check_io(
        "freeverb (live)",
        &|| FreeverbNode::<f32>::from(FreeverbConfig::new()),
        1,
        2,
        0,
    );
    check_io("fdn", &|| FdnNode::<f32>::from(FdnConfig::new()), 1, 2, 0);
    check_io(
        "shimmer",
        &|| ShimmerNode::<f32>::from(ShimmerConfig::new()),
        1,
        2,
        0,
    );
    check_io(
        "plate",
        &|| PlateReverbNode::<f32>::from(PlateReverbConfig::new()),
        2,
        2,
        0,
    );
}

/// A mix that moves ramps across the block it lands on, from where it was to the
/// new mix: moving the delay's (six wide) from `WET` to `DRY` leaves the block
/// `y * m_i + x * (1 - m_i)` with `m_i = 1 - (i + 1) / 64`, ending on the dry
/// input exactly.
#[test]
fn a_mix_move_on_a_delay_ramps_across_the_block() {
    let make = || {
        DelayNode::<f32>::from(
            DelayConfig::new(DelayLayout::new(
                BusTopology::default_for(ChannelLayout::from(6u16))
                    .expect("six channels are one bus"),
                MaxDelay::of(Seconds::new(0.1)),
                InterpolationMode::Linear,
            ))
            .controls(DelayControls {
                time: Seconds::new(0.0005),
                feedback: Feedback::new(0.3),
                ..DelayControls::DEFAULT
            }),
        )
    };
    let x: Vec<Vec<f32>> = (0..6).map(|c| noise(70 + c, 2 * BLOCK)).collect();
    let wet = {
        let mut s = solo(make());
        let refs: Vec<&[f32]> = x.iter().map(Vec::as_slice).collect();
        s.render_input(Planes::new(&refs).unwrap())
    };
    let mut s = solo(make());
    let first: Vec<&[f32]> = x.iter().map(|c| &c[..BLOCK]).collect();
    s.render_input(Planes::new(&first).unwrap());
    set_mix(&mut s, Mix::DRY);
    let second: Vec<&[f32]> = x.iter().map(|c| &c[BLOCK..]).collect();
    let got = s.render_input(Planes::new(&second).unwrap());
    for c in 0..6 {
        for i in 0..BLOCK {
            let m = 1.0 + (0.0 - 1.0) * ((i + 1) as f32 / BLOCK as f32);
            let want = wet[c][BLOCK + i] * m + x[c][BLOCK + i] * (1.0 - m);
            assert_eq!(
                got[c][i].to_bits(),
                want.to_bits(),
                "channel {c} frame {i}: m = {m}"
            );
        }
        assert_eq!(
            got[c][BLOCK - 1].to_bits(),
            x[c][2 * BLOCK - 1].to_bits(),
            "channel {c}: the block ends fully dry"
        );
    }
}

/// **A keyed node and a split are blended on their main bus.** The gate and the
/// compressor take their audio, then a key as wide (a `Sidechain` bus): the dry
/// side of their mix is the audio, never the key. The crossover writes its
/// input's channels once per band, band-major: the dry side is band 0, the
/// further bands silent. Each declares it; built from widths alone, each would
/// have no pass and the graph would refuse its mix.
#[test]
fn a_keyed_node_and_a_split_are_blended_on_their_main_bus() {
    let s = Seconds::new;
    check_io(
        "gate (stereo, keyed)",
        &|| {
            GateNode::<f32>::from(
                GateConfig::new(BusTopology::STEREO)
                    .key(KeySource::Sidechain)
                    .controls(GateControls {
                        threshold: Db::new(-20.0),
                        attack: s(0.001),
                        hold: s(0.01),
                        release: s(0.05),
                        range: Db::new(-80.0),
                        ..GateControls::DEFAULT
                    }),
            )
        },
        4,
        2,
        0,
    );
    check_io(
        "compressor (stereo, keyed)",
        &|| {
            CompressorNode::<f32>::from(
                CompressorConfig::new(BusTopology::STEREO)
                    .key(KeySource::Sidechain)
                    .controls(CompressorControls {
                        threshold: Db::new(-20.0),
                        ratio: CompressionRatio::new(4.0),
                        attack: s(0.005),
                        release: s(0.05),
                        ..CompressorControls::DEFAULT
                    }),
            )
        },
        4,
        2,
        0,
    );
    check_io(
        "crossover (stereo, two bands)",
        &|| {
            CrossoverNode::<f32>::try_from(CrossoverConfig::new(
                BusTopology::STEREO,
                CrossoverControls::try_from(&[Hz::new(1_000.0)][..]).unwrap(),
            ))
            .expect("one split")
        },
        2,
        4,
        0,
    );
    // Mono: band 0 is the input at DRY, band 1 silent (not a centre on both).
    let x = vec![noise(90, FRAMES)];
    let dry = rendered(
        &|| {
            CrossoverNode::<f32>::try_from(CrossoverConfig::new(
                BusTopology::MONO,
                CrossoverControls::try_from(&[Hz::new(1_000.0)][..]).unwrap(),
            ))
            .expect("one split")
        },
        &x,
        Mix::DRY,
    );
    assert_eq!(bits(&dry[0]), bits(&x[0]), "mono crossover: band 0 at DRY");
    assert!(
        dry[1].iter().all(|&v| v == 0.0),
        "mono crossover: band 1 at DRY is silent"
    );
}

fn unassigned_bus(width: impl Into<tutti_buffers::ChannelLayout>) -> tutti_buffers::BusTopology {
    tutti_buffers::BusTopology::unassigned(width.into()).expect("a test width fits a bus")
}

/// `width` unassigned channels sharing the response `ir`.
fn shared_ir_node<F: tutti_dsp::FftFloat + tutti_dsp::Lerp, S: tutti_buffers::Sample>(
    width: impl Into<tutti_buffers::ChannelLayout>,
    ir: tutti_buffers::Mono<'_, S>,
    block_size: tutti_units::Samples,
) -> Result<tutti_nodes::ConvolverNode<F>, tutti_nodes::ConvolverError> {
    tutti_nodes::ConvolverNode::try_from(
        tutti_nodes::ConvolverConfig::new(unassigned_bus(width), &[ir]).block_size(block_size),
    )
}

/// One response per channel, at as many unassigned channels as responses.
fn per_channel_node<F: tutti_dsp::FftFloat + tutti_dsp::Lerp, S: tutti_buffers::Sample>(
    irs: &[tutti_buffers::Mono<'_, S>],
    block_size: tutti_units::Samples,
) -> Result<tutti_nodes::ConvolverNode<F>, tutti_nodes::ConvolverError> {
    let width = tutti_buffers::ChannelLayout::try_from(irs.len()).expect("a test width fits");
    tutti_nodes::ConvolverNode::try_from(
        tutti_nodes::ConvolverConfig::new(unassigned_bus(width), irs).block_size(block_size),
    )
}

/// Every input folded to mono, then one response per output, at the engine's placement of
/// that many channels.
fn folded_node<F: tutti_dsp::FftFloat + tutti_dsp::Lerp, S: tutti_buffers::Sample>(
    irs: &[tutti_buffers::Mono<'_, S>],
    block_size: tutti_units::Samples,
) -> Result<tutti_nodes::ConvolverNode<F>, tutti_nodes::ConvolverError> {
    let width = tutti_buffers::ChannelLayout::try_from(irs.len()).expect("a test width fits");
    let bus = tutti_buffers::BusTopology::default_for(width).expect("a test width fits a bus");
    tutti_nodes::ConvolverNode::try_from(
        tutti_nodes::ConvolverConfig::new(bus, irs)
            .folded()
            .block_size(block_size),
    )
}

/// True stereo from `[LL, LR, RL, RR]`.
fn true_stereo_node<F: tutti_dsp::FftFloat + tutti_dsp::Lerp, S: tutti_buffers::Sample>(
    ll: tutti_buffers::Mono<'_, S>,
    lr: tutti_buffers::Mono<'_, S>,
    rl: tutti_buffers::Mono<'_, S>,
    rr: tutti_buffers::Mono<'_, S>,
    block_size: tutti_units::Samples,
) -> tutti_nodes::ConvolverNode<F> {
    tutti_nodes::ConvolverNode::try_from(
        tutti_nodes::ConvolverConfig::new(tutti_buffers::BusTopology::STEREO, &[ll, lr, rl, rr])
            .true_stereo()
            .block_size(block_size),
    )
    .expect("four responses on a stereo pair")
}
