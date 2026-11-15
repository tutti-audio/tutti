use std::time::Instant;

use tutti_midi_device::{
    CiDeviceId, CiHeader, CiMessage, Decoded, Endpoint, Muid, ProfileChannels, ProfileId,
    ProfileInitiator,
};
use tutti_ump::{FunctionBlockId, GroupSpan, MidiChannel};

use super::*;

fn dialect(zones: &[(u8, u16)]) -> Dialect {
    let (mut peer, _) = ProfileInitiator::new(Muid::of(1), Muid::of(2), Instant::now());
    for &(manager, channels) in zones {
        peer.on_message(&Decoded::Message(CiMessage::ProfileEnabled {
            header: CiHeader {
                device_id: CiDeviceId::Channel(MidiChannel::of(manager)),
                ..CiHeader::new(Muid::of(2))
            },
            profile: ProfileId::MPE,
            channels: ProfileChannels::of(channels),
        }));
    }
    let endpoint = Endpoint::local(GroupSpan::new(1).unwrap());
    Dialect::of(&endpoint, FunctionBlockId::new(0).unwrap(), peer.table()).unwrap()
}

#[test]
fn a_block_with_no_mpe_profile_has_no_zones() {
    assert_eq!(mpe_zones(&dialect(&[])), None);
}

#[test]
fn an_enabled_profile_is_a_profile_zone_with_the_default_bend_range() {
    // M2-120 §3.1: the Manager Channel and the number of channels; §3.5: the
    // sensitivity starts at 48 HCUs.
    let zones = mpe_zones(&dialect(&[(2, 6)])).unwrap();
    let zone = zones.zone_of(MidiChannel::of(2)).unwrap();
    assert_eq!(zone.master_channel(), MidiChannel::of(2));
    assert_eq!(zone.channel_count(), 6);
    assert_eq!(
        zone.pitch_bend_range_for(MidiChannel::of(5)),
        PitchBendSensitivity::MPE_PROFILE_DEFAULT
    );
    assert!(zones.zone_of(MidiChannel::of(8)).is_none());
}

#[test]
fn two_profiles_are_two_zones() {
    // M2-120 §3.4.
    let zones = mpe_zones(&dialect(&[(0, 8), (8, 8)])).unwrap();
    assert_eq!(zones.zones().count(), 2);
}
