//! What the note-logic tests share: a one-voice patch and a step at a channel.

use tutti_ump::{Attribute, MidiChannel, NoteNumber, NoteVelocity, Voice2};
use tutti_units::Amplitude;
use tutti_voicing::{Patch, VoiceAllocator, VoiceCommand, VoiceSpec};

pub(crate) struct One;
impl Patch for One {
    fn voices(
        &self,
        _: MidiChannel,
        _: NoteNumber,
        _: tutti_ump::NoteVelocity,
        spec: &mut dyn FnMut(VoiceSpec),
    ) {
        spec(VoiceSpec {
            group: None,
            scope: 0,
            tag: 0,
        });
    }
}

/// `message` on `channel`, every voice sounding at unit level.
pub(crate) fn step(core: &mut VoiceAllocator, channel: u8, message: Voice2) -> Vec<VoiceCommand> {
    core.step(MidiChannel::of(channel), message, &One, |_| {
        Some(Amplitude::new(1.0))
    })
    .to_vec()
}

/// A Note On of `key` at velocity `0x8000`.
pub(crate) fn note_on(key: u8) -> Voice2 {
    Voice2::NoteOn {
        note: NoteNumber::of(key),
        velocity: NoteVelocity::CENTER,
        attribute: Attribute::None,
    }
}
