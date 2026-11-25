#![doc = include_str!("../README.md")]
//! What a note-driven instrument decides before it makes sound, as one pure
//! step.
//!
//! [`VoiceAllocator::step`] reads one channel voice message and the voices that now
//! exist and returns the [`VoiceCommand`]s the instrument must carry out: start a
//! voice (taking over another's slot when none is free), release one, cut one,
//! move a channel's pitch. It renders no audio, owns no voice and allocates
//! nothing after [`VoiceAllocator::new`]. What differs between instruments is a value
//! of [`VoiceConfig`] and a [`Patch`]; the logic is the same for all of them. The
//! instrument keeps its voices and its render, and does what the commands say.
//!
//! - [`VoiceAllocator`]: the note logic. Read it top to bottom; it is one file.
//! - [`VoiceConfig`]: the declared differences ([`Mode`], [`Retrigger`], [`Steal`],
//!   the polyphony).
//! - [`Patch`]: what a note becomes.
//! - [`ChannelState`]: what a channel has been told apart from its notes (the
//!   14-bit controllers, pressure, the wheel and tuning, the Modulation Depth
//!   Range, the portamento and legato switches), one for each channel, read
//!   from [`VoiceAllocator::channel`]. Legato and Portamento Control are the note
//!   logic's and reach the instrument as a [`Glide`] on a [`VoiceCommand`].
//! - [`VoiceAllocator::universal`]: the Universal System Exclusive messages an
//!   instrument reads (Master Volume and Balance, Master Fine and Coarse
//!   Tuning, General MIDI System On and Off), read by
//!   [`UniversalReader`](tutti_midi_protocol::UniversalReader) at the
//!   instrument's edge; [`MasterGain`] is the gain they set. The MIDI Tuning Standard
//!   rides the same door: the tuning programs ([`TuningCapacity`]), RPN 3 and 4 and
//!   Scale/Octave Tuning reach a voice as the key's tuning (`VoiceCommand::Start`'s `tune`,
//!   [`VoiceCommand::Tune`]).
//! - [`VoiceTable`]: the voices an instrument sounds ([`NoteVoice`]), in the slots
//!   the allocator decides, with its commands carried out in order.
//! - [`Holds`]: the note gate (key, sustain, sostenuto) as a state machine.
//! - [`Controller`]: which channel-mode controllers mean the same to every
//!   instrument.
//! - [`Phase`]: what holds a voice open.
//! - [`PerNoteState`]: the per-note controllers of MIDI 2.0 (held values, a
//!   note id for each started note, Per-Note Management), which [`VoiceAllocator`]
//!   uses and a plugin host, which allocates no voices, can use as it is.
//! - [`MpeSender`]: the Member Channel each note of an MPE output takes, and the
//!   per-note state as what that channel is sent.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

mod channel;
mod controller;
mod holds;
mod master;
mod mpe_sender;
mod per_note;
mod phase;
mod table;
mod tuning;
mod voicing;

pub use channel::{ChannelState, DEFAULT_MODULATION_RANGE};
pub use controller::{Controller, Effect, Stop};
pub use holds::{Holds, Release};
pub use master::MasterGain;
pub use mpe_sender::MpeSender;
pub use per_note::{LIVE_NOTES, NOTE_CONTROLS, NoteControl, NoteEvent, PerNoteState};
pub use phase::Phase;
pub use table::{NoteStart, NoteVoice, VoiceTable};
pub use tuning::{NoSuchTuning, TuningCapacity, TuningCount, TuningCountOutOfRange};
pub use voicing::{
    Glide, Mode, Order, Patch, Retrigger, Steal, VoiceAllocator, VoiceCommand, VoiceConfig,
    VoiceId, VoiceSpec,
};
