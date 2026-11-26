use super::super::testing::{OTHER, OURS, PEER, channel, header_from, sent, target_at};
use super::super::*;
use super::ProfileResponder;
use tutti_ump::MidiChannel;

fn own() -> Muid {
    Muid::of(OURS)
}

fn piano() -> ProfileId {
    ProfileId::from_bytes([0x7E, 1, 2, 3, 4]).unwrap()
}

fn strings() -> ProfileId {
    ProfileId::from_bytes([0x7E, 9, 2, 3, 4]).unwrap()
}

fn one() -> ProfileOfferEntry {
    ProfileOfferEntry::new(ProfileChannels::of(1))
}

fn inquiry(address: CiDeviceId) -> CiMessage {
    CiMessage::ProfileInquiry {
        header: header_from(PEER, address),
        to: own(),
    }
}

fn set_on(address: CiDeviceId, channels: u16, profile: ProfileId) -> CiMessage {
    CiMessage::SetProfileOn {
        source: Muid::of(PEER),
        format: ProfileFormat::current(target_at(address, channels)),
        to: own(),
        profile,
    }
}

fn set_off(address: CiDeviceId, profile: ProfileId) -> CiMessage {
    CiMessage::SetProfileOff {
        header: header_from(PEER, address),
        to: own(),
        profile,
    }
}

fn responder(offer: &ProfileOffer) -> ProfileResponder {
    let mut responder = ProfileResponder::default();
    let _ = responder.offer(own(), offer);
    responder
}

/// What a Report says: its state, address, Profile and count.
fn reports(actions: &[CiAction]) -> Vec<(ProfileState, CiDeviceId, ProfileId, ProfileChannels)> {
    sent(actions)
        .into_iter()
        .filter_map(|message| match message {
            CiMessage::ProfileEnabled {
                format, profile, ..
            } => Some((
                ProfileState::Enabled,
                format.address(),
                *profile,
                format.channels()?,
            )),
            CiMessage::ProfileDisabled {
                format, profile, ..
            } => Some((
                ProfileState::Disabled,
                format.address(),
                *profile,
                format.channels()?,
            )),
            _ => None,
        })
        .collect()
}

fn lists_of(message: &CiMessage) -> (CiDeviceId, Vec<ProfileId>, Vec<ProfileId>) {
    let CiMessage::ProfileReply {
        header, profiles, ..
    } = message
    else {
        panic!("a Reply, got {message:?}");
    };
    (
        header.device_id,
        profiles.enabled().to_vec(),
        profiles.disabled().to_vec(),
    )
}

/// Spec: m2-102/2.4#4, m2-102/2.4#11, m2-102/2.4#12
#[test]
fn an_inquiry_on_the_function_block_is_answered_with_channels_then_the_group_then_the_function_block()
 {
    let offer = ProfileOffer::default()
        .with(CiDeviceId::FunctionBlock, strings(), one())
        .with(
            CiDeviceId::Group,
            piano(),
            one().enabled(ProfileChannels::of(1)),
        )
        .with(channel(5), piano(), one())
        .with(channel(2), piano(), one().enabled(ProfileChannels::of(1)))
        .with(channel(2), strings(), one());
    let mut responder = responder(&offer);
    let actions = responder.on_message(own(), &inquiry(CiDeviceId::FunctionBlock));
    let replies: Vec<_> = sent(&actions).into_iter().map(lists_of).collect();
    assert_eq!(
        replies,
        [
            (channel(2), vec![piano()], vec![strings()]),
            (channel(5), vec![], vec![piano()]),
            (CiDeviceId::Group, vec![piano()], vec![]),
            (CiDeviceId::FunctionBlock, vec![], vec![strings()]),
        ]
    );
    for message in sent(&actions) {
        assert_eq!(message.header().source, own());
        assert_eq!(message.destination(), Destination::Muid(Muid::of(PEER)));
    }
}

#[test]
fn an_inquiry_ends_with_an_empty_function_block_reply_when_it_has_no_profiles_there() {
    let offer = ProfileOffer::default().with(channel(0), piano(), one());
    let mut responder = responder(&offer);
    let actions = responder.on_message(own(), &inquiry(CiDeviceId::FunctionBlock));
    let replies: Vec<_> = sent(&actions).into_iter().map(lists_of).collect();
    assert_eq!(
        replies,
        [
            (channel(0), vec![], vec![piano()]),
            (CiDeviceId::FunctionBlock, vec![], vec![]),
        ]
    );
}

#[test]
fn an_inquiry_on_one_channel_is_answered_with_that_channel_only() {
    let offer = ProfileOffer::default()
        .with(channel(0), piano(), one())
        .with(channel(1), strings(), one());
    let mut responder = responder(&offer);
    let actions = responder.on_message(own(), &inquiry(channel(1)));
    let replies: Vec<_> = sent(&actions).into_iter().map(lists_of).collect();
    assert_eq!(replies, [(channel(1), vec![], vec![strings()])]);
}

/// Spec: m2-102/2.6#1, m2-102/2.6#2, m2-102/2.7#2
#[test]
fn set_on_and_set_off_are_reported_to_everyone_with_the_channels_and_tell_the_app() {
    let offer = ProfileOffer::default().with(
        channel(3),
        piano(),
        ProfileOfferEntry::new(ProfileChannels::of(4)),
    );
    let mut responder = responder(&offer);

    let on = responder.on_message(own(), &set_on(channel(3), 4, piano()));
    assert_eq!(
        reports(&on),
        [(
            ProfileState::Enabled,
            channel(3),
            piano(),
            ProfileChannels::of(4)
        )]
    );
    let [CiMessage::ProfileEnabled { source, .. }] = sent(&on)[..] else {
        panic!("one Report");
    };
    assert_eq!(*source, own());
    assert_eq!(sent(&on)[0].destination(), Destination::Broadcast);
    assert_eq!(
        on[1],
        CiAction::ProfileSwitched {
            peer: Muid::of(PEER),
            profile: piano(),
            target: target_at(channel(3), 4),
            state: ProfileState::Enabled,
        }
    );
    assert_eq!(
        responder
            .table
            .get(channel(3), piano())
            .map(ProfileEntry::state),
        Some(ProfileState::Enabled)
    );

    let off = responder.on_message(own(), &set_off(channel(3), piano()));
    assert_eq!(
        reports(&off),
        [(
            ProfileState::Disabled,
            channel(3),
            piano(),
            ProfileChannels::of(4)
        )]
    );
    assert_eq!(sent(&off)[0].destination(), Destination::Broadcast);
    assert!(matches!(
        off[1],
        CiAction::ProfileSwitched {
            state: ProfileState::Disabled,
            ..
        }
    ));
}

/// Spec: m2-102/2.6#5, m2-102/2.7#3, m2-102/2.7#6
#[test]
fn a_fixed_profile_answers_with_the_state_it_keeps_and_tells_no_one() {
    let offer = ProfileOffer::default()
        .with(channel(0), piano(), one().fixed())
        .with(
            channel(1),
            strings(),
            one().enabled(ProfileChannels::of(1)).fixed(),
        );
    let mut responder = responder(&offer);

    let on = responder.on_message(own(), &set_on(channel(0), 1, piano()));
    assert_eq!(
        reports(&on),
        [(
            ProfileState::Disabled,
            channel(0),
            piano(),
            ProfileChannels::of(1)
        )]
    );
    assert_eq!(on.len(), 1, "no switch to tell");
    let off = responder.on_message(own(), &set_off(channel(1), strings()));
    assert_eq!(
        reports(&off),
        [(
            ProfileState::Enabled,
            channel(1),
            strings(),
            ProfileChannels::of(1)
        )]
    );
    assert_eq!(off.len(), 1);
    assert_eq!(
        responder
            .table
            .get(channel(0), piano())
            .map(ProfileEntry::state),
        Some(ProfileState::Disabled)
    );
}

/// Spec: m2-102/2.6#6, m2-102/2.7#7
#[test]
fn a_profile_not_offered_at_that_address_is_nakked_with_its_id() {
    let offer = ProfileOffer::default().with(channel(0), piano(), one());
    let mut responder = responder(&offer);
    for (sub, message) in [
        (0x22u8, set_on(channel(1), 1, piano())),
        (0x22, set_on(channel(0), 1, strings())),
        (0x23, set_off(channel(1), piano())),
    ] {
        let actions = responder.on_message(own(), &message);
        let [CiMessage::Nak { header, to, nak }] = sent(&actions)[..] else {
            panic!("a NAK, got {actions:?}");
        };
        assert_eq!(*to, Muid::of(PEER));
        assert_eq!(header.device_id, message.header().device_id);
        assert_eq!(nak.nak_sub_id2.get(), sub);
        assert_eq!(nak.status, NakStatus::ProfileNotSupported);
        let wanted = match &message {
            CiMessage::SetProfileOn { profile, .. } | CiMessage::SetProfileOff { profile, .. } => {
                *profile
            }
            _ => unreachable!(),
        };
        assert_eq!(nak.details.map(|b| b.get()), wanted.to_bytes());
    }
}

/// Spec: m2-102/2.5.1#4
#[test]
fn a_set_on_for_more_channels_than_the_maximum_is_refused_with_the_state_kept() {
    let offer = ProfileOffer::default().with(
        channel(0),
        piano(),
        ProfileOfferEntry::new(ProfileChannels::of(4)),
    );
    let mut responder = responder(&offer);
    let actions = responder.on_message(own(), &set_on(channel(0), 5, piano()));
    assert_eq!(
        reports(&actions),
        [(
            ProfileState::Disabled,
            channel(0),
            piano(),
            ProfileChannels::of(4)
        )]
    );
    assert_eq!(actions.len(), 1);
    let at_max = responder.on_message(own(), &set_on(channel(0), 4, piano()));
    assert_eq!(
        reports(&at_max),
        [(
            ProfileState::Enabled,
            channel(0),
            piano(),
            ProfileChannels::of(4)
        )]
    );
}

fn mpe() -> ProfileOfferEntry {
    ProfileOfferEntry::new(ProfileChannels::of(15))
}

/// Spec: m2-120/3.4#2
#[test]
fn an_mpe_zone_that_shares_a_channel_with_an_enabled_zone_is_refused() {
    let offer = ProfileOffer::default()
        .with(channel(0), ProfileId::MPE, mpe())
        .with(channel(7), ProfileId::MPE, mpe())
        .with(channel(8), ProfileId::MPE, mpe());
    let mut responder = responder(&offer);
    let first = responder.on_message(own(), &set_on(channel(0), 8, ProfileId::MPE));
    assert_eq!(reports(&first)[0].0, ProfileState::Enabled);
    // Channels 8..=15 follow Manager 0's zone of 8 Channels: a zone from Channel 7 is shared.
    let second = responder.on_message(own(), &set_on(channel(7), 4, ProfileId::MPE));
    assert_eq!(
        reports(&second),
        [(
            ProfileState::Disabled,
            channel(7),
            ProfileId::MPE,
            ProfileChannels::of(15)
        )]
    );
    assert_eq!(second.len(), 1);
    // A zone clear of the first is accepted, and the two are held together.
    let clear = responder.on_message(own(), &set_on(channel(8), 8, ProfileId::MPE));
    assert_eq!(
        reports(&clear)[0].0,
        ProfileState::Enabled,
        "Channels 8 to 15 are free"
    );
    let zones = responder.table.mpe_zones();
    assert!(zones.refused.is_empty());
    assert_eq!(zones.zones.map(|set| set.zones().count()), Some(2));
}

#[test]
fn an_mpe_count_that_is_no_zone_is_refused() {
    let offer = ProfileOffer::default().with(channel(0), ProfileId::MPE, mpe());
    let mut responder = responder(&offer);
    let actions = responder.on_message(own(), &set_on(channel(0), 1, ProfileId::MPE));
    assert_eq!(reports(&actions)[0].0, ProfileState::Disabled);
}

#[test]
fn details_name_the_channels_in_use_and_the_maximum() {
    let offer = ProfileOffer::default().with(
        channel(0),
        piano(),
        ProfileOfferEntry::new(ProfileChannels::of(6)),
    );
    let mut responder = responder(&offer);
    let ask = || CiMessage::ProfileDetailsInquiry {
        header: header_from(PEER, channel(0)),
        to: own(),
        profile: piano(),
        target: InquiryTarget::NUMBER_OF_CHANNELS,
    };
    let detail = |actions: &[CiAction]| {
        let [CiMessage::ProfileDetailsReply { data, target, .. }] = sent(actions)[..] else {
            panic!("a Details Reply, got {actions:?}");
        };
        assert_eq!(*target, InquiryTarget::NUMBER_OF_CHANNELS);
        data.as_channels().unwrap()
    };
    let before = detail(&responder.on_message(own(), &ask()));
    assert_eq!((before.in_use.get(), before.max.get()), (0, 6));
    let _ = responder.on_message(own(), &set_on(channel(0), 3, piano()));
    let after = detail(&responder.on_message(own(), &ask()));
    assert_eq!((after.in_use.get(), after.max.get()), (3, 6));
}

#[test]
fn a_message_for_another_device_is_not_answered() {
    let offer = ProfileOffer::default().with(channel(0), piano(), one());
    let mut responder = responder(&offer);
    let to_other = CiMessage::SetProfileOn {
        source: Muid::of(PEER),
        format: ProfileFormat::current(target_at(channel(0), 1)),
        to: Muid::of(OTHER),
        profile: piano(),
    };
    assert!(responder.on_message(own(), &to_other).is_empty());
}

/// Spec: m2-101/7.4#3, m2-102/2.4#16
#[test]
fn an_added_profile_that_is_enabled_is_followed_at_once_by_its_enabled_report() {
    let mut responder = ProfileResponder::default();
    let offer = ProfileOffer::default()
        .with(channel(1), piano(), one().enabled(ProfileChannels::of(1)))
        .with(channel(2), strings(), one());
    let actions = responder.offer(own(), &offer);
    let kinds: Vec<_> = sent(&actions)
        .into_iter()
        .map(|m| (m.sub_id2(), m.header().device_id))
        .collect();
    assert_eq!(
        kinds,
        [
            (SUB_ID2_PROFILE_ADDED, channel(1)),
            (SUB_ID2_PROFILE_ENABLED, channel(1)),
            (SUB_ID2_PROFILE_ADDED, channel(2)),
        ]
    );
    assert!(
        sent(&actions)
            .iter()
            .all(|m| m.destination() == Destination::Broadcast)
    );
}

/// Spec: m2-102/2.4#19, m2-102/2.7#2
#[test]
fn removing_an_enabled_profile_sends_disabled_before_removed() {
    let offer = ProfileOffer::default()
        .with(channel(1), piano(), one().enabled(ProfileChannels::of(1)))
        .with(channel(2), strings(), one());
    let mut responder = responder(&offer);
    let actions = responder.offer(own(), &ProfileOffer::default());
    let kinds: Vec<_> = sent(&actions)
        .into_iter()
        .map(|m| (m.sub_id2(), m.header().device_id))
        .collect();
    assert_eq!(
        kinds,
        [
            (SUB_ID2_PROFILE_DISABLED, channel(1)),
            (SUB_ID2_PROFILE_REMOVED, channel(1)),
            (SUB_ID2_PROFILE_REMOVED, channel(2)),
        ]
    );
    assert!(responder.table.is_empty());
}

/// Spec: m2-102/2.6#3, m2-102/2.7#4
#[test]
fn the_app_switching_a_profile_is_reported_like_a_peer_switch() {
    let offer = ProfileOffer::default().with(channel(1), piano(), one());
    let mut responder = responder(&offer);
    let on = responder.offer(
        own(),
        &ProfileOffer::default().with(channel(1), piano(), one().enabled(ProfileChannels::of(1))),
    );
    assert_eq!(
        reports(&on),
        [(
            ProfileState::Enabled,
            channel(1),
            piano(),
            ProfileChannels::of(1)
        )]
    );
    let off = responder.offer(own(), &offer);
    assert_eq!(
        reports(&off),
        [(
            ProfileState::Disabled,
            channel(1),
            piano(),
            ProfileChannels::of(1)
        )]
    );
}

#[test]
fn an_equal_offer_sends_nothing() {
    let offer =
        ProfileOffer::default().with(channel(1), piano(), one().enabled(ProfileChannels::of(1)));
    let mut responder = responder(&offer);
    assert!(responder.offer(own(), &offer.clone()).is_empty());
}

#[test]
fn an_entry_left_equal_keeps_the_peers_switch_and_a_changed_one_drops_it() {
    let offer = ProfileOffer::default()
        .with(channel(1), piano(), one())
        .with(channel(2), strings(), one());
    let mut responder = responder(&offer);
    let _ = responder.on_message(own(), &set_on(channel(1), 1, piano()));
    let _ = responder.on_message(own(), &set_on(channel(2), 1, strings()));

    // Strings changes its cap; piano is left as it was.
    let changed = ProfileOffer::default()
        .with(channel(1), piano(), one())
        .with(
            channel(2),
            strings(),
            ProfileOfferEntry::new(ProfileChannels::of(2)),
        );
    let actions = responder.offer(own(), &changed);
    assert_eq!(
        reports(&actions),
        [(
            ProfileState::Disabled,
            channel(2),
            strings(),
            ProfileChannels::of(2)
        )],
        "strings goes back to the app's value, and only strings is reported"
    );
    let state = |at, p| responder.table.get(at, p).map(ProfileEntry::state);
    assert_eq!(state(channel(1), piano()), Some(ProfileState::Enabled));
    assert_eq!(state(channel(2), strings()), Some(ProfileState::Disabled));
}

#[test]
fn a_peer_switch_tells_the_app_with_the_channel_it_switched() {
    let offer = ProfileOffer::default().with(
        channel(0),
        piano(),
        ProfileOfferEntry::new(ProfileChannels::of(2)),
    );
    let mut responder = responder(&offer);
    let actions = responder.on_message(own(), &set_on(channel(0), 2, piano()));
    assert!(actions.contains(&CiAction::ProfileSwitched {
        peer: Muid::of(PEER),
        profile: piano(),
        target: ProfileTarget::Channel {
            channel: MidiChannel::of(0),
            channels: ProfileChannels::of(2),
        },
        state: ProfileState::Enabled,
    }));
}
