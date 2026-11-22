use tutti_platform::Instant;

use tutti_midi_ci::Muid;
use tutti_midi_ci::{
    CiDeviceId, CiHeader, CiMessage, Decoded, DetailsData, Initiator, Nak, NakStatus,
    ProfileAction, ProfileId,
};
use tutti_midi_protocol::{ControllerSupport, PitchBendSupport};

use tutti_midi_ci::{InquiryTarget, ProfileInitiator};
use tutti_ump::U7;

use super::*;

const OURS: u32 = 0x0101_0101;
const PEER: u32 = 0x0202_0202;

fn initiator() -> (ProfileInitiator, Instant) {
    let now = Instant::now();
    let (mut initiator, _) = ProfileInitiator::new(Muid::of(OURS), Muid::of(PEER), now);
    // The final Reply on the Function Block ends the Profile inquiry.
    initiator.on_message(
        &Decoded::Message(CiMessage::ProfileReply {
            header: CiHeader::new(Muid::of(PEER)),
            to: Muid::of(OURS),
            profiles: tutti_midi_ci::ProfileLists::new(
                tutti_midi_ci::ProfileList::try_from(vec![ProfileId::MPE]).unwrap(),
                tutti_midi_ci::ProfileList::default(),
            ),
        }),
        now,
    );
    (initiator, now)
}

fn features_target() -> InquiryTarget {
    InquiryTarget::new(U7::masked(MpeFeatures::INQUIRY_TARGET))
}

/// The Profile Details Inquiry for the MPE Profile's Optional Features.
fn ask(
    i: &mut ProfileInitiator,
    channel: tutti_ump::MidiChannel,
    now: Instant,
) -> Vec<ProfileAction> {
    i.details(
        CiDeviceId::Channel(channel),
        ProfileId::MPE,
        features_target(),
        now,
    )
    .unwrap()
}

fn reply(data: &[u8]) -> Decoded {
    Decoded::Message(CiMessage::ProfileDetailsReply {
        header: CiHeader::new(Muid::of(PEER)),
        to: Muid::of(OURS),
        profile: ProfileId::MPE,
        target: features_target(),
        data: DetailsData::new(data.to_vec()).unwrap(),
    })
}

fn outcome(actions: Vec<ProfileAction>) -> ProfileOutcome {
    let [ProfileAction::Resolved { outcome, .. }] = &actions[..] else {
        panic!("one resolved request, got {actions:?}");
    };
    outcome.clone()
}

/// M2-120 section 3.7.1: the inquiry is a Profile Details Inquiry (sub-ID
/// `0x28`) for the MPE Profile (`31 00 01 01 ..`) with Inquiry Target `0x01`.
#[test]
fn the_inquiry_asks_the_mpe_profile_for_target_one() {
    let (mut i, now) = initiator();
    let actions = ask(&mut i, tutti_ump::MidiChannel::of(0), now);
    let [
        ProfileAction::Send(CiMessage::ProfileDetailsInquiry {
            profile, target, ..
        }),
    ] = &actions[..]
    else {
        panic!("a Details Inquiry, got {actions:?}");
    };
    assert_eq!(*profile, ProfileId::MPE);
    assert_eq!(target.get().get(), 0x01);
}

/// M2-120 section 3.7.3: the four data bytes are the Channel Response
/// notification bit, Pitch Bend, Channel Pressure and Third Dimension; the
/// Reply the peer sends is read as exactly those.
#[test]
fn the_reply_reads_as_the_four_declared_features() {
    let (mut i, now) = initiator();
    let address = CiDeviceId::Channel(tutti_ump::MidiChannel::of(0));
    ask(&mut i, tutti_ump::MidiChannel::of(0), now);
    let mut answer = reply(&[0x01, 0x01, 0x01, 0x02]);
    if let Decoded::Message(CiMessage::ProfileDetailsReply { header, .. }) = &mut answer {
        header.device_id = address;
    }
    let features = mpe_features(&outcome(i.on_message(&answer, now))).unwrap();
    assert!(features.channel_response_notification);
    assert_eq!(features.pitch_bend, PitchBendSupport::Supported);
    assert_eq!(features.channel_pressure, ControllerSupport::ControlChange);
    assert_eq!(features.third_dimension, ControllerSupport::Bipolar);
}

/// A Reply whose data is not four bytes of defined values is an `Unreadable`
/// error carrying the data, never a nearest feature set; a NAK and a timeout
/// are their own errors.
#[test]
fn an_unreadable_refused_or_unanswered_reply_is_a_typed_error() {
    for bad in [
        &[0x01u8, 0x01, 0x00][..],
        &[0x02, 0x00, 0x00, 0x00],
        &[0x00, 0x02, 0x00, 0x00],
        &[0x00, 0x00, 0x03, 0x00],
    ] {
        let data = DetailsData::new(bad.to_vec()).unwrap();
        assert_eq!(
            mpe_features(&ProfileOutcome::Details(data.clone())),
            Err(MpeFeaturesError::Unreadable(data))
        );
    }
    let nak = Nak::new(tutti_ump::U7::of(0x28), NakStatus::ProfileNotSupported);
    assert_eq!(
        mpe_features(&ProfileOutcome::Refused(nak.clone())),
        Err(MpeFeaturesError::Refused(nak))
    );
    assert_eq!(
        mpe_features(&ProfileOutcome::TimedOut),
        Err(MpeFeaturesError::TimedOut)
    );
}
