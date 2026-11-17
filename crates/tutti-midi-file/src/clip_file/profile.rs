//! The Set Profile On messages at the head of a Clip Configuration Header
//! (M2-116 section 6.2) as SysEx7 packets: the framing only, the message is
//! [`ProfileOnRequest`]'s.

use tutti_midi_device::{NotSetProfileOn, ProfileOnRequest};
use tutti_ump::{Message, MidiGroup, SysexStatus, Ump};

/// `request` as the packets a header holds it in, on the first Group.
pub(crate) fn profile_packets(request: ProfileOnRequest) -> impl Iterator<Item = Ump> {
    request.packets(MidiGroup::FIRST)
}

/// The Set Profile On messages in `packets`, the leading packets of a header
/// (as [`leading_profile_len`](super::leading_profile_len) counts them), each
/// message's parts adjacent: a Complete packet, or a Start, Continues and an
/// End.
pub(crate) fn profiles(
    packets: impl Iterator<Item = Ump>,
) -> impl Iterator<Item = Result<ProfileOnRequest, NotSetProfileOn>> {
    packets
        .scan(Vec::new(), |payload: &mut Vec<u8>, ump| {
            let Ok(Message::Sysex7(_, packet)) = ump.decode() else {
                return Some(None);
            };
            if matches!(packet.status(), SysexStatus::Complete | SysexStatus::Start) {
                payload.clear();
            }
            payload.extend_from_slice(packet.data());
            let done = matches!(packet.status(), SysexStatus::Complete | SysexStatus::End);
            Some(done.then(|| ProfileOnRequest::try_from(payload.as_slice())))
        })
        .flatten()
}
