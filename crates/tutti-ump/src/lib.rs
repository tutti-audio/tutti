#![doc = include_str!("../README.md")]
//! The Universal MIDI Packet (M2-104): its storage, its addresses and its
//! messages.
//!
//! [`Ump`] is one packet as four `u32` words. [`MidiGroup`], [`MidiChannel`]
//! and [`CCNumber`] are the addresses a packet carries, as newtypes so that a
//! transposed `(group, channel)` pair does not compile. [`Message`] is one
//! defined message as a `Copy` value with no lifetime. [`Ump::decode`]
//! (and `TryFrom<Ump> for Message`) and `From<Message> for Ump` hold the bit
//! positions of every message. The packet-level readers and writers on [`Ump`]
//! (`words`, `message_type`, `group`, `channel`, `is_note_off`,
//! `is_system_real_time`, `note_key_event`, `note_off`, `sustain_up`) are the only
//! other code that shifts a packet's bits, and each agrees with `decode` and
//! `encode` on every packet (a test holds it). MIDI 1.0 on the wire is the
//! other place that knows bit positions: [`Voice1::bytes`] and
//! [`System::bytes`] write a message's bytes, [`frame`] adds a SysEx's
//! delimiters, and [`Midi1Parser`] reads a byte stream
//! into [`Message`]s. [`NoteId`] is a note's identity (`channel * 128 + note`,
//! the plugin formats' wire value), and [`SysexMessage`] and [`SysexAssembler`]
//! are a checked complete SysEx and the one SysEx7 reassembler (a stream per
//! Group), over a buffer the caller owns, as [`Sysex8Assembler`] and
//! [`MixedDataAssembler`] are of System Exclusive 8 (a message per Group and
//! Stream ID, [`Sysex8Stream::packets`] its inverse, its Stream IDs from the
//! receiver's [`Sysex8StreamIds`]) and Mixed Data Sets (a set
//! per Group and MDS ID, [`MixedDataSet::packets`] its inverse);
//! [`SysexMessage::packets`] is its inverse, the one writer of a payload as
//! SysEx7 packets ([`Sysex7Packets::from_payload`] for plain bytes), as
//! [`flex_text_packets`] is of a text as Flex Data packets. [`split_stream`]
//! cuts a buffer
//! of concatenated packets (a native-UMP transport's) into [`Ump`]s.
//!
//! ```
//! use tutti_ump::{Message, Ump, Voice2};
//!
//! // MIDI 2.0 Note On: group 0, channel 0, note 60, 16-bit velocity 0x8000.
//! let packet = Ump([0x4090_3C00, 0x8000_0000, 0, 0]);
//! let Ok(Message::Midi2(address, Voice2::NoteOn { note, velocity, .. })) = packet.decode() else {
//!     unreachable!()
//! };
//! assert_eq!((address.channel.get(), note.get(), velocity.get()), (0, 60, 0x8000));
//! ```
//!
//! Decoding reads no reserved bit. A reserved value in an enumerated field is
//! kept as a [`Reserved`], which only the decoder builds: a later revision's
//! message stays readable and re-encodes exactly, and no sender can name a
//! reserved value (M2-104 section 2.1.3). The same holds for a reserved Ticks
//! Per Quarter Note ([`Utility::ReservedTicksPerQuarterNote`]), a reserved
//! Function Block count ([`FunctionBlocks::Reserved`]), accents of a shape
//! Set Metronome does not have ([`BarAccents::Reserved`]) and an attribute type
//! no spec this crate types defines ([`Attribute::Reserved`]); the M2-123
//! articulation types are [`Attribute::Articulation`], which
//! [`NoteArticulation`] writes. A packet that cannot be framed comes back as
//! [`Unknown`] with its words, and a consumer forwards it unchanged, never
//! drops it. A packet
//! re-encodes to itself when it is canonical: its reserved bits are zero, and a
//! field another field makes meaningless has its canonical value (attribute
//! data under type 0, bank bytes under a clear Bank Valid bit, and a chord bass
//! accidental under bass note 0, which is -8). A MIDI 1.0 Note On at velocity
//! 0 is a Note On here: decoding never translates. Reading it as a Note Off, and
//! scaling between the two protocols, is `Voice2::from(voice1)`,
//! `Voice1::try_from(voice2)` and [`Message::to_midi2`].
//!
//! # Messages are values
//!
//! A [`Message`] has no lifetime: it can be matched, stored or queued on any
//! thread with no allocation. Every message M2-104 defines is one packet of at
//! most 16 bytes, so text and SysEx chunks are small arrays in their variants;
//! a whole string or SysEx message is reassembled above the codec, off the audio
//! thread, by [`FlexTextAssembler`] and [`SysexAssembler`]. The limits on a
//! reassembled name (Endpoint Name, Function Block Name, Product Instance Id)
//! are not the codec's: a consumer checks them where it builds a name.
//!
//! The fields are typed ([`NoteNumber`], [`U4`], [`U7`], [`U14`], [`Pitch`] and
//! the address types). A public `new` refuses an out-of-range raw, `of` builds
//! one from a literal or constant and panics out of range, and `masked` is the
//! one constructor that masks, for bits a decoder has just framed. Only the
//! codec shifts bits: a consumer matches on [`Message`], and the graph's queues
//! call [`Ump::is_note_off`].
//!
//! The crate names only `tutti-units`, which has no allocation either, so the
//! audio graph can carry packets without linking a MIDI stack. Only [`Held`]
//! allocates, once, in `new`: it boxes the tables it keeps of what a source has
//! started.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod articulation;
mod bend_value;
mod cc_number;
mod ci_version;
mod control_value;
mod decode;
mod encode;
mod field;
mod flex_tempo;
mod flex_text;
mod flex_write;
mod held_controllers;
mod held_keys;
mod iso8601;
mod jr_ticks;
mod lanes;
mod manufacturer;
mod message;
mod messages;
mod midi1;
mod midi_channel;
mod midi_group;
mod midi_set;
mod midi_translation;
mod mixed_data;
mod mpe_zone;
mod name;
mod note_id;
mod note_velocity;
mod orchestral;
mod os;
mod reserved;
mod rpn;
mod scale;
mod sysex;
mod sysex8;
mod text;
mod ump;

pub use articulation::{
    Articulation, ArticulationAttribute, ArticulationError, Classification, ColorSound, CoreSound,
    CustomClassification, EffectSound, GestureSound, IntervallicTrill, NoteArticulation,
    NoteEnding, NoteOffArticulation, ReleaseTime, RunSound, SameNoteSound, ShortSound,
    StringAssignment, Stroke,
};
pub use bend_value::BendValue;
pub use cc_number::{CCNumber, CCNumberOutOfRange};
pub use ci_version::{CiVersion, NotAVersion};
pub use control_value::ControlValue;
pub use field::{
    ChordType, ChordTypeOutOfRange, DeltaTicks, DeltaTicksOutOfRange, FirstGroup,
    FirstGroupOutOfRange, FunctionBlockCount, FunctionBlockCountOutOfRange, FunctionBlockId,
    FunctionBlockIdOutOfRange, GroupSpan, GroupSpanOutOfRange, MonoChannels,
    MonoChannelsOutOfRange, MpeChannels, MpeChannelsOutOfRange, NotOnMidiScale, NoteNumber,
    NoteNumberOutOfRange, Pitch, SharpsFlats, SharpsFlatsOutOfRange, TicksPerQuarterNote,
    TicksPerQuarterNoteOutOfRange, U4, U4OutOfRange, U7, U7OutOfRange, U14, U14OutOfRange, U28,
    U28OutOfRange, first_non_seven_bit,
};
pub use flex_tempo::FlexTempo;
pub use flex_text::{FlexText, FlexTextAssembler, FlexTextStep};
pub use flex_write::{
    Entry, EntryError, FLEX_TEXT_MAX_LEN, FlexTextError, FlexTextPackets, Iso8601Date,
    Iso8601DateError, LyricKind, LyricPackets, MelismaEndPackets, PlainTextKind, Syllable,
    SyllableBody, SyllableEnd, SyllableError, Text, WordEnd, flex_clip_name_packets,
    flex_lyric_packets, flex_melisma_end_packets, flex_project_name_packets,
    flex_recording_date_packets, flex_text_packets,
};
pub use held_controllers::HeldControllers;
pub use held_keys::{GroupChannelIndex, GroupChannelIndexOutOfRange, Held, KeyBitset};
pub use jr_ticks::JrTicks;
pub use manufacturer::{ManufacturerBytesError, ManufacturerId, NotManufacturerId};
pub use message::{
    Accidental, Address, Alteration, AlterationKind, Attribute, Bank, BarAccents, BlockDirection,
    BlockFilter, BlockSelect, ChordBass, ChordName, ChordPitch, Data128, DeviceIdentity,
    DiscoveryFilter, EndpointInfoNotification, Flex, FlexAddress, FlexTextKind, FlexTimeSignature,
    Form, FunctionBlockInfo, FunctionBlocks, KeySignature, Letter, Message, Metronome, Midi1Port,
    MixedDataHeader, NotASoftwareRevision, PerNoteManagement, Protocol,
    RegisteredPerNoteController, SoftwareRevision, Stream, StreamConfig, Sysex7Packet,
    Sysex8Packet, SysexStatus, System, UiHint, UmpVersion, Unknown, Utility, Voice1, Voice2,
};
pub use messages::{Join, PacketSpan, Spans, spans, spans_joined};
pub use midi_channel::{MidiChannel, MidiChannelOutOfRange};
pub use midi_group::{MidiGroup, MidiGroupOutOfRange};
pub use midi_set::{FunctionBlockSet, MidiChannelSet, MidiGroupSet};
pub use midi_translation::{ExtremeValues, NoMidi1Form};
pub use midi1::{Framed, Midi1Bytes, Midi1Parser, frame, parse_exact};
pub use mixed_data::{
    MdsChunk, MdsChunkSize, MdsChunkUmps, MdsChunkUmpsOutOfRange, MdsChunks, MdsIdentity,
    MixedDataAssembler, MixedDataPackets, MixedDataSet, MixedDataStep, NotChunkSize,
};
pub use mpe_zone::{MpeOverlap, MpeZoneSet, MpeZoneSpan};
pub use name::{
    BLOCK_NAME_BYTES, BlockName, ENDPOINT_NAME_BYTES, EndpointName, INSTANCE_ID_BYTES,
    InstanceIdError, Name, NameError, ProductInstanceId,
};
pub use note_id::NoteId;
pub use note_velocity::NoteVelocity;
pub use orchestral::{
    MuteAmount, MuteError, MuteMessage, MuteState, MuteType, PlayingPosition, ReservedMuteType,
};
pub use os::{OsFunctionBlock, read_function_block, read_protocol};
pub use reserved::{Field, Reserved, ReservedAccents, ReservedAttribute};
pub use rpn::{RegisteredData, Rpn};
pub use sysex::{
    IDENTITY_REQUEST, SYSEX7_MAX_PAYLOAD, Sysex7Packets, SysexAssembler, SysexError, SysexMessage,
    SysexPart, SysexRef, SysexStep, UNIVERSAL_NON_REAL_TIME, UNIVERSAL_REAL_TIME,
};
pub use sysex8::{
    Sysex8Assembler, Sysex8Message, Sysex8Packets, Sysex8Step, Sysex8Stream, Sysex8StreamCount,
    Sysex8StreamId, Sysex8StreamIds,
};
pub use text::{StreamTextAssembler, TextPackets, text_packets};
pub use ump::{MessageType, NoteKey, NoteKeyEvent, Ump, split_stream};
