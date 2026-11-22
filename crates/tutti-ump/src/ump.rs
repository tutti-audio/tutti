//! [`Ump`]: one Universal MIDI Packet, as the words it travels in.

use bytemuck::{Pod, Zeroable};

use crate::{
    Address, CCNumber, GroupChannelIndex, MidiChannel, MidiGroup, NoteNumber, Protocol, Reserved,
};

/// A UMP message type, the top nibble of a packet's first word (M2-104 Table
/// 4).
///
/// A nibble Table 4 reserves is [`Reserved`](Self::Reserved), as read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageType {
    /// `0x0`: Utility messages (§7.2).
    Utility,
    /// `0x1`: System Common and System Real Time messages (§7.6).
    System,
    /// `0x2`: MIDI 1.0 Channel Voice messages (§7.3).
    Midi1ChannelVoice,
    /// `0x3`: System Exclusive 7 messages (§7.7).
    Sysex7,
    /// `0x4`: MIDI 2.0 Channel Voice messages (§7.4).
    Midi2ChannelVoice,
    /// `0x5`: System Exclusive 8 and Mixed Data Set messages (§7.8, §7.9).
    Data128,
    /// `0xD`: Flex Data messages (§7.5).
    FlexData,
    /// `0xF`: UMP Stream messages (§7.1).
    Stream,
    /// A nibble Table 4 reserves, as read.
    Reserved(Reserved),
}

impl MessageType {
    /// The message type of the nibble `nibble`, `0x0` to `0xF`.
    const fn from_nibble(nibble: u8) -> Self {
        match nibble {
            0x0 => Self::Utility,
            0x1 => Self::System,
            0x2 => Self::Midi1ChannelVoice,
            0x3 => Self::Sysex7,
            0x4 => Self::Midi2ChannelVoice,
            0x5 => Self::Data128,
            0xD => Self::FlexData,
            0xF => Self::Stream,
            reserved => Self::Reserved(Reserved::wire(reserved)),
        }
    }

    /// The nibble, `0x0` to `0xF`.
    #[must_use]
    pub const fn get(self) -> u8 {
        match self {
            Self::Utility => 0x0,
            Self::System => 0x1,
            Self::Midi1ChannelVoice => 0x2,
            Self::Sysex7 => 0x3,
            Self::Midi2ChannelVoice => 0x4,
            Self::Data128 => 0x5,
            Self::FlexData => 0xD,
            Self::Stream => 0xF,
            Self::Reserved(reserved) => reserved.get(),
        }
    }
}

/// One Universal MIDI Packet: four 32-bit words, of which the message type in
/// the top nibble of the first word says how many are meaningful
/// ([`words`](Self::words)). Unused words are zero.
///
/// The type is storage. It names no message and checks no field. Its readers
/// and writers (the header fields, [`is_note_off`](Self::is_note_off),
/// [`is_panic_controller`](Self::is_panic_controller),
/// [`note_key_event`](Self::note_key_event), [`note_off`](Self::note_off),
/// [`sustain_up`](Self::sustain_up)) are the packet-level bit reads that
/// consumers without a decoded `Message` need; each agrees with
/// [`decode`](Self::decode) and `From<Message> for Ump` on every packet.
///
/// `Pod`, so a block of packets crosses a process boundary as one copy.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Pod, Zeroable)]
pub struct Ump(pub [u32; 4]);

impl Ump {
    /// The meaningful words, 1 to 4 of them, by the message type (M2-104 Table
    /// 4): types `0x0` to `0x2`, `0x6` and `0x7` are 32 bits, `0x3`, `0x4`,
    /// `0x8` to `0xA` 64 bits, `0xB` and `0xC` 96 bits, and `0x5`, `0xD` to
    /// `0xF` 128 bits. A reserved type has its size too, so its words travel.
    #[inline]
    #[must_use]
    pub const fn words(&self) -> &[u32] {
        let n = match self.0[0] >> 28 {
            0x0..=0x2 | 0x6 | 0x7 => 1,
            0x3 | 0x4 | 0x8..=0xA => 2,
            0xB | 0xC => 3,
            _ => 4,
        };
        self.0.split_at(n).0
    }

    /// The packet at the front of `words`: as many words as its message type
    /// takes ([`words`](Self::words)), the rest zero. Words past the packet are
    /// not read. `None` when `words` is empty or ends inside the packet.
    ///
    /// ```
    /// use tutti_ump::Ump;
    ///
    /// assert_eq!(Ump::from_words(&[0x4090_3C00, 0x8000_0000, 7]), Some(Ump([0x4090_3C00, 0x8000_0000, 0, 0])));
    /// assert_eq!(Ump::from_words(&[0x4090_3C00]), None);
    /// ```
    #[must_use]
    pub fn from_words(words: &[u32]) -> Option<Self> {
        let first = *words.first()?;
        let n = Ump([first, 0, 0, 0]).words().len();
        let packet = words.get(..n)?;
        let mut out = [0u32; 4];
        out[..n].copy_from_slice(packet);
        Some(Ump(out))
    }

    /// The message type: the top nibble of the first word (M2-104 Table 4).
    #[inline]
    #[must_use]
    pub const fn message_type(&self) -> MessageType {
        MessageType::from_nibble((self.0[0] >> 28) as u8)
    }

    /// Whether this is a UMP Stream message (type `0xF`, M2-104 §7.1): the
    /// Endpoint's own conversation with its peer, not music.
    #[inline]
    #[must_use]
    pub const fn is_stream(&self) -> bool {
        matches!(self.message_type(), MessageType::Stream)
    }

    /// Whether this is a JR Clock or a JR Timestamp (type `0x0`, status `0x1` or
    /// `0x2`, M2-104 §7.2.2): a sender's notion of time, not music.
    #[inline]
    #[must_use]
    pub const fn is_jr(&self) -> bool {
        matches!(self.message_type(), MessageType::Utility)
            && matches!((self.0[0] >> 20) & 0xF, 1 | 2)
    }

    /// Whether this is a System Real Time message (type `0x1`, status
    /// `0xF8..=0xFF`, M2-104 §7.6): a message that may sit between the packets
    /// of another message.
    #[inline]
    #[must_use]
    pub const fn is_system_real_time(&self) -> bool {
        matches!(self.message_type(), MessageType::System) && (self.0[0] >> 16) & 0xFF >= 0xF8
    }

    /// The Group of a message type that has one: System (`0x1`), MIDI 1.0
    /// and 2.0 channel voice (`0x2`, `0x4`), Data 64 and 128 (`0x3`, `0x5`)
    /// and Flex Data (`0xD`). `None` for Utility (`0x0`) and UMP Stream
    /// (`0xF`), which are groupless, and for a reserved type.
    #[inline]
    #[must_use]
    pub const fn group(&self) -> Option<MidiGroup> {
        match self.message_type() {
            MessageType::System
            | MessageType::Midi1ChannelVoice
            | MessageType::Sysex7
            | MessageType::Midi2ChannelVoice
            | MessageType::Data128
            | MessageType::FlexData => Some(MidiGroup::masked((self.0[0] >> 24) as u8)),
            MessageType::Utility | MessageType::Stream | MessageType::Reserved(_) => None,
        }
    }

    /// The Group and Channel of a channel voice message (MIDI 1.0, `0x2`, or
    /// MIDI 2.0, `0x4`); `None` for any other type.
    #[inline]
    #[must_use]
    pub const fn address(&self) -> Option<Address> {
        match self.message_type() {
            MessageType::Midi1ChannelVoice | MessageType::Midi2ChannelVoice => Some(Address {
                group: MidiGroup::masked((self.0[0] >> 24) as u8),
                channel: MidiChannel::masked((self.0[0] >> 16) as u8),
            }),
            _ => None,
        }
    }

    /// The Channel of a channel voice message ([`address`](Self::address)'s).
    #[inline]
    #[must_use]
    pub const fn channel(&self) -> Option<MidiChannel> {
        match self.address() {
            Some(a) => Some(a.channel),
            None => None,
        }
    }

    /// Whether this is a note-off: a MIDI 1.0 channel-voice (type `0x2`) Note
    /// Off, a MIDI 1.0 Note On at velocity 0 (its spelling of the same thing),
    /// or a MIDI 2.0 channel-voice (type `0x4`) Note Off.
    ///
    /// A MIDI 2.0 Note On at velocity 0 is a note-on (M2-104 §7.4.2).
    #[inline]
    #[must_use]
    pub const fn is_note_off(&self) -> bool {
        let w = self.0[0];
        let status = (w >> 20) & 0xf;
        match w >> 28 {
            0x2 => status == 0x8 || (status == 0x9 && w & 0x7f == 0),
            0x4 => status == 0x8,
            _ => false,
        }
    }

    /// Whether this is a channel-mode panic: a Control Change (a MIDI 1.0 or
    /// MIDI 2.0 channel-voice packet, types `0x2` and `0x4`) of All Sound Off
    /// (controller 120) or All Notes Off (controller 123).
    ///
    /// With a note-off ([`is_note_off`](Self::is_note_off)) these are the
    /// packets whose loss leaves a sound hanging: a queue that must drop an
    /// event keeps them in preference to any other. The other channel-mode
    /// controllers (121, 122, 124 to 127) end nothing and are not panics.
    /// Controller numbers and their meanings: MIDI 1.0 Detailed Specification,
    /// Channel Mode Messages (CC 120 All Sound Off, CC 123 All Notes Off);
    /// the MIDI 2.0 Control Change (M2-104, MIDI 2.0 Control Change
    /// Message) keeps the index in the same bits of the first word.
    #[inline]
    #[must_use]
    pub const fn is_panic_controller(&self) -> bool {
        let w = self.0[0];
        match w >> 28 {
            0x2 | 0x4 => {
                let status = (w >> 20) & 0xf;
                let controller = ((w >> 8) & 0x7f) as u8;
                status == 0xb
                    && (controller == CCNumber::ALL_SOUND_OFF.get()
                        || controller == CCNumber::ALL_NOTES_OFF.get())
            }
            _ => false,
        }
    }

    /// The held-key fact of this packet, for an observer that must close what
    /// a stream opened: a note on or off by its `(protocol, group, channel,
    /// note)` key, or the sustain pedal (CC 64) of a channel going down or up.
    /// A MIDI 1.0 Note On at velocity 0 is an off, a MIDI 2.0 one an on, as in
    /// [`is_note_off`](Self::is_note_off).
    #[inline]
    #[must_use]
    pub const fn note_key_event(&self) -> Option<NoteKeyEvent> {
        let w = self.0[0];
        let status = (w >> 20) & 0xf;
        let m2 = w >> 28 == 0x4;
        let address = match self.address() {
            Some(address) => address,
            None => return None,
        };
        let note = NoteNumber::masked((w >> 8) as u8);
        let key = NoteKey::pack(m2, address, note);
        match status {
            0x8 => Some(NoteKeyEvent::Off(key)),
            0x9 => {
                if !m2 && w & 0x7f == 0 {
                    Some(NoteKeyEvent::Off(key))
                } else {
                    Some(NoteKeyEvent::On(key))
                }
            }
            0xb if (w >> 8) & 0x7f == CCNumber::SUSTAIN.get() as u32 => {
                let down = if m2 {
                    self.0[1] >= 0x8000_0000
                } else {
                    w & 0x7f >= 64
                };
                Some(NoteKeyEvent::Sustain {
                    protocol: key.protocol(),
                    address,
                    down,
                })
            }
            _ => None,
        }
    }

    /// The note-off that closes `key`, velocity 0 (M1: status 8, M2: status 8
    /// with a zero 16-bit velocity).
    #[must_use]
    pub const fn note_off(key: NoteKey) -> Ump {
        let address = key.address();
        let head = ((address.group.get() as u32) << 24)
            | (0x8 << 20)
            | ((address.channel.get() as u32) << 16)
            | ((key.note().get() as u32) << 8);
        if key.is_midi2() {
            Ump([0x4000_0000 | head, 0, 0, 0])
        } else {
            Ump([0x2000_0000 | head, 0, 0, 0])
        }
    }

    /// Sustain pedal up (CC 64 = 0) on `address`, in `protocol`'s channel
    /// voice packet; `None` for a protocol with no channel voice message (a
    /// [`Protocol::Reserved`] one).
    #[must_use]
    pub const fn sustain_up(protocol: Protocol, address: Address) -> Option<Ump> {
        let head = ((address.group.get() as u32) << 24)
            | (0xb << 20)
            | ((address.channel.get() as u32) << 16)
            | ((CCNumber::SUSTAIN.get() as u32) << 8);
        match protocol {
            Protocol::Midi2 => Some(Ump([0x4000_0000 | head, 0, 0, 0])),
            Protocol::Midi1 => Some(Ump([0x2000_0000 | head, 0, 0, 0])),
            Protocol::Reserved(_) => None,
        }
    }
}

/// Splits a packed word stream into its packets.
///
/// A native-UMP transport (CoreMIDI's event packets, a MIDI 2.0 USB transfer)
/// delivers several messages concatenated in one buffer, with no separators:
/// each one's length is implied by its message type (M2-104 section 2.1.3,
/// Table 4), the one table [`Ump::words`] reads. A run too short for its
/// declared type is a truncated message and ends the stream with nothing
/// yielded for it. A reserved type has its size too, so it is skipped without
/// losing the packets after it. The packets carry no timing; a caller pairs
/// each with one from its own transport.
pub fn split_stream(words: &[u32]) -> impl Iterator<Item = Ump> + '_ {
    let mut rest = words;
    core::iter::from_fn(move || {
        let Some(ump) = Ump::from_words(rest) else {
            rest = &[];
            return None;
        };
        rest = &rest[ump.words().len()..];
        Some(ump)
    })
}

/// The meaningful words, in hexadecimal and separated by a space, as the
/// specification writes a packet: `40903C00 80000000`.
impl core::fmt::Display for Ump {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::UpperHex::fmt(self, f)
    }
}

/// [`Display`](core::fmt::Display) in lower case; the `#` flag puts `0x` on
/// each word.
impl core::fmt::LowerHex for Ump {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, word) in self.words().iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            if f.alternate() {
                write!(f, "{word:#010x}")?;
            } else {
                write!(f, "{word:08x}")?;
            }
        }
        Ok(())
    }
}

/// [`Display`](core::fmt::Display) itself; the `#` flag puts `0x` on each
/// word.
impl core::fmt::UpperHex for Ump {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, word) in self.words().iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            if f.alternate() {
                write!(f, "{word:#010X}")?;
            } else {
                write!(f, "{word:08X}")?;
            }
        }
        Ok(())
    }
}

/// A held-note key: protocol, group, channel and note (M2-104 identifies a
/// note by the last three), packed in 16 bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NoteKey(u16);

impl NoteKey {
    /// The key of `note` on `address` in `protocol`, or `None` for a protocol
    /// that has no channel voice message ([`Protocol::Reserved`]).
    #[must_use]
    pub const fn new(protocol: Protocol, address: Address, note: NoteNumber) -> Option<Self> {
        match protocol {
            Protocol::Midi1 => Some(Self::pack(false, address, note)),
            Protocol::Midi2 => Some(Self::pack(true, address, note)),
            Protocol::Reserved(_) => None,
        }
    }

    const fn pack(m2: bool, address: Address, note: NoteNumber) -> Self {
        NoteKey(((m2 as u16) << 15) | ((address.index() as u16) << 7) | note.get() as u16)
    }

    /// Packed form, `0..=65535`.
    #[must_use]
    pub const fn index(self) -> u16 {
        self.0
    }

    /// From the packed form.
    #[must_use]
    pub const fn from_index(i: u16) -> Self {
        NoteKey(i)
    }

    /// The protocol of the packet that opened the key: [`Protocol::Midi1`] or
    /// [`Protocol::Midi2`].
    #[must_use]
    pub const fn protocol(self) -> Protocol {
        if self.is_midi2() {
            Protocol::Midi2
        } else {
            Protocol::Midi1
        }
    }

    const fn is_midi2(self) -> bool {
        self.0 >> 15 != 0
    }

    /// The channel in its protocol.
    #[must_use]
    pub const fn lane(self) -> GroupChannelIndex {
        GroupChannelIndex::from_packed(self.0 >> 7)
    }

    /// The Group and Channel.
    #[must_use]
    pub const fn address(self) -> Address {
        Address::from_lane(((self.0 >> 7) & 0xff) as u8)
    }

    /// The note number.
    #[must_use]
    pub const fn note(self) -> NoteNumber {
        NoteNumber::masked((self.0 & 0x7f) as u8)
    }
}

/// What [`Ump::note_key_event`] reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteKeyEvent {
    /// A note starts.
    On(NoteKey),
    /// A note ends.
    Off(NoteKey),
    /// The sustain pedal of a channel changes.
    Sustain {
        /// The protocol of the packet: [`Protocol::Midi1`] or [`Protocol::Midi2`].
        protocol: Protocol,
        /// The Group and Channel.
        address: Address,
        /// Pedal down (value at least half).
        down: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UMP Stream is message type `0xF` and nothing else (M2-104 Table 4).
    #[test]
    fn only_type_f_is_a_stream_message() {
        for t in 0..=0xFu32 {
            assert_eq!(Ump([t << 28, 0, 0, 0]).is_stream(), t == 0xF, "type {t:#x}");
        }
    }

    /// Only the JR Clock (status 1) and JR Timestamp (status 2) of type 0 are
    /// JR (M2-104 §7.2.2): not a NOOP, a Ticks Per Quarter Note or a Delta
    /// Clockstamp, nor the same status under another type.
    #[test]
    fn only_the_jr_utility_messages_are_jr() {
        for status in 0..=0xFu32 {
            assert_eq!(
                Ump([status << 20, 0, 0, 0]).is_jr(),
                status == 1 || status == 2,
                "status {status:#x}"
            );
            assert!(!Ump([(1 << 28) | (status << 20), 0, 0, 0]).is_jr());
        }
    }

    /// System Real Time is type 1 with a status byte of `0xF8` to `0xFF`
    /// (M2-104 §7.6): `0xF7` (End of Exclusive) is a System Common status, and
    /// the same byte under another type is not Real Time.
    #[test]
    fn system_real_time_is_type_1_status_f8_and_above() {
        let system = |status: u32| Ump([0x1000_0000 | (status << 16), 0, 0, 0]);
        assert!(!system(0xF7).is_system_real_time());
        assert!(system(0xF8).is_system_real_time());
        assert!(system(0xFF).is_system_real_time());
        assert!(!system(0xF1).is_system_real_time());
        assert!(!Ump([0x2000_0000 | (0xF8 << 16), 0, 0, 0]).is_system_real_time());
    }

    /// The header readers against packets written out from M2-104's
    /// layouts: Group in bits 27-24, Channel in 19-16 of a channel voice
    /// message.
    #[test]
    fn the_header_readers_follow_the_message_type() {
        // MIDI 2.0 Note On, group 5, channel 9; MIDI 1.0 CC, group 2,
        // channel 14.
        let m2 = Ump([0x4599_3C00, 0x8000_0000, 0, 0]);
        let m1 = Ump([0x22BE_0740, 0, 0, 0]);
        assert_eq!(m2.message_type(), MessageType::Midi2ChannelVoice);
        assert_eq!(
            m2.address(),
            Some(Address {
                group: MidiGroup::of(5),
                channel: MidiChannel::of(9),
            })
        );
        assert_eq!(m1.channel(), Some(MidiChannel::of(14)));
        assert_eq!(m1.group(), Some(MidiGroup::of(2)));
        // A System message has a Group and no Channel.
        let clock = Ump([0x13F8_0000, 0, 0, 0]);
        assert_eq!(clock.group(), Some(MidiGroup::of(3)));
        assert_eq!(clock.address(), None);
        // Utility and UMP Stream are groupless; a reserved type has neither.
        for groupless in [Ump([0x0000_0000, 0, 0, 0]), Ump([0xF000_0000, 0, 0, 0])] {
            assert_eq!(groupless.group(), None);
            assert_eq!(groupless.channel(), None);
        }
        assert_eq!(Ump([0xE300_0000, 0, 0, 0]).group(), None);
        // Every type that has a Group reads it from bits 27-24.
        for mt in [0x1u32, 0x2, 0x3, 0x4, 0x5, 0xD] {
            let u = Ump([(mt << 28) | (0xA << 24), 0, 0, 0]);
            assert_eq!(u.group(), Some(MidiGroup::of(10)), "type {mt:#x}");
        }
    }

    #[test]
    fn the_word_count_follows_the_message_type() {
        let want = [1, 1, 1, 2, 2, 4, 1, 1, 2, 2, 2, 3, 3, 4, 4, 4];
        for (mt, n) in want.into_iter().enumerate() {
            let u = Ump([((mt as u32) << 28) | 0x0123_4567, 1, 2, 3]);
            assert_eq!(u.words().len(), n, "message type {mt:#x}");
            assert_eq!(u.words(), &u.0[..n]);
        }
    }

    #[test]
    fn note_offs_are_recognised() {
        let m1 =
            |status: u32, vel: u32| Ump([0x2000_0000 | (status << 16) | (60 << 8) | vel, 0, 0, 0]);
        assert!(m1(0x80, 64).is_note_off());
        assert!(m1(0x90, 0).is_note_off());
        assert!(!m1(0x90, 64).is_note_off());
        assert!(!m1(0xa0, 0).is_note_off());
        assert!(Ump([0x4080_3c00, 0x8000_0000, 0, 0]).is_note_off());
        assert!(!Ump([0x4090_3c00, 0, 0, 0]).is_note_off());
        assert!(!Ump([0x4090_3c00, 0x8000_0000, 0, 0]).is_note_off());
        assert!(!Ump([0x1080_0000, 0, 0, 0]).is_note_off());
        assert!(!Ump([0; 4]).is_note_off());
    }

    /// Controllers 120 and 123 of a Control Change, in either protocol, on any
    /// channel and group, are panics; no other controller, no other message
    /// type, and no other status with the same data byte is.
    #[test]
    fn panic_controllers_are_recognised() {
        let m1 = |status: u32, cc: u32| Ump([0x2000_0000 | (status << 16) | (cc << 8), 0, 0, 0]);
        assert!(m1(0xb0, 120).is_panic_controller());
        assert!(m1(0xb0, 123).is_panic_controller());
        assert!(m1(0xbf, 123).is_panic_controller());
        assert!(Ump([0x2000_0000 | (0xb3 << 16) | (120 << 8) | 5, 0, 0, 0]).is_panic_controller());
        assert!(Ump([0x40b5_7800, 0, 0, 0]).is_panic_controller());
        assert!(Ump([0x40bf_7b00, 0, 0, 0]).is_panic_controller());
        for cc in [0u32, 64, 119, 121, 122, 124, 127] {
            assert!(!m1(0xb0, cc).is_panic_controller(), "CC {cc}");
        }
        assert!(!m1(0x90, 120).is_panic_controller(), "a note-on, key 120");
        assert!(!m1(0x80, 123).is_panic_controller(), "a note-off, key 123");
        assert!(!Ump([0x4090_7800, 0, 0, 0]).is_panic_controller());
        assert!(!Ump([0x30b0_7800, 0, 0, 0]).is_panic_controller(), "SysEx7");
        assert!(!Ump([0x10b0_7800, 0, 0, 0]).is_panic_controller(), "System");
    }

    #[test]
    fn it_is_four_words_and_castable() {
        assert_eq!(core::mem::size_of::<Ump>(), 16);
        let u = Ump([1, 2, 3, 4]);
        assert_eq!(bytemuck::bytes_of(&u).len(), 16);
        assert_eq!(bytemuck::cast::<Ump, [u32; 4]>(u), [1, 2, 3, 4]);
    }
}
