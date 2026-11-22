//! An SMF's division word and format word (SMF 1.0 section 2.1): SMPTE time,
//! a metrical division, and a format the specification leaves undefined.

use crate::common::{encoded, read_bytes};
use crate::common::{metrical, sequences};
use crate::support::{Division, RawEvent, build_smf};
use tutti_fixtures::{RangeContract, check_range_contract};
use tutti_midi_file::{
    FileEvent, Meta, MicrosPerQuarter, MidiFile, OtherSmfFormat, ReadError, SmfDivision, SmfFormat,
    SmfParseError, SmfTicksPerFrame, SmfTicksPerQuarter, SmpteView, Tick, Timebase,
};
use tutti_midi_protocol::MtcRate;
use tutti_tempo::TempoMap;
use tutti_units::Seconds;
use tutti_units::{Beat, Bpm};

fn read(bytes: &[u8]) -> MidiFile {
    read_bytes(bytes).expect("reads")
}

/// The SMPTE view of one timeline of `file`.
fn smpte(file: &MidiFile) -> SmpteView<'_> {
    let Timebase::Smpte(view) = file.timebase() else {
        panic!("an SMPTE file of one timeline");
    };
    view
}

/// A format 0 file of one note at `division`.
fn one_note(division: Division) -> Vec<u8> {
    build_smf(0, division, &[vec![RawEvent::note_on(0, 0, 60, 100)]])
}

/// The two division bytes of a header (offset 12 of the file).
fn division_bytes(bytes: &[u8]) -> [u8; 2] {
    [bytes[12], bytes[13]]
}

/// SMF 1.0 section 2.1: with bit 15 set, bits 14 to 8 are -24, -25, -29 or -30
/// in two's complement and the low byte is the resolution in a frame. The
/// specification's own words: 25 frames a second at 40 a frame is the
/// millisecond file (`E7 28`), and 30 frames at bit resolution (80) is `E250`.
/// -29 is 30 drop frame, 29.97.
/// Spec: smf/header-chunk#11, smf/header-chunk#12, smf/header-chunk#14, smf/header-chunk#15, smf/header-chunk#16, smf/header-chunk#17, smf/header-chunk#18
#[test]
fn a_division_word_with_its_top_bit_set_is_a_frame_rate_and_ticks_per_frame() {
    for (word, fps, ticks_per_frame) in [
        ([0xE7, 0x28], MtcRate::Fps25, 40),
        ([0xE2, 0x50], MtcRate::Fps30, 80),
        ([0xE8, 0x04], MtcRate::Fps24, 4),
        ([0xE3, 0x64], MtcRate::Fps2997Df, 100),
    ] {
        let bytes = one_note(Division::Smpte {
            fps: i8::from_be_bytes([word[0]]),
            ticks_per_frame: word[1],
        });
        assert_eq!(division_bytes(&bytes), word);
        let file = read(&bytes);
        let division = SmfDivision::Smpte {
            fps,
            ticks_per_frame: SmfTicksPerFrame::of(ticks_per_frame),
        };
        assert_eq!(file.division(), Some(division), "{word:02X?}");
        assert_eq!(division.word(), u16::from_be_bytes(word));
        // A write gives the division word back.
        assert_eq!(division_bytes(&encoded(&file)), word);
        assert_eq!(read(&encoded(&file)), file);
    }
}

/// A metrical word keeps its 15 bits and writes back as it was.
/// Spec: smf/header-chunk#11, smf/header-chunk#12, smf/header-chunk#13
#[test]
fn a_division_word_with_its_top_bit_clear_is_ticks_per_quarter() {
    for tpq in [1u16, 96, 480, 0x7FFF] {
        let file = read(&one_note(Division::Metrical(tpq)));
        assert_eq!(file.division(), Some(SmfDivision::per_quarter(tpq)));
        assert_eq!(division_bytes(&encoded(&file)), tpq.to_be_bytes());
    }
}

/// A frame rate that is none of -24, -25, -29 and -30, and a resolution of zero,
/// are not divisions.
/// Spec: smf/header-chunk#15
#[test]
fn a_division_with_a_foreign_frame_rate_or_no_resolution_is_refused() {
    for (fps, ticks_per_frame, cause) in [
        (-23, 40, SmfParseError::InvalidFrameRate),
        (-26, 40, SmfParseError::InvalidFrameRate),
        (-28, 40, SmfParseError::InvalidFrameRate),
        (-31, 40, SmfParseError::InvalidFrameRate),
        (-128, 40, SmfParseError::InvalidFrameRate),
        (-25, 0, SmfParseError::ZeroTicksPerFrame),
    ] {
        let bytes = one_note(Division::Smpte {
            fps,
            ticks_per_frame,
        });
        assert!(
            matches!(
                read_bytes(&bytes),
                Err(ReadError::Smf { track: None, offset: 0, cause: c }) if c == cause
            ),
            "{fps} x {ticks_per_frame}"
        );
    }
}

/// A tick of an SMPTE file is `1 / (frames per second * ticks per frame)`
/// seconds. 25 x 40 is 1000 ticks a second (the specification's millisecond
/// file); 24 x 4 is 96; 30 x 80 is 2400.
/// Spec: smf/header-chunk#14
#[test]
fn an_smpte_tick_is_a_fixed_fraction_of_a_second() {
    for (fps, ticks_per_frame, ticks_per_second) in
        [(-25, 40, 1_000u64), (-24, 4, 96), (-30, 80, 2_400)]
    {
        let file = read(&one_note(Division::Smpte {
            fps,
            ticks_per_frame,
        }));
        assert_eq!(smpte(&file).seconds_at(Tick(0)), Seconds::ZERO);
        assert_eq!(
            smpte(&file).seconds_at(Tick(ticks_per_second)),
            Seconds::new(1.0)
        );
        assert_eq!(
            smpte(&file).seconds_at(Tick(3 * ticks_per_second)),
            Seconds::new(3.0)
        );
        assert_eq!(
            smpte(&file).seconds_at(Tick(ticks_per_second / 2)),
            Seconds::new(0.5),
            "{fps} x {ticks_per_frame}"
        );
    }
    let ms = read(&one_note(Division::Smpte {
        fps: -25,
        ticks_per_frame: 40,
    }));
    assert_eq!(smpte(&ms).seconds_at(Tick(2_500)), Seconds::new(2.5));
}

/// -29 is 29.97 frames a second, exactly 30000/1001: 30000 frames last 1001
/// seconds, so at 4 ticks a frame 120000 ticks are 1001 s, where 30 fps would
/// say 1000.
/// Spec: smf/header-chunk#15
#[test]
fn the_drop_frame_rate_is_30000_over_1001() {
    let file = read(&one_note(Division::Smpte {
        fps: -29,
        ticks_per_frame: 4,
    }));
    assert_eq!(
        smpte(&file).seconds_at(Tick(120_000)),
        Seconds::new(1_001.0)
    );
    assert_eq!(
        smpte(&file).seconds_at(Tick(240_000)),
        Seconds::new(2_002.0)
    );
}

/// The events of an SMPTE file are at seconds, and a Set Tempo moves none of
/// them: the file has no quarter note to apply a tempo to.
/// Spec: smf/header-chunk#14
#[test]
fn an_smpte_files_events_are_at_seconds_and_no_tempo_applies() {
    let bytes = build_smf(
        0,
        Division::Smpte {
            fps: -25,
            ticks_per_frame: 40,
        },
        &[vec![
            RawEvent::tempo(0, 1_000_000),
            RawEvent::time_signature(0, 4, 2),
            RawEvent::note_on(1_000, 0, 60, 100),
            RawEvent::note_off(1_500, 0, 60, 0),
        ]],
    );
    let file = read(&bytes);
    let Timebase::Smpte(smpte) = file.timebase() else {
        panic!("an SMPTE file");
    };
    let at: Vec<Seconds> = smpte.events().map(|(time, _)| time).collect();
    // The tempo, the Time Signature and the two notes.
    assert_eq!(
        at,
        [
            Seconds::ZERO,
            Seconds::ZERO,
            Seconds::new(1.0),
            Seconds::new(2.5)
        ]
    );
    assert_eq!(smpte.fps(), MtcRate::Fps25);
    assert_eq!(smpte.ticks_per_frame(), SmfTicksPerFrame::of(40));
    assert_eq!(smpte.seconds_at(smpte.end()), Seconds::new(2.5));
    // A quarter-note grid does not exist: no view of it, so no beat, tempo map or timeline.
    assert!(file.timebase().metrical().is_none());
    // The Set Tempo is still an event and is written back.
    assert!(matches!(
        file.tracks()[0].events()[0].1,
        FileEvent::Meta(Meta::Tempo {
            micros_per_quarter,
            ..
        }) if micros_per_quarter == MicrosPerQuarter::of(1_000_000)
    ));
    assert_eq!(encoded(&file), bytes);
}

/// A format 2 file in SMPTE time keeps its sequences apart as a metrical one's
/// are: it has sequences, each read in seconds on its own, and no
/// one-timeline view.
/// Spec: smf/header-chunk#8
#[test]
fn a_format_2_file_in_smpte_time_has_sequences() {
    let bytes = build_smf(
        2,
        Division::Smpte {
            fps: -25,
            ticks_per_frame: 40,
        },
        &[
            vec![RawEvent::note_on(1_000, 0, 60, 100)],
            vec![RawEvent::note_on(500, 0, 62, 100)],
        ],
    );
    let file = read(&bytes);
    assert!(file.timebase().metrical().is_none());
    let Timebase::SmpteSequences(smpte) = file.timebase() else {
        panic!("SMPTE sequences");
    };
    assert_eq!(smpte.fps(), MtcRate::Fps25);
    let at: Vec<Vec<Seconds>> = smpte
        .sequences()
        .map(|s| s.events().map(|(time, _)| time).collect())
        .collect();
    // Each sequence starts with the tempo and Time Signature it is given.
    assert_eq!(
        at,
        [
            [Seconds::ZERO, Seconds::ZERO, Seconds::new(1.0)],
            [Seconds::ZERO, Seconds::ZERO, Seconds::new(0.5)]
        ]
    );
}

/// SMF 1.0 section 2.1: "A program encountering an unknown format ID may still
/// read other MTrk chunks it finds". A format 3 file is read as its tracks,
/// keeps its format word and is written back byte for byte.
/// Spec: smf/formats#17
#[test]
fn a_format_3_file_reads_as_its_tracks_and_writes_back_exactly() {
    let bytes = build_smf(
        3,
        Division::Metrical(96),
        &[
            vec![
                RawEvent::tempo(0, 500_000),
                RawEvent::time_signature(0, 4, 2),
                RawEvent::note_on(0, 0, 60, 100),
            ],
            vec![
                RawEvent::tempo(0, 500_000),
                RawEvent::time_signature(0, 4, 2),
                RawEvent::note_on(48, 1, 62, 90),
            ],
        ],
    );
    assert_eq!(&bytes[8..10], [0x00, 0x03]);
    let file = read(&bytes);
    assert_eq!(file.format(), Some(SmfFormat::Other(OtherSmfFormat::of(3))));
    assert_eq!(file.tracks().len(), 2);
    assert_eq!(file.tracks()[1].events()[2].0, Tick(48));
    assert_eq!(encoded(&file), bytes);
    // Nothing says the tracks share a timeline, so they are kept apart as a
    // format 2 file's are: one sequence a track, no one-timeline view.
    assert!(file.timebase().metrical().is_none());
    assert_eq!(sequences(&file).sequences().len(), 2);
}

/// Every format word reads and writes back as it is, to the largest.
#[test]
fn a_format_word_is_kept_whatever_it_is() {
    for word in [0u16, 1, 2, 3, 4, 0x0100, u16::MAX] {
        let bytes = build_smf(word, Division::Metrical(96), &[vec![]]);
        let file = read(&bytes);
        assert_eq!(encoded(&file)[8..10], word.to_be_bytes());
        assert_eq!(
            matches!(file.format(), Some(SmfFormat::Other(_))),
            word > 2,
            "{word}"
        );
    }
}

/// An SMPTE file cannot be a Clip File, whose clockstamps are quarter-note
/// ticks: it has no metrical view to convert from. The same events at a
/// metrical division convert.
#[test]
fn a_file_in_smpte_time_has_no_clip() {
    let smpte = read(&one_note(Division::Smpte {
        fps: -25,
        ticks_per_frame: 40,
    }));
    assert!(matches!(smpte.timebase(), Timebase::Smpte(_)));
    let file = read(&one_note(Division::Metrical(96)));
    assert!(matches!(file.timebase(), Timebase::Metrical(_)));
    assert!(metrical(&file).to_clip().is_ok());
}

/// SMF 1.0 section 2.1: at -25 frames a second and 40 ticks a frame a tick is a
/// millisecond, so tick 1000 is 1.0 s: beat 2.0 over a 120 BPM map, and beat
/// 1.0 over 60 BPM, whatever Set Tempo the file carries. Each track of a
/// format 1 file and each sequence of a format 2 file is placed so.
#[test]
fn an_smpte_files_tracks_are_placed_in_beats_over_the_given_tempo_map() {
    let division = Division::Smpte {
        fps: -25,
        ticks_per_frame: 40,
    };
    let track = || {
        vec![
            RawEvent::tempo(0, 1_000_000),
            RawEvent::note_on(1000, 0, 60, 100),
        ]
    };
    let at = |tempo: f64| TempoMap::constant(Bpm::of(tempo)).expect("a tempo");
    let note_beats = |timelines: Vec<tutti_midi_file::SmfTimeline>| -> Vec<Beat> {
        timelines
            .iter()
            .map(|timeline| timeline.packets.last().expect("the note").0)
            .collect()
    };

    let format_1 = read(&build_smf(1, division, &[track(), track()]));
    let view = smpte(&format_1);
    let (at_120, at_60) = (at(120.0), at(60.0));
    let timelines = view.timelines(&at_120);
    assert_eq!(timelines.len(), 2);
    assert_eq!(note_beats(timelines.collect()), [Beat::new(2.0); 2]);
    assert_eq!(
        note_beats(view.timelines(&at_60).collect()),
        [Beat::new(1.0); 2]
    );

    let format_2 = read(&build_smf(2, division, &[track(), track()]));
    let Timebase::SmpteSequences(sequences) = format_2.timebase() else {
        panic!("an SMPTE file of independent sequences");
    };
    let beats: Vec<_> = sequences.sequences().map(|s| s.timeline(&at_120)).collect();
    assert_eq!(note_beats(beats), [Beat::new(2.0); 2]);
}

/// SMF 1.0 section 2.1: 15 bits of ticks per quarter note, zero is none.
/// Spec: smf/header-chunk#13
#[test]
fn an_smf_ticks_per_quarter_is_1_to_32767() {
    check_range_contract(&RangeContract {
        new: SmfTicksPerQuarter::new,
        get: SmfTicksPerQuarter::get,
        of: Some(SmfTicksPerQuarter::of),
        valid: &[1, 96, 480, 0x7FFF],
        invalid: &[0, 0x8000, 0xFFFF],
    });
}

/// The low byte of an SMPTE division: 1 to 255, zero is none.
#[test]
fn an_smf_ticks_per_frame_is_1_to_255() {
    check_range_contract(&RangeContract {
        new: SmfTicksPerFrame::new,
        get: SmfTicksPerFrame::get,
        of: Some(SmfTicksPerFrame::of),
        valid: &[1, 4, 40, 80, 255],
        invalid: &[0],
    });
}

/// The words 0, 1 and 2 are the named formats, so the others are 3 and up.
#[test]
fn an_other_smf_format_is_3_and_up() {
    check_range_contract(&RangeContract {
        new: OtherSmfFormat::new,
        get: OtherSmfFormat::get,
        of: Some(OtherSmfFormat::of),
        valid: &[3, 4, 0x1234, u16::MAX],
        invalid: &[0, 1, 2],
    });
}
