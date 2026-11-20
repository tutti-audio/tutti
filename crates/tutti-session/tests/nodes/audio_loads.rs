//! `load_audio` is a value the app runs anywhere and its handle is the app's:
//! two loads of one file are two waves, the wave is freed with the last clone,
//! `changed` reads the file's metadata, and a streamed load opens a stream.

use std::f64::consts::TAU;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tutti_platform::{Asset, Open, ReadSeek};
use tutti_sampler::StreamStart;
use tutti_session::{LoadError, Session, Tier, WorkerError};
use tutti_units::SampleRate;

const RATE: f64 = 48_000.0;
/// The sine's frequency: a period is a whole 48 frames.
const SINE_HZ: f64 = 1_000.0;
const AMPLITUDE: f64 = 0.5;
const FRAMES: usize = 4_800;

/// The test's own scratch under the target dir.
fn dir() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).expect("a temp dir")
}

/// Frame `n` of the sine, in `f64`.
fn sine(n: usize) -> f64 {
    AMPLITUDE * (TAU * SINE_HZ * n as f64 / RATE).sin()
}

fn write_sine(path: &Path) {
    let samples: Vec<f32> = (0..FRAMES).map(|n| sine(n) as f32).collect();
    tutti_fixtures::signal::write_wav_f32(path, SampleRate::of(RATE), 1, &samples);
}

fn session() -> Session {
    Session::builder()
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("builds")
}

fn at(path: &Path, seconds: u64) {
    File::options()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)))
        .expect("sets the modification time");
}

/// **A load holds nothing of the session, so it runs on any thread.** The wave
/// it returns is the file's sine: frame `n` is `0.5 * sin(2 pi * 1000 n / 48000)`.
#[test]
fn load_audio_runs_where_the_app_runs_it() {
    let dir = dir();
    let path = dir.path().join("sine.wav");
    write_sine(&path);
    let load = session().load_audio(&path).tier(Tier::Resident);
    let audio = std::thread::spawn(move || load.run())
        .join()
        .expect("the thread ends")
        .expect("loads");
    let wave = audio.wave().expect("a resident load");
    assert_eq!(wave.len().get(), FRAMES);
    for n in 0..FRAMES {
        let error = (f64::from(wave.at(0, n)) - sine(n)).abs();
        assert!(error < 1e-7, "frame {n} is {error} off the sine");
    }
}

/// **Two loads of one file are two values**: the session keeps no cache, so each
/// decodes its own wave.
#[test]
fn two_loads_of_one_file_are_two_values() {
    let dir = dir();
    let path = dir.path().join("sine.wav");
    write_sine(&path);
    let s = session();
    let load = || {
        s.load_audio(&path)
            .tier(Tier::Resident)
            .run()
            .expect("loads")
    };
    let (first, second) = (load(), load());
    assert!(!Arc::ptr_eq(
        first.wave().expect("resident"),
        second.wave().expect("resident")
    ));
    let clone = first.clone();
    assert!(Arc::ptr_eq(
        first.wave().expect("resident"),
        clone.wave().expect("resident")
    ));
}

/// **A handle outlives nothing it was not given**: the session holds no clone,
/// so dropping every clone of the handle frees the wave.
#[test]
fn a_handle_outlives_nothing_it_was_not_given() {
    let dir = dir();
    let path = dir.path().join("sine.wav");
    write_sine(&path);
    let s = session();
    let audio = s
        .load_audio(&path)
        .tier(Tier::Resident)
        .run()
        .expect("loads");
    let weak = Arc::downgrade(audio.wave().expect("resident"));
    let clone = audio.clone();
    drop(audio);
    assert!(weak.upgrade().is_some(), "a clone keeps the wave");
    drop(clone);
    assert!(weak.upgrade().is_none(), "the last clone frees it");
}

/// **`changed` compares the asset's identity (path, length, modification time)
/// now with the one taken before the load.** A rewrite that moves only the
/// modification time reads changed; bytes never do.
#[test]
fn changed_is_false_until_the_file_is_rewritten_and_true_after() {
    let dir = dir();
    let path = dir.path().join("sine.wav");
    write_sine(&path);
    at(&path, 1_000);
    let s = session();
    let audio = s.load_audio(&path).run().expect("loads");
    assert!(!audio.changed(), "nothing was written since the load");
    write_sine(&path);
    at(&path, 2_000);
    assert!(audio.changed(), "the modification time moved");
    let fresh = s.load_audio(audio.asset().clone()).run().expect("loads");
    assert!(!fresh.changed(), "a reload reads the file as it is");

    let bytes = std::fs::read(&path).expect("reads");
    let held = s.load_audio(bytes).run().expect("loads");
    assert!(!held.changed(), "an asset of bytes never changes");
}

/// **A streamed load reads the header, and its asset opens a stream** on the
/// session's streamer.
#[test]
fn a_streamed_handle_opens_a_stream() {
    let dir = dir();
    let path = dir.path().join("sine.wav");
    write_sine(&path);
    let mut s = session();
    let audio = s
        .load_audio(&path)
        .tier(Tier::Streamed)
        .run()
        .expect("loads");
    assert!(audio.wave().is_none(), "a streamed load decodes nothing");
    let info = audio.stream().expect("a streamed load");
    assert_eq!(info.facts().frames.map(|f| f.get()), Some(FRAMES));
    let opened = s
        .open_stream(info.as_ref(), StreamStart::HEAD)
        .expect("a streamer")
        .run()
        .expect("the file reads");
    let (stream, reader) = s.apply(opened).expect("the session serves it");
    assert_eq!(stream.id(), reader.id());
}

/// An asset whose reader panics: a decoder that does.
struct Panics;

impl Open for Panics {
    fn open(&self) -> io::Result<Box<dyn ReadSeek>> {
        panic!("the decoder panicked");
    }

    fn name(&self) -> String {
        "panics".to_owned()
    }
}

/// **A panic in a decoder is a load error**, naming the panic, and not an
/// unwind through the app's thread.
#[test]
fn a_decoder_panic_is_a_load_error() {
    let asset = Asset::from(Arc::new(Panics) as Arc<dyn Open>);
    let outcome = session().load_audio(asset).tier(Tier::Resident).run();
    match outcome {
        Err(LoadError::Job(WorkerError::Panicked { message })) => {
            assert!(message.contains("the decoder panicked"), "{message}");
        }
        other => panic!("a panic is a load error, got {:?}", other.map(|_| ())),
    }
}

/// **A file that is no audio is a decode error**, and a missing one too.
#[test]
fn a_file_that_cannot_be_read_is_a_load_error() {
    let dir = dir();
    let path: PathBuf = dir.path().join("none.wav");
    let s = session();
    assert!(matches!(
        s.load_audio(&path).tier(Tier::Resident).run(),
        Err(LoadError::Decode(_))
    ));
    std::fs::write(&path, b"not audio").expect("writes");
    assert!(matches!(
        s.load_audio(&path).tier(Tier::Streamed).run(),
        Err(LoadError::Probe(_))
    ));
}
