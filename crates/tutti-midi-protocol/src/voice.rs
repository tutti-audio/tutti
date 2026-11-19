//! Reading a channel voice message at MIDI 2.0.

use tutti_ump::{Address, Message, Ump, Voice2};

/// The channel voice message `ump` carries, as the MIDI 2.0 message it means
/// (M2-104 Appendix D.3: a MIDI 1.0 one is translated, so its Note On at velocity
/// 0 is a Note Off, while a MIDI 2.0 Note On at velocity 0 stays a Note On), or
/// `None` for any other packet, one [`Ump::decode`] cannot frame included.
///
/// What an instrument reads: one match on [`Voice2`], whichever protocol the
/// packet was sent in.
#[inline]
#[must_use]
pub fn channel_voice(packet: impl Into<Ump>) -> Option<(Address, Voice2)> {
    match packet.into().decode().ok().map(Message::to_midi2)? {
        Message::Midi2(address, voice) => Some((address, voice)),
        _ => None,
    }
}
