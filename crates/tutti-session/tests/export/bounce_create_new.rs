//! `Bounce::create_new`: a file that exists at a bounce's path is refused at the
//! commit, and the bounce's whole file is kept beside it.

use crate::bounced::bounced;
use tutti_buffers::ChannelLayout;
use tutti_codec::{Format, Pcm};
use tutti_graph::ForkByClone;
use tutti_nodes::testing::Const;
use tutti_session::{
    Bounce, BounceError, BounceLatency, BounceTail, DetachedSession, ExportClock, MainOutput,
    Range, Session,
};
use tutti_units::{SampleRate, Seconds};

const RATE: SampleRate = SampleRate::SR_48K;
/// 10 ms at 48 kHz.
const FRAMES: usize = 480;

fn session() -> DetachedSession {
    Session::builder()
        .inputs(ChannelLayout::EMPTY)
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(RATE)
        .expect("a detached session builds")
}

fn float_wav() -> Format {
    Format::Wav(Pcm::Float32)
}

fn names(dir: &tempfile::TempDir) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

fn bounce() -> Bounce {
    Bounce::new()
        .range(Range::Seconds(Seconds::new(0.0), Seconds::new(0.01)))
        .clock(ExportClock::frozen())
        .latency(BounceLatency::Keep)
        .tail(BounceTail::None)
}

/// **A bounce with `create_new` onto an existing main output fails with an
/// `AlreadyExists` commit, the path keeps its old bytes, and the kept file
/// holds the bounce's frames**: the node's constant 0.5, `FRAMES` long.
#[test]
fn a_create_new_bounce_onto_an_existing_main_output_keeps_the_bounce_and_leaves_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let main_output = dir.path().join("main_output.wav");
    std::fs::write(&main_output, b"old bytes").unwrap();
    let mut session = session();
    let node = session.graph().add(ForkByClone(Const::mono(0.5)));
    session.graph().connect(node, MainOutput).unwrap();
    let error = bounced(
        &mut session,
        bounce().main_output(&main_output, float_wav()).create_new(),
    )
    .map(|_| ())
    .expect_err("the path exists");
    let BounceError::Export(tutti_export::RenderError::Encode(tutti_codec::EncodeError::Commit(
        commit,
    ))) = &error
    else {
        panic!("not a commit failure: {error:?}");
    };
    assert_eq!(commit.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&main_output).unwrap(), b"old bytes");
    assert_eq!(
        commit.kept().parent(),
        main_output.parent(),
        "kept beside it"
    );
    let planes = tutti_fixtures::signal::read_wav_f32(commit.kept());
    assert!(planes.iter().all(|plane| plane.len() == FRAMES));
    assert!(planes.iter().flatten().all(|&s| s == 0.5));
    assert_eq!(names(&dir).len(), 2, "the path and the kept file only");
}

/// **A bounce without `create_new` replaces what is at its path and leaves no
/// temporary file beside it.**
#[test]
fn a_bounce_replaces_an_existing_main_output_and_leaves_nothing_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let main_output = dir.path().join("main_output.wav");
    std::fs::write(&main_output, b"old bytes").unwrap();
    let mut session = session();
    let node = session.graph().add(ForkByClone(Const::mono(0.5)));
    session.graph().connect(node, MainOutput).unwrap();
    bounced(
        &mut session,
        bounce().main_output(&main_output, float_wav()),
    )
    .expect("renders");
    let planes = tutti_fixtures::signal::read_wav_f32(&main_output);
    assert!(planes.iter().all(|plane| plane.len() == FRAMES));
    assert_eq!(names(&dir), [std::ffi::OsString::from("main_output.wav")]);
}
