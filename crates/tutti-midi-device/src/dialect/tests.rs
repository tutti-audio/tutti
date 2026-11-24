use tutti_platform::Instant;

use tutti_ump::{
    BlockDirection, FirstGroup, FunctionBlockId, GroupSpan, Midi1Port, MidiChannel, Protocol,
    StreamConfig,
};

use super::*;
use crate::endpoint::{Capabilities, FunctionBlock, Supports};
use tutti_fixtures::midi;
use tutti_midi_ci::Initiator;
use tutti_midi_ci::{
    CiMessage, Decoded, Muid, ProfileChannels, ProfileFormat, ProfileInitiator, ProfileTarget,
};

const OURS: u32 = 0x0100_0001;
const PEER: u32 = 0x0200_0002;

fn ch(n: u8) -> MidiChannel {
    MidiChannel::of(n)
}

fn block_id() -> FunctionBlockId {
    FunctionBlockId::new(0).unwrap()
}

fn endpoint(protocol: Protocol, direction: BlockDirection, active: bool) -> Endpoint {
    Endpoint::new(Supports::of(Capabilities {
        midi1: true,
        midi2: true,
        rx_jr: false,
        tx_jr: false,
    }))
    .in_use(StreamConfig {
        protocol,
        rx_jr: false,
        tx_jr: false,
    })
    .block(
        FunctionBlock::builder(
            block_id(),
            direction,
            FirstGroup::new(0).unwrap(),
            GroupSpan::new(1).unwrap(),
        )
        .active(active)
        .build()
        .unwrap(),
    )
}

fn endpoint_with(protocol: Protocol, port: Midi1Port, rx_jr: bool) -> Endpoint {
    Endpoint::new(Supports::of(Capabilities {
        midi1: true,
        midi2: true,
        rx_jr: true,
        tx_jr: true,
    }))
    .in_use(StreamConfig {
        protocol,
        rx_jr,
        tx_jr: false,
    })
    .block(
        FunctionBlock::builder(
            block_id(),
            BlockDirection::Bidirectional,
            FirstGroup::new(0).unwrap(),
            GroupSpan::new(1).unwrap(),
        )
        .midi1(port)
        .build()
        .unwrap(),
    )
}

#[test]
fn a_restricted_midi_1_port_is_a_restricted_midi_1_voice() {
    // M2-104 §7.1.8: the MIDI 1.0 field is 0x00 no, 0x01 yes, 0x02 yes with the
    // bandwidth restricted to 31.25 kb/s.
    for (port, rate) in [
        (Midi1Port::Restricted, Midi1Rate::Restricted),
        (Midi1Port::Unrestricted, Midi1Rate::Unrestricted),
        (Midi1Port::No, Midi1Rate::Unrestricted),
    ] {
        let endpoint = endpoint_with(Protocol::Midi1, port, false);
        assert_eq!(
            of(&endpoint, &table(&[])).unwrap().voice(),
            ChannelVoice::Midi1 { rate },
            "{port:?}"
        );
    }
    // The restriction is a MIDI 1.0 one: MIDI 2.0 messages carry none.
    let endpoint = endpoint_with(Protocol::Midi2, Midi1Port::Restricted, false);
    assert_eq!(
        of(&endpoint, &table(&[])).unwrap().voice(),
        ChannelVoice::Midi2
    );
}

#[test]
fn jr_timestamps_are_prepended_when_the_endpoint_receives_them() {
    // M2-104 §7.1.6.1: the Stream Configuration has a receive and a transmit
    // direction; what the sender sends is the Endpoint's receive.
    for (rx_jr, expected) in [(true, JrTimestamps::Prepended), (false, JrTimestamps::Off)] {
        let endpoint = endpoint_with(Protocol::Midi2, Midi1Port::No, rx_jr);
        assert_eq!(
            of(&endpoint, &table(&[])).unwrap().jr_timestamps(),
            expected
        );
    }
    let mut endpoint = endpoint_with(Protocol::Midi2, Midi1Port::No, false);
    endpoint = endpoint.in_use(StreamConfig {
        protocol: Protocol::Midi2,
        rx_jr: false,
        tx_jr: true,
    });
    assert_eq!(
        of(&endpoint, &table(&[])).unwrap().jr_timestamps(),
        JrTimestamps::Off,
        "the Endpoint's own timestamps are not ours to send"
    );
}

fn speaking(protocol: Protocol) -> Endpoint {
    endpoint(protocol, BlockDirection::Bidirectional, true)
}

/// The target at `address`; only a Channel names a count (§7.10 note).
fn target(address: CiDeviceId, channels: u16) -> ProfileTarget {
    match address {
        CiDeviceId::Channel(channel) => ProfileTarget::Channel {
            channel,
            channels: ProfileChannels::of(channels),
        },
        CiDeviceId::Group => ProfileTarget::Group,
        CiDeviceId::FunctionBlock => ProfileTarget::FunctionBlock,
    }
}

/// The table of a peer that reported each `(address, channels)` as an enabled
/// MPE Profile, in a version 2 report, or in a version 1 one for `None`.
fn table(reports: &[(CiDeviceId, Option<u16>)]) -> ProfileTable {
    let (mut peer, _) = ProfileInitiator::new(Muid::of(OURS), Muid::of(PEER), Instant::now());
    for &(address, channels) in reports {
        peer.on_message(
            &Decoded::Message(CiMessage::ProfileEnabled {
                source: Muid::of(PEER),
                format: match channels {
                    Some(channels) => ProfileFormat::current(target(address, channels)),
                    None => ProfileFormat::Version1 { address },
                },
                profile: ProfileId::MPE,
            }),
            Instant::now(),
        );
    }
    peer.table().clone()
}

fn channel(n: u8) -> CiDeviceId {
    CiDeviceId::Channel(ch(n))
}

fn of(endpoint: &Endpoint, table: &ProfileTable) -> Result<Dialect, NoDialect> {
    Dialect::of(endpoint, block_id(), table)
}

#[test]
fn the_active_protocol_is_the_channel_voice_to_send() {
    // M2-104 §7.1.6.2: the Protocol of the active stream configuration.
    let none = table(&[]);
    assert_eq!(
        of(&speaking(Protocol::Midi1), &none).unwrap().voice(),
        ChannelVoice::Midi1 {
            rate: Midi1Rate::Unrestricted
        }
    );
    assert_eq!(
        of(&speaking(Protocol::Midi2), &none).unwrap().voice(),
        ChannelVoice::Midi2
    );
}

#[test]
fn a_reserved_active_protocol_is_no_dialect() {
    // A peer's Stream Configuration Notification can name one, which a
    // discovered Endpoint keeps as it was read.
    let mut peer = speaking(Protocol::Midi2);
    peer.active.protocol = midi::reserved_protocol(0x03);
    assert_eq!(
        of(&peer, &table(&[])),
        Err(NoDialect::UnspokenProtocol(midi::reserved_protocol(0x03)))
    );
}

#[test]
fn a_block_without_a_profile_is_channelized() {
    assert_eq!(
        *of(&speaking(Protocol::Midi2), &table(&[]))
            .unwrap()
            .expression(),
        Expression::Channelized
    );
}

#[test]
fn an_mpe_profile_with_a_count_enables_a_zone_from_its_manager_channel_up() {
    // M2-120 §3.1: Set Profile On goes to the Manager Channel with a number of
    // Channels, the Manager included; the Members are the channels above it.
    let dialect = of(&speaking(Protocol::Midi2), &table(&[(channel(0), Some(8))])).unwrap();
    let Expression::Mpe(zones) = dialect.expression() else {
        panic!("an MPE block, got {dialect:?}");
    };
    let mut all = zones.zones();
    let (Some(zone), None) = (all.next(), all.next()) else {
        panic!("one zone, got {zones:?}");
    };
    assert_eq!(zone.manager(), ch(0));
    assert_eq!(zone.channels().get(), 8);
    assert!(zone.members().eq((1..8).map(ch)));
}

#[test]
fn mpe_over_midi_1_is_the_same_zone_in_the_other_voice() {
    // M2-120 §2.2: the Profile carries MIDI 1.0 or MIDI 2.0 messages.
    let reports = table(&[(channel(2), Some(4))]);
    let one = of(&speaking(Protocol::Midi1), &reports).unwrap();
    let two = of(&speaking(Protocol::Midi2), &reports).unwrap();
    assert_eq!(one.expression(), two.expression());
    assert_ne!(one.voice(), two.voice());
}

#[test]
fn two_mpe_profiles_on_separate_channels_are_two_zones_in_order() {
    // M2-120 §3.4: several Profiles are several zones on channels that do not overlap.
    let reports = table(&[(channel(8), Some(8)), (channel(0), Some(8))]);
    let dialect = of(&speaking(Protocol::Midi2), &reports).unwrap();
    let Expression::Mpe(zones) = dialect.expression() else {
        panic!("MPE");
    };
    let managers: Vec<u8> = zones.zones().map(|z| z.manager().get()).collect();
    assert_eq!(managers, [0, 8]);
    assert_eq!(zones.zone_of(ch(7)).unwrap().manager(), ch(0));
    assert_eq!(zones.zone_of(ch(8)).unwrap().manager(), ch(8));
}

/// Spec: m2-120/3.4#1
#[test]
fn overlapping_mpe_profiles_are_refused() {
    // M2-120 §3.4: two enabled Profiles shall not share a channel.
    let reports = table(&[(channel(0), Some(4)), (channel(3), Some(4))]);
    assert_eq!(
        of(&speaking(Protocol::Midi2), &reports),
        Err(NoDialect::MpeOverlap(MpeOverlap {
            held: MpeZoneSpan::new(ch(0), MpeChannels::of(4)).unwrap(),
            added: MpeZoneSpan::new(ch(3), MpeChannels::of(4)).unwrap(),
        }))
    );
}

/// Spec: m2-120/3.4#1
#[test]
fn zones_that_touch_are_not_overlapping() {
    let reports = table(&[(channel(0), Some(4)), (channel(4), Some(4))]);
    assert!(of(&speaking(Protocol::Midi2), &reports).is_ok());
}

#[test]
fn a_zone_needs_a_manager_and_a_member_and_fits_in_sixteen_channels() {
    // M2-120 §2.1: a Manager Channel and one or more Member Channels.
    for (manager, count) in [(0, 1), (0, 0), (15, 2), (4, 13), (0, 17)] {
        let reports = table(&[(channel(manager), Some(count))]);
        assert!(
            matches!(
                of(&speaking(Protocol::Midi2), &reports),
                Err(NoDialect::MpeNotAZone { .. })
            ),
            "{count} channels from {manager}"
        );
    }
}

#[test]
fn an_enabled_mpe_profile_of_unknown_width_is_not_guessed() {
    let reports = table(&[(channel(1), None)]);
    assert_eq!(
        of(&speaking(Protocol::Midi2), &reports),
        Err(NoDialect::MpeChannelsUnknown(ch(1)))
    );
}

#[test]
fn mpe_enabled_on_the_group_or_block_is_refused() {
    // M2-120 §3.1: the Profile is enabled by a message to the Manager Channel.
    for address in [CiDeviceId::Group, CiDeviceId::FunctionBlock] {
        let reports = table(&[(address, Some(8))]);
        assert_eq!(
            of(&speaking(Protocol::Midi2), &reports),
            Err(NoDialect::MpeNotOnChannel(address))
        );
    }
}

#[test]
fn a_disabled_mpe_profile_is_no_zone() {
    // M2-102 §7.3.3: a supported Profile that is not enabled does not apply.
    let (mut peer, _) = ProfileInitiator::new(Muid::of(OURS), Muid::of(PEER), Instant::now());
    peer.on_message(
        &Decoded::Message(CiMessage::ProfileDisabled {
            source: Muid::of(PEER),
            format: ProfileFormat::current(target(channel(0), 8)),
            profile: ProfileId::MPE,
        }),
        Instant::now(),
    );
    assert_eq!(
        *of(&speaking(Protocol::Midi2), peer.table())
            .unwrap()
            .expression(),
        Expression::Channelized
    );
}

/// Spec: m2-104/7.1.8#4
#[test]
fn a_block_that_is_unknown_inactive_or_transmit_only_is_no_dialect() {
    // M2-104 §7.1.8: the Active flag and the BlockDirection of a Function Block.
    let none = table(&[]);
    let other = FunctionBlockId::new(5).unwrap();
    assert_eq!(
        Dialect::of(&speaking(Protocol::Midi2), other, &none),
        Err(NoDialect::UnknownBlock(other))
    );
    assert_eq!(
        of(
            &endpoint(Protocol::Midi2, BlockDirection::Bidirectional, false),
            &none
        ),
        Err(NoDialect::Inactive(block_id()))
    );
    assert_eq!(
        of(
            &endpoint(Protocol::Midi2, BlockDirection::Output, true),
            &none
        ),
        Err(NoDialect::NotReceiving(block_id()))
    );
    assert!(
        of(
            &endpoint(Protocol::Midi2, BlockDirection::Input, true),
            &none
        )
        .is_ok()
    );
}
