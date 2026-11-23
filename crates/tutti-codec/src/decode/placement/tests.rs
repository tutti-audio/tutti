use super::*;
use Speaker::*;

/// The topology alone, for the cases that are about the speakers.
fn topology_of(channels: &Channels) -> ChannelTopology {
    super::channels_of(Some(channels))
        .0
        .topology()
        .cloned()
        .expect("a topology")
}

/// The samples come in ascending flag order, so the topology lists the
/// speakers in that order whatever order the flags were written in; the
/// layouts the three codecs state come out as the speakers their specs name.
#[test]
fn flags_become_speakers_in_sample_order() {
    // Vorbis 6: FL FC FR RL RR LFE written, FL FR FC LFE RL RR read.
    let vorbis_six = Channels::Positioned(
        Position::FRONT_LEFT
            | Position::FRONT_CENTER
            | Position::FRONT_RIGHT
            | Position::REAR_LEFT
            | Position::REAR_RIGHT
            | Position::LFE1,
    );
    assert_eq!(
        topology_of(&vorbis_six).positions().unwrap(),
        [
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight
        ]
    );
    let quad = Channels::Positioned(
        Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::REAR_LEFT | Position::REAR_RIGHT,
    );
    assert_eq!(
        topology_of(&quad).positions().unwrap(),
        [FrontLeft, FrontRight, BackLeft, BackRight]
    );
    // WAV 5.1 (0x60F): side surrounds.
    let wav_six = Channels::Positioned(Position::from_bits_truncate(0x60F));
    assert_eq!(
        topology_of(&wav_six),
        tutti_buffers::Bed::Surround51.topology()
    );
}

/// A lone channel is the engine's mono, however symphonia names it.
#[test]
fn a_lone_channel_is_mono() {
    assert_eq!(
        topology_of(&Channels::Positioned(Position::FRONT_LEFT))
            .positions()
            .unwrap(),
        [FrontCenter]
    );
    assert_eq!(
        topology_of(&Channels::Discrete(1)).positions().unwrap(),
        [FrontCenter]
    );
}

/// A flag with no speaker keeps its slot as `Unknown(bit index)`, never
/// `Unassigned` and never dropped: the second LFE is bit 18.
#[test]
fn a_flag_with_no_speaker_keeps_its_slot_as_unknown() {
    let t = topology_of(&Channels::Positioned(
        Position::LFE1 | Position::LFE2 | Position::FRONT_LEFT,
    ));
    assert_eq!(
        t.positions().unwrap(),
        [FrontLeft, LowFrequency, Unknown(18)]
    );
    assert_eq!(
        topology_of(&Channels::Positioned(
            Position::FRONT_LEFT_WIDE | Position::FRONT_RIGHT_WIDE
        ))
        .positions()
        .unwrap(),
        [Unknown(24), Unknown(25)]
    );
}

/// Channels that name no position feed no speaker: each keeps its slot as
/// `Unassigned`.
#[test]
fn channels_without_positions_are_unassigned() {
    assert_eq!(
        topology_of(&Channels::Discrete(3)).positions().unwrap(),
        [Unassigned; 3]
    );
}

/// A positioned set is stated, a container that states a count and no placement
/// is the default layout for that count and says so, and a container that
/// states nothing is stereo by default.
#[test]
fn a_count_without_positions_takes_the_default_layout_and_says_so() {
    use tutti_buffers::{Bed, ChannelLayout};
    let basis = |c: &Channels| super::channels_of(Some(c)).1;
    assert_eq!(
        basis(&Channels::Positioned(
            Position::FRONT_LEFT | Position::FRONT_RIGHT
        )),
        Placement::Stated
    );
    for (n, bed) in [
        (2u16, Bed::Stereo),
        (4, Bed::Quad),
        (6, Bed::Surround51),
        (8, Bed::Surround71),
        (12, Bed::Surround714),
    ] {
        assert_eq!(
            super::channels_of(Some(&Channels::Discrete(n))),
            (ChannelSet::from(bed.topology()), Placement::Default),
            "{n}"
        );
    }
    assert_eq!(
        super::channels_of(Some(&Channels::Discrete(3))),
        (
            ChannelSet::unassigned(ChannelLayout::from(3u16)),
            Placement::Default
        )
    );
    assert_eq!(
        super::channels_of(None),
        (ChannelSet::from(Bed::Stereo.topology()), Placement::Default)
    );
}

/// A positioned set wider than a topology holds keeps its width and has no
/// topology, stated, and is not truncated to the speakers a topology holds.
#[test]
fn a_positioned_set_wider_than_a_topology_has_none() {
    let all = Position::all();
    assert!(
        all.bits().count_ones() > 16,
        "the case needs more than 16 flags"
    );
    let (set, placement) = super::channels_of(Some(&Channels::Positioned(all)));
    assert_eq!(u32::from(set.width().count()), all.bits().count_ones());
    assert_eq!((set.topology(), placement), (None, Placement::Stated));
}

/// An ambisonic track is the ACN, SN3D format of its order, stated, from first
/// order to the cap (a zeroth-order track is one channel: the mono); a higher
/// order keeps its width and has no topology.
#[test]
fn an_ambisonic_track_is_its_format() {
    use tutti_buffers::{AmbisonicFormat, AmbisonicOrder};
    for order in 1u8..=3 {
        let (set, p) = super::channels_of(Some(&Channels::Ambisonic(order)));
        assert_eq!(
            set,
            ChannelSet::from(ChannelTopology::from(AmbisonicFormat::ambix(
                AmbisonicOrder::of(order)
            )))
        );
        assert_eq!(p, Placement::Stated);
    }
    let (set, p) = super::channels_of(Some(&Channels::Ambisonic(4)));
    assert_eq!(set.width().count(), 25);
    assert_eq!((set.topology(), p), (None, Placement::Stated));
}
