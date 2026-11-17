//! The Flex Data values the transport follows, as the project's unit types:
//! Set Tempo (M2-104 section 7.5.3) as a [`Bpm`] and Set Time Signature
//! (section 7.5.4) as a [`tutti_types::TimeSignature`]. [`tutti_ump::Flex`]
//! holds the wire's integers and depends on no unit crate; this is the one
//! reading of them.

use tutti_types::{BeatsPerBar, Bpm, NoteValue};
use tutti_ump::FlexTimeSignature;

/// 10-nanosecond units in one minute: 60 s is 6e9 of them, so a tempo of
/// `bpm` is `6e9 / bpm` units per quarter note.
const TEN_NS_UNITS_PER_MINUTE: f64 = 6_000_000_000.0;

/// The tempo a Set Tempo field declares: the field is 10 ns units per quarter
/// note (section 7.5.3), so `6e9 / field` beats per minute. `None` for `0`, no
/// valid tempo. The slowest the field holds is 1.39698 bpm.
///
/// ```
/// use tutti_midi_protocol::flex_tempo;
/// use tutti_types::Bpm;
///
/// // 0.5 s per quarter note is 120 bpm.
/// assert_eq!(flex_tempo(50_000_000), Bpm::new(120.0));
/// assert_eq!(flex_tempo(0), None);
/// ```
#[must_use]
pub fn flex_tempo(field: u32) -> Option<Bpm> {
    (field != 0)
        .then(|| Bpm::new(TEN_NS_UNITS_PER_MINUTE / f64::from(field)))
        .flatten()
}

/// The Set Tempo field that declares `bpm`: 10 ns units per quarter note,
/// `6e9 / bpm` rounded to the nearest unit, the inverse of [`flex_tempo`]. A
/// tempo too slow for the 32-bit field (under 1.39698 bpm) is the slowest the
/// field holds, and one too fast for a unit (over 6e9 bpm) is one unit.
///
/// ```
/// use tutti_midi_protocol::{flex_tempo, flex_tempo_field};
/// use tutti_types::Bpm;
///
/// let bpm = Bpm::new(120.0).unwrap();
/// assert_eq!(flex_tempo_field(bpm), 50_000_000);
/// assert_eq!(flex_tempo(flex_tempo_field(bpm)), Some(bpm));
/// ```
#[must_use]
pub fn flex_tempo_field(bpm: Bpm) -> u32 {
    // `as` saturates the float into the field; `max(1)` keeps a field that
    // `flex_tempo` reads as a tempo.
    (libm::round(TEN_NS_UNITS_PER_MINUTE / bpm.get()) as u32).max(1)
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
/// use tutti_types::TimeSignature;
/// use tutti_ump::FlexTimeSignature as Wire;
///
/// let seven_eight = Wire { numerator: 7, denominator: 3, thirty_seconds: 8 };
/// assert_eq!(flex_time_signature(seven_eight), Some(TimeSignature::of(7, 8)));
/// ```
#[must_use]
pub fn flex_time_signature(wire: FlexTimeSignature) -> Option<tutti_types::TimeSignature> {
    let beats = BeatsPerBar::new(u32::from(wire.numerator))?;
    // The field is an exponent, so a shift past the word is no note value.
    let value = 1u32
        .checked_shl(u32::from(wire.denominator))
        .filter(|_| wire.denominator != 0)?;
    Some(tutti_types::TimeSignature::new(
        beats,
        NoteValue::new(value)?,
    ))
}

/// The Set Time Signature fields that declare `signature`, the inverse of
/// [`flex_time_signature`]: the numerator is the beats per bar, the denominator
/// the exponent of the note value (a quarter note is 2, an eighth 3), and the
/// 1/32 notes in 24 MIDI clocks is 8, the 32nds in a quarter note, which the
/// 24-per-quarter clock fixes (section 7.5.4).
///
/// ```
/// use tutti_midi_protocol::{flex_time_signature, flex_time_signature_wire};
/// use tutti_types::TimeSignature;
///
/// let seven_eight = TimeSignature::of(7, 8);
/// let wire = flex_time_signature_wire(seven_eight);
/// assert_eq!((wire.numerator, wire.denominator, wire.thirty_seconds), (7, 3, 8));
/// assert_eq!(flex_time_signature(wire), Some(seven_eight));
/// ```
#[must_use]
pub fn flex_time_signature_wire(signature: tutti_types::TimeSignature) -> FlexTimeSignature {
    FlexTimeSignature {
        // A numerator is at most 64 and a note value a power of two at most 64:
        // both fit the wire's bytes, and the exponent is at most 6.
        numerator: signature.beats_per_bar().get() as u8,
        denominator: signature.note_value().get().trailing_zeros() as u8,
        thirty_seconds: 8,
    }
}
