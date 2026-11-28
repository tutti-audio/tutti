//! The VBAP panner over each bed: where its speakers sit, Pulkki's properties
//! of the gains at many directions.
//!
//! With a constant input pair and the width at zero the output of a settled
//! panner is the gain vector itself, so each case reads the gains off one
//! rendered block.

use tutti_buffers::{Bed, Planes};
use tutti_graph::ParamNode;
use tutti_graph::contract::{drive, prepared};
use tutti_nodes::testing::aim;
use tutti_units::{Azimuth, Elevation, SampleRate, UnitParam};

const RATE: SampleRate = SampleRate::SR_48K;

/// A bed's placed speakers at BS.2051-3 Table 1's nominal (azimuth, elevation),
/// in the bed's channel order with the LFE left out: the middle layer at
/// elevation 0, the upper at +30. Written out here, bed by bed, from the
/// systems' loudspeaker lists (positive azimuth to the left).
fn nominal(bed: Bed) -> Vec<(f64, f64)> {
    let front = [(30.0, 0.0), (-30.0, 0.0)];
    let centre = [(0.0, 0.0)];
    let side_110 = [(110.0, 0.0), (-110.0, 0.0)];
    let side_90 = [(90.0, 0.0), (-90.0, 0.0)];
    let back_135 = [(135.0, 0.0), (-135.0, 0.0)];
    let top_front_30 = [(30.0, 30.0), (-30.0, 30.0)];
    let top_front_45 = [(45.0, 30.0), (-45.0, 30.0)];
    let top_back_110 = [(110.0, 30.0), (-110.0, 30.0)];
    let top_back_135 = [(135.0, 30.0), (-135.0, 30.0)];
    let list: Vec<&[(f64, f64)]> = match bed {
        Bed::Mono => vec![&centre],
        Bed::Stereo => vec![&front],
        Bed::Quad => vec![&[(45.0, 0.0), (-45.0, 0.0)], &back_135],
        Bed::Surround50 | Bed::Surround51 => vec![&front, &centre, &side_110],
        Bed::Surround70 | Bed::Surround71 => vec![&front, &centre, &side_90, &back_135],
        Bed::Surround512 => vec![&front, &centre, &side_110, &top_front_30],
        Bed::Surround514 => vec![&front, &centre, &side_110, &top_front_30, &top_back_110],
        Bed::Surround712 => vec![&front, &centre, &side_90, &back_135, &top_front_45],
        Bed::Surround714 => vec![
            &front,
            &centre,
            &side_90,
            &back_135,
            &top_front_45,
            &top_back_135,
        ],
    };
    list.concat()
}

/// The LFE channel of a bed: index 3 where it has one (5.1, 7.1 and the
/// height beds), none where it does not.
fn lfe(bed: Bed) -> Option<usize> {
    use Bed::*;
    matches!(
        bed,
        Surround51 | Surround71 | Surround512 | Surround514 | Surround712 | Surround714
    )
    .then_some(3)
}

/// The channel each of the bed's placed speakers feeds.
fn channel_of_speaker(bed: Bed) -> Vec<usize> {
    (0..bed.width().count() as usize)
        .filter(|&c| Some(c) != lfe(bed))
        .collect()
}

/// The panner over `bed` with its width at zero (one gain vector), aimed and
/// seated, rendering a block of DC: the gains, one per channel.
fn gains(bed: Bed, azimuth: f64, elevation: f64, spread: f32) -> Vec<f32> {
    let mut node = prepared(vbap::<f32>(bed), RATE, 16);
    let params = node.param_set();
    params.set(UnitParam::StereoWidth, 0.0);
    params.set(UnitParam::Spread, f64::from(spread));
    aim(&params, Azimuth::new(azimuth), Elevation::new(elevation));
    let ones = [1.0f32; 16];
    let out = drive(&mut node, RATE, Planes::new(&[&ones, &ones]).unwrap(), &[]);
    out.iter().map(|c| c[15]).collect()
}

/// Every bed's speaker rows sit at the nominal figures: a source aimed at a
/// speaker's nominal (azimuth, elevation) feeds that speaker's channel at unit
/// gain and no other, the LFE channel silent.
#[test]
fn each_beds_speakers_sit_at_table_1s_nominal_angles() {
    for bed in Bed::ALL {
        if bed == Bed::Mono {
            continue;
        }
        let channels = channel_of_speaker(bed);
        let figures = nominal(bed);
        assert_eq!(channels.len(), figures.len(), "{bed:?}");
        for (&channel, &(azimuth, elevation)) in channels.iter().zip(&figures) {
            let g = gains(bed, azimuth, elevation, 0.0);
            for (c, &gain) in g.iter().enumerate() {
                let want = if c == channel { 1.0 } else { 0.0 };
                assert!(
                    (gain - want).abs() < 1e-4,
                    "{bed:?} at ({azimuth}, {elevation}): channel {c} reads {gain}, want {want}"
                );
            }
        }
    }
}

/// Pulkki's properties at many directions of every bed: every gain is
/// non-negative, the power sum is 1, and the LFE is never fed. Spread keeps the
/// power sum at 1 too.
#[test]
fn the_gains_are_non_negative_and_unit_power_and_never_feed_the_lfe() {
    for bed in Bed::ALL {
        if bed == Bed::Mono {
            continue;
        }
        for spread in [0.0f32, 0.4, 1.0] {
            for elevation in [-60.0f64, -20.0, 0.0, 15.0, 30.0, 55.0, 90.0] {
                for azimuth in (-180..180).step_by(11) {
                    let at = format!("{bed:?} at ({azimuth}, {elevation}), spread {spread}");
                    let g = gains(bed, f64::from(azimuth) + 0.5, elevation, spread);
                    assert!(g.iter().all(|&x| x >= -1e-6), "{at}: {g:?}");
                    let power: f32 = g.iter().map(|x| x * x).sum();
                    assert!((power - 1.0).abs() < 1e-4, "{at}: power {power}");
                    if let Some(lfe) = lfe(bed) {
                        assert_eq!(g[lfe], 0.0, "{at}: the LFE is fed");
                    }
                }
            }
        }
    }
}

/// A panner over `bed`.
fn vbap<F: tutti_buffers::Float>(bed: tutti_buffers::Bed) -> tutti_nodes::VbapPannerNode<F> {
    let bed = tutti_dsp::BedLayout::try_from(bed).expect("a bed with speakers to pan between");
    tutti_nodes::VbapPannerNode::from(tutti_nodes::VbapPannerConfig::new(bed))
}
