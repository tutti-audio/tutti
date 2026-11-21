use super::*;
use crate::test_support::RATE;
use tutti_buffers::{ChannelLayout, Interleaved};
use tutti_graph::contract::{Direct, assert_param_fork};
use tutti_units::{SampleRate, UnitAddr};

fn layout(width: usize) -> ChannelLayout {
    ChannelLayout::try_from(width).unwrap()
}

fn source(channels: &[f32], frames: usize) -> Arc<Wave<f32>> {
    let layout = ChannelLayout::try_from(channels.len()).unwrap();
    let mut wave = Wave::<f32>::new(channels.len(), SampleRate::SR_48K);
    let interleaved: Vec<f32> = (0..frames).flat_map(|_| channels.iter().copied()).collect();
    wave.append(Interleaved::new(&interleaved, layout).unwrap())
        .unwrap();
    Arc::new(wave)
}

/// A single grain of `length` frames: the density is one a second, so the next is a second on.
fn one_grain(channels: &[f32], length: usize) -> GranularNode<f32> {
    GranularNode::try_from(
        GranularConfig::new(layout(channels.len()), source(channels, 64))
            .density(Hz::new(1.0))
            .length(Seconds::new(length as f64 / 48_000.0)),
    )
    .unwrap()
}

fn hann(n: usize, length: usize) -> f32 {
    (0.5 - 0.5 * (2.0 * core::f64::consts::PI * n as f64 / length as f64).cos()) as f32
}

#[test]
fn a_fork_shares_no_cell() {
    assert_param_fork(one_grain(&[0.5], 8));
}

#[test]
fn the_shape_is_a_source_of_the_sources_channels_declaring_density_pitch_length_jitters_and_position()
 {
    let node = one_grain(&[0.5, 0.25], 8);
    let shape = Node::<f32>::shape(&node);
    assert_eq!(shape.audio_in().count(), 0);
    assert_eq!(shape.audio_out().count(), 2);
    assert_eq!(
        shape.params.as_slice(),
        &[
            UnitAddr::new(UnitParam::Rate, 0),
            UnitAddr::new(UnitParam::Pitch, 0),
            UnitAddr::new(UnitParam::GrainLength, 0),
            UnitAddr::new(UnitParam::Jitter, 0),
            UnitAddr::new(UnitParam::Jitter, 1),
            UnitAddr::new(UnitParam::Position, 0)
        ][..]
    );
    assert_eq!(Node::<f32>::param_base(&node, 0), Some(1.0));
    assert_eq!(Node::<f32>::param_base(&node, 2), Some(8.0 / 48_000.0));
}

/// One grain of a stereo constant is each channel's constant under the window.
#[test]
fn a_grain_is_the_window_times_each_channels_constant() {
    let mut d = Direct::new(one_grain(&[0.5, 0.25], 8), RATE, 16);
    d.block();
    for (c, level) in [0.5_f32, 0.25].into_iter().enumerate() {
        for (n, &y) in d.output().plane(c).samples().iter().enumerate() {
            let want = if n < 8 { level * hann(n, 8) } else { 0.0 };
            assert!(
                (y - want).abs() < 1e-6,
                "channel {c} frame {n}: {y} against {want}"
            );
        }
    }
}

/// A driven grain length (port 2) is the window's length: 16 frames.
#[test]
fn a_driven_grain_length_sets_the_window() {
    let length = [16.0_f32 / 48_000.0; 32];
    let mut d = Direct::new(one_grain(&[1.0], 8), RATE, 32);
    d.feed(2, Some(&length));
    d.block();
    for (n, &y) in d.output().plane(0).samples().iter().enumerate() {
        let want = if n < 16 { hann(n, 16) } else { 0.0 };
        assert!((y - want).abs() < 1e-6, "frame {n}: {y} against {want}");
    }
}

/// A non-finite driven density holds the last finite one: the block of density 2400 a second
/// starts a grain every 20 frames, so the next block (NaN) goes on at the same density.
#[test]
fn a_non_finite_driven_density_holds_the_last_finite_one() {
    let mut d = Direct::new(
        GranularNode::try_from(
            GranularConfig::new(ChannelLayout::MONO, source(&[1.0], 64))
                .density(Hz::new(2_400.0))
                .length(Seconds::new(20.0 / 48_000.0)),
        )
        .unwrap(),
        RATE,
        40,
    );
    d.feed(0, Some(&[2_400.0_f32; 40]));
    d.block();
    d.feed(0, Some(&[f32::NAN; 40]));
    d.block();
    // Grains of 20 frames every 20 start at frame 0 of each block: the sum of Hann at a hop of
    // its length is 0 at every grain start and positive between.
    let out = d.output().plane(0).samples();
    assert!(
        out[0].abs() < 1e-6 && out[20].abs() < 1e-6 && out[10] > 0.9,
        "{out:?}"
    );
}

/// A reset starts the cloud again: the first frames after it are the first grain's.
#[test]
fn a_reset_starts_the_first_grain_again() {
    let mut d = Direct::new(one_grain(&[1.0], 8), RATE, 8);
    d.block();
    d.block();
    Node::<f32>::reset(&mut *d.node);
    d.block();
    for (n, &y) in d.output().plane(0).samples().iter().enumerate() {
        assert!((y - hann(n, 8)).abs() < 1e-6, "frame {n}");
    }
}

/// A mono source feeds every output channel the same grain.
#[test]
fn a_mono_source_feeds_every_output_channel() {
    let node = GranularNode::try_from(
        GranularConfig::new(ChannelLayout::STEREO, source(&[0.5], 64))
            .density(Hz::new(1.0))
            .length(Seconds::new(8.0 / 48_000.0)),
    )
    .unwrap();
    let mut d = Direct::new(node, RATE, 8);
    d.block();
    for c in 0..2 {
        for (n, &y) in d.output().plane(c).samples().iter().enumerate() {
            assert!((y - 0.5 * hann(n, 8)).abs() < 1e-6, "channel {c} frame {n}");
        }
    }
}

/// A stereo source cannot feed one or three channels: no channel of it is the one they read.
#[test]
fn a_source_neither_mono_nor_as_wide_as_the_output_is_refused() {
    for width in [1, 3] {
        let config = GranularConfig::new(layout(width), source(&[0.5, 0.25], 64));
        assert_eq!(
            GranularNode::try_from(config).err(),
            Some(GranularError::Width {
                source_channels: 2,
                output_channels: width as u16
            })
        );
    }
}
