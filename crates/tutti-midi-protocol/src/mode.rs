//! The choices of a MIDI 2.0 to MIDI 1.0 translation (M2-104 Appendix D):
//! the Default Translation and the alternates a translator may add (D.1.5,
//! D.2.8, D.4).

use tutti_ump::ExtremeValues;

use crate::MpeLayout;

/// What a translation does with the messages MIDI 1.0 has no equivalent for:
/// Per-Note Controllers, Per-Note Pitch Bend and Per-Note Management (M2-104
/// D.2.8), which the Default Translation does not address and an Alternate
/// Translation Mode may (D.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PerNoteTranslation {
    /// The Default Translation: they have no MIDI 1.0 message and send
    /// nothing.
    #[default]
    Dropped,
    /// MIDI Polyphonic Expression: each note takes a member channel of the
    /// zone, so a channel Pitch Bend, Channel Pressure and Control Change 74 on
    /// it act on that note alone. The layout is a zone's
    /// ([`Mpe11Zone::layout`](crate::Mpe11Zone::layout) or
    /// [`ProfileZone::layout`](crate::ProfileZone::layout)): the zone the
    /// MIDI-CI MPE Profile enabled (D.4.1), or one set by other means such as
    /// the MPE Configuration Message (D.4.2).
    ///
    /// A [`Message::Midi1`](tutti_ump::Message::Midi1) input passes unchanged:
    /// its channel is the sender's, and a MIDI 1.0 note on a member channel is
    /// not seen by the allocator.
    Mpe(MpeLayout),
}

/// The Default Translation, or the Alternate Translation Modes a
/// [`Midi1Translator`](crate::Midi1Translator) runs (M2-104 D.1.5 and D.4). The default is the
/// Default Translation; a product with a non-default mode tells its user it is
/// active ([`is_alternate`](Self::is_alternate) is the question to ask).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TranslationMode {
    /// How the smallest and largest values are translated to MIDI 1.0.
    pub extremes: ExtremeValues,
    /// What becomes of the per-note messages.
    pub per_note: PerNoteTranslation,
}

impl TranslationMode {
    /// Whether any part of the mode is not the Default Translation.
    #[must_use]
    pub fn is_alternate(self) -> bool {
        self != Self::default()
    }
}
