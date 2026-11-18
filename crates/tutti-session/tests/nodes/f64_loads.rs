//! An `f64` session serves the pool and the spawner: an `audio_file` entry
//! loads, and a disk stream opens, lands as `OpenedF64` and its clip renders
//! into `f64` frames beside a figure `f32` cannot hold.

use std::time::Duration;

use tutti_buffers::InterleavedMut;
use tutti_fixtures::wait_until;
use tutti_graph::ForkByClone;
use tutti_sampler::Placement;
use tutti_session::{
    AudioEntry, Offline, OutPort, PoolState, Root, SamplerEvent, SessionBuilder, SessionEvent,
    audio_file,
};
use tutti_types::ChannelLayout;
use tutti_units::{Beat, SampleRate};

#[path = "../support/f64_nodes.rs"]
mod f64_nodes;
use f64_nodes::{Dc, FINE};

const RATE: f64 = 48_000.0;
const LEN: usize = 4_800;
const BLOCK: usize = 512;

/// Frame `i` of the ramp file: exact in `f32`.
fn value(i: usize) -> f32 {
    (i as f32 + 1.0) * 1e-5
}

fn session() -> tutti_session::OfflineSession<&'static str, f64> {
    SessionBuilder::<Offline<f64>>::default()
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("an f64 session builds")
}

fn write_ramp(path: &std::path::Path) {
    let samples: Vec<f32> = (0..LEN).map(value).collect();
    tutti_fixtures::signal::write_wav_f32(path, SampleRate::of(RATE), 1, &samples);
}

/// Updates `s` until `done` finds what it waits for in what the update landed.
fn until<T>(
    s: &mut tutti_session::OfflineSession<&'static str, f64>,
    what: &str,
    mut done: impl FnMut(
        &mut tutti_session::OfflineSession<&'static str, f64>,
        Vec<SessionEvent>,
    ) -> Option<T>,
) -> T {
    let mut found = None;
    wait_until(what, Duration::from_secs(20), || {
        let events = s.update();
        found = done(s, events);
        found.is_some()
    });
    found.expect("the wait ended on it")
}

/// **A pool `audio_file` entry loads on an `f64` session**, with the file's
/// frames.
#[test]
fn a_pool_entry_loads_on_an_f64_session() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let mut s = session();
    s.pool(audio_file("ramp", &path)).expect("a declaration");
    until(&mut s, "the entry", |s, _| {
        matches!(s.pool_state(&"ramp"), Some(PoolState::Loaded)).then_some(())
    });
    let AudioEntry::Resident(wave) = s.audio(&"ramp").expect("loaded") else {
        panic!("a resident entry");
    };
    assert_eq!(wave.len().get(), LEN);
    assert_eq!(wave.at(0, 7), value(7));
}

/// **A disk stream opened on an `f64` session lands as `OpenedF64`, and its
/// clip renders `FINE + value(k)` at the file's frame `k` in `f64`.**
#[test]
fn an_f64_session_streams_a_file_into_f64_frames() {
    assert_ne!(f64::from(FINE as f32), FINE, "f32 would round FINE away");
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let mut s = session();
    s.open_stream(&path, Placement::HEAD).expect("a streamer");
    let (stream, reader) = until(&mut s, "the open", |_, events| {
        events.into_iter().find_map(|event| match event {
            SessionEvent::Sampler(SamplerEvent::OpenedF64 { stream, reader, .. }) => {
                Some((stream, reader))
            }
            _ => None,
        })
    });
    let (dc, _) = s.graph().add(ForkByClone(Dc(FINE)));
    let (key, handle) = s
        .graph()
        .add(tutti_sampler::Clip::<f64>::streaming(reader, stream).at(Beat(0.0)));
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(dc, 0), OutPort::new(key, 0)])
        .expect("both into the root");
    let _ = s.update();
    wait_until("the stream to prime", Duration::from_secs(10), || {
        handle
            .status()
            .is_none_or(|status| status.phase != tutti_sampler::StreamPhase::Opening)
    });
    s.play().expect("room");
    let mut heard = Vec::new();
    let mut block = vec![0.0f64; BLOCK * 2];
    while heard.len() < 3_000 {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
        heard.extend(block.chunks(2).map(|frame| frame[0]));
    }
    // The first frame that is neither silent nor the dc alone is the file's
    // frame 0 under the start fade; the fade is long over by frame 1100.
    let first = heard
        .iter()
        .position(|&x| x != FINE && x != 0.0)
        .expect("something beside the dc");
    for k in 1_100..2_000 {
        assert_eq!(
            heard[first + k],
            FINE + f64::from(value(k)),
            "file frame {k}"
        );
    }
}
