//! The message catalogue: one value per defined M2-104 message, `Copy`, with
//! no lifetime.

use core::num::NonZeroU8;
use core::str::FromStr;

use crate::field::{
    ChordType, DeltaTicks, FirstGroup, FunctionBlockCount, FunctionBlockId, GroupSpan, NoteNumber,
    Pitch, SharpsFlats, TicksPerQuarterNote, U4, U7, U14,
};
use crate::{
    ArticulationAttribute, BendValue, CCNumber, ControlValue, Field, ManufacturerId, MdsChunk,
    MdsChunkSize, MdsChunks, MdsIdentity, MidiChannel, MidiGroup, NoteVelocity, Reserved,
    ReservedAccents, ReservedAttribute, Rpn, Sysex8StreamCount, Sysex8StreamId, Ump,
};

/// One UMP, decoded.
///
/// `Midi1` and `Midi2` are different message types on the wire and stay
/// different here. Utility and Stream messages have no group (M2-104 §2.1.2),
/// so they carry none.
///
/// Exhaustive on purpose, as [`Voice1`] and [`Voice2`] are: the wire format
/// closes the set of message types, and [`Unknown`] carries a packet no
/// revision defined (or this crate cannot frame), so a later revision is not a
/// new variant. A consumer that matches on it, and on the voice messages, is
/// made to decide what a new message means when one is added here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Message {
    /// Message type `0x0`.
    Utility(Utility),
    /// Message type `0x1`.
    System(MidiGroup, System),
    /// Message type `0x2`.
    Midi1(Address, Voice1),
    /// Message type `0x3`: one System Exclusive packet.
    Sysex7(MidiGroup, Sysex7Packet),
    /// Message type `0x4`.
    Midi2(Address, Voice2),
    /// Message type `0x5`: System Exclusive 8 and Mixed Data Set.
    Data128(MidiGroup, Data128),
    /// Message type `0xD`.
    Flex(MidiGroup, Flex),
    /// Message type `0xF`.
    Stream(Stream),
}

/// A packet no definition covers: a reserved message type, a defined type
/// with an undefined status, or a defined message with a reserved value in a
/// field. The words are kept, so a later revision's message can be forwarded
/// unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("no defined message in the packet {}", .0)]
pub struct Unknown(pub Ump);

/// A Group and a Channel within it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Address {
    /// The Group.
    pub group: MidiGroup,
    /// The Channel in that Group.
    pub channel: MidiChannel,
}

// ---- Utility ---------------------------------------------------------------

/// Utility messages (M2-104 §7.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Utility {
    /// No operation.
    Noop,
    /// The sender's clock, in ticks of 1/31250 s (wraps).
    JrClock(u16),
    /// The time of the following messages, in ticks of 1/31250 s.
    JrTimestamp(u16),
    /// Delta Clockstamp ticks per quarter note.
    TicksPerQuarterNote(TicksPerQuarterNote),
    /// A Ticks Per Quarter Note of the reserved `0`, as read from the wire.
    ReservedTicksPerQuarterNote(Reserved),
    /// Ticks since the last event.
    DeltaClockstamp(DeltaTicks),
}

impl Address {
    /// `channel` in the first Group, the Group a single-group sender uses.
    #[inline]
    #[must_use]
    pub const fn first(channel: MidiChannel) -> Self {
        Address {
            group: MidiGroup::FIRST,
            channel,
        }
    }

    /// `channel` in `group`.
    #[inline]
    #[must_use]
    pub const fn new(group: MidiGroup, channel: MidiChannel) -> Self {
        Address { group, channel }
    }

    /// The Group and Channel as one table index, `0..256`: the group's
    /// [`index`](MidiGroup::index) times sixteen, plus the channel's.
    #[inline]
    #[must_use]
    pub const fn index(self) -> usize {
        self.group.index() * MidiChannel::COUNT as usize + self.channel.index()
    }

    /// The address at table position `index` ([`index`](Self::index)'s
    /// inverse), or `None` from 256 up.
    #[inline]
    #[must_use]
    pub const fn from_index(index: usize) -> Option<Self> {
        if index < 256 {
            Some(Self::from_lane(index as u8))
        } else {
            None
        }
    }

    /// The address of the table position `lane`: the Group in its high four
    /// bits, the Channel in its low four.
    #[inline]
    pub(crate) const fn from_lane(lane: u8) -> Self {
        Address {
            group: MidiGroup::masked(lane >> 4),
            channel: MidiChannel::masked(lane),
        }
    }
}

/// A channel in the first Group, as [`Address::first`].
impl From<MidiChannel> for Address {
    #[inline]
    fn from(channel: MidiChannel) -> Self {
        Address::first(channel)
    }
}

// ---- Constructors ----------------------------------------------------------

impl Message {
    /// A MIDI 1.0 channel voice message (message type `0x2`) at `address`;
    /// a bare [`MidiChannel`] addresses the first Group.
    ///
    /// ```
    /// use tutti_ump::{Message, MidiChannel, NoteNumber, U7, Voice1};
    ///
    /// let note = Message::midi1(MidiChannel::FIRST, Voice1::note_on(NoteNumber::of(60), U7::of(100)));
    /// # let _ = note;
    /// ```
    #[inline]
    #[must_use]
    pub fn midi1(address: impl Into<Address>, voice: Voice1) -> Self {
        Self::Midi1(address.into(), voice)
    }

    /// A MIDI 2.0 channel voice message (message type `0x4`) at `address`;
    /// a bare [`MidiChannel`] addresses the first Group. A 7-bit value reaches
    /// a MIDI 2.0 field through M2-115's scaling, not by a shift.
    ///
    /// ```
    /// use tutti_ump::{
    ///     Address, BendValue, MidiChannel, MidiGroup, Message, NoteNumber, NoteVelocity, Voice2,
    /// };
    ///
    /// let channel = MidiChannel::of(3);
    /// let note = Message::midi2(
    ///     channel,
    ///     Voice2::note_on(NoteNumber::of(60), NoteVelocity::new(0xC000)),
    /// );
    /// let bend = Message::midi2(
    ///     Address::new(MidiGroup::of(2), channel),
    ///     Voice2::PitchBend(BendValue::CENTER),
    /// );
    /// # let _ = (note, bend);
    /// ```
    #[inline]
    #[must_use]
    pub fn midi2(address: impl Into<Address>, voice: Voice2) -> Self {
        Self::Midi2(address.into(), voice)
    }
}

// ---- System ----------------------------------------------------------------

/// System Common and System Real Time messages (M2-104 §7.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum System {
    /// MIDI Time Code quarter frame: the byte `0nnndddd`.
    TimeCode(U7),
    /// Song Position Pointer, in MIDI beats.
    SongPosition(U14),
    /// Song Select.
    SongSelect(U7),
    /// Tune Request.
    TuneRequest,
    /// Timing Clock.
    TimingClock,
    /// Start.
    Start,
    /// Continue.
    Continue,
    /// Stop.
    Stop,
    /// Active Sensing.
    ActiveSensing,
    /// Reset.
    Reset,
}

impl System {
    /// How many Timing Clock messages make a quarter note (MIDI 1.0
    /// Detailed Specification, System Real Time Messages).
    pub const TIMING_CLOCKS_PER_QUARTER: u32 = 24;
}

// ---- MIDI 1.0 channel voice ------------------------------------------------

/// MIDI 1.0 Channel Voice messages (M2-104 §7.3). A Note On at velocity 0 is
/// a Note On here; reading it as a Note Off is translation's.
///
/// Exhaustive on purpose: MIDI 1.0's channel voice set is closed, and a match
/// that ignores one is a match that should not compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Voice1 {
    /// Note Off.
    NoteOff {
        /// The note.
        note: NoteNumber,
        /// The release velocity.
        velocity: U7,
    },
    /// Note On.
    NoteOn {
        /// The note.
        note: NoteNumber,
        /// The velocity.
        velocity: U7,
    },
    /// Polyphonic key pressure.
    PolyPressure {
        /// The note.
        note: NoteNumber,
        /// The pressure.
        pressure: U7,
    },
    /// Control Change.
    ControlChange {
        /// The controller.
        controller: CCNumber,
        /// Its value.
        value: U7,
    },
    /// Program Change.
    ProgramChange(U7),
    /// Channel pressure.
    ChannelPressure(U7),
    /// Pitch Bend, `0x2000` centred.
    PitchBend(U14),
}

impl Voice1 {
    /// A Note On. At velocity 0 it is still a Note On here (a receiver reads
    /// it as a Note Off, M2-104 section 7.3.2).
    #[inline]
    #[must_use]
    pub const fn note_on(note: NoteNumber, velocity: U7) -> Self {
        Self::NoteOn { note, velocity }
    }

    /// A Note Off with release `velocity`.
    #[inline]
    #[must_use]
    pub const fn note_off(note: NoteNumber, velocity: U7) -> Self {
        Self::NoteOff { note, velocity }
    }

    /// Polyphonic key pressure.
    #[inline]
    #[must_use]
    pub const fn poly_pressure(note: NoteNumber, pressure: U7) -> Self {
        Self::PolyPressure { note, pressure }
    }

    /// A Control Change.
    #[inline]
    #[must_use]
    pub const fn control_change(controller: CCNumber, value: U7) -> Self {
        Self::ControlChange { controller, value }
    }
}

// ---- System Exclusive ------------------------------------------------------

/// The role of a packet in a System Exclusive message (Tables 18 and 19).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SysexStatus {
    /// The whole message.
    Complete,
    /// The first of several.
    Start,
    /// A middle one.
    Continue,
    /// The last of several.
    End,
}

/// One System Exclusive (7-bit) packet: up to 6 data bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(
        try_from = "(SysexStatus, u8, [U7; 6])",
        into = "(SysexStatus, u8, [U7; 6])"
    )
)]
pub struct Sysex7Packet {
    status: SysexStatus,
    len: u8,
    bytes: [U7; 6],
}

// The serde form of a packet is its fields as stored; a length above the
// packet's capacity is refused on the way in.
#[cfg(feature = "serde")]
impl TryFrom<(SysexStatus, u8, [U7; 6])> for Sysex7Packet {
    type Error = &'static str;

    fn try_from((status, len, bytes): (SysexStatus, u8, [U7; 6])) -> Result<Self, Self::Error> {
        if len > 6 {
            return Err("a SysEx7 packet holds at most six bytes");
        }
        if bytes[usize::from(len)..].iter().any(|b| b.get() != 0) {
            return Err("a SysEx7 packet's bytes past its length are zero");
        }
        Ok(Self { status, len, bytes })
    }
}

#[cfg(feature = "serde")]
impl From<Sysex7Packet> for (SysexStatus, u8, [U7; 6]) {
    fn from(packet: Sysex7Packet) -> Self {
        (packet.status, packet.len, packet.bytes)
    }
}

#[cfg(feature = "serde")]
impl TryFrom<(SysexStatus, Sysex8StreamId, u8, [u8; 13])> for Sysex8Packet {
    type Error = &'static str;

    fn try_from(
        (status, stream_id, len, bytes): (SysexStatus, Sysex8StreamId, u8, [u8; 13]),
    ) -> Result<Self, Self::Error> {
        if len > 13 {
            return Err("a SysEx8 packet holds at most 13 bytes");
        }
        if bytes[usize::from(len)..].iter().any(|&b| b != 0) {
            return Err("a SysEx8 packet's bytes past its length are zero");
        }
        Ok(Self {
            status,
            stream_id,
            len,
            bytes,
        })
    }
}

#[cfg(feature = "serde")]
impl From<Sysex8Packet> for (SysexStatus, Sysex8StreamId, u8, [u8; 13]) {
    fn from(packet: Sysex8Packet) -> Self {
        (packet.status, packet.stream_id, packet.len, packet.bytes)
    }
}

impl SysexStatus {
    /// The status of the next packet of a run: a first packet is a `Complete`
    /// when nothing follows it and a `Start` otherwise, a later one an `End`
    /// or a `Continue`.
    pub(crate) const fn of_run(started: bool, more: bool) -> Self {
        match (started, more) {
            (false, false) => Self::Complete,
            (false, true) => Self::Start,
            (true, true) => Self::Continue,
            (true, false) => Self::End,
        }
    }
}

impl Sysex7Packet {
    /// The packet of `status` and up to six bytes of `data` (the rest of a
    /// longer slice is not read).
    pub(crate) fn of_chunk(status: SysexStatus, data: &[U7]) -> Self {
        let chunk = &data[..data.len().min(6)];
        let mut bytes = [U7::default(); 6];
        bytes[..chunk.len()].copy_from_slice(chunk);
        Self {
            status,
            len: chunk.len() as u8,
            bytes,
        }
    }

    /// The packet, or `None` for more than 6 bytes or a byte of 128 or more.
    #[must_use]
    pub fn new(status: SysexStatus, data: &[u8]) -> Option<Self> {
        if data.len() > 6 {
            return None;
        }
        let mut bytes = [U7::default(); 6];
        for (slot, &b) in bytes.iter_mut().zip(data) {
            *slot = U7::new(b)?;
        }
        Some(Self {
            status,
            len: data.len() as u8,
            bytes,
        })
    }

    /// The packet of exactly these bytes. `N` is at most 6, checked when the
    /// call is compiled, and each byte is a `U7`, so nothing can fail.
    #[must_use]
    pub fn from_bytes<const N: usize>(status: SysexStatus, data: [U7; N]) -> Self {
        const { assert!(N <= 6, "a SysEx7 packet holds at most six bytes") };
        let mut bytes = [U7::default(); 6];
        bytes[..N].copy_from_slice(&data);
        Self {
            status,
            len: N as u8,
            bytes,
        }
    }

    /// The packet's role.
    #[must_use]
    pub const fn status(&self) -> SysexStatus {
        self.status
    }

    /// The valid data bytes.
    #[must_use]
    pub fn bytes(&self) -> &[U7] {
        self.bytes.split_at(self.len as usize).0
    }

    /// The valid data bytes as plain bytes: the same bytes as
    /// [`bytes`](Self::bytes), viewed in place without a copy. Each is below
    /// 128.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        bytemuck::cast_slice(self.bytes())
    }
}

/// One System Exclusive 8 packet: a stream id and up to 13 data bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(
        try_from = "(SysexStatus, Sysex8StreamId, u8, [u8; 13])",
        into = "(SysexStatus, Sysex8StreamId, u8, [u8; 13])"
    )
)]
pub struct Sysex8Packet {
    status: SysexStatus,
    stream_id: Sysex8StreamId,
    len: u8,
    bytes: [u8; 13],
}

impl Sysex8Packet {
    /// The packet of the first 13 (or fewer) of `payload`, and the bytes after
    /// them.
    pub(crate) fn take(started: bool, stream_id: Sysex8StreamId, payload: &[u8]) -> (Self, &[u8]) {
        let (chunk, rest) = payload.split_at(payload.len().min(13));
        let mut bytes = [0; 13];
        bytes[..chunk.len()].copy_from_slice(chunk);
        let packet = Self {
            status: SysexStatus::of_run(started, !rest.is_empty()),
            stream_id,
            len: chunk.len() as u8,
            bytes,
        };
        (packet, rest)
    }

    /// The packet, or `None` for more than 13 data bytes. Only a
    /// [`Sysex8Stream`](crate::Sysex8Stream) writes packets; the others are
    /// decoded.
    #[must_use]
    pub(crate) fn new(status: SysexStatus, stream_id: Sysex8StreamId, data: &[u8]) -> Option<Self> {
        if data.len() > 13 {
            return None;
        }
        let mut bytes = [0; 13];
        bytes[..data.len()].copy_from_slice(data);
        Some(Self {
            status,
            stream_id,
            len: data.len() as u8,
            bytes,
        })
    }

    /// The packet's role.
    #[must_use]
    pub const fn status(&self) -> SysexStatus {
        self.status
    }

    /// The stream id.
    #[must_use]
    pub const fn stream_id(&self) -> Sysex8StreamId {
        self.stream_id
    }

    /// The valid data bytes, after the stream id.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.bytes.split_at(self.len as usize).0
    }
}

/// Message type `0x5` (M2-104 §7.8 and §7.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Data128 {
    /// A System Exclusive 8 packet.
    Sysex8(Sysex8Packet),
    /// What a decoder returns for an End packet with `# of bytes` `0xF`, and the
    /// one SysEx 8 variant a caller builds with any Stream ID. A sender aborts
    /// with [`Sysex8Packets::abort`](crate::Sysex8Packets::abort) or
    /// [`Sysex8StreamIds::take_abort`](crate::Sysex8StreamIds::take_abort). The
    /// sender ran out of data, and the earlier data of this stream is
    /// incomplete (§7.8.1).
    /// The stream id is read from byte 3; §7.8.1 does not say it is kept, so a
    /// sender that zeroes it aborts stream 0.
    Sysex8Abort {
        /// The stream being aborted.
        stream_id: Sysex8StreamId,
    },
    /// A Mixed Data Set chunk header.
    MixedDataHeader(MixedDataHeader),
    /// A Mixed Data Set payload packet: 14 bytes, zero-padded.
    MixedDataPayload {
        /// The set this belongs to.
        mds_id: U4,
        /// The payload.
        bytes: [u8; 14],
    },
}

/// A Mixed Data Set chunk header (§7.9): the first UMP of a chunk, which says
/// how many payload UMPs follow it ([`MdsChunkSize::payload_umps`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MixedDataHeader {
    /// The set this chunk belongs to.
    pub mds_id: U4,
    /// The size of this chunk in bytes, header included.
    pub chunk_size: MdsChunkSize,
    /// The chunks in the set; [`MdsChunks::Unknown`] for a stream.
    pub chunks: MdsChunks,
    /// This chunk's number, from 1; [`MdsChunk::Unverified`] ends the set
    /// with its data unverified.
    pub chunk: MdsChunk,
    /// Whose data this is: the manufacturer, device and sub ids.
    pub identity: MdsIdentity,
}

// ---- MIDI 2.0 channel voice ------------------------------------------------

/// A Note On or Note Off attribute (§7.4.14). An attribute type of `0x00`
/// carries no data. Types `0x10` to `0x1F` are the articulation attributes of
/// the M2-123 Profile; the rest of `0x04` to `0xFF` are reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Attribute {
    /// Type `0x00`.
    None,
    /// Type `0x01`: manufacturer specific.
    Manufacturer(u16),
    /// Type `0x02`: specific to the active MIDI-CI Profile.
    Profile(u16),
    /// Type `0x03`: a pitch, on the wire Q7.9 in half-tone units. Encoding
    /// keeps the top 16 bits of the [`Pitch`] ([`Pitch::q7_9`]); a finer
    /// fraction is dropped.
    Pitch(Pitch),
    /// Types `0x10` to `0x1F`: an articulation of the M2-123 Profile, which
    /// [`NoteArticulation`](crate::NoteArticulation) and
    /// [`NoteOffArticulation`](crate::NoteOffArticulation) write.
    Articulation(ArticulationAttribute),
    /// A type neither M2-104 nor a Profile this crate types defines, with its
    /// data: read from the wire, and written by no sender.
    Reserved(ReservedAttribute),
}

/// A Registered Per-Note Controller number (M2-104 Appendix A, Table 22, and
/// M2-123 §8.2's Playing Position). A number the table leaves reserved is read
/// as [`Reserved`](Self::Reserved), which only the decoder builds, so its
/// message re-encodes exactly and no sender names one. [`TryFrom<u8>`] names a
/// number the table defines and refuses a reserved one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum RegisteredPerNoteController {
    /// 1: Modulation.
    Modulation,
    /// 2: Breath.
    Breath,
    /// 3: Pitch 7.25 (§7.4.15.2); the value is a [`Pitch`] in Q7.25.
    Pitch,
    /// 7: Volume.
    Volume,
    /// 8: Balance.
    Balance,
    /// 10: Pan.
    Pan,
    /// 11: Expression.
    Expression,
    /// 12: Playing Position (M2-123 §8.2); the value is a
    /// [`PlayingPosition`](crate::PlayingPosition).
    PlayingPosition,
    /// 70: Sound Controller 1, Sound Variation.
    SoundVariation,
    /// 71: Sound Controller 2, Timbre / Harmonic Intensity.
    Timbre,
    /// 72: Sound Controller 3, Release Time.
    ReleaseTime,
    /// 73: Sound Controller 4, Attack Time.
    AttackTime,
    /// 74: Sound Controller 5, Brightness.
    Brightness,
    /// 75: Sound Controller 6, Decay Time.
    DecayTime,
    /// 76: Sound Controller 7, Vibrato Rate.
    VibratoRate,
    /// 77: Sound Controller 8, Vibrato Depth.
    VibratoDepth,
    /// 78: Sound Controller 9, Vibrato Delay.
    VibratoDelay,
    /// 79: Sound Controller 10, undefined.
    SoundController10,
    /// 91: Effects 1 Depth, Reverb Send Level.
    ReverbSend,
    /// 92: Effects 2 Depth (formerly Tremolo Depth).
    Effects2Depth,
    /// 93: Effects 3 Depth, Chorus Send Level.
    ChorusSend,
    /// 94: Effects 4 Depth (formerly Celeste Depth).
    Effects4Depth,
    /// 95: Effects 5 Depth (formerly Phaser Depth).
    Effects5Depth,
    /// A number the table reserves.
    Reserved(Reserved),
}

impl RegisteredPerNoteController {
    /// The controller number on the wire.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Self::Modulation => 1,
            Self::Breath => 2,
            Self::Pitch => 3,
            Self::Volume => 7,
            Self::Balance => 8,
            Self::Pan => 10,
            Self::Expression => 11,
            Self::PlayingPosition => 12,
            Self::SoundVariation => 70,
            Self::Timbre => 71,
            Self::ReleaseTime => 72,
            Self::AttackTime => 73,
            Self::Brightness => 74,
            Self::DecayTime => 75,
            Self::VibratoRate => 76,
            Self::VibratoDepth => 77,
            Self::VibratoDelay => 78,
            Self::SoundController10 => 79,
            Self::ReverbSend => 91,
            Self::Effects2Depth => 92,
            Self::ChorusSend => 93,
            Self::Effects4Depth => 94,
            Self::Effects5Depth => 95,
            Self::Reserved(r) => r.get(),
        }
    }
}

impl RegisteredPerNoteController {
    /// The controller of a wire number: a number the table defines is named,
    /// any other is [`Reserved`](Self::Reserved).
    pub(crate) const fn wire(number: u8) -> Self {
        match number {
            1 => Self::Modulation,
            2 => Self::Breath,
            3 => Self::Pitch,
            7 => Self::Volume,
            8 => Self::Balance,
            10 => Self::Pan,
            11 => Self::Expression,
            12 => Self::PlayingPosition,
            70 => Self::SoundVariation,
            71 => Self::Timbre,
            72 => Self::ReleaseTime,
            73 => Self::AttackTime,
            74 => Self::Brightness,
            75 => Self::DecayTime,
            76 => Self::VibratoRate,
            77 => Self::VibratoDepth,
            78 => Self::VibratoDelay,
            79 => Self::SoundController10,
            91 => Self::ReverbSend,
            92 => Self::Effects2Depth,
            93 => Self::ChorusSend,
            94 => Self::Effects4Depth,
            95 => Self::Effects5Depth,
            n => Self::Reserved(Reserved::wire(n)),
        }
    }
}

impl TryFrom<u8> for RegisteredPerNoteController {
    type Error = Reserved;

    /// The controller Appendix A defines at `number`, or the reserved number
    /// itself.
    fn try_from(number: u8) -> Result<Self, Reserved> {
        match Self::wire(number) {
            Self::Reserved(r) => Err(r),
            defined => Ok(defined),
        }
    }
}

impl From<RegisteredPerNoteController> for u8 {
    fn from(controller: RegisteredPerNoteController) -> u8 {
        controller.number()
    }
}

impl Attribute {
    /// The pitch of an [`Attribute::Pitch`] (§7.4.15.3), or `None` for any other
    /// attribute.
    #[must_use]
    pub const fn pitch(self) -> Option<Pitch> {
        match self {
            Self::Pitch(pitch) => Some(pitch),
            _ => None,
        }
    }
}

/// A Program Change's bank (§7.4.9), present when the Bank Valid bit is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Bank {
    /// Bank MSB.
    pub msb: U7,
    /// Bank LSB.
    pub lsb: U7,
}

/// What a Per-Note Management message does (M2-104 §7.4.5, the D and S option
/// flags). A message that does neither has no defined function, so it is not a
/// value of this type, and the decoder refuses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PerNoteManagement {
    /// D: the controllers are detached from the notes that sound, which keep
    /// their values until they end.
    Detach,
    /// S: the controllers are set back to their defaults.
    Reset,
    /// D and S: detach first, then reset, so the sounding notes keep their
    /// values and the defaults apply to future notes only.
    DetachAndReset,
}

impl PerNoteManagement {
    /// The management a message with these flags asks for, or `None` for D=0
    /// and S=0, which has no defined function.
    #[must_use]
    pub const fn from_flags(detach: bool, reset: bool) -> Option<Self> {
        match (detach, reset) {
            (true, false) => Some(Self::Detach),
            (false, true) => Some(Self::Reset),
            (true, true) => Some(Self::DetachAndReset),
            (false, false) => None,
        }
    }

    /// Whether it detaches (the D flag).
    #[must_use]
    pub const fn detaches(self) -> bool {
        matches!(self, Self::Detach | Self::DetachAndReset)
    }

    /// Whether it resets (the S flag).
    #[must_use]
    pub const fn resets(self) -> bool {
        matches!(self, Self::Reset | Self::DetachAndReset)
    }

    /// The Per-Note Management message for `note`.
    #[must_use]
    pub const fn voice(self, note: NoteNumber) -> Voice2 {
        Voice2::PerNoteManagement {
            note,
            management: self,
        }
    }
}

/// MIDI 2.0 Channel Voice messages (M2-104 §7.4). Values are the wire's
/// integers; M2-115 scaling is translation's.
///
/// Exhaustive on purpose: a message added to the set should break the match of
/// an instrument that would otherwise ignore it without anyone choosing to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Voice2 {
    /// Registered Per-Note Controller.
    RegisteredPerNote {
        /// The note.
        note: NoteNumber,
        /// The controller.
        controller: RegisteredPerNoteController,
        /// Its value.
        value: ControlValue,
    },
    /// Assignable Per-Note Controller.
    AssignablePerNote {
        /// The note.
        note: NoteNumber,
        /// The controller, `0..=255`. Not a [`CCNumber`]: a Control Change
        /// index is 7 bits, and this field is a full byte with 256
        /// controllers (§7.4.4), so a `CCNumber` could not hold one above 127.
        controller: u8,
        /// Its value.
        value: ControlValue,
    },
    /// Registered Controller (RPN).
    Registered {
        /// Bank (the RPN MSB).
        bank: U7,
        /// Index (the RPN LSB).
        index: U7,
        /// Its value.
        value: ControlValue,
    },
    /// Assignable Controller (NRPN).
    Assignable {
        /// Bank (the NRPN MSB).
        bank: U7,
        /// Index (the NRPN LSB).
        index: U7,
        /// Its value.
        value: ControlValue,
    },
    /// Relative Registered Controller.
    RelativeRegistered {
        /// Bank.
        bank: U7,
        /// Index.
        index: U7,
        /// The two's-complement change.
        delta: i32,
    },
    /// Relative Assignable Controller.
    RelativeAssignable {
        /// Bank.
        bank: U7,
        /// Index.
        index: U7,
        /// The two's-complement change.
        delta: i32,
    },
    /// Per-Note Pitch Bend, [`BendValue::CENTER`] centred.
    PerNotePitchBend {
        /// The note.
        note: NoteNumber,
        /// The bend.
        value: BendValue,
    },
    /// Note Off.
    NoteOff {
        /// The note.
        note: NoteNumber,
        /// The 16-bit release velocity.
        velocity: NoteVelocity,
        /// The attribute.
        attribute: Attribute,
    },
    /// Note On. Velocity 0 is a Note On (§7.4.2).
    NoteOn {
        /// The note.
        note: NoteNumber,
        /// The 16-bit velocity.
        velocity: NoteVelocity,
        /// The attribute.
        attribute: Attribute,
    },
    /// Polyphonic key pressure.
    PolyPressure {
        /// The note.
        note: NoteNumber,
        /// The pressure.
        value: ControlValue,
    },
    /// Control Change.
    ControlChange {
        /// The controller.
        controller: CCNumber,
        /// Its value.
        value: ControlValue,
    },
    /// Program Change, with an optional bank.
    ProgramChange {
        /// The program.
        program: U7,
        /// The bank, when the Bank Valid bit is set.
        bank: Option<Bank>,
    },
    /// Channel pressure.
    ChannelPressure(ControlValue),
    /// Pitch Bend, [`BendValue::CENTER`] centred.
    PitchBend(BendValue),
    /// Per-Note Management.
    PerNoteManagement {
        /// The note.
        note: NoteNumber,
        /// What it does.
        management: PerNoteManagement,
    },
}

impl Voice2 {
    /// A Note On with no attribute.
    #[inline]
    #[must_use]
    pub const fn note_on(note: NoteNumber, velocity: NoteVelocity) -> Self {
        Self::NoteOn {
            note,
            velocity,
            attribute: Attribute::None,
        }
    }

    /// A Note Off with no attribute.
    #[inline]
    #[must_use]
    pub const fn note_off(note: NoteNumber, velocity: NoteVelocity) -> Self {
        Self::NoteOff {
            note,
            velocity,
            attribute: Attribute::None,
        }
    }

    /// A Control Change.
    #[inline]
    #[must_use]
    pub const fn control_change(controller: CCNumber, value: ControlValue) -> Self {
        Self::ControlChange { controller, value }
    }

    /// Polyphonic key pressure.
    #[inline]
    #[must_use]
    pub const fn poly_pressure(note: NoteNumber, value: ControlValue) -> Self {
        Self::PolyPressure { note, value }
    }

    /// A Per-Note Pitch Bend.
    #[inline]
    #[must_use]
    pub const fn per_note_pitch_bend(note: NoteNumber, value: BendValue) -> Self {
        Self::PerNotePitchBend { note, value }
    }

    /// A Program Change with no bank.
    #[inline]
    #[must_use]
    pub const fn program_change(program: U7) -> Self {
        Self::ProgramChange {
            program,
            bank: None,
        }
    }

    /// A Registered Controller at the address `rpn`.
    #[inline]
    #[must_use]
    pub const fn registered(rpn: Rpn, value: ControlValue) -> Self {
        Self::Registered {
            bank: rpn.bank(),
            index: rpn.index(),
            value,
        }
    }

    /// A Relative Registered Controller at the address `rpn`.
    #[inline]
    #[must_use]
    pub const fn relative_registered(rpn: Rpn, delta: i32) -> Self {
        Self::RelativeRegistered {
            bank: rpn.bank(),
            index: rpn.index(),
            delta,
        }
    }

    /// An Assignable Controller.
    #[inline]
    #[must_use]
    pub const fn assignable(bank: U7, index: U7, value: ControlValue) -> Self {
        Self::Assignable { bank, index, value }
    }

    /// A Relative Assignable Controller.
    #[inline]
    #[must_use]
    pub const fn relative_assignable(bank: U7, index: U7, delta: i32) -> Self {
        Self::RelativeAssignable { bank, index, delta }
    }

    /// The note a message addresses: the one in byte 3 of a Note On or Off,
    /// Polyphonic Pressure, Per-Note Pitch Bend, a Registered or Assignable
    /// Per-Note Controller or Per-Note Management (M2-104 section 7.4). `None`
    /// for a channel-wide message.
    #[inline]
    #[must_use]
    pub const fn note(&self) -> Option<NoteNumber> {
        match *self {
            Self::RegisteredPerNote { note, .. }
            | Self::AssignablePerNote { note, .. }
            | Self::PerNotePitchBend { note, .. }
            | Self::NoteOff { note, .. }
            | Self::NoteOn { note, .. }
            | Self::PolyPressure { note, .. }
            | Self::PerNoteManagement { note, .. } => Some(note),
            Self::Registered { .. }
            | Self::Assignable { .. }
            | Self::RelativeRegistered { .. }
            | Self::RelativeAssignable { .. }
            | Self::ControlChange { .. }
            | Self::ProgramChange { .. }
            | Self::ChannelPressure(_)
            | Self::PitchBend(_) => None,
        }
    }
}

// ---- Flex Data -------------------------------------------------------------

/// Where a Flex Data message goes (Table 10): one Channel, or the whole Group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum FlexAddress {
    /// The Channel in the packet's Group.
    Channel(MidiChannel),
    /// The Group.
    Group,
    /// A reserved address (2 or 3), as read.
    Reserved(Reserved),
}

/// A note letter: `A` is 1 to `G` is 7. The field's `0` is `None` where a
/// message uses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Letter {
    /// A.
    A,
    /// B.
    B,
    /// C.
    C,
    /// D.
    D,
    /// E.
    E,
    /// F.
    F,
    /// G.
    G,
    /// A reserved value (8 to 15), as read.
    Reserved(Reserved),
}

/// Flex Data messages (M2-104 §7.5). Set Tempo, Set Time Signature and Set
/// Metronome are Group messages and carry no address.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Flex {
    /// Set Tempo: units of 10 ns per quarter note.
    SetTempo(u32),
    /// Set Time Signature.
    SetTimeSignature(FlexTimeSignature),
    /// Set Metronome.
    SetMetronome(Metronome),
    /// Set Key Signature.
    SetKeySignature(FlexAddress, KeySignature),
    /// Set Chord Name.
    SetChordName(FlexAddress, ChordName),
    /// A text message: one packet's 12 bytes, zero-padded.
    Text {
        /// Where it goes.
        address: FlexAddress,
        /// Which text.
        kind: FlexTextKind,
        /// The packet's place in the message.
        form: Form,
        /// The bytes.
        bytes: [u8; 12],
    },
}

/// Flex Data Set Time Signature fields (§7.5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FlexTimeSignature {
    /// The numerator field.
    pub numerator: u8,
    /// The denominator as a negative power of 2; `0` is non-standard.
    pub denominator: u8,
    /// 1/32 notes in 24 MIDI clocks.
    pub thirty_seconds: u8,
}

/// The Bar Accent fields of Set Metronome (§7.5.5): how a bar divides.
///
/// The three parts are the beats of each division, and they add up to the
/// beats in the bar. An undivided bar has its beat count in Part 1 and zero in
/// Parts 2 and 3; a bar divided in two has Part 3 zero; a part is never zero
/// before one that is not. The variants hold exactly those shapes, so the
/// message cannot be written with a part after a zero part, or an undivided
/// bar with a second part. A packet of another shape is read as
/// [`Reserved`](Self::Reserved), which only the decoder builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BarAccents {
    /// The bar is not divided: Part 1 is the bar's beat count.
    Undivided(NonZeroU8),
    /// The bar is divided in two, like 3 + 2 in 5/4.
    Two(NonZeroU8, NonZeroU8),
    /// The bar is divided in three, like 3 + 3 + 3 or 4 + 4 + 1 in 9/4.
    Three(NonZeroU8, NonZeroU8, NonZeroU8),
    /// A shape §7.5.5 does not have, as read from the wire: Part 1 zero, or a
    /// part after a zero part.
    Reserved(ReservedAccents),
}

impl BarAccents {
    /// The beats in the bar these parts add up to.
    ///
    /// ```
    /// use core::num::NonZeroU8;
    /// use tutti_ump::BarAccents;
    ///
    /// let part = |n| NonZeroU8::new(n).unwrap();
    /// assert_eq!(BarAccents::Three(part(4), part(4), part(1)).beats(), 9);
    /// ```
    #[must_use]
    pub const fn beats(self) -> u16 {
        let [a, b, c] = self.parts();
        a as u16 + b as u16 + c as u16
    }

    /// Part 1, 2 and 3 as the fields carry them, a part the bar does not have
    /// being zero.
    #[must_use]
    pub const fn parts(self) -> [u8; 3] {
        match self {
            Self::Undivided(a) => [a.get(), 0, 0],
            Self::Two(a, b) => [a.get(), b.get(), 0],
            Self::Three(a, b, c) => [a.get(), b.get(), c.get()],
            Self::Reserved(r) => r.parts(),
        }
    }

    /// The accents of three fields, or `None` for a shape §7.5.5 does not
    /// have: Part 1 zero, or a part after a zero part.
    #[must_use]
    pub const fn from_parts(parts: [u8; 3]) -> Option<Self> {
        match (
            NonZeroU8::new(parts[0]),
            NonZeroU8::new(parts[1]),
            NonZeroU8::new(parts[2]),
        ) {
            (Some(a), None, None) => Some(Self::Undivided(a)),
            (Some(a), Some(b), None) => Some(Self::Two(a, b)),
            (Some(a), Some(b), Some(c)) => Some(Self::Three(a, b, c)),
            _ => None,
        }
    }

    /// The accents of the three fields as the wire carried them: a shape
    /// §7.5.5 does not have is kept as [`Reserved`](Self::Reserved).
    pub(crate) const fn wire(parts: [u8; 3]) -> Self {
        match Self::from_parts(parts) {
            Some(accents) => accents,
            None => Self::Reserved(ReservedAccents::wire(parts)),
        }
    }
}

/// Set Metronome fields (§7.5.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Metronome {
    /// MIDI clocks per primary click.
    pub clocks_per_click: u8,
    /// How the bar divides, its parts adding up to the bar's beats.
    pub accents: BarAccents,
    /// Subdivision clicks, first set.
    pub subdivision_1: u8,
    /// Subdivision clicks, second set.
    pub subdivision_2: u8,
}

/// Set Key Signature fields (§7.5.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct KeySignature {
    /// Sharps or flats; `None` for the field's `-8`, unknown or non-standard.
    pub sharps_flats: Option<SharpsFlats>,
    /// The tonic; `None` is unknown or non-standard.
    pub tonic: Option<Letter>,
}

/// An accidental on a chord note (Tables 13 and 15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Accidental {
    /// Two flats.
    DoubleFlat,
    /// One flat.
    Flat,
    /// Natural.
    Natural,
    /// One sharp.
    Sharp,
    /// Two sharps.
    DoubleSharp,
    /// A value the tables reserve, as read.
    Reserved(Reserved),
}

/// What an alteration does to a chord degree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AlterationKind {
    /// No alteration.
    None,
    /// Add the degree.
    Add,
    /// Subtract the degree.
    Subtract,
    /// Raise the degree, adding it if needed.
    Raise,
    /// Lower the degree, adding it if needed.
    Lower,
    /// A reserved type (5 to 15), as read.
    Reserved(Reserved),
}

/// A chord alteration: a kind and the degree it acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Alteration {
    /// What it does.
    pub kind: AlterationKind,
    /// The degree (1 is the root, 3 the third).
    pub degree: U4,
}

/// A chord note with its accidental.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ChordPitch {
    /// The accidental.
    pub accidental: Accidental,
    /// The note letter.
    pub letter: Letter,
}

/// A chord's bass note and the chord on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ChordBass {
    /// The bass note; `None` is the chord's own tonic (sharps/flats `-8`,
    /// note `0`).
    pub pitch: Option<ChordPitch>,
    /// The bass chord type; `0` clears the bass chord.
    pub chord_type: Field<ChordType>,
    /// Up to two alterations of the bass chord.
    pub alterations: [Alteration; 2],
}

/// Set Chord Name fields (§7.5.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ChordName {
    /// The tonic's accidental.
    pub tonic_accidental: Accidental,
    /// The tonic; `None` is unknown (no chord).
    pub tonic: Option<Letter>,
    /// The chord type.
    pub chord_type: Field<ChordType>,
    /// Up to four alterations.
    pub alterations: [Alteration; 4],
    /// The bass.
    pub bass: ChordBass,
}

/// The messages that use the Flex text format (Table 16).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum FlexTextKind {
    /// Bank 1, status 0.
    UnknownMetadata,
    /// Bank 1, status 1.
    ProjectName,
    /// Bank 1, status 2.
    CompositionName,
    /// Bank 1, status 3.
    ClipName,
    /// Bank 1, status 4.
    Copyright,
    /// Bank 1, status 5.
    Composer,
    /// Bank 1, status 6.
    Lyricist,
    /// Bank 1, status 7.
    Arranger,
    /// Bank 1, status 8.
    Publisher,
    /// Bank 1, status 9.
    PrimaryPerformer,
    /// Bank 1, status 0xA.
    AccompanyingPerformer,
    /// Bank 1, status 0xB (ISO 8601).
    RecordingDate,
    /// Bank 1, status 0xC.
    RecordingLocation,
    /// Bank 2, status 0.
    UnknownPerformance,
    /// Bank 2, status 1.
    Lyrics,
    /// Bank 2, status 2.
    LyricsLanguage,
    /// Bank 2, status 3.
    Ruby,
    /// Bank 2, status 4.
    RubyLanguage,
}

// ---- UMP Stream ------------------------------------------------------------

/// A packet's place in a multi-packet message (Stream and Flex Format).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Form {
    /// The whole message.
    Complete,
    /// The first packet.
    Start,
    /// A middle packet.
    Continue,
    /// The last packet.
    End,
}

/// A UMP version (Table 5).
///
/// Versions are ordered by major, then minor: a device that receives an
/// Endpoint Discovery compares the version it carries with its own to choose
/// how much of the message it reads (§7.1.1).
///
/// ```
/// use tutti_ump::UmpVersion;
///
/// assert!(UmpVersion::new(1, 0) < UmpVersion::V1_1);
/// assert!(UmpVersion::new(1, 255) < UmpVersion::new(2, 0));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct UmpVersion {
    /// Major. Declared first, so it orders before the minor.
    pub major: u8,
    /// Minor.
    pub minor: u8,
}

impl UmpVersion {
    /// UMP 1.1, the version of M2-104 v1.1 (Table 5).
    pub const V1_1: Self = Self::new(1, 1);

    /// The version `major.minor`.
    #[must_use]
    pub const fn new(major: u8, minor: u8) -> Self {
        Self { major, minor }
    }
}

/// The Function Block count of an Endpoint Info Notification: `0..=32`, or a
/// reserved count (`33..=127`) as read from the wire, which only the decoder
/// builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum FunctionBlocks {
    /// A count M2-104 defines.
    Count(FunctionBlockCount),
    /// A reserved count, as read.
    Reserved(Reserved),
}

impl FunctionBlocks {
    /// The count, or `None` for a reserved one.
    #[must_use]
    pub const fn count(self) -> Option<FunctionBlockCount> {
        match self {
            Self::Count(count) => Some(count),
            Self::Reserved(_) => None,
        }
    }
}

impl From<FunctionBlockCount> for FunctionBlocks {
    fn from(count: FunctionBlockCount) -> Self {
        Self::Count(count)
    }
}

/// Which notifications an Endpoint Discovery asks for (§7.1.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DiscoveryFilter {
    /// Endpoint Info.
    pub endpoint_info: bool,
    /// Device Identity.
    pub device_identity: bool,
    /// Endpoint Name.
    pub endpoint_name: bool,
    /// Product Instance Id.
    pub product_instance_id: bool,
    /// Stream Configuration.
    pub stream_configuration: bool,
}

/// An Endpoint Info Notification (§7.1.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct EndpointInfoNotification {
    /// The UMP version.
    pub version: UmpVersion,
    /// The Function Blocks never change.
    pub static_function_blocks: bool,
    /// How many Function Blocks: `0..=32` is defined, a `33..=127` read from
    /// the wire is reserved and kept.
    pub function_blocks: FunctionBlocks,
    /// Supports the MIDI 2.0 Protocol.
    pub midi2: bool,
    /// Supports the MIDI 1.0 Protocol.
    pub midi1: bool,
    /// Receives JR Timestamps.
    pub rx_jr: bool,
    /// Transmits JR Timestamps.
    pub tx_jr: bool,
}

/// A device's identity, as in a Device Inquiry reply (§7.1.3). Every byte is 7-bit.
///
/// The serde form is the plain derive over fields that each refuse what they
/// cannot hold (`ManufacturerId` and `U7` read back through their own checks).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DeviceIdentity {
    /// The manufacturer's SysEx id (on the wire, three bytes: `xx 00 00` or `00 xx yy`).
    pub manufacturer: ManufacturerId,
    /// The family id, LSB first.
    pub family: [U7; 2],
    /// The model id, LSB first.
    pub model: [U7; 2],
    /// The software revision.
    pub revision: SoftwareRevision,
}

/// A Device's Software Revision Level (§7.1.3): four bytes of 7 bits, "typically
/// the version of software or firmware but may also be the version of
/// hardware". The format is the Device's own; a product's version is
/// `major.minor.patch` in the first three bytes and a build number in the
/// fourth (`str::parse` reads one from an app's version text, such as
/// `CARGO_PKG_VERSION`).
///
/// ```
/// use tutti_ump::SoftwareRevision;
///
/// let revision = "1.2.3".parse::<SoftwareRevision>().unwrap();
/// assert_eq!(revision.get().map(|byte| byte.get()), [1, 2, 3, 0]);
/// assert!("1.128.0".parse::<SoftwareRevision>().is_err());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "[U7; 4]", into = "[U7; 4]"))]
pub struct SoftwareRevision([U7; 4]);

impl From<[U7; 4]> for SoftwareRevision {
    /// The four bytes as the wire carries them: a revision a peer declares in
    /// its own format.
    fn from(bytes: [U7; 4]) -> Self {
        Self(bytes)
    }
}

impl From<SoftwareRevision> for [U7; 4] {
    fn from(revision: SoftwareRevision) -> Self {
        revision.get()
    }
}

/// Why a text is not a [`SoftwareRevision`] (its `FromStr`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[error("not a software revision: `major.minor[.patch[.build]]`, each part 0 to 127")]
pub struct NotASoftwareRevision;

impl SoftwareRevision {
    /// The four bytes.
    #[must_use]
    pub const fn get(self) -> [U7; 4] {
        self.0
    }
}

impl FromStr for SoftwareRevision {
    type Err = NotASoftwareRevision;

    /// The app-facing entry: an app's own version text
    /// (`env!("CARGO_PKG_VERSION")`) becomes its revision. `major.minor`, `.patch` and `.build` after it
    /// if present (a part left out is 0), each 0 to 127. A pre-release or build
    /// suffix (`-` or `+` and what follows) is not part of the revision.
    fn from_str(version: &str) -> Result<Self, NotASoftwareRevision> {
        let core = version.split(['-', '+']).next().unwrap_or("");
        let mut bytes = [U7::masked(0); 4];
        let mut parts = core.split('.');
        for (i, byte) in bytes.iter_mut().enumerate() {
            match parts.next() {
                Some(part) if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) => {
                    *byte = part
                        .parse()
                        .ok()
                        .and_then(U7::new)
                        .ok_or(NotASoftwareRevision)?;
                }
                None if i >= 2 => {}
                _ => return Err(NotASoftwareRevision),
            }
        }
        match parts.next() {
            None => Ok(Self(bytes)),
            Some(_) => Err(NotASoftwareRevision),
        }
    }
}

/// The MIDI Protocol of a stream (§7.1.6.2).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Protocol {
    /// MIDI 1.0 (`0x01`).
    Midi1,
    /// MIDI 2.0 (`0x02`).
    Midi2,
    /// A reserved value, as read.
    Reserved(Reserved),
}

impl Protocol {
    /// The Protocol a byte reads as: `0x01` MIDI 1.0, `0x02` MIDI 2.0, any
    /// other value the reserved raw as read.
    pub(crate) const fn from_raw(raw: u8) -> Self {
        match raw {
            0x01 => Self::Midi1,
            0x02 => Self::Midi2,
            n => Self::Reserved(Reserved::wire(n)),
        }
    }
}

/// The payload of Stream Configuration Request and Notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct StreamConfig {
    /// The protocol.
    pub protocol: Protocol,
    /// Receive JR Timestamps.
    pub rx_jr: bool,
    /// Transmit JR Timestamps.
    pub tx_jr: bool,
}

/// Which Function Block a Discovery asks about (§7.1.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BlockSelect {
    /// One block.
    One(FunctionBlockId),
    /// All of them (`0xFF`).
    All,
    /// A number above 31 other than `0xFF`, as read.
    Reserved(Reserved),
}

/// Which notifications a Function Block Discovery asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BlockFilter {
    /// Function Block Info.
    pub info: bool,
    /// Function Block Name.
    pub name: bool,
}

/// A Function Block's connection direction (§7.1.8), from the device's own side; `0b00` is
/// reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BlockDirection {
    /// Receives only.
    Input,
    /// Transmits only.
    Output,
    /// Both.
    Bidirectional,
    /// The reserved `0b00`, as read.
    Reserved(Reserved),
}

impl BlockDirection {
    /// The direction a value reads as: `0b01` input, `0b10` output, `0b11`
    /// bidirectional, any other value the reserved raw as read.
    pub(crate) const fn from_raw(raw: u8) -> Self {
        match raw {
            0b01 => Self::Input,
            0b10 => Self::Output,
            0b11 => Self::Bidirectional,
            n => Self::Reserved(Reserved::wire(n)),
        }
    }
}

/// The primary role a Function Block declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum UiHint {
    /// Unknown or undeclared.
    Unknown,
    /// Mainly a receiver.
    Receiver,
    /// Mainly a sender.
    Sender,
    /// Both.
    Both,
}

/// Whether a Function Block is a MIDI 1.0 port; `0x03` is reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Midi1Port {
    /// Not a MIDI 1.0 port.
    No,
    /// A MIDI 1.0 port, bandwidth unrestricted.
    Unrestricted,
    /// A MIDI 1.0 port restricted to 31.25 kbps.
    Restricted,
    /// The reserved `0x03`, as read.
    Reserved(Reserved),
}

impl Midi1Port {
    /// The port status a value reads as: `0` not a port, `1` unrestricted, `2`
    /// restricted, any other value the reserved raw as read.
    pub(crate) const fn from_raw(raw: u8) -> Self {
        match raw {
            0 => Self::No,
            1 => Self::Unrestricted,
            2 => Self::Restricted,
            n => Self::Reserved(Reserved::wire(n)),
        }
    }
}

/// A Function Block Info Notification (§7.1.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FunctionBlockInfo {
    /// Whether the block is active.
    pub active: bool,
    /// Which block.
    pub id: Field<FunctionBlockId>,
    /// Its direction.
    pub direction: BlockDirection,
    /// Its UI hint.
    pub ui_hint: UiHint,
    /// MIDI 1.0 port status.
    pub midi1: Midi1Port,
    /// The first Group it spans.
    pub first_group: Field<FirstGroup>,
    /// How many Groups it spans.
    pub groups_spanned: Field<GroupSpan>,
    /// The MIDI-CI message version, `0` for none.
    pub midi_ci_version: u8,
    /// Simultaneous SysEx 8 streams supported.
    pub max_sysex8_streams: Sysex8StreamCount,
}

/// UMP Stream messages (M2-104 §7.1). They address the endpoint, so they have
/// no Group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Stream {
    /// Endpoint Discovery.
    EndpointDiscovery {
        /// The sender's UMP version.
        version: UmpVersion,
        /// What it asks for.
        filter: DiscoveryFilter,
    },
    /// Endpoint Info Notification.
    EndpointInfo(EndpointInfoNotification),
    /// Device Identity Notification.
    DeviceIdentity(DeviceIdentity),
    /// Endpoint Name Notification: 14 UTF-8 bytes, zero-padded.
    EndpointName(Form, [u8; 14]),
    /// Product Instance Id Notification: 14 ASCII bytes, zero-padded.
    ProductInstanceId(Form, [u8; 14]),
    /// Stream Configuration Request.
    StreamConfigurationRequest(StreamConfig),
    /// Stream Configuration Notification.
    StreamConfigurationNotification(StreamConfig),
    /// Function Block Discovery.
    FunctionBlockDiscovery {
        /// The block asked about.
        block: BlockSelect,
        /// What it asks for.
        filter: BlockFilter,
    },
    /// Function Block Info Notification.
    FunctionBlockInfo(FunctionBlockInfo),
    /// Function Block Name Notification: 13 UTF-8 bytes, zero-padded.
    FunctionBlockName {
        /// Which block.
        block: Field<FunctionBlockId>,
        /// The packet's place.
        form: Form,
        /// The bytes.
        bytes: [u8; 13],
    },
    /// Start of Clip.
    StartOfClip,
    /// End of Clip.
    EndOfClip,
}
