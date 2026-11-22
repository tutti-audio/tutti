//! An `f64` session serves the loads and the spawner: `load_audio`
//! returns its resident wave in `f64`, and a disk stream opens, lands and its
//! clip renders into `f64` frames beside a figure `f32` cannot hold.

use std::sync::Arc;
use std::time::Duration;

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_fixtures::wait_until;
use tutti_graph::ForkByClone;
use tutti_sampler::StreamStart;
use tutti_session::{AudioData, MainOutput, OutPort, Session, Tier};
use tutti_units::{Beat, SampleRate};

#[path = "../support/f64_nodes.rs"]
mod f64_nodes;
use f64_nodes::{Dc, FINE};

const RATE: f64 = 48_000.0;
const LEN: usize = 4_800;
const BLOCK: usize = 512;

/// Frame `i` of the ramp file: the file is `f32` PCM, so its frames are `f32`
/// values and widen exactly into the `f64` render.
fn value(i: usize) -> f32 {
    (i as f32 + 1.0) * 1e-5
}

fn session() -> tutti_session::OfflineSession<&'static str, f64> {
    Session::builder()
        .float::<f64>()
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("an f64 session builds")
}

fn write_ramp(path: &std::path::Path) {
    let samples: Vec<f32> = (0..LEN).map(value).collect();
    tutti_fixtures::signal::write_wav_f32(path, SampleRate::of(RATE), 1, &samples);
}

/// Frame `i` of the `f64` ramp file: `FINE` scaled, so no frame is an `f32`.
fn fine_value(i: usize) -> f64 {
    FINE * (i as f64 + 1.0) * 1e-5
}

/// A mono Float64 WAV of `LEN` frames of [`fine_value`].
fn write_f64_ramp(path: &std::path::Path) {
    let data: Vec<u8> = (0..LEN).flat_map(|i| fine_value(i).to_le_bytes()).collect();
    let mut file = Vec::new();
    file.extend(b"RIFF");
    file.extend((36 + data.len() as u32).to_le_bytes());
    file.extend(b"WAVEfmt ");
    file.extend(16u32.to_le_bytes());
    file.extend(3u16.to_le_bytes()); // IEEE float
    file.extend(1u16.to_le_bytes());
    file.extend((RATE as u32).to_le_bytes());
    file.extend((RATE as u32 * 8).to_le_bytes());
    file.extend(8u16.to_le_bytes());
    file.extend(64u16.to_le_bytes());
    file.extend(b"data");
    file.extend((data.len() as u32).to_le_bytes());
    file.extend(data);
    std::fs::write(path, file).expect("the file writes");
}

/// **A `load_audio` of a Float64 file holds its frames as `f64` on an `f64`
/// session, and a `Clip<f64>` over the loaded wave plays them exactly.** No frame survives a trip through `f32`.
#[test]
fn a_loaded_resident_wave_holds_f64_frames_and_its_clip_plays_them() {
    assert_ne!(f64::from(fine_value(7) as f32), fine_value(7));
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp64.wav");
    write_f64_ramp(&path);
    let mut s = session();
    let audio = tutti_fixtures::block_on(s.load_audio(&path).tier(Tier::Resident)).expect("loads");
    let AudioData::Resident(wave) = audio.data() else {
        panic!("a resident load");
    };
    let wave = Arc::clone(wave);
    assert_eq!(wave.len().get(), LEN);
    assert_eq!(wave.at(0, 7), fine_value(7));

    let clip = tutti_sampler::Clip::<f64>::new(wave).expect("a placeable wave");
    let key = s.graph().add(clip.at(Beat::new(0.0)));
    s.graph()
        .set_sources(MainOutput::channel(0), [OutPort::new(key, 0)])
        .expect("the clip into the main output");
    let _ = s.update();
    s.play().expect("room");
    let mut heard = Vec::new();
    let mut block = vec![0.0f64; BLOCK * 2];
    while heard.len() < 3_000 {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
        heard.extend(block.chunks(2).map(|frame| frame[0]));
    }
    let first = heard.iter().position(|&x| x != 0.0).expect("sound");
    for k in 1_100..2_000 {
        assert_eq!(heard[first + k], fine_value(k), "file frame {k}");
    }
}

/// **A disk stream opened on an `f64` session is applied in `f64`, and its
/// clip renders `FINE + value(k)` at the file's frame `k` in `f64`.**
#[test]
fn an_f64_session_streams_a_file_into_f64_frames() {
    assert_ne!(f64::from(FINE as f32), FINE, "f32 would round FINE away");
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);
    let mut s = session();
    let opened =
        tutti_fixtures::block_on(s.open_stream(&path, StreamStart::HEAD).expect("a streamer"))
            .expect("the file reads");
    let (stream, reader) = s.apply(opened).expect("the session serves it");
    let dc = s.graph().add(ForkByClone(Dc(FINE)));
    let key = s
        .graph()
        .add(tutti_sampler::Clip::<f64>::streaming(reader, stream).at(Beat::new(0.0)));
    s.graph()
        .set_sources(
            MainOutput::channel(0),
            [OutPort::new(dc, 0), OutPort::new(key, 0)],
        )
        .expect("both into the main output");
    let _ = s.update();
    wait_until("the stream to prime", Duration::from_secs(10), || {
        s.graph_ref()
            .handle(&key)
            .and_then(|handle| handle.status())
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
