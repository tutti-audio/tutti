//! The General MIDI 2 Profiles as what a host needs to address a peer (M2-118,
//! M2-119).
//!
//! [`Gm2FunctionBlock`] is the Profile on a whole Function Block (M2-118): its
//! Profile Id, the Function Block address every message of it uses (§3.3), the
//! Channel count a Function Block gives it and the role each Channel starts
//! in. [`Gm2SingleChannel`] is the Profile on one Channel (M2-119), whose
//! Channel is a Melody Channel. A host reads from a [`ProfileTable`] whether a
//! peer has either enabled.
//!
//! The sound set, the controllers and the Mode messages are the General MIDI 2
//! specification's, which both Profiles adopt unchanged (M2-118 §3.1, M2-119
//! §3); a General MIDI 2 synthesizer is a device's side of the Profile, and
//! tutti has none here. The original GM System On and Off messages are read by
//! `tutti_midi_protocol`'s universal messages; a device that turns General MIDI
//! on or off by them reports the change with Profile Enabled or Disabled
//! (M2-118 §3.2.1), which is the device's side.

use tutti_ump::MidiChannel;

use super::{CiDeviceId, ProfileId, ProfileState, ProfileTable};

/// What a Channel is when General MIDI 2 turns on (General MIDI 2, adopted by
/// M2-118 §3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Gm2Role {
    /// A Melody Channel.
    Melody,
    /// A Rhythm Channel.
    Rhythm,
}

tutti_units::ranged! {
    /// How many Channels a Function Block gives the Profile: a multiple of 16
    /// from 16 to 256, one set of 16 per Group (M2-118 §3.3).
    #[derive(Eq, Hash)]
    Gm2ChannelCount(u16), NotAGm2ChannelCount, "a Channel count (a multiple of 16, 16 to 256)",
    |raw| raw >= 16 && raw <= 256 && raw.is_multiple_of(16)
}

impl Gm2ChannelCount {
    /// The number of Groups the Channels span.
    #[must_use]
    pub fn groups(self) -> u8 {
        // The count is at most 256, so the Groups are at most 16.
        match u8::try_from(self.get() / 16) {
            Ok(groups) => groups,
            Err(_) => unreachable!("a Channel count is at most 256"),
        }
    }
}

/// The General MIDI 2 Function Block Profile (M2-118).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Gm2FunctionBlock {}

impl Gm2FunctionBlock {
    /// The Device ID of every Profile Configuration message of the Profile:
    /// `0x7F`, the Function Block (M2-118 §3.3).
    pub const ADDRESS: CiDeviceId = CiDeviceId::FunctionBlock;

    /// The Channel that starts as a Rhythm Channel in each Group: Channel 10
    /// (M2-118 §3.3).
    pub const RHYTHM_CHANNEL: MidiChannel = MidiChannel::of(9);

    /// The role `channel` has in each Group when the Profile turns on: Channel
    /// 10 is a Rhythm Channel and the others Melody Channels (M2-118 §3.3).
    #[must_use]
    pub const fn default_role(channel: MidiChannel) -> Gm2Role {
        if channel.get() == Self::RHYTHM_CHANNEL.get() {
            Gm2Role::Rhythm
        } else {
            Gm2Role::Melody
        }
    }

    /// Whether the Profile is enabled on the Function Block in `table`.
    #[must_use]
    pub fn enabled_on(table: &ProfileTable) -> bool {
        table
            .get(Self::ADDRESS, ProfileId::GM2_FUNCTION_BLOCK)
            .is_some_and(|entry| entry.state() == ProfileState::Enabled)
    }
}

/// The General MIDI 2 Single Channel Profile (M2-119).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Gm2SingleChannel {}

impl Gm2SingleChannel {
    /// The role of the Channel: a Melody Channel. Switching to a Rhythm
    /// Channel is outside the Profile (M2-119 §3.1.1).
    pub const ROLE: Gm2Role = Gm2Role::Melody;

    /// Whether the Profile is enabled on `channel` in `table`.
    #[must_use]
    pub fn enabled_on(table: &ProfileTable, channel: MidiChannel) -> bool {
        table
            .get(CiDeviceId::Channel(channel), ProfileId::GM2_SINGLE_CHANNEL)
            .is_some_and(|entry| entry.state() == ProfileState::Enabled)
    }
}

#[cfg(test)]
mod tests;
