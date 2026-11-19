#![doc = include_str!("../README.md")]
//! Translation between the MIDI 1.0 and MIDI 2.0 protocols.
//!
//! [`widen`] turns a MIDI 1.0 channel voice message into the MIDI 2.0 message
//! that means the same, so a consumer matches [`Voice2`](tutti_ump::Voice2)
//! once. It is pure: no state, no allocation, no clock. The group and channel
//! belong to the packet's [`Address`](tutti_ump::Address), which the caller
//! keeps unchanged.
//!
//! [`Midi2Translator`] is the stateful rest of the translation (M2-104 Appendix
//! D.3): RPN and NRPN runs into Registered and Assignable Controllers, and
//! Bank Select onto the next Program Change, per group and channel. An input
//! edge runs it once, so what follows carries MIDI 2.0 channel voice messages.
//! It calls `widen` for every message that needs no state.
//!
//! [`Midi1Translator`] is its inverse (M2-104 Appendix D.2), per group and channel:
//! a Registered or Assignable Controller as its four Control Changes, and a
//! Program Change with a bank as Bank Select MSB and LSB then the Program
//! Change, every time. An output edge that speaks MIDI 1.0 holds one.
//!
//! [`channel_voice`] is what an instrument reads a packet with: the channel
//! voice message it carries as a [`Voice2`](tutti_ump::Voice2), a MIDI 1.0 one
//! widened, or `None`. [`widen_message`] widens a whole
//! [`Message`](tutti_ump::Message) and passes every other kind through, for an
//! edge that forwards what it does not read (a plugin host's MIDI output).
//! [`widen_bytes`] is the same for the wire bytes of one MIDI 1.0 message.
//!
//! [`read()`](fn@read) parses one whole System Exclusive message as a Universal one
//! ([`Universal`]: Identity Request and Reply, General MIDI On and Off, Master
//! Volume, Balance, Fine and Coarse Tuning, and MIDI Machine Control commands and
//! responses, [`MmcCommand`] and [`MmcInfoField`]), and [`UniversalReader`] is an
//! instrument's edge for it: a Group's UMPs in, the messages addressed to the
//! instrument ([`SysexDeviceId`], or the all-call) out.
//!
//! ```
//! use tutti_midi_protocol::widen;
//! use tutti_ump::{NoteNumber, U7, Voice1, Voice2};
//!
//! let note = NoteNumber::of(60);
//! let on = Voice1::NoteOn { note, velocity: U7::of(64) };
//! // 7-bit centre 0x40 is 16-bit centre 0x8000 (M2-115 section 3).
//! let Voice2::NoteOn { velocity, .. } = widen(on) else { unreachable!() };
//! assert_eq!(velocity.get(), 0x8000);
//! ```
//!
//! [`narrow_velocity`], [`narrow_value`] and [`narrow_bend`] go the other way
//! for a consumer that has MIDI 1.0 resolution only (M2-115 section 3.4): the
//! top bits, as a right shift. `widen` then narrow returns every 7-bit and
//! 14-bit value it started with. A consumer such as a SoundFont synth works
//! at MIDI 1.0 resolution and narrows what reaches it.
//! The widening of one value is [`widen_velocity`], [`widen_value`] and
//! [`widen_bend`]: they build a [`NoteVelocity`](tutti_ump::NoteVelocity), a
//! [`ControlValue`](tutti_ump::ControlValue) or a
//! [`BendValue`](tutti_ump::BendValue) from a 7-bit or 14-bit one, which a
//! MIDI 2.0 constructor such as `Voice2::note_on` wants.
//!
//! Going down has four entry points; pick by what you hold and what you need.
//!
//! - [`narrow_message()`](fn@narrow_message) for one stateless
//!   [`Message`](tutti_ump::Message) to its MIDI 1.0 [`Message`](tutti_ump::Message),
//!   the group and channel kept (the inverse of [`widen_message`]).
//! - [`narrow()`](fn@narrow) for one [`Voice2`](tutti_ump::Voice2) to its
//!   [`Voice1`](tutti_ump::Voice1), when you already hold the voice message.
//! - [`Midi1Translator`] for a stream of packets that must keep Registered and
//!   Assignable Controllers and Bank Select: it is the only one with state,
//!   and the only one that does not refuse them.
//! - [`midi1()`](fn@midi1) for the bytes of the wire, a Program Change with a
//!   bank as its three messages.
//!
//! A bend as a position on `-1..=1` is
//! [`BendValue::bipolar`](tutti_ump::BendValue::bipolar) and its inverse
//! [`BendValue::from_bipolar`](tutti_ump::BendValue::from_bipolar); a velocity
//! as a [`Velocity`](tutti_units::Velocity) is
//! [`NoteVelocity::velocity`](tutti_ump::NoteVelocity::velocity) and its inverse
//! [`NoteVelocity::from_velocity`](tutti_ump::NoteVelocity::from_velocity); a
//! controller value as a position on `0..=1` is
//! [`ControlValue::normalized`](tutti_ump::ControlValue::normalized).
//!
//! What the widening does and does not do:
//!
//! - **Scaling** is Min-Center-Max (M2-115 section 3): minimum, centre and
//!   maximum map to the same points at every width, and values above the
//!   centre repeat their low bits. It applies to velocity (7 to 16 bits),
//!   poly pressure, channel pressure and Control Change values (7 to 32 bits)
//!   and Pitch Bend (14 to 32 bits).
//! - **A Note On at velocity 0** is a Note Off with release velocity `0x8000`
//!   (M2-104 Appendix D.3.1). This is the one place that rule lives. A MIDI 1.0
//!   Note Off keeps its velocity, scaled, so a release velocity of 0 stays 0.
//! - **Attributes** are `None` on both notes, as D.3.1 sets them.
//! - **A Program Change** carries no bank: choosing one needs the Bank Select
//!   latched on the channel, which is state.
//! - **Control Changes stay Control Changes.** D.3.3 turns a properly formed
//!   RPN or NRPN run (CC 101/100, 99/98, 6, 38) into one Registered or
//!   Assignable Controller message, and Bank Select (CC 0 and 32) into a
//!   Program Change bank. Both need per-group, per-channel assembly, which is
//!   the [`Midi2Translator`]'s. Here those controllers widen by value like any
//!   other, so a consumer that wants RPN or NRPN data must not rely on this
//!   function for it. The 14-bit pairs (CC 1 to 31 with 33 to 63) stay two
//!   independent messages, as D.3.3 says.
//!
//! Translation runs once, at the edge where MIDI 1.0 enters or leaves. Reading
//! never translates: `tutti-ump` cannot call this crate, so a decoded
//! [`Message`](tutti_ump::Message) keeps the protocol it arrived in.
//!
//! The crate also holds the values the translation and the instruments share:
//! [`flex_tempo`] and [`flex_time_signature`] read a Flex Set Tempo and Set Time
//! Signature as a `Bpm` and a project meter (`None` for a field the type cannot
//! hold, never clamped), and [`flex_tempo_field`] and [`flex_time_signature_wire`]
//! write them back;
//! [`song_position`](fn@song_position) converts a beat to a Song Position
//! Pointer; [`SmpteTimecode::from_seconds`] and
//! [`MtcTimeCode::nibbles`] are the timecode of a time and its Quarter Frame
//! pieces; [`MpeZoneConfig`], [`MpeZones`] and [`MpeMode`] are the
//! MPE zone layout (RP-053, and the MIDI-CI MPE Profile's, M2-120), checked
//! at construction; and [`TuningMessage`] and
//! its types are the MIDI Tuning Standard's wire values.
//!
//! [`PitchBendSensitivity`] is the 7.25 fixed-point range of RPN #00/07 (M2-104
//! section 7.4.13), and also the range of RPN #00/00, whose data is whole
//! semitones and cents ([`RegisteredData`](tutti_ump::RegisteredData), M2-104
//! section 7.4.7.1): it converts between the two.
//!
//! **One input type at the edge:** a packet is a [`Ump`](tutti_ump::Ump), and
//! [`Midi2Translator::translate`], [`Midi1Translator::feed`], [`midi1()`](fn@midi1) and
//! [`channel_voice`] take `impl Into<Ump>`, so a decoded
//! [`Message`](tutti_ump::Message) goes in as it is. The conversion between the
//! two protocols is `widen`/`narrow` (and `widen_message`, `widen_bytes`) rather
//! than `From`/`TryFrom` on the voice types: the orphan rule would put the impls in
//! `tutti-ump`, and with them the M2-115 scaling and the Note On at velocity 0
//! rule, which `tutti-ump` leaves to translation.
//!
//! The crate depends on `tutti-ump` for the wire types, `tutti-units` for the
//! quantities it reads and writes (`Semitones`, `Cents`, `Bpm`, `Seconds`) and
//! `tutti-music` for the time signature. Translation allocates nothing after
//! construction. Its `serde` feature (off by default) derives serde on the
//! values a project saves, each checked as it is read back.

#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod flex;
mod midi1;
mod midi1_translator;
mod midi2_translator;
mod mode;
mod mpe;
mod narrow;
mod per_note_management;
mod scale;
mod sensitivity;
mod smpte;
mod song_position;
mod tuning;
mod universal;
mod voice;

pub use flex::{flex_tempo, flex_tempo_field, flex_time_signature, flex_time_signature_wire};
pub use midi1::{NoMidi1Form, midi1, narrow, narrow_message, narrow_with};
pub use midi1_translator::Midi1Translator;
pub use midi2_translator::Midi2Translator;
pub use mode::{ExtremeValues, PerNoteTranslation, TranslationMode};
#[cfg(feature = "serde")]
pub use mpe::InvalidMpeZoneConfig;
pub use mpe::InvalidMpeZones;
pub use mpe::{
    ChannelResponse, ControllerSupport, MpeFeatures, MpeMode, MpeSpec, MpeZone, MpeZoneConfig,
    MpeZones, PitchBendSupport,
};
pub use narrow::{narrow_bend, narrow_note_on_velocity, narrow_value, narrow_velocity};
pub use per_note_management::{PerNoteManager, PerNotePolicy};
pub use sensitivity::PitchBendSensitivity;
#[cfg(feature = "serde")]
pub use smpte::InvalidSmpteTimecode;
pub use smpte::{MtcRate, SmpteFrameRate, SmpteTimecode};
pub use song_position::song_position;
pub use tuning::{
    ChannelMask, InvalidKeyChanges, InvalidKeyTable, KeyChange, KeyChanges, KeyTable,
    MAX_KEY_CHANGES, NotFrequencyData, OctaveOffsets, Timing, TuningFrequency, TuningMessage,
    TuningName,
};
#[cfg(feature = "serde")]
pub use universal::InvalidMtcTimeCode;
pub use universal::{
    Addressed, Addressee, BadChecksum, GeneralMidiMode, GeneratorAction, GeneratorSetUp,
    GeneratorTally, Heard, JamMode, JamReference, MMC_MAX_DATA, MMC_MAX_SEGMENTED,
    MMC_MAX_SEGMENTS, MMC_MAX_TRACK_BITMAP_BYTES, MMC_SEGMENT_BYTES, McpSuccess, McsSuccess,
    MmcArithmetic, MmcBytes, MmcBytesRefused, MmcChaseMode, MmcCodeStatus, MmcCommand,
    MmcCommandReassembler, MmcCommandSuccess, MmcCommands, MmcControlDisable, MmcDevices,
    MmcDirection, MmcErrorCode, MmcErrorCommand, MmcErrorFlags, MmcErrorKind, MmcErrorReport,
    MmcEvent, MmcEventDirection, MmcEventFlags, MmcEventResponse, MmcExtension, MmcFastMode,
    MmcFields, MmcFragment, MmcGlobalMonitor, MmcGroup, MmcInfoField, MmcItem, MmcItems,
    MmcLifterDefeat, MmcLocate, MmcMaskedWrite, MmcMove, MmcName, MmcNames, MmcOtherField,
    MmcPayload, MmcPlaySpeedReference, MmcPosition, MmcProcedure, MmcProcedureResponse,
    MmcReassembler, MmcRecordActivity, MmcRecordMode, MmcRecordMonitor, MmcRecordStatus,
    MmcReservedCommand, MmcResolvedPlayMode, MmcResponseReassembler, MmcSegment, MmcSegmentBytes,
    MmcSegmentError, MmcSegmentPlace, MmcSegments, MmcSegmentsRefused, MmcSelection, MmcSequence,
    MmcShortTimeCode, MmcSign, MmcSignature, MmcSlot, MmcSpeed, MmcStep, MmcStopMode, MmcString,
    MmcSubframes, MmcSupportBitmap, MmcSystemMaster, MmcTimeCode, MmcTimeCodeName,
    MmcTimeCodeSource, MmcTimeLabel, MmcTimeTail, MmcTrack, MmcTrackBitmap, MmcUpdate,
    MmcUserBitFlags, MmcUserBits, MotionControlTally, MotionProcess, MotionState, MtcAction,
    MtcFlags, MtcFullMessage, MtcSetUp, MtcTally, MtcTimeCode, NotMmcPosition, NotMmcSlot,
    NotMmcSubframes, NotQuarterFramePiece, NotSysexDeviceId, QuarterFrame, QuarterFrameCycle,
    QuarterFramePiece, Recipient, RunReference, SysexDeviceId, Universal, UniversalRead,
    UniversalReader, VitcInsert, read,
};
pub use voice::{
    channel_voice, widen, widen_bend, widen_bytes, widen_message, widen_value, widen_velocity,
};
