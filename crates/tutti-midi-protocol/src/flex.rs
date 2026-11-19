//! The Flex Data values the transport follows, as the project's unit types:
//! Set Tempo (M2-104 section 7.5.3) as a [`Bpm`] and Set Time Signature
//! (section 7.5.4) as a [`tutti_music::TimeSignature`]; [`flex_metronome`] is the
//! Set Metronome (section 7.5.5) of a click on every beat of a meter. [`tutti_ump::Flex`]
//! holds the wire's integers; this is the one reading of them as units.

use core::num::NonZeroU8;
use tutti_music::{BeatsPerBar, NoteValue};
use tutti_ump::{BarAccents, FlexTimeSignature, Metronome, System};
use tutti_units::{Bpm, Seconds};

/// 10-nanosecond units in one second: the Set Tempo field counts them per
/// quarter note (M2-104 section 7.5.3).
const TEN_NS_UNITS_PER_SECOND: f64 = 100_000_000.0;

/// The tempo a Set Tempo field declares: the field is 10 ns units per quarter
/// note (section 7.5.3), so a beat lasts `field / 1e8` seconds. `None` for `0`,
/// no valid tempo. The slowest the field holds is 1.39698 bpm.
///
/// ```
/// use tutti_midi_protocol::flex_tempo;
/// use tutti_units::Bpm;
///
/// // 0.5 s per quarter note is 120 bpm.
/// assert_eq!(flex_tempo(50_000_000), Bpm::new(120.0));
/// assert_eq!(flex_tempo(0), None);
/// ```
#[must_use]
pub fn flex_tempo(field: u32) -> Option<Bpm> {
    (field != 0)
        .then(|| Bpm::from_beat_period(Seconds::new(f64::from(field) / TEN_NS_UNITS_PER_SECOND)))
        .flatten()
}

/// The Set Tempo field that declares `bpm`: 10 ns units per quarter note,
/// the beat period in 10 ns units, rounded to the nearest unit, the inverse of [`flex_tempo`]. A
/// tempo too slow for the 32-bit field (under 1.39698 bpm) is the slowest the
/// field holds, and one too fast for a unit (over 6e9 bpm) is one unit.
///
/// ```
/// use tutti_midi_protocol::{flex_tempo, flex_tempo_field};
/// use tutti_units::Bpm;
///
/// let bpm = Bpm::new(120.0).unwrap();
/// assert_eq!(flex_tempo_field(bpm), 50_000_000);
/// assert_eq!(flex_tempo(flex_tempo_field(bpm)), Some(bpm));
/// ```
#[must_use]
pub fn flex_tempo_field(bpm: Bpm) -> u32 {
    // `as` saturates the float into the field; `max(1)` keeps a field that
    // `flex_tempo` reads as a tempo.
    (libm::round(bpm.beat_period().get() * TEN_NS_UNITS_PER_SECOND) as u32).max(1)
}

/// The meter a Set Time Signature message declares: the numerator field is the
/// beats per bar, the denominator field a negative power of 2 (2 is a quarter
/// note, 3 an eighth). `None` for what the project's meter cannot hold: a
/// numerator of 0 or above 64 ([`BeatsPerBar`]'s ceiling),
/// a denominator field of 0 (non-standard, section 7.5.4) or
/// one that names a note shorter than a 64th note ([`NoteValue`]'s ceiling).
///
/// The message's third field, the number of 1/32 notes in 24 MIDI Clocks, is
/// how the sender relates a quarter note to its notation and does not change
/// the meter; it is not read here.
///
/// ```
/// use tutti_midi_protocol::flex_time_signature;
/// use tutti_music::TimeSignature;
/// use tutti_ump::FlexTimeSignature as Wire;
///
/// let seven_eight = Wire { numerator: 7, denominator: 3, thirty_seconds: 8 };
/// assert_eq!(flex_time_signature(seven_eight), Some(TimeSignature::of(7, 8)));
/// ```
#[must_use]
pub fn flex_time_signature(wire: FlexTimeSignature) -> Option<tutti_music::TimeSignature> {
    let beats = BeatsPerBar::new(u32::from(wire.numerator))?;
    // The field is an exponent, so a shift past the word is no note value.
    let value = 1u32
        .checked_shl(u32::from(wire.denominator))
        .filter(|_| wire.denominator != 0)?;
    Some(tutti_music::TimeSignature::new(
        beats,
        NoteValue::new(value)?,
    ))
}

/// The Set Time Signature fields that declare `signature`, the inverse of
/// [`flex_time_signature`]: the numerator is the beats per bar, the denominator
/// the exponent of the note value (a quarter note is 2, an eighth 3), and the
/// 1/32 notes in 24 MIDI clocks is 8, the 32nds in a quarter note, which the
/// 24-per-quarter clock fixes (section 7.5.4). `None` for a whole-note
/// denominator: the exponent 0 it would need is non-standard (section 7.5.4),
/// and [`flex_time_signature`] reads it as no meter.
///
/// ```
/// use tutti_midi_protocol::{flex_time_signature, flex_time_signature_wire};
/// use tutti_music::TimeSignature;
///
/// let seven_eight = TimeSignature::of(7, 8);
/// let wire = flex_time_signature_wire(seven_eight).unwrap();
/// assert_eq!((wire.numerator, wire.denominator, wire.thirty_seconds), (7, 3, 8));
/// assert_eq!(flex_time_signature(wire), Some(seven_eight));
/// ```
#[must_use]
pub fn flex_time_signature_wire(
    signature: tutti_music::TimeSignature,
) -> Option<FlexTimeSignature> {
    let denominator = signature.note_value().get().trailing_zeros() as u8;
    (denominator != 0).then(|| FlexTimeSignature {
        // A numerator is at most 64 and a note value a power of two at most 64:
        // both fit the wire's bytes, and the exponent is at most 6.
        numerator: signature.beats_per_bar().get() as u8,
        denominator,
        thirty_seconds: 8,
    })
}

/// The Set Metronome fields of a metronome that clicks once on each beat of
/// `signature` and accents the bar's first beat: the primary click is a beat,
/// which is `4 * 24 / note value` MIDI clocks, the bar is undivided (Part 1 the
/// beats per bar, Parts 2 and 3 zero) and no subdivision clicks sound
/// (section 7.5.5). `None` for a note value whose beat is not a whole number of
/// clocks (a 64th note is a clock and a half): no Set Metronome has that form,
/// so none is sent and a receiver keeps the click it was last told. A whole
/// note has one (96 clocks), though its Set Time Signature does not
/// ([`flex_time_signature_wire`]).
///
/// ```
/// use tutti_midi_protocol::flex_metronome;
/// use tutti_music::TimeSignature;
///
/// let six_eight = flex_metronome(TimeSignature::of(6, 8)).unwrap();
/// assert_eq!(six_eight.clocks_per_click, 12);
/// assert_eq!(six_eight.accents.parts(), [6, 0, 0]);
/// assert_eq!(flex_metronome(TimeSignature::of(4, 64)), None);
/// ```
#[must_use]
pub fn flex_metronome(signature: tutti_music::TimeSignature) -> Option<Metronome> {
    let clocks = 4 * System::TIMING_CLOCKS_PER_QUARTER;
    let value = signature.note_value().get();
    if !clocks.is_multiple_of(value) {
        return None;
    }
    Some(Metronome {
        // At most 96, for a whole note.
        clocks_per_click: (clocks / value) as u8,
        // A bar has 1 to 64 beats, never zero.
        accents: BarAccents::Undivided(NonZeroU8::new(signature.beats_per_bar().get() as u8)?),
        subdivision_1: 0,
        subdivision_2: 0,
    })
}
