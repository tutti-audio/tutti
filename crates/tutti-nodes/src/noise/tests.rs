use super::*;
use crate::test_support::RATE;
use tutti_graph::contract::{Direct, assert_param_fork};
use tutti_rt::SplitMix64;

/// `frames` of a node of `colour`, from the start.
fn render(config: NoiseConfig, frames: usize) -> Vec<f32> {
    let mut d = Direct::new(NoiseNode::<f32>::from(config), RATE, frames);
    d.block();
    d.output().plane(0).samples().to_vec()
}

/// The `n`th white sample of the stream of `seed` at `f32`: the word's top 24 bits as a
/// fraction of `2^24`, doubled and shifted to `[-1, 1)`: exact in `f32`.
fn white(seed: u64, n: usize) -> f32 {
    let mut rng = SplitMix64::new(seed);
    let word = (0..=n).map(|_| rng.next_u64()).last().unwrap();
    2.0 * ((word >> 40) as f32 / 16_777_216.0) - 1.0
}

#[test]
fn a_fork_shares_no_cell() {
    assert_param_fork(NoiseNode::<f32>::from(NoiseConfig::new(NoiseColour::Pink)));
}

#[test]
fn the_shape_is_a_mono_source_declaring_the_level() {
    let node =
        NoiseNode::<f32>::from(NoiseConfig::new(NoiseColour::White).level(Amplitude::new(0.25)));
    let shape = Node::<f32>::shape(&node);
    assert_eq!(shape.audio_in().count(), 0);
    assert_eq!(shape.audio_out().count(), 1);
    assert_eq!(shape.params.as_slice(), &[UnitParam::Volume][..]);
    assert_eq!(Node::<f32>::param_base(&node, 0), Some(0.25));
}

/// White at level 0.25 from seed 9 is `0.25` times the SplitMix64 word `n` as a unit fraction,
/// doubled and shifted: the first frames are exactly those, in sample order.
#[test]
fn white_is_the_level_times_the_seeded_words() {
    let out = render(
        NoiseConfig::new(NoiseColour::White)
            .seed(9)
            .level(Amplitude::new(0.25)),
        32,
    );
    for (n, &y) in out.iter().enumerate() {
        assert_eq!(y, 0.25 * white(9, n), "sample {n}");
    }
}

/// A driven level is read per frame: 0 silences a frame, a NaN holds the last finite level,
/// and the frames are the seeded words times those levels.
#[test]
fn a_driven_level_is_read_per_frame_and_a_non_finite_one_holds() {
    let signal = [0.0f32, 0.5, f32::NAN, f32::INFINITY, 0.25, 1.0, 1.0, 1.0];
    let mut d = Direct::new(
        NoiseNode::<f32>::from(NoiseConfig::new(NoiseColour::White).seed(3)),
        RATE,
        signal.len(),
    );
    d.feed(0, Some(&signal));
    d.block();
    let out = d.output().plane(0).samples();
    let levels = [0.0f32, 0.5, 0.5, 0.5, 0.25, 1.0, 1.0, 1.0];
    for (n, (&y, &l)) in out.iter().zip(&levels).enumerate() {
        assert_eq!(y, l * white(3, n), "sample {n}");
    }
}

/// A reset starts the stream again: the first frame after it is the stream's first.
#[test]
fn a_reset_replays_the_stream_from_its_seed() {
    let mut d = Direct::new(
        NoiseNode::<f32>::from(NoiseConfig::new(NoiseColour::White).seed(5)),
        RATE,
        16,
    );
    d.block();
    d.block();
    Node::<f32>::reset(&mut *d.node);
    d.block();
    for (n, &y) in d.output().plane(0).samples().iter().enumerate() {
        assert_eq!(y, white(5, n), "sample {n}");
    }
}
