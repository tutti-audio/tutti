//! Notes in twelve-tone equal temperament.
//!
//! A note is a musical fact: C#4 exists whether or not MIDI does. [`Note`] can
//! name A9 or A-2, which have no MIDI note number, so no MIDI range limits what
//! it expresses. The MIDI note number encoding of a note lives in
//! `tutti_ump::NoteNumber`, as its `From` and `TryFrom` conversions.

use alloc::format;
use alloc::string::String;
use tutti_units::{Cents, FrequencyRatio, Hz, Semitones};

/// One of the twelve pitch classes.
///
/// Ordered C-first, and spelled with sharps throughout; the flat spelling is
/// [`flat_name`](Self::flat_name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum PitchClass {
    /// C — the origin of the octave numbering.
    C,
    /// C sharp, enharmonically D flat.
    CSharp,
    /// D.
    D,
    /// D sharp, enharmonically E flat.
    DSharp,
    /// E.
    E,
    /// F.
    F,
    /// F sharp, enharmonically G flat.
    FSharp,
    /// G.
    G,
    /// G sharp, enharmonically A flat.
    GSharp,
    /// A — A4 is the 440 Hz tuning reference.
    A,
    /// A sharp, enharmonically B flat.
    ASharp,
    /// B.
    B,
}

impl PitchClass {
    /// All twelve classes, in ascending order from C.
    pub const ALL: [PitchClass; 12] = [
        Self::C,
        Self::CSharp,
        Self::D,
        Self::DSharp,
        Self::E,
        Self::F,
        Self::FSharp,
        Self::G,
        Self::GSharp,
        Self::A,
        Self::ASharp,
        Self::B,
    ];

    /// Returns the semitones above C, in `0..12`.
    #[inline]
    #[must_use]
    pub const fn semitones_above_c(self) -> u8 {
        self as u8
    }

    /// Returns the class `semitones` above C, wrapping every 12.
    #[inline]
    #[must_use]
    pub const fn from_semitones_above_c(semitones: u8) -> Self {
        Self::ALL[(semitones % 12) as usize]
    }

    /// Returns the sharp spelling: `C`, `C#`, `D`, …
    #[inline]
    #[must_use]
    pub const fn sharp_name(self) -> &'static str {
        match self {
            Self::C => "C",
            Self::CSharp => "C#",
            Self::D => "D",
            Self::DSharp => "D#",
            Self::E => "E",
            Self::F => "F",
            Self::FSharp => "F#",
            Self::G => "G",
            Self::GSharp => "G#",
            Self::A => "A",
            Self::ASharp => "A#",
            Self::B => "B",
        }
    }

    /// Returns the flat spelling: `C`, `Db`, `D`, … The same twelve pitches,
    /// respelled.
    #[inline]
    #[must_use]
    pub const fn flat_name(self) -> &'static str {
        match self {
            Self::C => "C",
            Self::CSharp => "Db",
            Self::D => "D",
            Self::DSharp => "Eb",
            Self::E => "E",
            Self::F => "F",
            Self::FSharp => "Gb",
            Self::G => "G",
            Self::GSharp => "Ab",
            Self::A => "A",
            Self::ASharp => "Bb",
            Self::B => "B",
        }
    }

    /// Returns whether this class is spelled with an accidental.
    #[inline]
    #[must_use]
    pub const fn is_accidental(self) -> bool {
        matches!(
            self,
            Self::CSharp | Self::DSharp | Self::FSharp | Self::GSharp | Self::ASharp
        )
    }
}

/// A note: a pitch class in a numbered octave.
///
/// Octaves use scientific pitch notation, where middle C is C4 and A4 is
/// 440 Hz. This type names no MIDI: the MIDI note number encoding of a note is
/// `tutti_ump::NoteNumber`'s `From` and `TryFrom` conversions.
///
/// # Examples
///
/// ```
/// use tutti_units::{Hz, Semitones};
/// use tutti_music::{Note, PitchClass};
///
/// let a4 = Note::A4;
/// assert_eq!(a4.frequency(), Hz::new(440.0));
///
/// let c_sharp_5 = a4.transpose(Semitones::new(4.0));
/// assert_eq!(c_sharp_5.class(), PitchClass::CSharp);
/// assert_eq!(c_sharp_5.flat_label(), "Db5");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Note {
    class: PitchClass,
    octave: i8,
}

impl Note {
    /// Concert A — 440 Hz by definition.
    pub const A4: Note = Note {
        class: PitchClass::A,
        octave: 4,
    };

    /// Middle C.
    pub const C4: Note = Note {
        class: PitchClass::C,
        octave: 4,
    };

    /// Builds a note from its class and octave.
    ///
    /// Any `i8` octave is accepted, including ones with no MIDI note number.
    #[inline]
    #[must_use]
    pub const fn new(class: PitchClass, octave: i8) -> Self {
        Self { class, octave }
    }

    /// Returns the pitch class, discarding the octave.
    #[inline]
    #[must_use]
    pub const fn class(self) -> PitchClass {
        self.class
    }

    /// Returns the octave in scientific pitch notation, where middle C is 4.
    #[inline]
    #[must_use]
    pub const fn octave(self) -> i8 {
        self.octave
    }

    /// Returns the semitones from C0, which may be negative for sub-zero
    /// octaves.
    #[inline]
    #[must_use]
    pub const fn semitones_from_c0(self) -> i32 {
        self.octave as i32 * 12 + self.class.semitones_above_c() as i32
    }

    /// Returns the note `semitones` above C0 (below it when negative).
    #[inline]
    #[must_use]
    pub fn from_semitones_from_c0(semitones: i32) -> Self {
        // `rem_euclid`, not `%`: a negative index must wrap into the octave
        // below rather than reflecting to a negative pitch class.
        Self {
            class: PitchClass::from_semitones_above_c(semitones.rem_euclid(12) as u8),
            octave: semitones.div_euclid(12) as i8,
        }
    }

    /// Returns the interval from `self` up to `other` (negative when `other`
    /// is lower).
    #[inline]
    #[must_use]
    pub fn interval_to(self, other: Note) -> Semitones {
        Semitones::new(f64::from(
            other.semitones_from_c0() - self.semitones_from_c0(),
        ))
    }

    /// Returns the note a whole number of semitones away. Fractional input is
    /// truncated —
    /// a note is an unassigned pitch, so use [`frequency`](Self::frequency) and
    /// detune from there when you want the space between two notes.
    #[inline]
    #[must_use]
    pub fn transpose(self, by: Semitones) -> Self {
        Self::from_semitones_from_c0(self.semitones_from_c0() + by.get() as i32)
    }

    /// Returns the frequency in A440 twelve-tone equal temperament.
    ///
    /// This is the plain default. Alternative temperaments — just intonation,
    /// Pythagorean, meantone — are a tuning table's job, not a note's.
    #[inline]
    #[must_use]
    pub fn frequency(self) -> Hz {
        let steps = f64::from(self.semitones_from_c0() - Self::A4.semitones_from_c0());
        Self::A4_HZ * Semitones::new(steps).to_pitch_ratio()
    }

    /// Returns the nearest note to `freq`, and how far off it is.
    ///
    /// The offset is signed and lands in −50..=+50 cents: past that, a
    /// different note is nearer. `None` for a frequency that is not finite and
    /// above zero.
    pub fn nearest_to(freq: Hz) -> Option<(Self, Cents)> {
        // The named inverse of `frequency`'s `Semitones::to_pitch_ratio`, not a
        // hand-rolled `12.0 * ratio.log2()`: both directions go through the one
        // converter. A frequency whose ratio is not a `FrequencyRatio` has no
        // nearest note.
        let ratio = FrequencyRatio::new(freq.get() / Self::A4_HZ.get())?;
        let steps = Semitones::from_pitch_ratio(ratio);
        let nearest = libm::round(steps.get());
        let note = Self::from_semitones_from_c0(Self::A4.semitones_from_c0() + nearest as i32);
        Some((note, Semitones::new(steps.get() - nearest).to_cents()))
    }

    /// Returns the sharp spelling with octave: `A4`, `C#5`. Same as `Display`.
    #[must_use]
    pub fn sharp_label(self) -> String {
        format!("{}{}", self.class.sharp_name(), self.octave)
    }

    /// Returns the flat spelling with octave: `A4`, `Db5`.
    #[must_use]
    pub fn flat_label(self) -> String {
        format!("{}{}", self.class.flat_name(), self.octave)
    }

    /// Concert A's frequency. The one tuning constant this module carries.
    const A4_HZ: Hz = Hz::new(440.0);
}

impl core::fmt::Display for Note {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}{}", self.class.sharp_name(), self.octave)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a4_is_440_hz_by_definition() {
        assert!((Note::A4.frequency().get() - 440.0).abs() < 1e-3);
        assert_eq!(Note::A4.sharp_label(), "A4");
    }

    #[test]
    fn an_octave_doubles_the_frequency() {
        let a5 = Note::A4.transpose(Semitones::OCTAVE);
        assert_eq!(a5, Note::new(PitchClass::A, 5));
        assert!((a5.frequency().get() - 880.0).abs() < 1e-2);

        let a3 = Note::A4.transpose(Semitones::new(-12.0));
        assert!((a3.frequency().get() - 220.0).abs() < 1e-2);
    }

    #[test]
    fn the_two_spellings_name_the_same_pitch() {
        let cs4 = Note::new(PitchClass::CSharp, 4);
        assert_eq!(cs4.sharp_label(), "C#4");
        assert_eq!(cs4.flat_label(), "Db4");
        assert!(cs4.class().is_accidental());
        assert!(!Note::C4.class().is_accidental());
    }

    #[test]
    fn nearest_note_reports_a_signed_offset() {
        let (note, cents) = Note::nearest_to(Hz::new(440.0)).unwrap();
        assert_eq!(note, Note::A4);
        assert!(cents.get().abs() < 1e-3);

        // Slightly sharp of A4.
        let (note, cents) = Note::nearest_to(Hz::new(445.0)).unwrap();
        assert_eq!(note, Note::A4);
        assert!(cents.get() > 0.0 && cents.get() < 50.0);

        // Slightly flat.
        let (note, cents) = Note::nearest_to(Hz::new(435.0)).unwrap();
        assert_eq!(note, Note::A4);
        assert!(cents.get() < 0.0 && cents.get() > -50.0);

        assert!(Note::nearest_to(Hz::new(0.0)).is_none());
        assert!(Note::nearest_to(Hz::new(-1.0)).is_none());
    }

    #[test]
    fn every_note_round_trips_through_its_own_frequency() {
        for semitones in -12..120 {
            let note = Note::from_semitones_from_c0(semitones);
            let (back, cents) = Note::nearest_to(note.frequency()).unwrap();
            assert_eq!(back, note, "{note} did not round trip");
            assert!(cents.get().abs() < 0.01, "{note} drifted {cents}");
        }
    }

    /// Sub-zero octaves must wrap down, not reflect — `%` would give a
    /// negative pitch class.
    #[test]
    fn negative_octaves_wrap_downward() {
        let b_minus_1 = Note::from_semitones_from_c0(-1);
        assert_eq!(b_minus_1.class(), PitchClass::B);
        assert_eq!(b_minus_1.octave(), -1);

        let c_minus_1 = Note::from_semitones_from_c0(-12);
        assert_eq!(c_minus_1.class(), PitchClass::C);
        assert_eq!(c_minus_1.octave(), -1);
    }

    #[test]
    fn intervals_are_signed_and_symmetric() {
        let c4 = Note::C4;
        let g4 = Note::new(PitchClass::G, 4);

        assert_eq!(c4.interval_to(g4), Semitones::new(7.0));
        assert_eq!(g4.interval_to(c4), Semitones::new(-7.0));
        assert_eq!(c4.interval_to(c4), Semitones::UNISON);
    }

    #[test]
    fn pitch_classes_enumerate_in_order() {
        assert_eq!(PitchClass::ALL.len(), 12);
        for (i, class) in PitchClass::ALL.iter().enumerate() {
            assert_eq!(class.semitones_above_c(), i as u8);
            assert_eq!(PitchClass::from_semitones_above_c(i as u8), *class);
        }
        // Wraps at the octave.
        assert_eq!(PitchClass::from_semitones_above_c(12), PitchClass::C);
        assert_eq!(PitchClass::from_semitones_above_c(13), PitchClass::CSharp);
    }
}
