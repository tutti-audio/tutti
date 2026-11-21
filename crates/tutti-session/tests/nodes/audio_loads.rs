//! `load_audio` is a value the app awaits anywhere and its handle is the app's:
//! two loads of one file are two waves, the wave is freed with the last clone,
//! `changed` reads the file's metadata, and a streamed load opens a stream.

use std::f64::consts::TAU;
use std::fs::File;
use std::future::IntoFuture;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use tutti_platform::{Asset, Open, ReadSeek};
use tutti_sampler::StreamStart;
use tutti_session::{AudioData, LoadAudio, LoadError, LoadingAudio, Session, Tier};
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

/// **A load holds nothing of the session but its executor, so any thread
/// awaits it.** The wave it resolves to is the file's sine: frame `n` is
/// `0.5 * sin(2 pi * 1000 n / 48000)`.
#[test]
fn load_audio_is_awaited_on_any_thread() {
    let dir = dir();
    let path = dir.path().join("sine.wav");
    write_sine(&path);
    let s = session();
    let load = s.load_audio(&path).tier(Tier::Resident);
    let audio = std::thread::spawn(move || tutti_fixtures::block_on(load))
        .join()
        .expect("the thread ends")
        .expect("loads");
    let AudioData::Resident(wave) = audio.data() else {
        panic!("a resident load");
    };
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
    let load =
        || tutti_fixtures::block_on(s.load_audio(&path).tier(Tier::Resident)).expect("loads");
    let (first, second) = (load(), load());
    let clone = first.clone();
    let (AudioData::Resident(first), AudioData::Resident(second), AudioData::Resident(clone)) =
        (first.data(), second.data(), clone.data())
    else {
        panic!("resident loads");
    };
    assert!(!Arc::ptr_eq(first, second));
    assert!(Arc::ptr_eq(first, clone));
}

/// **The session keeps no clone of a loaded wave**: the session holds no clone,
/// so dropping every clone of the handle frees the wave.
#[test]
fn the_session_keeps_no_clone_of_a_loaded_wave() {
    let dir = dir();
    let path = dir.path().join("sine.wav");
    write_sine(&path);
    let s = session();
    let audio = tutti_fixtures::block_on(s.load_audio(&path).tier(Tier::Resident)).expect("loads");
    let AudioData::Resident(wave) = audio.data() else {
        panic!("a resident load");
    };
    let weak = Arc::downgrade(wave);
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
    let audio = tutti_fixtures::block_on(s.load_audio(&path)).expect("loads");
    assert!(!audio.changed(), "nothing was written since the load");
    write_sine(&path);
    at(&path, 2_000);
    assert!(audio.changed(), "the modification time moved");
    let fresh = tutti_fixtures::block_on(s.load_audio(audio.asset().clone())).expect("loads");
    assert!(!fresh.changed(), "a reload reads the file as it is");

    let bytes = std::fs::read(&path).expect("reads");
    let held = tutti_fixtures::block_on(s.load_audio(bytes)).expect("loads");
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
    let audio = tutti_fixtures::block_on(s.load_audio(&path).tier(Tier::Streamed)).expect("loads");
    let AudioData::Streamed(info) = audio.data() else {
        panic!("a streamed load decodes nothing");
    };
    assert_eq!(info.frames.map(|f| f.get()), Some(FRAMES));
    let opened = tutti_fixtures::block_on(
        s.open_stream(audio.asset().clone(), StreamStart::HEAD)
            .expect("a streamer"),
    )
    .expect("the file reads");
    let (stream, reader) = s.apply(opened).expect("the session serves it");
    assert_eq!(stream.id(), reader.id());
}

/// **A file removed after the load reads changed**: its identity is gone, so
/// the handle no longer matches its asset, and a reload names why.
#[test]
fn a_file_removed_after_the_load_reads_changed() {
    let dir = dir();
    let path = dir.path().join("sine.wav");
    write_sine(&path);
    let s = session();
    let audio = tutti_fixtures::block_on(s.load_audio(&path)).expect("loads");
    assert!(!audio.changed());
    std::fs::remove_file(&path).expect("removes");
    assert!(audio.changed(), "the file is gone");
    assert!(matches!(
        tutti_fixtures::block_on(s.load_audio(audio.asset().clone()).tier(Tier::Resident)),
        Err(LoadError::Decode(_))
    ));
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
    let outcome = tutti_fixtures::block_on(session().load_audio(asset).tier(Tier::Resident));
    match outcome {
        Err(LoadError::Panicked { message }) => {
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
        tutti_fixtures::block_on(s.load_audio(&path).tier(Tier::Resident)),
        Err(LoadError::Decode(_))
    ));
    std::fs::write(&path, b"not audio").expect("writes");
    assert!(matches!(
        tutti_fixtures::block_on(s.load_audio(&path).tier(Tier::Streamed)),
        Err(LoadError::Probe(_))
    ));
}

/// An asset of the sine file's bytes that counts how often it is opened.
struct Counting {
    bytes: Vec<u8>,
    opens: Arc<AtomicUsize>,
}

impl Open for Counting {
    fn open(&self) -> io::Result<Box<dyn ReadSeek>> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(io::Cursor::new(self.bytes.clone())))
    }

    fn name(&self) -> String {
        "counting".to_owned()
    }
}

fn counting(dir: &Path) -> (Asset, Arc<AtomicUsize>) {
    let path = dir.join("sine.wav");
    write_sine(&path);
    let opens = Arc::new(AtomicUsize::new(0));
    let asset = Counting {
        bytes: std::fs::read(&path).expect("reads"),
        opens: Arc::clone(&opens),
    };
    (Asset::from(Arc::new(asset) as Arc<dyn Open>), opens)
}

/// **A load of a session that was dropped is `ShutDown`**: the executor is gone,
/// so nothing reads the file, and the file did not fail.
#[test]
fn a_load_of_a_dropped_session_is_shut_down() {
    let (asset, opens) = counting(dir().path());
    let load = session().load_audio(asset).tier(Tier::Resident);
    assert!(matches!(
        tutti_fixtures::block_on(load),
        Err(LoadError::ShutDown)
    ));
    assert_eq!(opens.load(Ordering::SeqCst), 0, "nothing read the file");
}

/// **A load dropped before its first poll reads nothing**, and one awaited
/// reads the file once.
#[test]
fn a_load_dropped_before_its_first_poll_never_opens_the_file() {
    let dir = dir();
    let (dropped, dropped_opens) = counting(dir.path());
    let (awaited, awaited_opens) = counting(dir.path());
    let s = session();
    drop(s.load_audio(dropped.clone()).tier(Tier::Resident));
    drop(s.load_audio(dropped).tier(Tier::Resident).into_future());
    tutti_fixtures::block_on(s.load_audio(awaited).tier(Tier::Resident)).expect("loads");
    assert_eq!(
        awaited_opens.load(Ordering::SeqCst),
        1,
        "an awaited load reads once"
    );
    assert_eq!(
        dropped_opens.load(Ordering::SeqCst),
        0,
        "a dropped one reads nothing"
    );
}

/// **The load and its future are `Send`**, so any thread awaits them.
const fn send<T: Send>() {}
const _: () = {
    send::<LoadAudio<f32>>();
    send::<LoadingAudio<f32>>();
};
