//! Note-expression (MPE-style) vocabulary shared across the plugin host
//! crates and the `tutti-plugin` IPC protocol.
//!
//! [`NoteExpressionType`] covers the dimensions any hosted format names: the
//! five VST3 standard ones plus CLAP's `Pressure` and `Expression`, with
//! [`Custom`](NoteExpressionType::Custom) carrying a vendor id the enum does
//! not name. Decoding a plugin's output into this enum is lossless.
//!
//! *Encoding* toward a format that lacks a dimension is the partial direction —
//! each host crate owns that conversion and is explicit about what it cannot
//! represent (e.g. VST3 has no `typeId` for `Pressure`), rather than silently
//! coercing to a different dimension.
//!
//! The `Serialize`/`Deserialize` derives are gated behind the `serde`
//! feature (the IPC wire path enables it; pure in-process consumers don't
//! pay for it).

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use tutti_midi_protocol::{PitchBendSensitivity, signed_f32_to_bend, unit_f32_to_value};
use tutti_ump::{CCNumber, NoteNumber, Voice2};

/// A per-note expression dimension.
///
/// The union of what VST3 and CLAP support: VST3 covers the first five, CLAP
/// adds [`Pressure`](Self::Pressure) and [`Expression`](Self::Expression).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum NoteExpressionType {
    /// Per-note volume / loudness.
    Volume,
    /// Per-note pan.
    Pan,
    /// Per-note tuning, in the format's native convention.
    Tuning,
    /// Vibrato intensity.
    Vibrato,
    /// Brightness / filter cutoff.
    Brightness,
    /// Per-note pressure (poly aftertouch). CLAP-native; VST3 cannot encode
    /// this as a note-expression `typeId`.
    Pressure,
    /// Generic per-note expression. CLAP-native; VST3 cannot encode this as a
    /// note-expression `typeId`.
    Expression,
    /// A dimension the plugin defined itself, carried by its native id.
    ///
    /// VST3's `NoteExpressionTypeID` is a `u32` and the spec reserves
    /// everything from `kCustomStart` upward for plugin-defined dimensions,
    /// discovered at runtime through `INoteExpressionController`. Without this
    /// variant a decoder has nowhere to put such an event and drops it, so a
    /// plugin whose expressiveness is entirely custom appears to send nothing at
    /// all.
    ///
    /// The id is only meaningful against the plugin that issued it — it is not
    /// a shared namespace, and two plugins may use the same number for
    /// different things. That is why this carries the raw id rather than
    /// pretending to name it.
    Custom(u32),
}

/// A single note-expression sample bound to a specific active voice
/// (`note_id`) at a frame offset within the current block.
///
/// This is the **neutral** value the protocol and VST3 speak. CLAP carries
/// extra voice-addressing fields (port / channel / key) in its own local
/// wrapper, converting to/from this shape at its boundary.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct NoteExpressionValue {
    /// Frame offset from the start of the process block.
    pub sample_offset: i32,
    /// The voice this drives, as a host note id — see
    /// [`plugin_note_id`](crate::plugin_note_id). An id no active voice holds
    /// addresses nothing.
    pub note_id: i32,
    /// Which dimension is being driven.
    pub expression_type: NoteExpressionType,
    /// The value, in the dimension's own convention — **not** uniformly
    /// normalized, and not a [`Normalized`](crate::Normalized). VST3 scales most
    /// dimensions `0..=1` but tuning in semitones; each host crate converts at
    /// its own boundary.
    pub value: f64,
}

/// Storage for one block's note-expression samples.
///
/// A plain `Vec`. The producers reuse one value across blocks and clear it in
/// place ([`ProcessOutput::clear`](crate::ProcessOutput::clear)), so its
/// capacity survives and a warm block does not allocate.
pub(crate) type NoteExpressionVec = Vec<NoteExpressionValue>;

/// One block's note-expression samples.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct NoteExpressionChanges {
    /// The block's samples, in the order the producer appended them.
    pub changes: NoteExpressionVec,
}

impl NoteExpressionChanges {
    /// Builds an empty batch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends one sample.
    ///
    /// Order is the caller's to maintain; nothing here sorts by
    /// `sample_offset`.
    pub fn add_change(&mut self, change: NoteExpressionValue) {
        self.changes.push(change);
    }

    /// Returns `true` when the block carries no note expression.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}

/// The per-note controllers a plugin's note expression stands for, and the
/// expression each is: Volume is controller 7, Pan 10 and Brightness 74, the
/// usual CC assignments. The one table both directions of every format read
/// (an assignable and a registered per-note controller share the number).
pub const PER_NOTE_EXPRESSIONS: [(CCNumber, NoteExpressionType); 3] = [
    (CCNumber::VOLUME, NoteExpressionType::Volume),
    (CCNumber::PAN, NoteExpressionType::Pan),
    (CCNumber::BRIGHTNESS, NoteExpressionType::Brightness),
];

/// The expression the per-note controller `number` is ([`PER_NOTE_EXPRESSIONS`]),
/// or `None` for a number with no counterpart.
#[must_use]
pub fn expression_of_per_note_controller(number: u8) -> Option<NoteExpressionType> {
    PER_NOTE_EXPRESSIONS
        .iter()
        .find(|(controller, _)| controller.get() == number)
        .map(|&(_, expression)| expression)
}

/// The per-note controller `expression` is ([`PER_NOTE_EXPRESSIONS`]), or
/// `None` for a dimension that is not one.
#[must_use]
pub fn per_note_controller_of_expression(expression: NoteExpressionType) -> Option<CCNumber> {
    PER_NOTE_EXPRESSIONS
        .iter()
        .find(|&&(_, ty)| ty == expression)
        .map(|&(controller, _)| controller)
}

/// The Per-Note Pitch Bend a plugin's Tuning of `semitones` from `note`'s key
/// is, over the MPE default range of ±48 semitones (RP-053): the range a
/// receiver assumes until a stream sets another with RPN 0/7, and the host
/// sends none alongside.
#[must_use]
pub fn per_note_tuning(note: NoteNumber, semitones: f64) -> Voice2 {
    let range = f64::from(PitchBendSensitivity::MPE_MEMBER_DEFAULT.semitones().get());
    #[allow(clippy::cast_possible_truncation, reason = "a bend position is f32")]
    let signed = (semitones / range) as f32;
    Voice2::PerNotePitchBend {
        note,
        value: signed_f32_to_bend(signed),
    }
}

/// The MIDI 2.0 per-note message a plugin's `expression` on `note` is at
/// `unit` (`0..=1`, clamped): Polyphonic Pressure for Pressure, the Assignable
/// Per-Note Controller of its number for Volume, Pan and Brightness. `None`
/// for a dimension with no such message: Tuning (see [`per_note_tuning`]),
/// Vibrato, Expression and a custom id.
#[must_use]
pub fn per_note_message(
    expression: NoteExpressionType,
    note: NoteNumber,
    unit: f32,
) -> Option<Voice2> {
    let value = unit_f32_to_value(unit);
    if expression == NoteExpressionType::Pressure {
        return Some(Voice2::PolyPressure { note, value });
    }
    Some(Voice2::AssignablePerNote {
        note,
        controller: per_note_controller_of_expression(expression)?.get(),
        value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_ump::ControlValue;

    /// Volume, Pan and Brightness are the per-note controllers 7, 10 and 74, and
    /// the two lookups are one table read both ways; nothing else is in it.
    #[test]
    fn the_table_is_the_usual_controller_assignments_both_ways() {
        for (number, expression) in [
            (7, NoteExpressionType::Volume),
            (10, NoteExpressionType::Pan),
            (74, NoteExpressionType::Brightness),
        ] {
            assert_eq!(expression_of_per_note_controller(number), Some(expression));
            assert_eq!(
                per_note_controller_of_expression(expression).map(CCNumber::get),
                Some(number)
            );
        }
        assert_eq!(expression_of_per_note_controller(1), None);
        for expression in [
            NoteExpressionType::Tuning,
            NoteExpressionType::Pressure,
            NoteExpressionType::Vibrato,
            NoteExpressionType::Expression,
            NoteExpressionType::Custom(5),
        ] {
            assert_eq!(per_note_controller_of_expression(expression), None);
        }
    }

    /// A tuning of 12 semitones is a quarter of the MPE default range of ±48
    /// (RP-053): a bend a quarter of the way from the centre `0x8000_0000` to
    /// the top (M2-115 section 3, min-centre-max), and a tuning of 0 is the
    /// centre.
    #[test]
    fn a_tuning_is_a_bend_over_the_mpe_default_range() {
        let note = NoteNumber::of(60);
        let bend = |semitones| match per_note_tuning(note, semitones) {
            Voice2::PerNotePitchBend { value, .. } => value.get(),
            other => panic!("not a per-note bend: {other:?}"),
        };
        assert_eq!(bend(0.0), 0x8000_0000);
        assert_eq!(bend(12.0), 0xA000_0000);
        assert_eq!(bend(-48.0), 0);
    }

    /// Pressure is Polyphonic Pressure, the table's expressions are the
    /// Assignable Per-Note Controller of their number, over the 32 bits of the
    /// unit, clamped; the rest have no message.
    #[test]
    fn the_message_of_an_expression_is_its_per_note_message() {
        let note = NoteNumber::of(64);
        assert_eq!(
            per_note_message(NoteExpressionType::Pressure, note, 1.0),
            Some(Voice2::PolyPressure {
                note,
                value: ControlValue::new(u32::MAX)
            })
        );
        assert_eq!(
            per_note_message(NoteExpressionType::Pan, note, 0.5),
            Some(Voice2::AssignablePerNote {
                note,
                controller: 10,
                value: ControlValue::new(0x8000_0000)
            })
        );
        assert_eq!(
            per_note_message(NoteExpressionType::Volume, note, 7.0),
            Some(Voice2::AssignablePerNote {
                note,
                controller: 7,
                value: ControlValue::new(u32::MAX)
            })
        );
        for none in [
            NoteExpressionType::Tuning,
            NoteExpressionType::Vibrato,
            NoteExpressionType::Expression,
            NoteExpressionType::Custom(9),
        ] {
            assert_eq!(per_note_message(none, note, 0.5), None);
        }
    }
}
