//! A MIDI file load is a value: `Midi::load_midi` returns a `LoadMidi` and
//! `run()` returns the `LoadedMidi`, each track a clip value. A SoundFont is a
//! value too: `load_soundfont` returns a `LoadSoundFont`.
//!
//! The fixtures are bytes written by hand, so every figure is the file's own
//! (SMF 1.0): ticks over the division are beats, a Set Tempo of 500 000
//! microseconds is a quarter note in half a second.

use std::path::{Path, PathBuf};

use tutti_midi_file::{MidiFile, ReadRepair, Tick, TimelineDrop, Track};
use tutti_music::TimeSignature;
use tutti_session::{
    DetachedSession, LoadError, LoadedMidi, NoCommonTimelineError, Session, TrackError,
};
use tutti_tempo::TempoMap;
use tutti_ump::{
    Flex, FlexTempo, FlexTimeSignature, Message, MidiChannel, MidiGroup, NoteNumber,
    TicksPerQuarterNote, U7,
};
use tutti_units::{Beat, Bpm, SampleRate, Seconds};

fn session() -> DetachedSession {
    Session::builder()
        .build_detached(SampleRate::SR_48K)
        .expect("builds")
}

/// **A MIDI load of a dropped session is `ShutDown`**: the file did not fail.
#[test]
fn a_midi_load_of_a_dropped_session_is_shut_down() {
    let load = session().midi().load_midi(vec![0u8]);
    assert!(matches!(
        tutti_fixtures::block_on(load),
        Err(LoadError::ShutDown)
    ));
}

/// **A MIDI load and its future are `Send`.**
const fn send<T: Send>() {}
const _: () = {
    send::<tutti_session::LoadMidi>();
    send::<tutti_session::LoadingMidi>();
};

/// Set Tempo, 120 beats a minute (500 000 microseconds a quarter), at tick 0.
const TEMPO_120: [u8; 7] = [0, 0xFF, 0x51, 3, 0x07, 0xA1, 0x20];
/// Set Tempo, 60 beats a minute (1 000 000 microseconds a quarter), at tick 0.
const TEMPO_60: [u8; 7] = [0, 0xFF, 0x51, 3, 0x0F, 0x42, 0x40];
/// Time Signature 3/4, at tick 0.
const THREE_FOUR: [u8; 8] = [0, 0xFF, 0x58, 4, 3, 2, 24, 8];
/// A note one quarter long (480 ticks at a division of 480) at the delta before it.
fn note(delta: u8, key: u8) -> [u8; 9] {
    [delta, 0x90, key, 100, 0x83, 0x60, 0x80, key, 64]
}

/// An MTrk chunk of `events`, ended by End of Track.
fn track(events: &[&[u8]]) -> Vec<u8> {
    let body: Vec<u8> = events
        .iter()
        .flat_map(|e| e.iter().copied())
        .chain([0, 0xFF, 0x2F, 0])
        .collect();
    let mut chunk = b"MTrk".to_vec();
    chunk.extend((body.len() as u32).to_be_bytes());
    chunk.extend(body);
    chunk
}

/// An SMF of `format` over a division of 480 ticks a quarter.
fn smf(format: u16, tracks: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = b"MThd\0\0\0\x06".to_vec();
    bytes.extend(format.to_be_bytes());
    bytes.extend((tracks.len() as u16).to_be_bytes());
    bytes.extend(480u16.to_be_bytes());
    bytes.extend(tracks.concat());
    bytes
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("writes");
    path
}

/// Loads the file at `path`.
fn load(s: &mut DetachedSession, path: PathBuf) -> Result<LoadedMidi, LoadError> {
    tutti_fixtures::block_on(s.midi().load_midi(path))
}

/// A file held only as bytes loads as the file at its path does: the same
/// tracks, over the same timeline. The `Vec<u8>` is the asset.
#[test]
fn a_midi_file_loads_from_bytes_as_its_tracks() {
    let bytes = smf(
        1,
        &[
            track(&[&TEMPO_120, &note(0, 60)]),
            track(&[&note(0, 64), &note(0, 67)]),
        ],
    );
    let mut s = session();
    let loaded = tutti_fixtures::block_on(s.midi().load_midi(bytes)).expect("loads");
    assert_eq!(loaded.file().tracks().len(), 2);
    assert_eq!(loaded.track(1).unwrap().events.len(), 4);
    let tempo = loaded.tempo().expect("one timeline");
    assert_eq!(tempo.seconds_at(Beat::new(2.0)), Seconds::new(1.0));
}

/// A MIDI 2.0 Clip File (M2-116) loads as its notes over its Flex Data tempo: at
/// 96 ticks a quarter, a note from tick 0 to 96 is beat 0 to 1, one from tick 192
/// to 288 is beat 2 to 3, and a Set Tempo of 120 beats a minute makes two beats a
/// second. Its Set Time Signature of 3/4 is the loaded meter.
#[test]
fn a_clip_file_loads_as_its_notes_and_its_flex_data_tempo() {
    let division = TicksPerQuarterNote::of(96);
    let track = Track::new()
        .push(
            Tick(0),
            Message::Flex(MidiGroup::FIRST, Flex::SetTempo(FlexTempo::new(50_000_000))),
        )
        .unwrap()
        .push(
            Tick(0),
            Message::Flex(
                MidiGroup::FIRST,
                Flex::SetTimeSignature(FlexTimeSignature {
                    numerator: 3,
                    denominator: 2,
                    thirty_seconds: 8,
                }),
            ),
        )
        .unwrap()
        .note(
            Tick(0),
            Tick(96),
            MidiChannel::FIRST,
            NoteNumber::of(60),
            U7::of(100),
        )
        .note(
            Tick(192),
            Tick(288),
            MidiChannel::FIRST,
            NoteNumber::of(64),
            U7::of(100),
        )
        .ending_at(Tick(288));
    let mut bytes = Vec::new();
    MidiFile::clip(division, track)
        .expect("a clip")
        .write(&mut bytes)
        .expect("a Vec takes the write");
    assert!(bytes.starts_with(b"SMF2CLIP"));

    let loaded = tutti_fixtures::block_on(session().midi().load_midi(bytes)).expect("loads");

    // The Set Tempo and Set Time Signature messages and two notes, a note-on and a
    // note-off each.
    assert_eq!(loaded.track(0).unwrap().events.len(), 6);
    let view = loaded
        .file()
        .timebase()
        .metrical()
        .expect("a metrical clip");
    let notes = loaded.file().tracks()[0].notes();
    let beats: Vec<_> = notes
        .iter()
        .map(|n| (view.beat_at(n.start), view.beat_at(n.end)))
        .collect();
    assert_eq!(
        beats,
        [
            (Beat::new(0.0), Beat::new(1.0)),
            (Beat::new(2.0), Beat::new(3.0))
        ]
    );
    let seconds = loaded.tempo().unwrap().seconds_at(Beat::new(2.0));
    assert!((seconds.get() - 1.0).abs() < 1e-9, "{seconds:?}");
    let meter = loaded.meter().expect("one timeline").current();
    assert_eq!(meter.changes().len(), 1);
    assert_eq!(meter.changes()[0].signature, TimeSignature::of(3, 4));
}

/// A format 1 file lands each track as a clip over the file's one tempo map
/// and meter, and a file that needed no repair reports clean.
#[test]
fn a_format_1_file_lands_each_track_with_its_clip_and_the_shared_timeline() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = smf(
        1,
        &[
            track(&[&TEMPO_120, &THREE_FOUR, &note(0, 60)]),
            track(&[&note(0, 64), &note(0, 67)]),
        ],
    );
    let loaded = load(&mut session(), write(dir.path(), "f1.mid", &bytes)).expect("loads");

    assert_eq!(loaded.file().tracks().len(), 2);
    // A note is its note-on and its note-off.
    assert_eq!(loaded.track(0).unwrap().events.len(), 2);
    assert_eq!(loaded.track(1).unwrap().events.len(), 4);
    assert!(loaded.report().is_clean(), "{:?}", loaded.report());

    // 500 000 microseconds a quarter: two quarters are one second.
    let tempo = loaded.tempo().expect("one timeline");
    assert_eq!(tempo.seconds_at(Beat::new(2.0)), Seconds::new(1.0));
    let meter = loaded.meter().expect("one timeline");
    let changes = meter.current();
    assert_eq!(changes.changes().len(), 1);
    assert_eq!(changes.changes()[0].signature, TimeSignature::of(3, 4));
    for i in 0..2 {
        let track = loaded.track(i).unwrap();
        assert!(std::sync::Arc::ptr_eq(&track.tempo, tempo));
        assert!(std::sync::Arc::ptr_eq(&track.meter, meter));
    }
}

/// A format 2 file's sequences are independent: it has no common timeline,
/// and each track keeps its own tempo map.
#[test]
fn a_format_2_file_has_no_common_timeline_and_each_track_its_own_tempo() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = smf(
        2,
        &[
            track(&[&TEMPO_120, &note(0, 60)]),
            track(&[&TEMPO_60, &note(0, 62)]),
        ],
    );
    let loaded = load(&mut session(), write(dir.path(), "f2.mid", &bytes)).expect("loads");

    assert_eq!(loaded.tempo().unwrap_err(), NoCommonTimelineError::PerTrack);
    assert_eq!(loaded.meter().unwrap_err(), NoCommonTimelineError::PerTrack);
    let (first, second) = (loaded.track(0).unwrap(), loaded.track(1).unwrap());
    assert_eq!(first.tempo.seconds_at(Beat::new(1.0)), Seconds::new(0.5));
    assert_eq!(second.tempo.seconds_at(Beat::new(1.0)), Seconds::new(1.0));
    assert_eq!(first.events.len(), 2);
}

#[test]
fn a_track_past_the_end_is_no_such_track() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = smf(
        1,
        &[track(&[&TEMPO_120, &THREE_FOUR]), track(&[&note(0, 60)])],
    );
    let loaded = load(&mut session(), write(dir.path(), "two.mid", &bytes)).expect("loads");
    assert_eq!(
        loaded.track(2).unwrap_err(),
        TrackError::NoSuch {
            index: 2,
            tracks: 2
        }
    );
}

/// A file with no tempo and no time signature is read as SMF 1.0 section 2.2
/// says (120 beats a minute, 4/4) and the load reports what it added.
#[test]
fn a_repaired_file_reports_what_the_read_added() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = smf(0, &[track(&[&note(0, 60)])]);
    let loaded = load(&mut session(), write(dir.path(), "bare.mid", &bytes)).expect("loads");
    let report = loaded.report();
    assert_eq!(
        report.repairs,
        vec![
            ReadRepair::DefaultTempo { track: 0 },
            ReadRepair::DefaultTimeSignature { track: 0 },
        ]
    );
    assert!(report.dropped.is_empty());
    assert!(!report.is_clean());
    assert_eq!(
        loaded.tempo().unwrap().seconds_at(Beat::new(2.0)),
        Seconds::new(1.0),
        "the 120 beats a minute the repair added"
    );
}

/// A bare `F7` escape has no SysEx7 packet: the load reports it at its beat
/// (tick 240 of 480 a quarter is beat 0.5) and the clip holds the rest.
#[test]
fn an_event_with_no_packet_is_reported_at_its_beat() {
    let dir = tempfile::tempdir().unwrap();
    // Delta 240, F7 escape of the one byte 0x55.
    let escape: [u8; 5] = [0x81, 0x70, 0xF7, 1, 0x55];
    let bytes = smf(
        0,
        &[track(&[&TEMPO_120, &THREE_FOUR, &escape, &note(0, 60)])],
    );
    let loaded = load(&mut session(), write(dir.path(), "esc.mid", &bytes)).expect("loads");
    let dropped = &loaded.report().dropped;
    assert_eq!(dropped.len(), 1);
    assert_eq!((dropped[0].track, dropped[0].beat), (0, Beat::new(0.5)));
    assert_eq!(dropped[0].why, TimelineDrop::BareEscape);
    assert_eq!(loaded.track(0).unwrap().events.len(), 2);
}

/// A Time Signature of numerator 0 is no meter: the load reports it at its
/// beat (tick 960 of 480 a quarter is beat 2) and the meter stays 4/4.
#[test]
fn a_time_signature_no_meter_holds_is_reported_at_its_beat() {
    let dir = tempfile::tempdir().unwrap();
    // Delta 960 (`87 40`), Time Signature 0/4.
    let zero: [u8; 8] = [0x87, 0x40, 0xFF, 0x58, 4, 0, 2, 24];
    let mut zero = zero.to_vec();
    zero.push(8);
    let bytes = smf(0, &[track(&[&TEMPO_120, &zero])]);
    let loaded = load(&mut session(), write(dir.path(), "zero.mid", &bytes)).expect("loads");
    let refused = &loaded.report().refused_time_signatures;
    assert_eq!(refused.len(), 1);
    assert_eq!((refused[0].0, refused[0].1.beat), (None, Beat::new(2.0)));
    assert_eq!(refused[0].1.numerator, 0);
    assert!(!loaded.report().is_clean());
    assert_eq!(loaded.meter().unwrap().current().changes().len(), 1);
}

/// An SMF in SMPTE time with the division word `division` and one track of
/// `events`.
fn smpte_smf(division: u16, events: &[&[u8]]) -> Vec<u8> {
    let mut bytes = b"MThd\0\0\0\x06\0\0\0\x01".to_vec();
    bytes.extend(division.to_be_bytes());
    bytes.extend(track(events));
    bytes
}

/// Where the bare `F7` escape of `file`'s track, the one event with no
/// packet, lands, in beats.
fn escape_beat(loaded: &LoadedMidi) -> Beat {
    let dropped = &loaded.report().dropped;
    assert_eq!(dropped.len(), 1);
    dropped[0].beat
}

/// Delta 1000 (`87 68`), then the F7 escape of the one byte 0x55.
const ESCAPE_AT_1000: [u8; 5] = [0x87, 0x68, 0xF7, 1, 0x55];

/// **SMF 1.0 section 2.1: an SMPTE file's tick is a fixed fraction of a second.**
/// At -25 frames a second and 40 ticks a frame (`E7 28`) a tick is a
/// millisecond, so tick 1000 is 1.0 s and lands on beat 2.0 of the session's
/// 120 BPM map. The file's own Set Tempo (60 BPM) moves nothing.
#[test]
fn an_smpte_file_places_its_seconds_on_the_sessions_tempo_map() {
    let bytes = smpte_smf(0xE728, &[&TEMPO_60, &ESCAPE_AT_1000, &note(0, 60)]);
    let mut s = session();
    let loaded = tutti_fixtures::block_on(s.midi().load_midi(bytes)).expect("loads");
    assert_eq!(escape_beat(&loaded), Beat::new(2.0));
    assert_eq!(loaded.track(0).unwrap().events.len(), 2);
    let tempo = loaded.tempo().expect("one timeline");
    assert_eq!(tempo.seconds_at(Beat::new(2.0)), Seconds::new(1.0));
    assert_eq!(loaded.track(0).unwrap().tempo, *tempo);

    // Format 2: each sequence is placed over the same map.
    let mut bytes = b"MThd\0\0\0\x06\0\x02\0\x02\xE7\x28".to_vec();
    bytes.extend(track(&[&ESCAPE_AT_1000]));
    bytes.extend(track(&[&note(0, 60)]));
    let loaded = tutti_fixtures::block_on(s.midi().load_midi(bytes)).expect("loads");
    assert_eq!(loaded.tempo().unwrap_err(), NoCommonTimelineError::PerTrack);
    assert_eq!(escape_beat(&loaded), Beat::new(2.0));
    assert_eq!(loaded.track(1).unwrap().events.len(), 2);
}

/// **SMF 1.0 section 2.1: "-29 corresponds to 30 drop frame"**, the
/// 30000/1001 rate. At 40 ticks a frame a tick is 1001/1 200 000 s, so tick
/// 1000 is 1.001/1.2 = 0.834166... s: 1001/1200 s, and beat 1001/600 at
/// 120 BPM.
#[test]
fn a_29_97_drop_frame_file_counts_frames_at_30000_over_1001() {
    // -29 is 0xE3.
    let bytes = smpte_smf(0xE328, &[&ESCAPE_AT_1000]);
    let loaded = tutti_fixtures::block_on(session().midi().load_midi(bytes)).expect("loads");
    let beat = escape_beat(&loaded).get();
    assert!((beat - 1001.0 / 600.0).abs() < 1e-12, "{beat}");
}

/// **A tempo change in the session's map moves the beats, not the seconds**:
/// at 60 BPM the same 1.0 s is beat 1.0, and the map the clip plays on still
/// puts that beat at 1.0 s.
#[test]
fn a_tempo_change_in_the_session_moves_an_smpte_files_beats_not_its_seconds() {
    let bytes = smpte_smf(0xE728, &[&ESCAPE_AT_1000]);
    let mut s = session();
    let map = TempoMap::constant(Bpm::of(60.0)).expect("a tempo");
    s.set(s.transport_handle().tempo().to(map)).expect("sets");
    let _published = s.update();
    let loaded = tutti_fixtures::block_on(s.midi().load_midi(bytes)).expect("loads");
    assert_eq!(escape_beat(&loaded), Beat::new(1.0));
    assert_eq!(
        loaded.tempo().unwrap().seconds_at(Beat::new(1.0)),
        Seconds::new(1.0)
    );
}

#[test]
fn a_missing_file_is_an_io_read_error() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = load(&mut session(), dir.path().join("none.mid"));
    assert!(
        matches!(
            &outcome,
            Err(LoadError::Midi(error)) if matches!(**error, tutti_midi_file::ReadError::Io(_))
        ),
        "{outcome:?}"
    );
}

#[cfg(feature = "soundfont")]
mod soundfont {
    use super::*;
    use tutti_session::{LoadError, LoadSoundFont, LoadingSoundFont};
    use tutti_soundfont::{SoundFontConfig, SoundFontUnit};

    /// **A SoundFont load of a dropped session is `ShutDown`.**
    #[test]
    fn a_soundfont_load_of_a_dropped_session_is_shut_down() {
        let bytes = std::fs::read(tutti_fixtures::asset("soundfonts/TimGM6mb.sf2")).unwrap();
        let load = session().load_soundfont(bytes);
        assert!(matches!(
            tutti_fixtures::block_on(load),
            Err(LoadError::ShutDown)
        ));
    }

    /// **The SoundFont load and its future are `Send`.**
    const _: () = {
        const fn send<T: Send>() {}
        send::<LoadSoundFont>();
        send::<LoadingSoundFont>();
    };

    /// The record count of the `phdr` chunk of the SoundFont `bytes` (SF2 2.04
    /// section 7.2): a 38-byte record per preset, and a terminal record.
    fn declared_presets(bytes: &[u8]) -> usize {
        fn chunks(mut body: &[u8]) -> Vec<(&[u8], &[u8])> {
            let mut found = Vec::new();
            while body.len() >= 8 {
                let size = u32::from_le_bytes(body[4..8].try_into().unwrap()) as usize;
                let end = (8 + size).min(body.len());
                found.push((&body[..4], &body[8..end]));
                body = &body[(end + (size & 1)).min(body.len())..];
            }
            found
        }
        let (_, riff) = chunks(bytes).into_iter().next().expect("a RIFF chunk");
        let pdta = chunks(&riff[4..])
            .into_iter()
            .find(|(id, body)| *id == b"LIST" && body.starts_with(b"pdta"))
            .expect("a pdta list");
        let (_, phdr) = chunks(&pdta.1[4..])
            .into_iter()
            .find(|(id, _)| *id == b"phdr")
            .expect("a phdr chunk");
        phdr.len() / 38 - 1
    }

    /// **A load reads the presets the file declares**: the `phdr` record count
    /// less its terminal record, and a unit builds from the handle's font.
    #[test]
    fn load_soundfont_reads_the_presets_the_file_declares() {
        let path = tutti_fixtures::asset("soundfonts/TimGM6mb.sf2");
        let declared = declared_presets(&std::fs::read(&path).unwrap());
        assert!(declared > 0);
        let font = tutti_fixtures::block_on(session().load_soundfont(path)).unwrap();
        assert_eq!(font.font().presets().len(), declared);
        let unit = SoundFontUnit::<f32>::try_from(SoundFontConfig::new(std::sync::Arc::clone(
            font.font(),
        )));
        assert!(unit.is_ok(), "{:?}", unit.err());
    }

    #[test]
    fn a_soundfont_loads_from_bytes_and_never_changes() {
        let bytes = std::fs::read(tutti_fixtures::asset("soundfonts/TimGM6mb.sf2")).unwrap();
        let declared = declared_presets(&bytes);
        let font = tutti_fixtures::block_on(session().load_soundfont(bytes)).unwrap();
        assert_eq!(font.font().presets().len(), declared);
        assert!(!font.changed(), "an asset of bytes never changes");
    }

    #[test]
    fn a_file_that_is_no_soundfont_is_a_soundfont_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk.sf2");
        std::fs::write(&path, b"this is not a RIFF file at all").unwrap();
        assert!(matches!(
            tutti_fixtures::block_on(session().load_soundfont(path)),
            Err(LoadError::SoundFont(_))
        ));
    }

    /// **`changed` is the file's identity moving**: its modification time here.
    #[test]
    fn a_soundfont_is_changed_once_its_file_is_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gm.sf2");
        std::fs::copy(tutti_fixtures::asset("soundfonts/TimGM6mb.sf2"), &path).unwrap();
        let at = |seconds: u64| {
            std::fs::File::options()
                .write(true)
                .open(&path)
                .and_then(|file| {
                    file.set_modified(
                        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds),
                    )
                })
                .unwrap();
        };
        at(1_000);
        let font = tutti_fixtures::block_on(session().load_soundfont(&path)).unwrap();
        assert!(!font.changed());
        at(2_000);
        assert!(font.changed());
    }
}
