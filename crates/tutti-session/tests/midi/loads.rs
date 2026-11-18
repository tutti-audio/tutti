//! A MIDI file load is a job: `Midi::load_file` returns a `MidiLoad` at once
//! and the result lands as `MidiReport::FileLoaded` carrying that id, in
//! `update()`. A SoundFont is a pool entry, loaded the pool's way.

use std::path::Path;
use std::time::{Duration, Instant};

use tutti_midi_file::{MidiFile, SmfDivision, SmfFormat, Tick, Track};
use tutti_session::{
    DetachedSession, MidiError, MidiLoad, MidiReport, Session, SessionEvent, WorkerError,
};
use tutti_types::SampleRate;
use tutti_ump::{MidiChannel, NoteNumber, U7};

fn session() -> DetachedSession {
    Session::builder()
        .build_detached(SampleRate::SR_48K)
        .expect("builds")
}

/// A one-note SMF written to `dir`; returns the file as built.
fn write_file(dir: &Path, name: &str, key: u8) -> (std::path::PathBuf, MidiFile) {
    let track = Track::new().note(
        Tick(0),
        Tick(480),
        MidiChannel::FIRST,
        NoteNumber::of(key),
        U7::of(100),
    );
    let file = MidiFile::smf(
        SmfFormat::SingleTrack,
        SmfDivision::per_quarter(480),
        vec![track],
    )
    .expect("builds");
    let path = dir.join(name);
    file.write(&path).expect("writes");
    (path, file)
}

/// Updates until `want` events of `pick` have landed, or panics after 10 s.
fn land<T>(
    s: &mut DetachedSession,
    want: usize,
    mut pick: impl FnMut(MidiReport) -> Option<T>,
) -> Vec<T> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = Vec::new();
    while got.len() < want {
        assert!(Instant::now() < deadline, "the load never landed");
        for event in s.update() {
            if let SessionEvent::Midi(event) = event
                && let Some(t) = pick(event)
            {
                got.push(t);
            }
        }
        std::thread::yield_now();
    }
    got
}

fn files(event: MidiReport) -> Option<(MidiLoad, Result<MidiFile, MidiError>)> {
    match event {
        MidiReport::FileLoaded { load, file } => Some((load, file)),
        _ => None,
    }
}

/// A file lands with the id `load_file` returned, holding what was written,
/// and not before `update()`.
#[test]
fn a_file_lands_in_update_with_its_id_and_its_tracks() {
    let dir = tempfile::tempdir().unwrap();
    let (path, built) = write_file(dir.path(), "a.mid", 60);
    let mut s = session();
    let id = s.midi().load_file(path);
    let mut landed = land(&mut s, 1, files);
    let (load, file) = landed.pop().unwrap();
    assert_eq!(load, id);
    assert_eq!(file.expect("reads").tracks(), built.tracks());
}

#[test]
fn a_missing_file_lands_as_a_file_error_with_its_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = session();
    let id = s.midi().load_file(dir.path().join("none.mid"));
    let (load, file) = land(&mut s, 1, files).pop().unwrap();
    assert_eq!(load, id);
    assert!(matches!(file, Err(MidiError::Read(_))), "{file:?}");
}

#[test]
fn two_loads_land_each_with_its_own_id_and_file() {
    let dir = tempfile::tempdir().unwrap();
    let (pa, fa) = write_file(dir.path(), "a.mid", 60);
    let (pb, fb) = write_file(dir.path(), "b.mid", 72);
    assert_ne!(fa.tracks(), fb.tracks());
    let mut s = session();
    let a = s.midi().load_file(pa);
    let b = s.midi().load_file(pb);
    assert_ne!(a, b);
    for (load, file) in land(&mut s, 2, files) {
        let want = if load == a { &fa } else { &fb };
        assert_eq!(file.unwrap().tracks(), want.tracks());
    }
}

/// A spawner that drops the job lands the load as `Job(Dropped)`.
#[test]
fn a_dropped_job_lands_as_a_job_error() {
    let mut s = Session::builder()
        .spawn_with(drop)
        .build_detached(SampleRate::SR_48K)
        .expect("builds");
    let id = s.midi().load_file("never-read.mid");
    let (load, file) = land(&mut s, 1, files).pop().unwrap();
    assert_eq!(load, id);
    assert!(
        matches!(file, Err(MidiError::Job(WorkerError::Dropped))),
        "{file:?}"
    );
}

#[cfg(feature = "soundfont")]
mod soundfont {
    use super::*;
    use tutti_session::{LoadError, PoolState, soundfont};
    use tutti_soundfont::{Settings, SoundFontUnit};

    /// Updates until the entry under `key` is no longer pending.
    fn settled(s: &mut DetachedSession, key: &'static str) -> PoolState {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let _ = s.update();
            match s.pool_state(&key) {
                Some(PoolState::Loading) => {}
                Some(state) => return state,
                None => panic!("the entry is not declared"),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the font never landed"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn a_soundfont_entry_loads_a_font_a_unit_is_built_from() {
        let mut s = session();
        s.pool(soundfont(
            "gm",
            tutti_fixtures::asset("soundfonts/TimGM6mb.sf2"),
        ))
        .unwrap();
        assert!(matches!(settled(&mut s, "gm"), PoolState::Loaded));
        let font = s.soundfont(&"gm").unwrap();
        let unit = SoundFontUnit::new(font, Settings::new());
        assert!(unit.is_ok(), "{:?}", unit.err());
    }

    #[test]
    fn a_file_that_is_no_soundfont_fails_its_entry_with_a_soundfont_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk.sf2");
        std::fs::write(&path, b"this is not a RIFF file at all").unwrap();
        let mut s = session();
        s.pool(soundfont("junk", path)).unwrap();
        assert!(matches!(
            settled(&mut s, "junk"),
            PoolState::Failed(LoadError::SoundFont(_))
        ));
    }
}
