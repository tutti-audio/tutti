//! A MIDI file load is a value: `Midi::load_midi` returns a `LoadMidi` and
//! `run()` returns the `LoadedMidi`, each track a clip value. A SoundFont is a
//! value too: `load_soundfont` returns a `LoadSoundFont`.
//!
//! The fixtures are bytes written by hand, so every figure is the file's own
//! (SMF 1.0): ticks over the division are beats, a Set Tempo of 500 000
//! microseconds is a quarter note in half a second.

use std::path::{Path, PathBuf};

use tutti_midi_file::{Normalized, TimelineDrop};
use tutti_music::TimeSignature;
use tutti_session::{
    DetachedSession, LoadError, LoadedMidi, NoCommonTimelineError, Session, TrackError,
};
use tutti_units::{Beat, SampleRate, Seconds};

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
        report.normalized,
        vec![
            Normalized::DefaultTempo { track: 0 },
            Normalized::DefaultTimeSignature { track: 0 },
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

/// A file in SMPTE time has seconds and no beats: it lands, and each
/// of its tracks and timeline answers `SmpteTiming`.
#[test]
fn a_file_in_smpte_time_lands_and_says_why_it_has_no_clip_or_timeline() {
    let dir = tempfile::tempdir().unwrap();
    // -25 frames a second, 40 ticks a frame (`E7 28`).
    let bytes = b"MThd\0\0\0\x06\0\0\0\x01\xE7\x28MTrk\0\0\0\x04\0\xFF\x2F\0";
    let loaded = load(&mut session(), write(dir.path(), "smpte.mid", bytes)).expect("loads");
    assert_eq!(loaded.track(0).unwrap_err(), TrackError::SmpteTiming);
    assert_eq!(
        loaded.tempo().unwrap_err(),
        NoCommonTimelineError::SmpteTiming
    );
    assert_eq!(
        loaded.meter().unwrap_err(),
        NoCommonTimelineError::SmpteTiming
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
    use tutti_soundfont::Settings;

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
        let unit = Settings::new().build::<f32>(std::sync::Arc::clone(font.font()));
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
