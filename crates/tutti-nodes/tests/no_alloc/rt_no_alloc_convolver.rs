//! The convolver at every corner: each layout at its widths, short and long responses, host
//! blocks of one frame to past a partition, a hostile wet gain (in the cell and on its param
//! port). The output stays finite and nothing allocates.

use tutti_buffers::Mono;
use tutti_graph::contract::Direct;
use tutti_nodes::ConvolverNode;
use tutti_types::{AmbisonicFormat, AmbisonicOrder, Bed, ChannelTopology, MAX_CHANNELS};
use tutti_units::{Amplitude, SampleRate};

/// The largest host block the rig is prepared for.
const MAX_BLOCK: usize = 128;
/// Host block lengths, ragged around the partition of 32.
const BLOCKS: [usize; 6] = [1, 7, 31, 33, 64, 100];

/// A response of `len` taps with a slow decay: short or long enough to be cut into levels.
fn response(seed: usize, len: usize) -> Vec<f32> {
    tutti_fixtures::noise(seed as u32, len)
        .iter()
        .enumerate()
        .map(|(k, v)| v * 0.5 / (1.0 + k as f32 * 0.01))
        .collect()
}

/// The nodes under test: every layout at the widths it takes, with `len`-tap responses.
fn nodes(len: usize) -> Vec<(String, ConvolverNode)> {
    let mut all = Vec::new();
    let irs = |n: usize| -> Vec<Vec<f32>> { (0..n).map(|p| response(3 + p, len)).collect() };
    for width in [1usize, 2, 6, MAX_CHANNELS] {
        let irs = irs(width);
        let refs: Vec<Mono<'_, f32>> = irs.iter().map(|ir| Mono::new(ir)).collect();
        all.push((
            format!("per channel x{width}"),
            ConvolverNode::per_channel(&refs, 32).unwrap(),
        ));
    }
    let four = irs(4);
    all.push((
        "true stereo".into(),
        ConvolverNode::true_stereo(
            Mono::new(&four[0]),
            Mono::new(&four[1]),
            Mono::new(&four[2]),
            Mono::new(&four[3]),
            32,
        ),
    ));
    // First order in, stereo out: the widths differ.
    let eight = irs(8);
    let views: Vec<Mono<'_, f32>> = eight.iter().map(|ir| Mono::new(ir)).collect();
    all.push((
        "ambisonic 1 to stereo".into(),
        ConvolverNode::ambisonic(
            AmbisonicFormat::ambix(AmbisonicOrder::of(1)),
            ChannelTopology::from(Bed::Stereo),
            &views,
            32,
        )
        .unwrap(),
    ));
    for order in 0..=3u8 {
        let format = AmbisonicFormat::ambix(AmbisonicOrder::of(order));
        let width = usize::from(format.width().count());
        // The widest sets have hundreds of paths: shorter responses keep the sweep quick.
        let irs = irs(width * width).into_iter().map(|mut ir| {
            ir.truncate(if order > 1 { len.min(300) } else { len });
            ir
        });
        let irs: Vec<Vec<f32>> = irs.collect();
        let views: Vec<Mono<'_, f32>> = irs.iter().map(|ir| Mono::new(ir)).collect();
        all.push((
            format!("ambisonic order {order}"),
            ConvolverNode::ambisonic(format, ChannelTopology::from(format), &views, 32).unwrap(),
        ));
    }
    all
}

/// The gain written into the cell, each a corner: out of range, huge, not a number.
const CELL_GAINS: [f32; 8] = [
    0.0,
    1.0,
    4.0,
    1.0e20,
    -5.0,
    f32::NAN,
    f32::INFINITY,
    f32::NEG_INFINITY,
];

/// The gain driven on the param port: negative, huge and not finite frames among ordinary ones.
fn lane() -> Vec<f32> {
    let corners = [
        0.5,
        f32::NAN,
        -2.0,
        1.0e20,
        f32::INFINITY,
        0.0,
        f32::NEG_INFINITY,
        1.0,
    ];
    (0..MAX_BLOCK).map(|i| corners[i % corners.len()]).collect()
}

/// `node` over every block length at each gain, under `assert_no_alloc`.
fn sweep(name: &str, node: ConvolverNode, gains: &[Option<f32>], driven: bool) {
    let width = usize::from(node.outputs().count());
    let cell = node.gain();
    let mut rig = Direct::new(node, SampleRate::SR_48K, MAX_BLOCK);
    for (c, x) in rig.inputs_mut().iter_mut().enumerate() {
        let x = x.into_samples_mut();
        for (i, s) in x.iter_mut().enumerate() {
            *s = (((i * 7 + c * 13) % 29) as f32 / 14.0 - 1.0) * 0.9;
        }
    }
    if driven {
        rig.feed(0, Some(&lane()));
    }
    for &gain in gains {
        if let Some(g) = gain {
            cell.store(Amplitude(g));
        }
        // Past the first partitions, so the blocks take the hot path.
        for _ in 0..12 {
            rig.block();
        }
        let mut finite = true;
        assert_no_alloc::assert_no_alloc(|| {
            for round in 0..40 {
                rig.set_block_len(BLOCKS[round % BLOCKS.len()]);
                rig.block();
                for c in 0..width {
                    finite &= rig
                        .output()
                        .plane(c)
                        .samples()
                        .iter()
                        .all(|y| y.is_finite());
                }
            }
        });
        assert!(
            finite,
            "{name}: gain {gain:?} driven={driven} rendered a sample that is not finite"
        );
        rig.set_block_len(MAX_BLOCK);
    }
}

/// Short responses (three taps, inside one partition) and long ones (past a partition, cut
/// into levels), in every layout, at every cell gain and with the lane driven.
#[test]
fn the_convolver_corners_are_allocation_free_and_finite() {
    for len in [3usize, 4_000] {
        for (name, node) in nodes(len) {
            let name = format!("{name}, {len} taps");
            let cells: Vec<Option<f32>> = CELL_GAINS.iter().map(|&g| Some(g)).collect();
            sweep(&name, node.clone(), &cells, false);
            sweep(&name, node, &[None], true);
        }
    }
}
