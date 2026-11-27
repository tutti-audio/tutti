//! `Bounce::replace`: a file that exists at a bounce's path is refused before anything is
//! rendered, unless the bounce replaces it.

use crate::bounced::bounced;
use tutti_buffers::ChannelLayout;
use tutti_buffers::FloatWidth;
use tutti_codec::Encoding;
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
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(RATE)
        .expect("a detached session builds")
}

fn float_wav() -> Format {
    Format::Wav(Pcm::Float(FloatWidth::F32))
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

/// **A bounce onto an existing main output is refused with `Exists`, the path keeps its old
/// bytes, and nothing is left beside it.**
#[test]
fn a_bounce_onto_an_existing_main_output_is_refused_and_leaves_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let main_output = dir.path().join("main_output.wav");
    std::fs::write(&main_output, b"old bytes").unwrap();
    let mut session = session();
    let node = session.graph().add(ForkByClone(Const::mono(0.5)));
    session.graph().connect(node, MainOutput).unwrap();
    let error = bounced!(
        &mut session,
        bounce().main_output(tutti_codec::ToFile::new(
            &main_output,
            Encoding::new(float_wav()),
        )),
    )
    .map(|_| ())
    .expect_err("the path exists");
    assert!(
        matches!(
            &error,
            BounceError::Export(tutti_export::RenderError::Encode(
                tutti_codec::EncodeError::Exists { path }
            )) if *path == main_output
        ),
        "{error:?}"
    );
    assert_eq!(std::fs::read(&main_output).unwrap(), b"old bytes");
    assert_eq!(names(&dir), [std::ffi::OsString::from("main_output.wav")]);
}

/// **A bounce with `replace` replaces what is at its path and leaves no temporary file
/// beside it.**
#[test]
fn a_replacing_bounce_replaces_an_existing_main_output_and_leaves_nothing_beside_it() {
    let dir = tempfile::tempdir().unwrap();
    let main_output = dir.path().join("main_output.wav");
    std::fs::write(&main_output, b"old bytes").unwrap();
    let mut session = session();
    let node = session.graph().add(ForkByClone(Const::mono(0.5)));
    session.graph().connect(node, MainOutput).unwrap();
    bounced!(
        &mut session,
        bounce().main_output(
            tutti_codec::ToFile::new(&main_output, Encoding::new(float_wav())).replace(),
        ),
    )
    .expect("renders");
    let planes = tutti_fixtures::signal::read_wav_f32(&main_output);
    assert!(planes.iter().all(|plane| plane.len() == FRAMES));
    assert_eq!(names(&dir), [std::ffi::OsString::from("main_output.wav")]);
}
