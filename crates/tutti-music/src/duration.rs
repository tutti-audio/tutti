//! [`NoteDuration`]: how long a written note lasts, as a note value with a modifier.

use tutti_units::BeatDuration;

use crate::NoteValue;

/// What a note value is lengthened or shortened by.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum NoteModifier {
    /// The note value as written.
    #[default]
    Plain,
    /// A dot: one and a half times the note value (3/2).
    Dotted,
    /// Three in the time of two: two thirds of the note value (2/3).
    Triplet,
}

/// The duration of a written note: a [`NoteValue`] (whole to sixty-fourth) with a
/// [`NoteModifier`], as a tempo-synced delay or LFO names its time.
///
/// Every pair is a duration a score can write, so no value is refused: a third of a
/// beat is a triplet eighth, and a fraction a note cannot be (1/3 of a whole note) is not
/// a `NoteDuration`. A quarter note is one beat ([`QUARTER`](Self::QUARTER)); the beats
/// of any other are [`to_beat_duration`](Self::to_beat_duration).
///
/// ```
/// use tutti_music::{NoteDuration, NoteModifier, NoteValue};
/// use tutti_units::BeatDuration;
///
/// let dotted_eighth = NoteDuration::new(NoteValue::EIGHTH, NoteModifier::Dotted);
/// assert_eq!(dotted_eighth.to_beat_duration(), BeatDuration::new(0.75));
/// ```
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct NoteDuration {
    value: NoteValue,
    modifier: NoteModifier,
}

impl NoteDuration {
    /// A quarter note, one beat.
    pub const QUARTER: Self = Self::new(NoteValue::QUARTER, NoteModifier::Plain);

    /// The note `value` under `modifier`.
    #[inline]
    #[must_use]
    pub const fn new(value: NoteValue, modifier: NoteModifier) -> Self {
        Self { value, modifier }
    }

    /// The note value as written.
    #[inline]
    #[must_use]
    pub const fn value(self) -> NoteValue {
        self.value
    }

    /// What the note value is modified by.
    #[inline]
    #[must_use]
    pub const fn modifier(self) -> NoteModifier {
        self.modifier
    }

    /// This duration in beats, a quarter note being one: `4 / value`, times 3/2 for a dot
    /// and 2/3 for a triplet. A whole note is 4 beats and a plain sixty-fourth 1/16.
    ///
    /// The beats are one division of exact integers, `4·num / (value·den)` for the
    /// modifier's ratio `num/den`, so each is the correctly rounded fraction: a quarter
    /// triplet is exactly `2.0 / 3.0` and a sixty-fourth triplet `1.0 / 24.0`.
    #[inline]
    #[must_use]
    pub fn to_beat_duration(self) -> BeatDuration {
        let (num, den) = match self.modifier {
            NoteModifier::Plain => (1, 1),
            NoteModifier::Dotted => (3, 2),
            NoteModifier::Triplet => (2, 3),
        };
        BeatDuration::new(f64::from(4 * num) / f64::from(self.value.get() * den))
    }
}

impl From<NoteDuration> for BeatDuration {
    /// The note's beats ([`NoteDuration::to_beat_duration`]).
    #[inline]
    fn from(note: NoteDuration) -> Self {
        note.to_beat_duration()
    }
}

impl From<NoteValue> for NoteDuration {
    /// The note value, unmodified.
    #[inline]
    fn from(value: NoteValue) -> Self {
        Self::new(value, NoteModifier::Plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The beats of a written note are the score's: a quarter is 1, a whole 4, a dotted
    /// quarter 3/2, a triplet eighth 1/3, a dotted eighth 3/4, a sixty-fourth 1/16.
    #[test]
    fn written_notes_have_their_beats() {
        let beats = |v: NoteValue, m| NoteDuration::new(v, m).to_beat_duration().get();
        assert_eq!(beats(NoteValue::QUARTER, NoteModifier::Plain), 1.0);
        assert_eq!(beats(NoteValue::WHOLE, NoteModifier::Plain), 4.0);
        assert_eq!(beats(NoteValue::HALF, NoteModifier::Plain), 2.0);
        assert_eq!(beats(NoteValue::QUARTER, NoteModifier::Dotted), 1.5);
        assert_eq!(beats(NoteValue::EIGHTH, NoteModifier::Dotted), 0.75);
        assert_eq!(beats(NoteValue::EIGHTH, NoteModifier::Plain), 0.5);
        assert_eq!(beats(NoteValue::of(64), NoteModifier::Plain), 1.0 / 16.0);
        assert_eq!(beats(NoteValue::EIGHTH, NoteModifier::Triplet), 1.0 / 3.0);
        assert_eq!(beats(NoteValue::QUARTER, NoteModifier::Triplet), 2.0 / 3.0);
    }

    /// The beats are the score's fractions exactly, as `f64` rounds them, including the
    /// ones a multiplication order could break: a quarter is 1, a dotted eighth 3/4, a
    /// quarter triplet 2/3, a whole 4 and a sixty-fourth triplet 1/24. `From` agrees with
    /// `to_beat_duration` on every pair.
    #[test]
    fn the_beats_are_the_exact_fractions_and_from_agrees() {
        let cases = [
            (NoteValue::QUARTER, NoteModifier::Plain, 1.0),
            (NoteValue::EIGHTH, NoteModifier::Dotted, 3.0 / 4.0),
            (NoteValue::QUARTER, NoteModifier::Triplet, 2.0 / 3.0),
            (NoteValue::WHOLE, NoteModifier::Plain, 4.0),
            (NoteValue::of(64), NoteModifier::Triplet, 1.0 / 24.0),
        ];
        for (value, modifier, beats) in cases {
            let note = NoteDuration::new(value, modifier);
            assert_eq!(
                note.to_beat_duration(),
                BeatDuration::new(beats),
                "{note:?}"
            );
        }
        for exponent in 0..=6 {
            for modifier in [
                NoteModifier::Plain,
                NoteModifier::Dotted,
                NoteModifier::Triplet,
            ] {
                let note = NoteDuration::new(NoteValue::of(1 << exponent), modifier);
                assert_eq!(BeatDuration::from(note), note.to_beat_duration());
            }
        }
    }

    /// Three triplet notes fill the time of two plain ones and a dotted note is a plain
    /// one and its half, at every note value: the ratios 2/3 and 3/2 hold across 1/1 to 1/64.
    #[test]
    fn the_modifiers_scale_every_value_by_two_thirds_and_three_halves() {
        for exponent in 0..=6 {
            let value = NoteValue::of(1 << exponent);
            let plain = NoteDuration::from(value).to_beat_duration().get();
            let dotted = NoteDuration::new(value, NoteModifier::Dotted).to_beat_duration();
            let triplet = NoteDuration::new(value, NoteModifier::Triplet).to_beat_duration();
            assert_eq!(
                dotted.get(),
                plain + plain / 2.0,
                "dotted 1/{}",
                1 << exponent
            );
            assert!(
                (3.0 * triplet.get() - 2.0 * plain).abs() < 1e-15,
                "triplet 1/{}",
                1 << exponent
            );
        }
    }
}
