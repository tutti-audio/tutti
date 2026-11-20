use tutti_buffers::{AmbisonicFormat, AmbisonicOrder, Bed, Speaker};
use tutti_units::{Azimuth, Elevation};

use super::*;

/// Table 4's edges: 1.41 needs |elevation| < 30 (open) and 60 <= |azimuth|
/// <= 120 (closed), on either side. A `<=` on the elevation or an open
/// azimuth range moves one of these rows.
#[test]
fn table_4_edges_are_closed_on_azimuth_and_open_on_elevation() {
    let at = |azimuth: f64, elevation: f64| {
        ChannelWeight::of(Direction::new(
            Azimuth::new(azimuth),
            Elevation::new(elevation),
        )) == ChannelWeight::Surround
    };
    for sign in [1.0f64, -1.0] {
        assert!(at(sign * 60.0, 0.0), "60 is inside");
        assert!(at(sign * 120.0, 0.0), "120 is inside");
        assert!(!at(sign * 59.9, 0.0), "59.9 is outside");
        assert!(!at(sign * 120.1, 0.0), "120.1 is outside");
        assert!(at(sign * 90.0, 29.9), "elevation 29.9 is inside");
        assert!(!at(sign * 90.0, 30.0), "elevation 30 is outside");
        assert!(at(sign * 90.0, -29.9), "elevation -29.9 is inside");
        assert!(!at(sign * 90.0, -30.0), "elevation -30 is outside");
    }
}

/// A quad given as VST3 spells it (`FL FR SL SR`) is the bed's M+-135 backs, which weigh 1.0.
#[test]
fn a_quads_side_speakers_stand_for_its_backs_and_weigh_one() {
    let vst3_quad = ChannelTopology::new([
        Speaker::FrontLeft,
        Speaker::FrontRight,
        Speaker::SideLeft,
        Speaker::SideRight,
    ])
    .unwrap();
    let weights = channel_weights(&vst3_quad).unwrap();
    assert!(
        weights.iter().all(|&w| w == ChannelWeight::Unity),
        "{weights:?}"
    );
}

/// Table 5: a 5.1's side speakers (M+-110) weigh 1.41, its front three 1.0 and its LFE nothing.
#[test]
fn a_51_weighs_its_sides_141_and_its_lfe_nothing() {
    let weights = channel_weights(&Bed::Surround51.topology()).unwrap();
    use ChannelWeight::{Silent, Surround, Unity};
    // FL FR FC LFE SL SR
    assert_eq!(
        &*weights,
        &[Unity, Unity, Unity, Silent, Surround, Surround]
    );
    assert_eq!(Surround.power(), 1.41);
}

/// A 7.1's rear speakers (M+-135) and a 7.1.4's upper speakers weigh 1.0, as Table 5 lists.
#[test]
fn a_714s_backs_and_uppers_weigh_one() {
    let weights = channel_weights(&Bed::Surround714.topology()).unwrap();
    let surround = weights
        .iter()
        .filter(|&&w| w == ChannelWeight::Surround)
        .count();
    assert_eq!(surround, 2, "only the two side speakers: {weights:?}");
}

/// Ambisonics and an unplaced set have no angle to weigh.
#[test]
fn ambisonic_and_unplaced_topologies_are_refused() {
    let unplaced = ChannelTopology::unassigned(ChannelLayout::from(3u16));
    assert_eq!(
        channel_weights(&unplaced),
        Err(ChannelWeightError::Unplaced {
            width: ChannelLayout::from(3u16)
        })
    );
    let ambisonic: ChannelTopology = AmbisonicFormat::ambix(AmbisonicOrder::of(1)).into();
    assert_eq!(
        channel_weights(&ambisonic),
        Err(ChannelWeightError::Ambisonic)
    );
}
