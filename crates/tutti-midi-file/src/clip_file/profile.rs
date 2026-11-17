//! The Set Profile On messages at the head of a Clip Configuration Header
//! (M2-116 section 6.2), as typed values.

use std::vec::Vec;

use tutti_midi_device::{
    CI_SUB_ID_1, CI_UNIVERSAL_SYSEX, CI_VERSION, CiDeviceId, ProfileChannels, ProfileId,
};
use tutti_ump::{Message, MidiGroup, Sysex7Packets, SysexStatus, U14, Ump};

/// MIDI-CI sub-ID#2 of Set Profile On (M2-101 section 7.8).
const SET_PROFILE_ON: u8 = 0x22;

/// The Broadcast MUID, four 7-bit bytes of `0x7F` (M2-101 section 3.3.3).
const BROADCAST: [u8; 4] = [0x7F; 4];

/// The payload bytes before the Profile Id: the preamble of M2-101 section
/// 5.2.1, the Device ID, the sub-IDs, the version and the two MUIDs.
const PREAMBLE: usize = 13;

/// The payload of a Message Format Version 1 Set Profile On: the preamble and
/// the Profile Id.
const VERSION_1_LEN: usize = PREAMBLE + 5;

/// The payload of a Message Format Version 2 Set Profile On: the Version 1
/// payload and the two bytes of the Number of Channels Requested.
const VERSION_2_LEN: usize = VERSION_1_LEN + 2;

/// A Set Profile On message of a Clip Configuration Header (M2-116 section
/// 6.2): the Profile the sequence data is meant for, and the Channels, Group
/// or Function Block it is for.
///
/// The message is written with the Source and Destination MUIDs set to
/// Broadcast, as section 6.2 asks: a Clip File has no device to address. A
/// message read keeps whatever MUIDs it had in the file and a write of the
/// file gives them back; this value is what is left once they are set aside.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProfileOn {
    /// The Destination Device ID: a Channel, the Group or the Function Block
    /// the Profile is for (M2-101 section 3.2).
    pub device_id: CiDeviceId,
    /// The Profile the sequence data is intended for.
    pub profile: ProfileId,
    /// The Number of Channels Requested; 0 for a Group or Function Block
    /// address. A Version 1 message has no such field and reads as 0.
    pub channels: ProfileChannels,
}

/// Why the bytes of a Set Profile On in a Clip Configuration Header are not
/// one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum ProfileOnError {
    /// The message is shorter than its Message Format Version holds.
    #[error("a Set Profile On of {len} bytes is short of its fields")]
    Short {
        /// The bytes the message has.
        len: usize,
    },
    /// The Message Format Version byte is 0 or has its top bit set.
    #[error("{0:#04x} is not a MIDI-CI Message Format Version")]
    Version(u8),
    /// The Device ID is a reserved value.
    #[error("{0:#04x} is not a MIDI-CI Device ID (reserved)")]
    DeviceId(u8),
    /// The five Profile Id bytes are not a Profile Id (a byte with its top bit
    /// set, or a manufacturer id that is not one).
    #[error("the Profile Id of a Set Profile On is not one")]
    Profile,
    /// A Number of Channels Requested byte has its top bit set.
    #[error("the Number of Channels Requested has a byte with its top bit set")]
    Channels,
}

impl ProfileOn {
    /// A Set Profile On for `profile` at `device_id`, asking for `channels`.
    #[must_use]
    pub const fn new(device_id: CiDeviceId, profile: ProfileId, channels: ProfileChannels) -> Self {
        Self {
            device_id,
            profile,
            channels,
        }
    }

    /// The message's SysEx7 payload (M2-101 section 7.8) at the current
    /// Message Format Version, both MUIDs Broadcast.
    fn payload(self) -> Vec<u8> {
        let mut out = vec![
            CI_UNIVERSAL_SYSEX,
            self.device_id.get(),
            CI_SUB_ID_1,
            SET_PROFILE_ON,
            CI_VERSION.get(),
        ];
        out.extend(BROADCAST);
        out.extend(BROADCAST);
        out.extend(self.profile.to_bytes());
        out.extend(U14::masked(self.channels.get()).to_le7());
        out
    }

    /// The message as SysEx7 packets on Group 1.
    pub(crate) fn packets(self) -> Vec<Ump> {
        // Every payload byte is 7-bit: the fields are typed to be.
        Sysex7Packets::from_payload(MidiGroup::FIRST, &self.payload())
            .into_iter()
            .flatten()
            .collect()
    }

    /// The message of a SysEx7 `payload`, the bytes between the SysEx start and
    /// end.
    fn read(payload: &[u8]) -> Result<Self, ProfileOnError> {
        let Some(&version) = payload.get(4) else {
            return Err(ProfileOnError::Short { len: payload.len() });
        };
        let wanted = match version {
            0 | 0x80.. => return Err(ProfileOnError::Version(version)),
            1 => VERSION_1_LEN,
            _ => VERSION_2_LEN,
        };
        if payload.len() < wanted {
            return Err(ProfileOnError::Short { len: payload.len() });
        }
        let device_id = CiDeviceId::new(payload[1]).ok_or(ProfileOnError::DeviceId(payload[1]))?;
        let profile = ProfileId::from_bytes([
            payload[PREAMBLE],
            payload[PREAMBLE + 1],
            payload[PREAMBLE + 2],
            payload[PREAMBLE + 3],
            payload[PREAMBLE + 4],
        ])
        .ok_or(ProfileOnError::Profile)?;
        let channels = if version == 1 {
            ProfileChannels::of(0)
        } else {
            let wire = U14::from_le7([payload[VERSION_1_LEN], payload[VERSION_1_LEN + 1]])
                .ok_or(ProfileOnError::Channels)?;
            ProfileChannels::new(wire.get()).ok_or(ProfileOnError::Channels)?
        };
        Ok(Self {
            device_id,
            profile,
            channels,
        })
    }
}

/// The Set Profile On messages in `packets`, the leading packets of a header
/// (as [`leading_profile_len`](super::leading_profile_len) counts them), each
/// message's bytes read: its parts are adjacent, a Complete packet or a Start,
/// Continues and an End.
pub(crate) fn read_all(packets: &[Ump]) -> Vec<Result<ProfileOn, ProfileOnError>> {
    let mut messages = Vec::new();
    let mut payload: Vec<u8> = Vec::new();
    for ump in packets {
        let Ok(Message::Sysex7(_, packet)) = ump.decode() else {
            continue;
        };
        if matches!(packet.status(), SysexStatus::Complete | SysexStatus::Start) {
            payload.clear();
        }
        payload.extend_from_slice(packet.data());
        if matches!(packet.status(), SysexStatus::Complete | SysexStatus::End) {
            messages.push(ProfileOn::read(&payload));
        }
    }
    messages
}
