//! The choices of a MIDI 2.0 to MIDI 1.0 translation (M2-104 Appendix D):
//! the Default Translation and the alternates a translator may add (D.1.5,
//! D.2.8, D.4).

use tutti_ump::{U7, U14, Voice1, Voice2};

use crate::{MpeZoneConfig, NoMidi1Form, narrow};

/// How a value that narrows to the smallest or largest MIDI 1.0 value is
/// translated (M2-104 D.1.5): some devices give those two values a meaning of
/// their own, and a translator that knows of it may keep a value that is not
/// the extreme away from them.
///
/// Up-scaling needs no such mode: a MIDI 1.0 value above 0 widens above 0 and
/// one below the maximum widens below it (M2-115 section 3.1), so
/// [`Translator`](crate::Translator) never lands a value on an extreme it was
/// not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ExtremeValues {
    /// The Default Translation: the plain shift of M2-115 section 3.4, so a
    /// value just above 0 narrows to 0 and one just below the maximum to the
    /// maximum. A Note On velocity is the one exception, and an exception in
    /// every mode: it never narrows to 0 (D.2.1).
    #[default]
    Scaled,
    /// A value above its minimum narrows to at least 1, and one below its
    /// maximum to at most the maximum less 1. The minimum and the maximum
    /// narrow to themselves. It covers every Min-Center-Max scaled value: a
    /// velocity, a pressure, a Control Change, a Pitch Bend and the data of a
    /// Registered or Assignable Controller that is scaled that way. D.1.5 says
    /// "some properties" and names none; this reads the Registered Controllers
    /// that M2-115 section 4.1 zero-extends (indices 0 to 31) as not among
    /// them, since they are fixed-point numbers or pairs of fields with no
    /// special extreme, and they narrow as in [`Scaled`](Self::Scaled).
    Avoided,
}

impl ExtremeValues {
    /// `voice` at MIDI 1.0 resolution as [`narrow`] gives it, with the extremes
    /// of this mode applied to its value.
    ///
    /// # Errors
    ///
    /// [`NoMidi1Form`] for a message with no single MIDI 1.0 message, as for
    /// [`narrow`].
    pub fn narrow(self, voice: Voice2) -> Result<Voice1, NoMidi1Form> {
        let narrowed = narrow(voice)?;
        if self == Self::Scaled {
            return Ok(narrowed);
        }
        let seven = |narrowed: U7, source: u32, source_bits: u32| {
            U7::masked(avoid(u32::from(narrowed.get()), source, source_bits, 7) as u8)
        };
        Ok(match (voice, narrowed) {
            (Voice2::NoteOn { velocity, .. }, Voice1::NoteOn { note, velocity: v }) => {
                Voice1::NoteOn {
                    note,
                    velocity: seven(v, u32::from(velocity.get()), 16),
                }
            }
            (Voice2::NoteOff { velocity, .. }, Voice1::NoteOff { note, velocity: v }) => {
                Voice1::NoteOff {
                    note,
                    velocity: seven(v, u32::from(velocity.get()), 16),
                }
            }
            (Voice2::PolyPressure { value, .. }, Voice1::PolyPressure { note, pressure }) => {
                Voice1::PolyPressure {
                    note,
                    pressure: seven(pressure, value.get(), 32),
                }
            }
            (
                Voice2::ControlChange { value, .. },
                Voice1::ControlChange {
                    controller,
                    value: v,
                },
            ) => Voice1::ControlChange {
                controller,
                value: seven(v, value.get(), 32),
            },
            (Voice2::ChannelPressure(value), Voice1::ChannelPressure(v)) => {
                Voice1::ChannelPressure(seven(v, value.get(), 32))
            }
            (Voice2::PitchBend(bend), Voice1::PitchBend(v)) => Voice1::PitchBend(U14::masked(
                avoid(u32::from(v.get()), bend.get(), 32, 14) as u16,
            )),
            (_, other) => other,
        })
    }
}

/// `narrowed`, the `dst_bits` wide narrowing of `source` (`source_bits` wide),
/// moved off the minimum when `source` is not 0 and off the maximum when
/// `source` is not its maximum (D.1.5).
pub(crate) fn avoid(narrowed: u32, source: u32, source_bits: u32, dst_bits: u32) -> u32 {
    let source_max = u64::MAX >> (64 - source_bits);
    let dst_max = (1u32 << dst_bits) - 1;
    if narrowed == 0 && source != 0 {
        1
    } else if narrowed == dst_max && u64::from(source) != source_max {
        dst_max - 1
    } else {
        narrowed
    }
}

/// What a translation does with the messages MIDI 1.0 has no equivalent for:
/// Per-Note Controllers, Per-Note Pitch Bend and Per-Note Management (M2-104
/// D.2.8), which the Default Translation does not address and an Alternate
/// Translation Mode may (D.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PerNoteTranslation {
    /// The Default Translation: they have no MIDI 1.0 message and send
    /// nothing.
    #[default]
    Dropped,
    /// MIDI Polyphonic Expression: each note takes a member channel of the
    /// zone, so a channel Pitch Bend, Channel Pressure and Control Change 74 on
    /// it act on that note alone. The zone's [specification](MpeZoneConfig::spec)
    /// says how the mode was selected: [`MpeSpec::Profile`](crate::MpeSpec::Profile)
    /// by the MIDI-CI MPE Profile (D.4.1), [`MpeSpec::Mpe11`](crate::MpeSpec::Mpe11)
    /// by other means such as the MPE Configuration Message (D.4.2).
    ///
    /// A [`Message::Midi1`](tutti_ump::Message::Midi1) input passes unchanged:
    /// its channel is the sender's, and a MIDI 1.0 note on a member channel is
    /// not seen by the allocator.
    Mpe(MpeZoneConfig),
}

/// The Default Translation, or the Alternate Translation Modes a
/// [`Narrower`](crate::Narrower) runs (M2-104 D.1.5 and D.4). The default is the
/// Default Translation; a product with a non-default mode tells its user it is
/// active ([`is_alternate`](Self::is_alternate) is the question to ask).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TranslationMode {
    /// How the smallest and largest values are narrowed.
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
