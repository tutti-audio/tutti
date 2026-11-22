//! The Note On Orchestral Articulation Profile (M2-123) against its tables and
//! against words derived by hand from Figures 1 and 2. Nothing here asks the
//! code for an expected value.

use tutti_ump::{
    Address, Articulation, ArticulationError, Attribute, Classification, ColorSound, CoreSound,
    CustomClassification as Cc, EffectSound, GestureSound, IntervallicTrill, Message, MidiChannel,
    MidiGroup, NoteArticulation, NoteEnding, NoteNumber, NoteOffArticulation, NoteVelocity, Pitch,
    ReleaseTime, RunSound, SameNoteSound, ShortSound, StringAssignment, Stroke, U4, Ump, Voice2,
};

fn u4(raw: u8) -> U4 {
    U4::of(raw)
}

/// Every Subclass `0x0..=0xF` against a literal table: the rows the spec names,
/// and `None` for the rest.
fn pin_table<T: Copy + PartialEq + std::fmt::Debug>(
    rows: &[(u8, T)],
    from: fn(U4) -> Option<T>,
    to: fn(T) -> U4,
) {
    for raw in 0..16 {
        let expected = rows.iter().find(|(n, _)| *n == raw).map(|(_, row)| *row);
        assert_eq!(from(u4(raw)), expected, "subclass {raw:#x}");
    }
    for (n, row) in rows {
        assert_eq!(to(*row).get(), *n, "{row:?}");
    }
}

/// M2-123 5.2.1/table-8.
#[test]
fn table_8_core_sounds() {
    use CoreSound::*;
    pin_table(
        &[
            (0x0, NormalPart1),
            (0x1, NormalPart2),
            (0x2, Legato),
            (0x3, MoltoLegato),
            (0x4, Glissando),
            (0x5, Detache),
            (0x6, Marcato),
            (0x7, Martele),
            (0x8, SenzaVibrato),
            (0x9, ConVibrato),
            (0xA, SynchronizedVibrato),
        ],
        CoreSound::from_subclass,
        CoreSound::subclass,
    );
}

/// M2-123 5.2.2/table-9.
#[test]
fn table_9_staccatos_and_shorts() {
    use ShortSound::*;
    pin_table(
        &[
            (0x0, NormalStaccatoOffString),
            (0x1, NormalStaccatoOnString),
            (0x2, SlurredStaccato),
            (0x3, AccentedStaccato),
            (0x4, Staccatissimo),
            (0x5, Spiccato),
            (0x6, Sautille),
            (0x7, Martellato),
            (0x8, LongStaccato),
            (0x9, Portato),
            (0xA, Pizzicato),
            (0xB, BartokPizzicato),
            (0xC, ColLegnoBattuto),
            (0xD, ColLegnoGestrichen),
            (0xE, StringHandTap),
            (0xF, Jete),
        ],
        ShortSound::from_subclass,
        ShortSound::subclass,
    );
}

/// M2-123 5.2.3/table-10.
#[test]
fn table_10_same_note_trills_and_repeats() {
    use SameNoteSound::*;
    pin_table(
        &[
            (0x0, Tremolo),
            (0x1, Growl),
            (0x2, OtherColoristicTremolo),
            (0x3, OneNoteTrill),
            (0x4, Repeats2),
            (0x5, Repeats3),
            (0x6, Repeats4),
            (0x7, Repeats5),
            (0x8, Repeats6),
            (0x9, FasterRepeats),
        ],
        SameNoteSound::from_subclass,
        SameNoteSound::subclass,
    );
}

/// M2-123 5.2.4/table-11.
#[test]
fn table_11_intervallic_trills() {
    use IntervallicTrill::*;
    pin_table(
        &[
            (0x0, HalfStepClassical),
            (0x1, HalfStepBaroque),
            (0x2, WholeStepClassical),
            (0x3, WholeStepBaroque),
            (0x4, MinorThird),
            (0x5, MajorThird),
            (0x6, PerfectFourth),
            (0x7, Tritone),
            (0x8, PerfectFifth),
            (0x9, MinorSixth),
            (0xA, MajorSixth),
            (0xB, MinorSeventh),
            (0xC, MajorSeventh),
            (0xD, Octave),
        ],
        IntervallicTrill::from_subclass,
        IntervallicTrill::subclass,
    );
}

/// M2-123 5.2.5/table-12.
#[test]
fn table_12_additional_colors() {
    use ColorSound::*;
    pin_table(
        &[
            (0x0, HarmonicsNatural),
            (0x1, HarmonicsArtificial),
            (0x2, ColLegnoTratto),
            (0x3, Flautando),
            (0x4, PolyphonyOctaves),
            (0x5, PolyphonyChords),
            (0x6, Cuivre),
            (0x7, Lontano),
            (0x8, SingingIntoInstrument),
        ],
        ColorSound::from_subclass,
        ColorSound::subclass,
    );
}

/// M2-123 5.2.6/table-13.
#[test]
fn table_13_pitch_and_dynamic_gestures() {
    use GestureSound::*;
    pin_table(
        &[
            (0x0, PitchFallEnd),
            (0x1, PitchFallStart),
            (0x2, PitchRiseEnd),
            (0x3, PitchRiseStart),
            (0x4, BlueNoteDown),
            (0x5, BlueNoteUp),
            (0x6, GraceNotesClassical),
            (0x7, GraceNotesBaroque),
            (0x8, Shakes),
            (0x9, Crescendo),
            (0xA, Decrescendo),
            (0xB, CrescendoDecrescendo),
            (0xC, DecrescendoCrescendo),
            (0xD, SfzCrescendo),
        ],
        GestureSound::from_subclass,
        GestureSound::subclass,
    );
}

/// M2-123 5.2.7/table-14.
#[test]
fn table_14_scales_runs_and_arpeggios() {
    use RunSound::*;
    pin_table(
        &[
            (0x0, PlayableRuns),
            (0x1, PlayableTremolos),
            (0x2, PlayableGlissando),
            (0x3, Major),
            (0x4, Minor),
            (0x5, Dominant7th),
            (0x6, DiminishedWholeHalf),
            (0x7, DiminishedHalfWhole),
            (0x8, WholeToneOnC),
            (0x9, WholeToneOnCSharp),
            (0xA, PentatonicMajor),
            (0xB, PentatonicMinor),
            (0xC, Lydian),
            (0xD, LydianFlat7),
            (0xE, Chromatic),
            (0xF, Other),
        ],
        RunSound::from_subclass,
        RunSound::subclass,
    );
}

/// M2-123 5.2.8/table-15.
#[test]
fn table_15_effects_and_noises() {
    use EffectSound::*;
    pin_table(
        &[
            (0x0, AssortedSustained),
            (0x1, BehindTheBridge),
            (0x2, RandomPizzicato),
            (0x3, HarmonicGlissando),
            (0x4, RandomGlissando),
            (0x5, ShortPizzicato),
            (0x6, MechanicalSustained),
            (0x7, MechanicalShort),
            (0x8, FingerGlide),
            (0x9, FretBuzz),
            (0xA, Thump),
            (0xB, Knock),
            (0xC, Slap),
            (0xD, Pop),
            (0xE, Tap),
            (0xF, Click),
        ],
        EffectSound::from_subclass,
        EffectSound::subclass,
    );
}

/// M2-123 5.1/table-7.
#[test]
fn table_7_classifications_by_attribute_type() {
    use Classification::*;
    let rows = [
        (0x10, Core),
        (0x11, Shorts),
        (0x12, SameNote),
        (0x13, IntervallicTrills),
        (0x14, SustainedColors),
        (0x15, Gestures),
        (0x16, Runs),
        (0x17, Effects),
        (0x1A, Custom(Cc::C1A)),
        (0x1B, Custom(Cc::C1B)),
        (0x1C, Custom(Cc::C1C)),
        (0x1D, Custom(Cc::C1D)),
        (0x1E, Custom(Cc::C1E)),
        (0x1F, Custom(Cc::C1F)),
    ];
    for kind in 0..=255u8 {
        let expected = rows.iter().find(|(k, _)| *k == kind).map(|(_, c)| *c);
        match (Classification::from_attribute_type(kind), expected) {
            (Ok(got), Some(want)) => assert_eq!(got, want, "{kind:#x}"),
            (Err(ArticulationError::ReservedClassification(k)), None) => {
                assert!(matches!(kind, 0x18 | 0x19) && k == kind, "{kind:#x}");
            }
            (Err(ArticulationError::NotAnArticulation(k)), None) => {
                assert!(!matches!(kind, 0x10..=0x1F) && k == kind, "{kind:#x}");
            }
            (got, want) => panic!("{kind:#x}: {got:?} against {want:?}"),
        }
    }
    for (kind, class) in rows {
        assert_eq!(class.attribute_type(), kind);
    }
}

/// M2-123 4.1/table-5.
#[test]
fn table_5_stroke_direction() {
    for (bits, stroke) in [
        (0u16, Stroke::Automatic),
        (1, Stroke::Down),
        (2, Stroke::Up),
    ] {
        let attribute = attr(0x10, bits << 6);
        assert_eq!(
            NoteArticulation::from_attribute(attribute).unwrap().stroke,
            stroke
        );
    }
    let reserved = attr(0x10, 3 << 6);
    assert_eq!(
        NoteArticulation::from_attribute(reserved),
        Err(ArticulationError::ReservedStroke)
    );
}

/// M2-123 4.1/table-6: the Note On string field leaves the round-robin reset
/// bit beside it alone.
#[test]
fn table_6_string_assignment_in_a_note_on() {
    use StringAssignment::*;
    let rows = [
        (0, Unassigned),
        (1, First),
        (2, Second),
        (3, Third),
        (4, Fourth),
        (5, Fifth),
        (6, Sixth),
        (7, Other),
    ];
    for (bits, string) in rows {
        // Bit 3 is the reset flag, set here.
        let attribute = attr(0x10, bits | 0x0008);
        let read = NoteArticulation::from_attribute(attribute).unwrap();
        assert_eq!((read.string, read.reset_round_robin), (string, true));
    }
}

/// M2-123 7.1/table-18: the Note Off string field leaves the Subclass above it
/// alone.
#[test]
fn table_18_string_assignment_in_a_note_off() {
    use StringAssignment::*;
    let rows = [
        (0, Unassigned),
        (1, First),
        (2, Second),
        (3, Third),
        (4, Fourth),
        (5, Fifth),
        (6, Sixth),
        (7, Other),
    ];
    for (bits, string) in rows {
        // Subclass 1 (Soft Ending) in bits 15-12.
        let attribute = attr(0x10, bits | 0x1000);
        let read = NoteOffArticulation::from_attribute(attribute).unwrap();
        assert_eq!((read.string, read.ending), (string, NoteEnding::Soft));
    }
}

/// M2-123 7.1/table-17.
#[test]
fn table_17_note_off_subclasses() {
    use NoteEnding::*;
    let rows = [
        (0x0, NoSample),
        (0x1, Soft),
        (0x2, Hard),
        (0x3, PitchRise),
        (0x4, PitchFall),
    ];
    for raw in 0..16u8 {
        let expected = rows.iter().find(|(n, _)| *n == raw).map(|(_, e)| *e);
        assert_eq!(NoteEnding::from_subclass(u4(raw)), expected, "{raw:#x}");
        let attribute = attr(0x10, u16::from(raw) << 12);
        match expected {
            Some(ending) => {
                assert_eq!(
                    NoteOffArticulation::from_attribute(attribute)
                        .unwrap()
                        .ending,
                    ending
                );
            }
            None => assert_eq!(
                NoteOffArticulation::from_attribute(attribute),
                Err(ArticulationError::ReservedEnding(u4(raw)))
            ),
        }
    }
}

fn word_of(voice: Voice2) -> [u32; 4] {
    let address = Address {
        group: MidiGroup::of(0),
        channel: MidiChannel::of(0),
    };
    Ump::from(Message::Midi2(address, voice)).0
}

/// Note On words: bits 31-28 type 4, 27-24 group 0, 23-20 status 9, 19-16
/// channel 0, 15-8 note 60, 7-0 attribute type; then velocity in 31-16 and the
/// attribute data in 15-0 (M2-104 Figure 14; M2-123 Figure 1).
///
/// M2-123 4.1/figure-1.
#[test]
fn note_on_words_follow_figure_1() {
    let note = NoteNumber::of(60);
    let cases = [
        // Type 0x10, everything zero: Normal part 1, Variation 0.
        (
            NoteArticulation::new(Articulation::NORMAL),
            0xFFFF,
            [0x4090_3C10, 0xFFFF_0000],
        ),
        // Type 0x11, subclass A (Pizzicato) 1010, variation 3 0011, down 01,
        // reserved 00, reset 1, string 010: 1010_0011_0100_1010.
        (
            NoteArticulation {
                articulation: Articulation::Short(ShortSound::Pizzicato),
                variation: u4(3),
                stroke: Stroke::Down,
                reset_round_robin: true,
                string: StringAssignment::Second,
            },
            0x8000,
            [0x4090_3C11, 0x8000_A34A],
        ),
        // Type 0x13, subclass D (Octave), variation 0.
        (
            NoteArticulation::new(Articulation::IntervallicTrill(IntervallicTrill::Octave)),
            0x0001,
            [0x4090_3C13, 0x0001_D000],
        ),
        // Type 0x15, subclass D (Sfz Crescendo), variation 1, string 110:
        // 1101_0001_0000_0110.
        (
            NoteArticulation {
                articulation: Articulation::Gesture(GestureSound::SfzCrescendo),
                variation: u4(1),
                stroke: Stroke::Automatic,
                reset_round_robin: false,
                string: StringAssignment::Sixth,
            },
            0x0000,
            [0x4090_3C15, 0x0000_D106],
        ),
        // Type 0x1F custom, subclass F, variation F, up 10, string 111:
        // 1111_1111_1000_0111.
        (
            NoteArticulation {
                articulation: Articulation::Custom(Cc::C1F, u4(0xF)),
                variation: u4(0xF),
                stroke: Stroke::Up,
                reset_round_robin: false,
                string: StringAssignment::Other,
            },
            0x1234,
            [0x4090_3C1F, 0x1234_FF87],
        ),
    ];
    for (articulation, velocity, [w0, w1]) in cases {
        let voice = articulation.note_on(note, NoteVelocity::new(velocity));
        assert_eq!(word_of(voice), [w0, w1, 0, 0], "{articulation:?}");
        let decoded = Ump([w0, w1, 0, 0]).decode().unwrap();
        let Message::Midi2(_, voice) = decoded else {
            panic!("{decoded:?}")
        };
        assert_eq!(NoteArticulation::from_note_on(&voice), Ok(articulation));
    }
}

/// M2-123 7.1/figure-2.
#[test]
fn note_off_words_follow_figure_2() {
    // Type 0x10, subclass 2 (Hard Ending) 0010, variation 5 0101, reserved 0,
    // string 011: 0010_0101_0000_0011; release velocity 0x8000 is the default
    // release time (§7.1).
    let off = NoteOffArticulation {
        ending: NoteEnding::Hard,
        variation: u4(5),
        string: StringAssignment::Third,
    };
    let voice = off.note_off(NoteNumber::of(60), ReleaseTime::DEFAULT);
    assert_eq!(word_of(voice), [0x4080_3C10, 0x8000_2503, 0, 0]);
    assert_eq!(
        NoteOffArticulation::from_attribute(attr(0x10, 0x2503)),
        Ok(off)
    );
}

/// M2-123 7.1: the release time is the Note Off's 16-bit velocity, with the
/// three anchors the section states.
#[test]
fn the_note_off_word_carries_the_release_time_anchors() {
    let off = NoteOffArticulation {
        ending: NoteEnding::Soft,
        variation: u4(0),
        string: StringAssignment::Unassigned,
    };
    // Type 0x10, subclass 1 (Soft Ending): attribute data 0x1000.
    for (release, word1) in [
        (ReleaseTime::LONGEST, 0x0000_1000),
        (ReleaseTime::DEFAULT, 0x8000_1000),
        (ReleaseTime::SHORTEST, 0xFFFF_1000),
    ] {
        let voice = off.note_off(NoteNumber::of(60), release);
        assert_eq!(word_of(voice), [0x4080_3C10, word1, 0, 0]);
    }
    assert_eq!(ReleaseTime::default(), ReleaseTime::DEFAULT);
}

/// M2-123 7.1, Figure 2.
#[test]
fn a_note_off_reads_back_what_note_off_wrote() {
    for ending in NoteEnding::ALL.iter().copied() {
        for (variation, string, release) in [
            (0, StringAssignment::Unassigned, ReleaseTime::DEFAULT),
            (9, StringAssignment::Fourth, ReleaseTime::LONGEST),
            (15, StringAssignment::Other, ReleaseTime::SHORTEST),
            (3, StringAssignment::First, ReleaseTime::new(0x1234)),
        ] {
            let off = NoteOffArticulation {
                ending,
                variation: u4(variation),
                string,
            };
            let voice = off.note_off(NoteNumber::of(60), release);
            assert_eq!(
                NoteOffArticulation::from_note_off(&voice),
                Ok((off, release))
            );
        }
    }
}

/// M2-123 5.2/tables-8-to-15.
#[test]
fn a_reserved_subclass_is_refused_with_its_classification() {
    // The Subclasses each table lists as reserved, typed from the tables.
    let reserved: [(u8, &[u8]); 6] = [
        (0x10, &[0xB, 0xC, 0xD, 0xE, 0xF]),
        (0x12, &[0xA, 0xB, 0xC, 0xD, 0xE, 0xF]),
        (0x13, &[0xE, 0xF]),
        (0x14, &[0x9, 0xA, 0xB, 0xC, 0xD, 0xE, 0xF]),
        (0x15, &[0xE, 0xF]),
        (0x16, &[]),
    ];
    for (kind, subclasses) in reserved {
        for &subclass in subclasses {
            let attribute = attr(kind, u16::from(subclass) << 12);
            assert_eq!(
                NoteArticulation::from_attribute(attribute),
                Err(ArticulationError::ReservedSubclass {
                    classification: Classification::from_attribute_type(kind).unwrap(),
                    subclass: u4(subclass),
                }),
                "{kind:#x}/{subclass:#x}"
            );
        }
    }
}

/// M2-123 5.2.9, 5.1.
/// Spec: m2-123/5.1#5, m2-123/5.2.9#1
#[test]
fn an_attribute_that_is_not_an_articulation_is_refused() {
    for kind in [0x18, 0x19] {
        assert_eq!(
            NoteArticulation::from_attribute(attr(kind, 0)),
            Err(ArticulationError::ReservedClassification(kind))
        );
    }
    assert_eq!(
        NoteArticulation::from_attribute(Attribute::None),
        Err(ArticulationError::NotAnArticulation(0x00))
    );
    assert_eq!(
        NoteArticulation::from_attribute(Attribute::Manufacturer(0x1000)),
        Err(ArticulationError::NotAnArticulation(0x01))
    );
    assert_eq!(
        NoteArticulation::from_attribute(Attribute::Pitch(Pitch::from_q7_9(0))),
        Err(ArticulationError::NotAnArticulation(0x03))
    );
    assert_eq!(
        NoteArticulation::from_attribute(attr(0x20, 0)),
        Err(ArticulationError::NotAnArticulation(0x20))
    );
    let off = Voice2::note_off(NoteNumber::of(60), NoteVelocity::new(0));
    assert_eq!(
        NoteArticulation::from_note_on(&off),
        Err(ArticulationError::NotANoteOn)
    );
    let on = Voice2::note_on(NoteNumber::of(60), NoteVelocity::new(0));
    assert_eq!(
        NoteOffArticulation::from_note_off(&on),
        Err(ArticulationError::NotANoteOff)
    );
    // A Note Off carries only type 0x10 of the Profile (Table 16).
    assert_eq!(
        NoteOffArticulation::from_attribute(attr(0x11, 0)),
        Err(ArticulationError::NotAnArticulation(0x11))
    );
}

/// M2-123 4.1/figure-1, 7.1/figure-2.
#[test]
fn a_reserved_bit_of_the_data_is_refused() {
    // Note On: bits 5 and 4 (Figure 1).
    for bit in [0x0020, 0x0010] {
        assert_eq!(
            NoteArticulation::from_attribute(attr(0x10, bit)),
            Err(ArticulationError::ReservedBits(bit))
        );
    }
    // Note Off: bits 7 to 3 (Figure 2).
    for bit in [0x0080, 0x0040, 0x0020, 0x0010, 0x0008] {
        assert_eq!(
            NoteOffArticulation::from_attribute(attr(0x10, bit)),
            Err(ArticulationError::ReservedBits(bit))
        );
    }
}

/// M2-123 4.1/figure-1.
#[test]
fn every_value_the_types_hold_round_trips_through_the_attribute() {
    for class in Classification::STANDARD
        .iter()
        .copied()
        .chain(Cc::ALL.iter().copied().map(Classification::Custom))
    {
        for subclass in 0..16 {
            let Ok(articulation) = Articulation::from_parts(class, u4(subclass)) else {
                continue;
            };
            for variation in [0, 7, 15] {
                for stroke in [Stroke::Automatic, Stroke::Down, Stroke::Up] {
                    for reset in [false, true] {
                        let value = NoteArticulation {
                            articulation,
                            variation: u4(variation),
                            stroke,
                            reset_round_robin: reset,
                            string: StringAssignment::Fifth,
                        };
                        assert_eq!(
                            NoteArticulation::from_attribute(value.to_attribute()),
                            Ok(value)
                        );
                    }
                }
            }
        }
    }
}

/// An attribute of type `kind` as a receiver reads it from the wire: no sender
/// can name a reserved type, so the test takes it from a Note On packet.
fn attr(kind: u8, data: u16) -> Attribute {
    let packet = Ump([
        0x4090_3C00 | u32::from(kind),
        0x8000_0000 | u32::from(data),
        0,
        0,
    ]);
    match packet.decode() {
        Ok(Message::Midi2(_, Voice2::NoteOn { attribute, .. })) => attribute,
        other => panic!("{other:?}"),
    }
}

/// The articulation types `0x10` to `0x1F` (M2-123 Table 15) are their own
/// attribute, which an articulation writes and the decoder reads for the whole
/// range; no other type is one, and `Attribute::Reserved` holds only what
/// neither spec defines.
/// Spec: m2-104/7.4.14#9
#[test]
fn an_articulation_is_an_articulation_attribute_and_no_other_type_is() {
    let written = NoteArticulation::new(Articulation::NORMAL).to_attribute();
    assert!(matches!(written, Attribute::Articulation(a) if a.kind() == 0x10));
    for kind in 0..=255u8 {
        let read = attr(kind, 0x1234);
        match kind {
            0x10..=0x1F => assert!(
                matches!(read, Attribute::Articulation(a) if a.kind() == kind && a.data() == 0x1234),
                "{kind:#x}: {read:?}"
            ),
            0x04..=0x0F | 0x20..=0xFF => assert!(
                matches!(read, Attribute::Reserved(r) if r.kind() == kind),
                "{kind:#x}: {read:?}"
            ),
            _ => assert!(!matches!(
                read,
                Attribute::Reserved(_) | Attribute::Articulation(_)
            )),
        }
    }
}
