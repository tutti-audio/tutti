//! The translation between the engine's events and VST3's.
//!
//! VST3's event list is richer than MIDI 1: typed note-on/off and poly-pressure
//! structs with `f32` velocity, per-note expression, chords and scales, and
//! generic `Data` events for everything else. [`MidiEncoder`] turns a UMP
//! event (`(Offset, Ump)`) into an [`Event`] at the stream's per-note bend ranges;
//! [`EventExt::to_midi`] reads one back. [`Stager`] fills a plugin's input
//! list from a block's sources, and [`drain_midi`] reads the MIDI a plugin
//! wrote to its output list.
//!
//! Narrowing and sign-changing casts are denied here: every bug this boundary
//! has had was a cast that changed a value's meaning (a `u32` frame offset
//! wrapping negative into an `i32` `sampleOffset`, a dropped release
//! velocity). Where a cast is sound, its `#[allow]` says why.
#![deny(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use tutti_plugin_types::WireMidi;

mod harmony;
mod legacy_cc;
mod midi;
mod note_expression;
mod output;
mod stage;

pub use harmony::{EventText, Utf16Chord, Utf16Scale, Utf16Text, event_text};
pub use midi::MidiEncoder;
pub use note_expression::{expression_type_from_id, expression_type_id};
pub use output::OutputStream;
pub use stage::{InputEvents, Stager, drain_midi};
pub use tutti_vst3_ffi::{
    Chord, Data, Event, EventHeader, EventKind, ExpressionInt, ExpressionText, ExpressionValue,
    K_DATA_TYPE_MIDI_SYSEX, LegacyCcOut, NoteOff, NoteOn, Payload, PolyPressure, Scale, TextRef,
};

use crate::types::NoteExpressionIntValue;

/// Reads an [`Event`] as the engine's types.
pub trait EventExt {
    /// The event as a UMP [`WireMidi`] on the Group of its event bus (the
    /// offset the event carries, unclamped). A MIDI 1.0 payload goes through
    /// `stream`, the output stream's own (M2-104 Appendix D.3); a note
    /// expression with a per-note message is that message. `None` for the
    /// other non-MIDI events, for a message the translator absorbs and for a
    /// reassembled [`Payload::SysEx`] (input-only; the list's buffer holds its
    /// bytes). The other direction is [`MidiEncoder`], which keeps the per-note
    /// bend ranges one event cannot carry.
    fn to_midi(&self, stream: &mut OutputStream) -> Option<WireMidi>;
}

/// An engine value that stages as one [`Event`].
pub trait ToEvent {
    /// The event, or `None` for a value VST3 cannot encode.
    fn to_event(&self) -> Option<Event>;
}

impl EventExt for Event {
    fn to_midi(&self, stream: &mut OutputStream) -> Option<WireMidi> {
        midi::decode(self, stream)
    }
}

/// The frame offset a UMP event stages at: `sampleOffset` is an `i32`, and a
/// `u32` past `i32::MAX` would wrap negative and index the plugin's buffers
/// out of bounds.
pub(crate) fn sample_offset(frame_offset: u32) -> i32 {
    i32::try_from(frame_offset).unwrap_or(i32::MAX)
}

/// Tests build SDK structs and counts by hand, so they repeat the bounded
/// narrowings the production code justifies; one that overflows fails on its
/// own assertion, and the deny stays tight where the ABI is crossed.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
#[cfg(test)]
mod tests;
