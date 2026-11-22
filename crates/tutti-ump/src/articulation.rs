//! The Note On Orchestral Articulation Profile's fields (M2-123), carried in a
//! MIDI 2.0 Note On's attribute.
//!
//! While the Profile is enabled, a Note On's attribute type `0x10` to `0x1F` is
//! the Classification of the articulation, and the 16-bit attribute data is
//! the Subclass, Variation, stroke direction, round-robin reset and string
//! (M2-123 §4.1, Figure 1). [`NoteArticulation`] is that Note On, with every
//! field typed so a value the specification reserves cannot be written;
//! [`NoteArticulation::from_attribute`] refuses one on the wire with an
//! [`ArticulationError`]. [`NoteOffArticulation`] is the Note Off's side
//! (§7, Figure 2). Whether the Profile is enabled is a MIDI-CI matter, not
//! this module's.

use crate::{Attribute, NoteNumber, NoteVelocity, U4, Voice2};

/// Declares a Subclass table: the enum of the rows the table names, in the
/// order of the Subclass value, and the conversions to and from the 4-bit
/// Subclass. A Subclass the table leaves reserved has no variant.
macro_rules! subclasses {
    (
        $(#[$meta:meta])*
        $name:ident { $($(#[$vmeta:meta])* $variant:ident = $value:literal,)+ }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)+
        }

        impl $name {
            /// Every row of the table, in the order of the Subclass value.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            /// The 4-bit Subclass value of the row.
            #[must_use]
            pub const fn subclass(self) -> U4 {
                match self {
                    $(Self::$variant => U4::of($value),)+
                }
            }

            /// The row with this Subclass value, or `None` when the table
            /// reserves it.
            #[must_use]
            pub const fn from_subclass(subclass: U4) -> Option<Self> {
                match subclass.get() {
                    $($value => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

subclasses! {
    /// The Subclasses of Classification `0x10`, Core Sounds - Sustains and
    /// Strikes (M2-123 §5.2.1, Table 8).
    CoreSound {
        /// `0x0`: Normal Sustains and Strikes, part 1. With part 2 it holds up
        /// to 32 Variations of the default sound.
        NormalPart1 = 0x0,
        /// `0x1`: Normal Sustains and Strikes, part 2.
        NormalPart2 = 0x1,
        /// `0x2`: Legato and Legato Slurred.
        Legato = 0x2,
        /// `0x3`: Molto Legato.
        MoltoLegato = 0x3,
        /// `0x4`: Glissando.
        Glissando = 0x4,
        /// `0x5`: Detache.
        Detache = 0x5,
        /// `0x6`: Marcato.
        Marcato = 0x6,
        /// `0x7`: Martele.
        Martele = 0x7,
        /// `0x8`: Senza Vibrato.
        SenzaVibrato = 0x8,
        /// `0x9`: Con Vibrato.
        ConVibrato = 0x9,
        /// `0xA`: Synchronized Vibrato.
        SynchronizedVibrato = 0xA,
    }
}

subclasses! {
    /// The Subclasses of Classification `0x11`, Staccatos and Shorts (M2-123
    /// §5.2.2, Table 9).
    ShortSound {
        /// `0x0`: Normal Staccato Off String.
        NormalStaccatoOffString = 0x0,
        /// `0x1`: Normal Staccato On String.
        NormalStaccatoOnString = 0x1,
        /// `0x2`: Slurred Staccato.
        SlurredStaccato = 0x2,
        /// `0x3`: Accented Staccato.
        AccentedStaccato = 0x3,
        /// `0x4`: Staccatissimo.
        Staccatissimo = 0x4,
        /// `0x5`: Spiccato.
        Spiccato = 0x5,
        /// `0x6`: Sautille.
        Sautille = 0x6,
        /// `0x7`: Martellato.
        Martellato = 0x7,
        /// `0x8`: Long Staccato.
        LongStaccato = 0x8,
        /// `0x9`: Portato.
        Portato = 0x9,
        /// `0xA`: Pizzicato.
        Pizzicato = 0xA,
        /// `0xB`: Bartok Pizzicato.
        BartokPizzicato = 0xB,
        /// `0xC`: Col Legno Battuto.
        ColLegnoBattuto = 0xC,
        /// `0xD`: Col Legno Gestrichen.
        ColLegnoGestrichen = 0xD,
        /// `0xE`: String Hand Tap.
        StringHandTap = 0xE,
        /// `0xF`: Jete.
        Jete = 0xF,
    }
}

subclasses! {
    /// The Subclasses of Classification `0x12`, Same Note Trills/Repeats
    /// (M2-123 §5.2.3, Table 10).
    SameNoteSound {
        /// `0x0`: Tremolo or Flutter Tongue.
        Tremolo = 0x0,
        /// `0x1`: Growl or Razz.
        Growl = 0x1,
        /// `0x2`: Other Coloristic Tremolo.
        OtherColoristicTremolo = 0x2,
        /// `0x3`: One Note Trills.
        OneNoteTrill = 0x3,
        /// `0x4`: 2 Repeats.
        Repeats2 = 0x4,
        /// `0x5`: 3 Repeats.
        Repeats3 = 0x5,
        /// `0x6`: 4 Repeats.
        Repeats4 = 0x6,
        /// `0x7`: 5 Repeats.
        Repeats5 = 0x7,
        /// `0x8`: 6 Repeats.
        Repeats6 = 0x8,
        /// `0x9`: Faster Repeats.
        FasterRepeats = 0x9,
    }
}

subclasses! {
    /// The Subclasses of Classification `0x13`, Intervallic Trills (M2-123
    /// §5.2.4, Table 11): the interval the trill alternates over, and for the
    /// two smallest whether it starts on the root (Classical) or the note above
    /// (Baroque).
    IntervallicTrill {
        /// `0x0`: Half Step (Classical).
        HalfStepClassical = 0x0,
        /// `0x1`: Half Step (Baroque).
        HalfStepBaroque = 0x1,
        /// `0x2`: Whole Step (Classical).
        WholeStepClassical = 0x2,
        /// `0x3`: Whole Step (Baroque).
        WholeStepBaroque = 0x3,
        /// `0x4`: Minor 3rd.
        MinorThird = 0x4,
        /// `0x5`: Major 3rd.
        MajorThird = 0x5,
        /// `0x6`: Perfect 4th.
        PerfectFourth = 0x6,
        /// `0x7`: Tritone.
        Tritone = 0x7,
        /// `0x8`: Perfect 5th.
        PerfectFifth = 0x8,
        /// `0x9`: Minor 6th.
        MinorSixth = 0x9,
        /// `0xA`: Major 6th.
        MajorSixth = 0xA,
        /// `0xB`: Minor 7th.
        MinorSeventh = 0xB,
        /// `0xC`: Major 7th.
        MajorSeventh = 0xC,
        /// `0xD`: Octave.
        Octave = 0xD,
    }
}

subclasses! {
    /// The Subclasses of Classification `0x14`, Additional Colors - Sustained
    /// (M2-123 §5.2.5, Table 12).
    ColorSound {
        /// `0x0`: Harmonics, Natural.
        HarmonicsNatural = 0x0,
        /// `0x1`: Harmonics, Artificial.
        HarmonicsArtificial = 0x1,
        /// `0x2`: Col Legno Tratto.
        ColLegnoTratto = 0x2,
        /// `0x3`: Flautando.
        Flautando = 0x3,
        /// `0x4`: Polyphony, multiple octaves.
        PolyphonyOctaves = 0x4,
        /// `0x5`: Polyphony, intervals, chords and so on.
        PolyphonyChords = 0x5,
        /// `0x6`: Cuivre.
        Cuivre = 0x6,
        /// `0x7`: Lontano.
        Lontano = 0x7,
        /// `0x8`: Singing into Instrument.
        SingingIntoInstrument = 0x8,
    }
}

subclasses! {
    /// The Subclasses of Classification `0x15`, Pitch and Dynamic Gestures
    /// (M2-123 §5.2.6, Table 13).
    GestureSound {
        /// `0x0`: Pitch Fall End.
        PitchFallEnd = 0x0,
        /// `0x1`: Pitch Fall Start.
        PitchFallStart = 0x1,
        /// `0x2`: Pitch Rise End.
        PitchRiseEnd = 0x2,
        /// `0x3`: Pitch Rise Start.
        PitchRiseStart = 0x3,
        /// `0x4`: Blue Note Down.
        BlueNoteDown = 0x4,
        /// `0x5`: Blue Note Up.
        BlueNoteUp = 0x5,
        /// `0x6`: Grace Notes, Classical (starts below).
        GraceNotesClassical = 0x6,
        /// `0x7`: Grace Notes, Baroque (starts above).
        GraceNotesBaroque = 0x7,
        /// `0x8`: Shakes.
        Shakes = 0x8,
        /// `0x9`: Crescendo.
        Crescendo = 0x9,
        /// `0xA`: Decrescendo.
        Decrescendo = 0xA,
        /// `0xB`: Crescendo to Decrescendo.
        CrescendoDecrescendo = 0xB,
        /// `0xC`: Decrescendo to Crescendo.
        DecrescendoCrescendo = 0xC,
        /// `0xD`: Sfz Crescendo.
        SfzCrescendo = 0xD,
    }
}

subclasses! {
    /// The Subclasses of Classification `0x16`, Scales, Runs and Arpeggios
    /// (M2-123 §5.2.7, Table 14).
    RunSound {
        /// `0x0`: Playable Runs.
        PlayableRuns = 0x0,
        /// `0x1`: Playable Tremolos.
        PlayableTremolos = 0x1,
        /// `0x2`: Playable Glissando.
        PlayableGlissando = 0x2,
        /// `0x3`: Scales/Runs Major.
        Major = 0x3,
        /// `0x4`: Scales/Runs Minor.
        Minor = 0x4,
        /// `0x5`: Scales/Runs Dominant 7th.
        Dominant7th = 0x5,
        /// `0x6`: Scales/Runs diminished (whole/half) or Other 1.
        DiminishedWholeHalf = 0x6,
        /// `0x7`: Scales/Runs diminished (half/whole) or Other 2.
        DiminishedHalfWhole = 0x7,
        /// `0x8`: Whole Tone Scale 1 (includes C).
        WholeToneOnC = 0x8,
        /// `0x9`: Whole Tone Scale 2 (includes C sharp).
        WholeToneOnCSharp = 0x9,
        /// `0xA`: Pentatonic Scale (major).
        PentatonicMajor = 0xA,
        /// `0xB`: Pentatonic Scale (minor).
        PentatonicMinor = 0xB,
        /// `0xC`: Lydian.
        Lydian = 0xC,
        /// `0xD`: Lydian flat 7.
        LydianFlat7 = 0xD,
        /// `0xE`: Chromatic.
        Chromatic = 0xE,
        /// `0xF`: Other Scales and Runs.
        Other = 0xF,
    }
}

subclasses! {
    /// The Subclasses of Classification `0x17`, Effects and Noises (M2-123
    /// §5.2.8, Table 15).
    EffectSound {
        /// `0x0`: Assorted noises, Air Sound or effects, sustained.
        AssortedSustained = 0x0,
        /// `0x1`: Behind the Bridge, sustained.
        BehindTheBridge = 0x1,
        /// `0x2`: Random Pizz, sustained.
        RandomPizzicato = 0x2,
        /// `0x3`: Harmonic Glissando.
        HarmonicGlissando = 0x3,
        /// `0x4`: Random Glissando.
        RandomGlissando = 0x4,
        /// `0x5`: Pizz, short.
        ShortPizzicato = 0x5,
        /// `0x6`: Mechanical sounds, sustained.
        MechanicalSustained = 0x6,
        /// `0x7`: Mechanical sounds, short.
        MechanicalShort = 0x7,
        /// `0x8`: Finger Glide.
        FingerGlide = 0x8,
        /// `0x9`: Fret Buzz or Noise.
        FretBuzz = 0x9,
        /// `0xA`: Thump.
        Thump = 0xA,
        /// `0xB`: Knock.
        Knock = 0xB,
        /// `0xC`: Slap.
        Slap = 0xC,
        /// `0xD`: Pop.
        Pop = 0xD,
        /// `0xE`: Tap.
        Tap = 0xE,
        /// `0xF`: Click.
        Click = 0xF,
    }
}

/// One of the six Classifications `0x1A` to `0x1F` for custom, library or device
/// specific sounds (M2-123 §5.2.10), whose Subclass the manufacturer indexes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CustomClassification {
    /// `0x1A`, Classification 11.
    C1A,
    /// `0x1B`, Classification 12.
    C1B,
    /// `0x1C`, Classification 13.
    C1C,
    /// `0x1D`, Classification 14.
    C1D,
    /// `0x1E`, Classification 15.
    C1E,
    /// `0x1F`, Classification 16.
    C1F,
}

impl CustomClassification {
    /// Every custom Classification, in the order of the attribute type.
    pub const ALL: &'static [Self] = &[
        Self::C1A,
        Self::C1B,
        Self::C1C,
        Self::C1D,
        Self::C1E,
        Self::C1F,
    ];

    /// The attribute type, `0x1A` to `0x1F`.
    #[must_use]
    pub const fn attribute_type(self) -> u8 {
        0x1A + self as u8
    }

    /// The Classification for an attribute type, or `None` outside `0x1A` to
    /// `0x1F`.
    #[must_use]
    pub const fn from_attribute_type(kind: u8) -> Option<Self> {
        match kind {
            0x1A => Some(Self::C1A),
            0x1B => Some(Self::C1B),
            0x1C => Some(Self::C1C),
            0x1D => Some(Self::C1D),
            0x1E => Some(Self::C1E),
            0x1F => Some(Self::C1F),
            _ => None,
        }
    }
}

/// A Classification of articulation, set by the attribute type (M2-123 §5.1,
/// Table 7). Attribute types `0x18` and `0x19` are reserved and are not one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Classification {
    /// `0x10`: Core Sounds - Sustains and Strikes.
    Core,
    /// `0x11`: Staccatos and Shorts.
    Shorts,
    /// `0x12`: Same Note Trills/Repeats.
    SameNote,
    /// `0x13`: Intervallic Trills.
    IntervallicTrills,
    /// `0x14`: Additional Colors - Sustained.
    SustainedColors,
    /// `0x15`: Pitch and Dynamic Gestures.
    Gestures,
    /// `0x16`: Scales, Runs and Arpeggios.
    Runs,
    /// `0x17`: Effects and Noises.
    Effects,
    /// `0x1A` to `0x1F`: custom sounds.
    Custom(CustomClassification),
}

impl Classification {
    /// The eight defined Classifications, in the order of the attribute type.
    pub const STANDARD: &'static [Self] = &[
        Self::Core,
        Self::Shorts,
        Self::SameNote,
        Self::IntervallicTrills,
        Self::SustainedColors,
        Self::Gestures,
        Self::Runs,
        Self::Effects,
    ];

    /// The attribute type, `0x10` to `0x17` or `0x1A` to `0x1F`.
    #[must_use]
    pub const fn attribute_type(self) -> u8 {
        match self {
            Self::Core => 0x10,
            Self::Shorts => 0x11,
            Self::SameNote => 0x12,
            Self::IntervallicTrills => 0x13,
            Self::SustainedColors => 0x14,
            Self::Gestures => 0x15,
            Self::Runs => 0x16,
            Self::Effects => 0x17,
            Self::Custom(custom) => custom.attribute_type(),
        }
    }

    /// The Classification an attribute type names.
    ///
    /// # Errors
    ///
    /// [`ArticulationError::ReservedClassification`] for `0x18` and `0x19`;
    /// [`ArticulationError::NotAnArticulation`] for a type outside `0x10` to
    /// `0x1F`.
    pub const fn from_attribute_type(kind: u8) -> Result<Self, ArticulationError> {
        match kind {
            0x10 => Ok(Self::Core),
            0x11 => Ok(Self::Shorts),
            0x12 => Ok(Self::SameNote),
            0x13 => Ok(Self::IntervallicTrills),
            0x14 => Ok(Self::SustainedColors),
            0x15 => Ok(Self::Gestures),
            0x16 => Ok(Self::Runs),
            0x17 => Ok(Self::Effects),
            0x18 | 0x19 => Err(ArticulationError::ReservedClassification(kind)),
            0x1A..=0x1F => match CustomClassification::from_attribute_type(kind) {
                Some(custom) => Ok(Self::Custom(custom)),
                None => Err(ArticulationError::NotAnArticulation(kind)),
            },
            _ => Err(ArticulationError::NotAnArticulation(kind)),
        }
    }
}

/// An articulation: a Classification and the Subclass within it (M2-123 §5.2).
/// Each Classification has its own table, so a Subclass of one cannot be
/// written under another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Articulation {
    /// Classification `0x10`.
    Core(CoreSound),
    /// Classification `0x11`.
    Short(ShortSound),
    /// Classification `0x12`.
    SameNote(SameNoteSound),
    /// Classification `0x13`.
    IntervallicTrill(IntervallicTrill),
    /// Classification `0x14`.
    Color(ColorSound),
    /// Classification `0x15`.
    Gesture(GestureSound),
    /// Classification `0x16`.
    Run(RunSound),
    /// Classification `0x17`.
    Effect(EffectSound),
    /// Classifications `0x1A` to `0x1F`: the manufacturer indexes the Subclass.
    Custom(CustomClassification, U4),
}

impl Articulation {
    /// The articulation of a Normal Sustains and Strikes note, Subclass `0x0` of
    /// Classification `0x10`: the sound M2-123 §6.1 falls back to.
    pub const NORMAL: Self = Self::Core(CoreSound::NormalPart1);

    /// The Classification.
    #[must_use]
    pub const fn classification(self) -> Classification {
        match self {
            Self::Core(_) => Classification::Core,
            Self::Short(_) => Classification::Shorts,
            Self::SameNote(_) => Classification::SameNote,
            Self::IntervallicTrill(_) => Classification::IntervallicTrills,
            Self::Color(_) => Classification::SustainedColors,
            Self::Gesture(_) => Classification::Gestures,
            Self::Run(_) => Classification::Runs,
            Self::Effect(_) => Classification::Effects,
            Self::Custom(custom, _) => Classification::Custom(custom),
        }
    }

    /// The 4-bit Subclass value.
    #[must_use]
    pub const fn subclass(self) -> U4 {
        match self {
            Self::Core(s) => s.subclass(),
            Self::Short(s) => s.subclass(),
            Self::SameNote(s) => s.subclass(),
            Self::IntervallicTrill(s) => s.subclass(),
            Self::Color(s) => s.subclass(),
            Self::Gesture(s) => s.subclass(),
            Self::Run(s) => s.subclass(),
            Self::Effect(s) => s.subclass(),
            Self::Custom(_, subclass) => subclass,
        }
    }

    /// The articulation of a Classification and Subclass.
    ///
    /// # Errors
    ///
    /// [`ArticulationError::ReservedSubclass`] when the Classification's table
    /// reserves the Subclass.
    pub const fn from_parts(
        classification: Classification,
        subclass: U4,
    ) -> Result<Self, ArticulationError> {
        macro_rules! row {
            ($variant:ident, $table:ident) => {
                match $table::from_subclass(subclass) {
                    Some(row) => Ok(Self::$variant(row)),
                    None => Err(ArticulationError::ReservedSubclass {
                        classification,
                        subclass,
                    }),
                }
            };
        }
        match classification {
            Classification::Core => row!(Core, CoreSound),
            Classification::Shorts => row!(Short, ShortSound),
            Classification::SameNote => row!(SameNote, SameNoteSound),
            Classification::IntervallicTrills => row!(IntervallicTrill, IntervallicTrill),
            Classification::SustainedColors => row!(Color, ColorSound),
            Classification::Gestures => row!(Gesture, GestureSound),
            Classification::Runs => row!(Run, RunSound),
            Classification::Effects => row!(Effect, EffectSound),
            Classification::Custom(custom) => Ok(Self::Custom(custom, subclass)),
        }
    }
}

/// The stroke direction of a Note On (M2-123 §4.1, Table 5): the bowing or
/// plucking direction, or the hand that strikes. The value `0x3` is reserved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Stroke {
    /// `0x0`: determined by the receiver.
    #[default]
    Automatic,
    /// `0x1`: down stroke, right hand pizzicato or right-hand strike.
    Down,
    /// `0x2`: up stroke, left hand pizzicato or left-hand strike.
    Up,
}

/// The string a note is played on (M2-123 §4.1, Table 6; §7.1, Table 18). For an
/// instrument without strings `First` to `Other` may mean alternate fingerings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum StringAssignment {
    /// `0x0`: none; the receiver may determine it.
    #[default]
    Unassigned,
    /// `0x1`: the first string, highest pitched.
    First,
    /// `0x2`: the second string.
    Second,
    /// `0x3`: the third string.
    Third,
    /// `0x4`: the fourth string.
    Fourth,
    /// `0x5`: the fifth string.
    Fifth,
    /// `0x6`: the sixth string.
    Sixth,
    /// `0x7`: another string.
    Other,
}

impl StringAssignment {
    const fn bits(self) -> u16 {
        self as u16
    }

    const fn from_bits(bits: u16) -> Self {
        match bits & 0x7 {
            0 => Self::Unassigned,
            1 => Self::First,
            2 => Self::Second,
            3 => Self::Third,
            4 => Self::Fourth,
            5 => Self::Fifth,
            6 => Self::Sixth,
            _ => Self::Other,
        }
    }
}

/// Why an attribute is not an articulation of this Profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ArticulationError {
    /// The attribute type is not `0x10` to `0x1F` (Note On) or `0x10` (Note
    /// Off): a receiver chooses the articulation itself (M2-123 §5.1).
    #[error("attribute type {0:#04x} is not an articulation")]
    NotAnArticulation(u8),
    /// The message is not a Note On.
    #[error("the message is not a Note On")]
    NotANoteOn,
    /// The message is not a Note Off.
    #[error("the message is not a Note Off")]
    NotANoteOff,
    /// Attribute types `0x18` and `0x19` are reserved (M2-123 §5.2.9).
    #[error("attribute type {0:#04x} is a reserved Classification")]
    ReservedClassification(u8),
    /// The Classification's table reserves the Subclass (M2-123 §5.2).
    #[error("Subclass {} of {classification:?} is reserved", .subclass.get())]
    ReservedSubclass {
        /// The Classification.
        classification: Classification,
        /// The Subclass.
        subclass: U4,
    },
    /// The Note Off Subclass is one Table 17 reserves.
    #[error("Note Off Subclass {} is reserved", .0.get())]
    ReservedEnding(U4),
    /// The stroke direction `0x3` is reserved (M2-123 Table 5).
    #[error("stroke direction 3 is reserved")]
    ReservedStroke,
    /// A reserved bit of the attribute data is set (M2-123 Figures 1 and 2).
    #[error("reserved attribute data bits {0:#06x} are set")]
    ReservedBits(u16),
}

// Note On attribute data (M2-123 Figure 1): Subclass 15-12, Variation 11-8,
// direction 7-6, reserved 5-4, reset round robin 3, string 2-0.
const ON_RESERVED: u16 = 0x0030;

/// A Note On's articulation under the Profile (M2-123 §4.1): the attribute type
/// and data of Figure 1, with the Classification, Subclass, Variation, stroke,
/// round-robin reset and string typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NoteArticulation {
    /// The Classification and Subclass.
    pub articulation: Articulation,
    /// Which of the Subclass's up to 16 Variations.
    pub variation: U4,
    /// The stroke direction.
    pub stroke: Stroke,
    /// The receiver plays its first round-robin sound and restarts the
    /// sequence.
    pub reset_round_robin: bool,
    /// The string.
    pub string: StringAssignment,
}

impl NoteArticulation {
    /// An articulation with its first Variation, an automatic stroke, no
    /// round-robin reset and no string.
    #[must_use]
    pub const fn new(articulation: Articulation) -> Self {
        Self {
            articulation,
            variation: U4::of(0),
            stroke: Stroke::Automatic,
            reset_round_robin: false,
            string: StringAssignment::Unassigned,
        }
    }

    /// The attribute of a Note On carrying this articulation: type `0x10` to
    /// `0x1F` and the data of Figure 1.
    #[must_use]
    pub const fn to_attribute(self) -> Attribute {
        let data = (self.articulation.subclass().get() as u16) << 12
            | (self.variation.get() as u16) << 8
            | (self.stroke as u16) << 6
            | (self.reset_round_robin as u16) << 3
            | self.string.bits();
        Attribute::Articulation(ArticulationAttribute::wire(
            self.articulation.classification().attribute_type(),
            data,
        ))
    }

    /// The articulation a Note On attribute carries.
    ///
    /// # Errors
    ///
    /// [`ArticulationError`] when the attribute type is not an articulation's,
    /// or names a reserved Classification, Subclass or stroke, or sets a
    /// reserved bit.
    pub fn from_attribute(attribute: Attribute) -> Result<Self, ArticulationError> {
        let (kind, data) = match attribute {
            Attribute::None => (0x00, 0),
            Attribute::Manufacturer(data) => (0x01, data),
            Attribute::Profile(data) => (0x02, data),
            Attribute::Pitch(data) => (0x03, data.q7_9()),
            Attribute::Reserved(r) => (r.kind(), r.data()),
            Attribute::Articulation(a) => (a.kind(), a.data()),
        };
        let classification = Classification::from_attribute_type(kind)?;
        let subclass = U4::masked((data >> 12) as u8);
        let articulation = Articulation::from_parts(classification, subclass)?;
        let stroke = match (data >> 6) & 0x3 {
            0 => Stroke::Automatic,
            1 => Stroke::Down,
            2 => Stroke::Up,
            _ => return Err(ArticulationError::ReservedStroke),
        };
        if data & ON_RESERVED != 0 {
            return Err(ArticulationError::ReservedBits(data & ON_RESERVED));
        }
        Ok(Self {
            articulation,
            variation: U4::masked((data >> 8) as u8),
            stroke,
            reset_round_robin: data & 0x8 != 0,
            string: StringAssignment::from_bits(data),
        })
    }

    /// The articulation of a Note On.
    ///
    /// # Errors
    ///
    /// [`ArticulationError::NotANoteOn`] for a message that is not a Note On,
    /// and the errors of [`from_attribute`](Self::from_attribute).
    pub fn from_note_on(message: &Voice2) -> Result<Self, ArticulationError> {
        match message {
            Voice2::NoteOn { attribute, .. } => Self::from_attribute(*attribute),
            _ => Err(ArticulationError::NotANoteOn),
        }
    }

    /// A Note On of `note` carrying this articulation.
    ///
    /// ```
    /// use tutti_ump::{
    ///     Articulation, NoteArticulation, NoteNumber, NoteVelocity, ShortSound, Voice2,
    /// };
    ///
    /// let articulation = NoteArticulation::new(Articulation::Short(ShortSound::Pizzicato));
    /// let note_on = articulation.note_on(NoteNumber::of(60), NoteVelocity::new(0x8000));
    /// assert_eq!(NoteArticulation::from_note_on(&note_on), Ok(articulation));
    /// assert!(matches!(note_on, Voice2::NoteOn { .. }));
    /// ```
    #[must_use]
    pub const fn note_on(self, note: NoteNumber, velocity: NoteVelocity) -> Voice2 {
        Voice2::NoteOn {
            note,
            velocity,
            attribute: self.to_attribute(),
        }
    }
}

/// How a note ends, the Subclass of a Note Off (M2-123 §7.1, Table 17). The
/// values `0x5` to `0xF` are reserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoteEnding {
    /// `0x0`: no Note Off sample.
    NoSample,
    /// `0x1`: a soft ending.
    Soft,
    /// `0x2`: a hard ending.
    Hard,
    /// `0x3`: a pitch rise, such as a doit.
    PitchRise,
    /// `0x4`: a pitch fall.
    PitchFall,
}

impl NoteEnding {
    /// Every ending, in the order of the Subclass value.
    pub const ALL: &'static [Self] = &[
        Self::NoSample,
        Self::Soft,
        Self::Hard,
        Self::PitchRise,
        Self::PitchFall,
    ];

    /// The 4-bit Subclass value.
    #[must_use]
    pub const fn subclass(self) -> U4 {
        U4::of(self as u8)
    }

    /// The ending of a Note Off Subclass, or `None` when Table 17 reserves it.
    #[must_use]
    pub const fn from_subclass(subclass: U4) -> Option<Self> {
        match subclass.get() {
            0 => Some(Self::NoSample),
            1 => Some(Self::Soft),
            2 => Some(Self::Hard),
            3 => Some(Self::PitchRise),
            4 => Some(Self::PitchFall),
            _ => None,
        }
    }
}

/// A Note Off's release time (M2-123 §7.1), carried as the message's 16-bit
/// release velocity: [`LONGEST`](Self::LONGEST) is the receiver's longest
/// release, [`DEFAULT`](Self::DEFAULT) the natural release of the chosen sound
/// and [`SHORTEST`](Self::SHORTEST) an immediate one. A sender that cannot vary
/// the release time sends [`DEFAULT`](Self::DEFAULT), which is what
/// [`Default`] gives. The receiver applies it only when it declared Note Off
/// velocity support in its Profile Details reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReleaseTime(u16);

impl ReleaseTime {
    /// `0x0000`: the longest release time of the receiver.
    pub const LONGEST: Self = Self(0x0000);
    /// `0x8000`: the default release time.
    pub const DEFAULT: Self = Self(0x8000);
    /// `0xFFFF`: the shortest release time, an immediate release.
    pub const SHORTEST: Self = Self(0xFFFF);

    /// A release time from its 16-bit value; every value is one. §7.1 states
    /// only the three anchors, and the receiver orders the values between.
    #[must_use]
    pub const fn new(raw: u16) -> Self {
        Self(raw)
    }

    /// The 16-bit value.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl Default for ReleaseTime {
    fn default() -> Self {
        Self::DEFAULT
    }
}

// Note Off attribute data (M2-123 Figure 2): Subclass 15-12, Variation 11-8,
// reserved 7-3, string 2-0.
const OFF_RESERVED: u16 = 0x00F8;

/// A Note Off's articulation under the Profile (M2-123 §7.1, Figure 2). The
/// release time is the Note Off's velocity, which the message carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NoteOffArticulation {
    /// How the note ends.
    pub ending: NoteEnding,
    /// Which of the ending's up to 16 Variations.
    pub variation: U4,
    /// Which string's note ends.
    pub string: StringAssignment,
}

impl NoteOffArticulation {
    /// The attribute type of a Note Off that carries the Profile's fields
    /// (M2-123 Table 16).
    pub const ATTRIBUTE_TYPE: u8 = 0x10;

    /// The attribute of a Note Off carrying this articulation.
    #[must_use]
    pub const fn to_attribute(self) -> Attribute {
        let data = (self.ending.subclass().get() as u16) << 12
            | (self.variation.get() as u16) << 8
            | self.string.bits();
        Attribute::Articulation(ArticulationAttribute::wire(Self::ATTRIBUTE_TYPE, data))
    }

    /// The articulation a Note Off attribute carries.
    ///
    /// # Errors
    ///
    /// [`ArticulationError`] when the attribute type is not `0x10`, the Subclass
    /// is reserved, or a reserved bit is set.
    pub fn from_attribute(attribute: Attribute) -> Result<Self, ArticulationError> {
        let (kind, data) = match attribute {
            Attribute::None => (0x00, 0),
            Attribute::Manufacturer(data) => (0x01, data),
            Attribute::Profile(data) => (0x02, data),
            Attribute::Pitch(data) => (0x03, data.q7_9()),
            Attribute::Reserved(r) => (r.kind(), r.data()),
            Attribute::Articulation(a) => (a.kind(), a.data()),
        };
        if kind != Self::ATTRIBUTE_TYPE {
            return Err(ArticulationError::NotAnArticulation(kind));
        }
        let subclass = U4::masked((data >> 12) as u8);
        let ending = NoteEnding::from_subclass(subclass)
            .ok_or(ArticulationError::ReservedEnding(subclass))?;
        if data & OFF_RESERVED != 0 {
            return Err(ArticulationError::ReservedBits(data & OFF_RESERVED));
        }
        Ok(Self {
            ending,
            variation: U4::masked((data >> 8) as u8),
            string: StringAssignment::from_bits(data),
        })
    }

    /// A Note Off of `note` with `release` carrying this articulation.
    #[must_use]
    pub const fn note_off(self, note: NoteNumber, release: ReleaseTime) -> Voice2 {
        Voice2::NoteOff {
            note,
            velocity: NoteVelocity::new(release.get()),
            attribute: self.to_attribute(),
        }
    }

    /// The articulation and release time of a Note Off.
    ///
    /// # Errors
    ///
    /// [`ArticulationError::NotANoteOff`] for a message that is not a Note Off,
    /// and the errors of [`from_attribute`](Self::from_attribute).
    pub fn from_note_off(message: &Voice2) -> Result<(Self, ReleaseTime), ArticulationError> {
        match message {
            Voice2::NoteOff {
                velocity,
                attribute,
                ..
            } => Ok((
                Self::from_attribute(*attribute)?,
                ReleaseTime::new(velocity.get()),
            )),
            _ => Err(ArticulationError::NotANoteOff),
        }
    }
}

/// A Note On or Note Off attribute of an M2-123 articulation type (`0x10` to
/// `0x1F`), with its data. [`NoteArticulation::to_attribute`] and
/// [`NoteOffArticulation::to_attribute`] write one for a defined articulation;
/// the decoder reads one for any type of the range, a reserved Classification
/// included, which [`NoteArticulation::from_attribute`] then refuses by name.
///
/// ```compile_fail,E0451
/// use tutti_ump::ArticulationAttribute;
/// let _ = ArticulationAttribute { kind: 0x18, data: 0 };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "(u8, u16)", into = "(u8, u16)"))]
pub struct ArticulationAttribute {
    kind: u8,
    data: u16,
}

// The serde form is the `(kind, data)` pair, read back only for a kind of the
// articulation range.
#[cfg(feature = "serde")]
impl TryFrom<(u8, u16)> for ArticulationAttribute {
    type Error = &'static str;

    fn try_from((kind, data): (u8, u16)) -> Result<Self, Self::Error> {
        match kind {
            0x10..=0x1F => Ok(Self { kind, data }),
            _ => Err("an articulation attribute type is 0x10 to 0x1F"),
        }
    }
}

#[cfg(feature = "serde")]
impl From<ArticulationAttribute> for (u8, u16) {
    fn from(attribute: ArticulationAttribute) -> Self {
        (attribute.kind, attribute.data)
    }
}

impl ArticulationAttribute {
    pub(crate) const fn wire(kind: u8, data: u16) -> Self {
        Self { kind, data }
    }

    /// The attribute type, `0x10..=0x1F`.
    #[must_use]
    pub const fn kind(self) -> u8 {
        self.kind
    }

    /// The attribute data.
    #[must_use]
    pub const fn data(self) -> u16 {
        self.data
    }
}
