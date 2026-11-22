//! The VBAP panner's gains ramp on a grid of 16 frames counted from the last
//! reset: the render does not depend on where the blocks fall, a reset node
//! renders what a fresh one does.

// These tests pin the `f32` instantiation, the session's audio format, so their tolerances are `f32`'s.

use tutti_buffers::{Bed, Planes};
use tutti_graph::ParamNode;
use tutti_graph::contract::{drive, prepared};
use tutti_nodes::VbapPannerNode;
use tutti_nodes::testing::aim;
use tutti_units::{Azimuth, Elevation, SampleRate, UnitParam};

const RATE: SampleRate = SampleRate::SR_48K;
const GRID: usize = 16;

/// Deterministic decorrelated input, so the width path's two virtual sources
/// are told apart.
fn noise(frames: usize, mut state: u32) -> [Vec<f32>; 2] {
    let mut channel = || {
        (0..frames)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
            })
            .collect::<Vec<f32>>()
    };
    [channel(), channel()]
}

/// `input` through `node` in blocks of the given lengths (cycled), one `Vec`
/// per output channel.
fn render(
    node: &mut VbapPannerNode<f32>,
    input: &[Vec<f32>; 2],
    splits: &[usize],
) -> Vec<Vec<f32>> {
    let mut out = vec![Vec::new(); node.channels().count() as usize];
    let (mut at, mut i) = (0, 0);
    while at < input[0].len() {
        let end = (at + splits[i % splits.len()]).min(input[0].len());
        i += 1;
        let block = drive(
            node,
            RATE,
            Planes::new(&[&input[0][at..end], &input[1][at..end]]).unwrap(),
            &[],
        );
        for (o, b) in out.iter_mut().zip(block) {
            o.extend(b);
        }
        at = end;
    }
    out
}

fn bits(channels: &[Vec<f32>]) -> Vec<Vec<u32>> {
    channels
        .iter()
        .map(|c| c.iter().map(|s| s.to_bits()).collect())
        .collect()
}

/// A panner of `bed`, seated by one grid segment at front-centre, then moved:
/// the gains ramp for the whole render.
fn moving(bed: Bed) -> VbapPannerNode<f32> {
    let mut node = prepared(vbap::<f32>(bed), RATE, 2_048);
    let params = node.param_set();
    params.set(UnitParam::Spread, 0.2);
    let seat = noise(GRID, 1);
    render(&mut node, &seat, &[GRID]);
    aim(&params, Azimuth::new(-120.0), Elevation::new(20.0));
    params.set(UnitParam::StereoWidth, 0.8);
    node
}

/// Any block split gives the same bits, on the width branch and the mono fold,
/// while the bearing, the height and the width are all in flight.
#[test]
fn any_block_split_renders_the_same_bits() {
    let input = noise(2_000, 7);
    for bed in [Bed::Stereo, Bed::Surround51, Bed::Surround714] {
        for width in [0.8f32, 0.0] {
            let mut reference = moving(bed);
            reference
                .param_set()
                .set(UnitParam::StereoWidth, f64::from(width));
            let want = bits(&render(&mut reference, &input, &[2_000]));
            for splits in [
                &[1usize][..],
                &[7],
                &[16],
                &[64],
                &[3, 29, 1, 64, 16, 15, 17],
            ] {
                let mut node = moving(bed);
                node.param_set()
                    .set(UnitParam::StereoWidth, f64::from(width));
                assert_eq!(
                    bits(&render(&mut node, &input, splits)),
                    want,
                    "{bed:?}, width {width}, blocks {splits:?}"
                );
            }
        }
    }
}

/// A reset node renders what a fresh one does, bit for bit: the grid restarts
/// and the first grid point lands on the controls whole.
#[test]
fn a_reset_node_renders_what_a_fresh_one_does() {
    let input = noise(1_000, 11);
    let mut used = moving(Bed::Surround51);
    render(&mut used, &input, &[40]);
    tutti_graph::Node::reset(&mut used);
    // Cut part-way into a grid segment, then reset.
    let mut cut = moving(Bed::Surround51);
    render(&mut cut, &noise(37, 3), &[37]);
    tutti_graph::Node::reset(&mut cut);

    let mut fresh = prepared(vbap::<f32>(tutti_buffers::Bed::Surround51), RATE, 64);
    let params = fresh.param_set();
    params.set(UnitParam::Spread, 0.2);
    aim(&params, Azimuth::new(-120.0), Elevation::new(20.0));
    params.set(UnitParam::StereoWidth, 0.8);
    let want = bits(&render(&mut fresh, &input, &[64]));
    assert_eq!(bits(&render(&mut used, &input, &[64])), want);
    assert_eq!(bits(&render(&mut cut, &input, &[64])), want);
}

/// A width that crosses the mono-fold threshold at a grid point crossfades the
/// two branches along the next 16 frames in even steps, instead of stepping.
#[test]
fn a_width_crossing_the_fold_threshold_crossfades_in_even_steps() {
    let mut node = prepared(vbap::<f32>(tutti_buffers::Bed::Stereo), RATE, 64);
    let params = node.param_set();
    aim(&params, Azimuth::new(-15.0), Elevation::LEVEL);
    params.set(UnitParam::StereoWidth, 1.0);
    // The left input alone: the two-source branch and the fold weigh it
    // differently.
    let input = [vec![1.0f32; 2 * GRID], vec![0.0f32; 2 * GRID]];
    let first = render(
        &mut node,
        &[input[0][..GRID].to_vec(), input[1][..GRID].to_vec()],
        &[GRID],
    );
    params.set(UnitParam::StereoWidth, 0.0);
    let second = render(
        &mut node,
        &[input[0][..GRID].to_vec(), input[1][..GRID].to_vec()],
        &[GRID],
    );
    let (start, end) = (first[0][GRID - 1], second[0][GRID - 1]);
    assert!(
        (end - start).abs() > 0.1,
        "the two branches should differ: {start} against {end}"
    );
    let step = (end - start) / GRID as f32;
    for (i, &s) in second[0].iter().enumerate() {
        let want = start + step * (i + 1) as f32;
        assert!(
            (s - want).abs() < 1e-5,
            "frame {i}: {s} is off the even steps ({want})"
        );
    }
}

/// A panner over `bed`.
fn vbap<F: tutti_buffers::Float>(bed: tutti_buffers::Bed) -> tutti_nodes::VbapPannerNode<F> {
    tutti_nodes::VbapPannerNode::try_from(tutti_nodes::VbapPannerConfig::new(bed.into()))
        .expect("a bed with speakers to pan between")
}
