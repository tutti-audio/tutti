//! A MIDI file load is a job: `Midi::load_midi` returns a `Job<LoadMidi>` at
//! once and the `LoadedMidi` arrives through `JobOutput::take` in `update()`, or
//! `Job::wait` returns it. Each track arrives as a clip value built on the
//! worker. A SoundFont is a pool entry, loaded the pool's way.
//!
//! The fixtures are bytes written by hand, so every figure is the file's own
//! (SMF 1.0): ticks over the division are beats, a Set Tempo of 500 000
//! microseconds is a quarter note in half a second.

use std::path::{Path, PathBuf};
use std::time::Duration;
use tutti_platform::Instant;

use tutti_midi_file::{Normalized, TimelineDrop};
use tutti_music::TimeSignature;
use tutti_session::{
    DetachedSession, LoadMidiError, LoadedMidi, NoCommonTimelineError, Session, SessionEvent,
    TrackError,
};
use tutti_units::{Beat, SampleRate, Seconds};

fn session() -> (DetachedSession, tutti_session::Background) {
    let (session, mut background) = Session::builder()
        .build_detached(SampleRate::SR_48K)
        .expect("builds");
    background
        .spawn_threads(std::num::NonZeroUsize::MIN)
        .expect("a background thread starts");
    (session, background)
}

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

/// Loads the file at `path` and waits for it.
fn load(
    s: &mut DetachedSession,
    background: &mut tutti_session::Background,
    path: PathBuf,
) -> Result<LoadedMidi, LoadMidiError> {
    let job = s.midi().load_midi(path);
    job.wait(s, background)
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
    let (mut s, mut bg) = session();
    let job = s.midi().load_midi(bytes);
    let loaded = job.wait(&mut s, &mut bg).expect("loads");
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
    let (mut s, mut bg) = session();
    let loaded = load(&mut s, &mut bg, write(dir.path(), "f1.mid", &bytes)).expect("loads");

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
    let (mut s, mut bg) = session();
    let loaded = load(&mut s, &mut bg, write(dir.path(), "f2.mid", &bytes)).expect("loads");

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
    let (mut s, mut bg) = session();
    let loaded = load(&mut s, &mut bg, write(dir.path(), "two.mid", &bytes)).expect("loads");
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
    let (mut s, mut bg) = session();
    let loaded = load(&mut s, &mut bg, write(dir.path(), "bare.mid", &bytes)).expect("loads");
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
    let (mut s, mut bg) = session();
    let loaded = load(&mut s, &mut bg, write(dir.path(), "esc.mid", &bytes)).expect("loads");
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
    let (mut s, mut bg) = session();
    let loaded = load(&mut s, &mut bg, write(dir.path(), "zero.mid", &bytes)).expect("loads");
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
    let (mut s, mut bg) = session();
    let loaded = load(&mut s, &mut bg, write(dir.path(), "smpte.mid", bytes)).expect("loads");
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
fn a_missing_file_lands_a_read_error() {
    let dir = tempfile::tempdir().unwrap();
    let (mut s, mut bg) = session();
    let outcome = load(&mut s, &mut bg, dir.path().join("none.mid"));
    assert!(
        matches!(outcome, Err(LoadMidiError::Read(_))),
        "{outcome:?}"
    );
}

/// The result arrives in `update()` through `JobOutput::take`, each load with its
/// own job and file.
#[test]
fn two_loads_land_in_update_each_for_its_own_job() {
    let dir = tempfile::tempdir().unwrap();
    let a = write(
        dir.path(),
        "a.mid",
        &smf(0, &[track(&[&TEMPO_120, &THREE_FOUR, &note(0, 60)])]),
    );
    let b = write(
        dir.path(),
        "b.mid",
        &smf(
            0,
            &[track(&[
                &TEMPO_120,
                &THREE_FOUR,
                &note(0, 60),
                &note(0, 62),
            ])],
        ),
    );
    let (mut s, _bg) = session();
    let (ja, jb) = (s.midi().load_midi(a), s.midi().load_midi(b));
    assert_ne!(ja.id(), jb.id());
    let (mut got_a, mut got_b) = (None, None);
    let deadline = Instant::now() + Duration::from_secs(10);
    while got_a.is_none() || got_b.is_none() {
        assert!(Instant::now() < deadline, "the loads never landed");
        for event in s.update() {
            if let SessionEvent::JobDone(mut output) = event {
                got_a = got_a.or(output.take(&ja));
                got_b = got_b.or(output.take(&jb));
            }
        }
        std::thread::yield_now();
    }
    let events = |got: Option<Result<LoadedMidi, LoadMidiError>>| {
        got.unwrap().unwrap().track(0).unwrap().events.len()
    };
    assert_eq!((events(got_a), events(got_b)), (2, 4));
}

#[cfg(feature = "soundfont")]
mod soundfont {
    use super::*;
    use tutti_session::{LoadError, PoolState, soundfont};
    use tutti_soundfont::{Settings, SoundFontUnit};

    /// Updates until the entry under `key` is no longer pending.
    fn settled(s: &mut DetachedSession, key: &'static str) -> PoolState {
        let deadline = tutti_platform::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let _ = s.update();
            match s.pool_state(&key) {
                Some(PoolState::Loading) => {}
                Some(state) => return state,
                None => panic!("the entry is not declared"),
            }
            assert!(
                tutti_platform::Instant::now() < deadline,
                "the font never landed"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn a_soundfont_entry_loads_a_font_a_unit_is_built_from() {
        let (mut s, _bg) = session();
        s.pool(soundfont(
            "gm",
            tutti_fixtures::asset("soundfonts/TimGM6mb.sf2"),
        ))
        .unwrap();
        assert!(matches!(settled(&mut s, "gm"), PoolState::Loaded));
        let font = s.soundfont(&"gm").unwrap();
        let unit = SoundFontUnit::<f32>::new(font, Settings::new());
        assert!(unit.is_ok(), "{:?}", unit.err());
    }

    #[test]
    fn a_soundfont_entry_loads_from_bytes() {
        let bytes = std::fs::read(tutti_fixtures::asset("soundfonts/TimGM6mb.sf2")).unwrap();
        let (mut s, _bg) = session();
        s.pool(soundfont("gm", bytes)).unwrap();
        assert!(matches!(settled(&mut s, "gm"), PoolState::Loaded));
        assert!(!s.soundfont(&"gm").unwrap().presets().is_empty());
    }

    #[test]
    fn a_file_that_is_no_soundfont_fails_its_entry_with_a_soundfont_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk.sf2");
        std::fs::write(&path, b"this is not a RIFF file at all").unwrap();
        let (mut s, _bg) = session();
        s.pool(soundfont("junk", path)).unwrap();
        assert!(matches!(
            settled(&mut s, "junk"),
            PoolState::Failed(LoadError::SoundFont(_))
        ));
    }
}
