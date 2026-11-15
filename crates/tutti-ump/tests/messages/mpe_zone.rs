//! M2-120 §2.1, §3.1 and §3.4: the channels of a Profile zone, and zones that
//! share none.

use tutti_ump::{MidiChannel, MpeChannels, MpeOverlap, MpeZoneSet, MpeZoneSpan};

fn ch(n: u8) -> MidiChannel {
    MidiChannel::of(n)
}

fn span(manager: u8, channels: u8) -> MpeZoneSpan {
    MpeZoneSpan::new(ch(manager), MpeChannels::new(channels).unwrap()).unwrap()
}

#[test]
fn a_zone_has_a_manager_and_one_to_fifteen_members() {
    // §2.1: a Manager Channel and one or more Member Channels.
    assert_eq!(MpeChannels::new(1), None);
    assert_eq!(MpeChannels::new(0), None);
    assert_eq!(MpeChannels::new(17), None);
    assert_eq!(MpeChannels::new(2).map(MpeChannels::get), Some(2));
    assert_eq!(MpeChannels::new(16).map(MpeChannels::get), Some(16));
}

#[test]
fn a_zone_ends_by_channel_16() {
    for (manager, count, fits) in [(0, 16, true), (14, 2, true), (15, 2, false), (4, 13, false)] {
        let zone = MpeZoneSpan::new(ch(manager), MpeChannels::new(count).unwrap());
        assert_eq!(zone.is_some(), fits, "{count} channels from {manager}");
    }
}

#[test]
fn the_members_are_the_channels_above_the_manager() {
    let zone = span(2, 6);
    assert!(zone.members().eq((3..8).map(ch)));
    assert!(zone.contains(ch(2)) && zone.contains(ch(7)));
    assert!(!zone.contains(ch(1)) && !zone.contains(ch(8)));
}

#[test]
fn zones_overlap_when_they_share_a_channel_and_not_when_they_touch() {
    // §3.4.
    assert!(span(0, 4).overlaps(span(3, 4)));
    assert!(span(3, 4).overlaps(span(0, 4)));
    assert!(span(0, 8).overlaps(span(2, 2)));
    assert!(!span(0, 4).overlaps(span(4, 4)));
}

#[test]
fn a_set_is_ordered_by_manager_whatever_the_order_added() {
    let set = MpeZoneSet::single(span(8, 8)).with(span(0, 8)).unwrap();
    let managers: Vec<u8> = set.zones().map(|z| z.manager().get()).collect();
    assert_eq!(managers, [0, 8]);
    assert_eq!(set.zone_of(ch(7)), Some(span(0, 8)));
    assert_eq!(set.zone_of(ch(8)), Some(span(8, 8)));
}

#[test]
fn a_zone_that_shares_a_channel_is_refused_with_both() {
    let held = span(0, 4);
    let added = span(3, 4);
    assert_eq!(
        MpeZoneSet::single(held).with(added),
        Err(MpeOverlap { held, added })
    );
}

#[test]
fn sixteen_channels_hold_eight_zones_and_no_ninth() {
    let mut set = MpeZoneSet::single(span(0, 2));
    for manager in (2..16).step_by(2) {
        set = set.with(span(manager, 2)).unwrap();
    }
    assert_eq!(set.zones().count(), MpeZoneSet::MAX);
    assert!((0..15).all(|m| set.with(span(m, 2)).is_err()));
}
