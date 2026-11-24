//! A voice of either tier arrives where the compiler puts it, and nowhere
//! else: doc 025 stage 5 cut 2 (the preroll left the streamer, §2.9).
//!
//! The graph is one `QUAD` main output. Outputs 0 and 1 are a stereo voice (the same
//! file, as a disk stream or in memory) placed at beat 1; output 2 is a
//! latent path (a limiter's lookahead of `L` frames, whatever it reports), so
//! the compiler delays outputs 0 and 1 by `L` to line them up with it
//! (`Session::compensation()` is `[L, L, 0, 0]`). A voice reports no latency
//! of its own, so its audio, started at beat 1 (frame 24 000 at 120 BPM and
//! 48 kHz), must be heard at frame `24 000 + L` and not a frame sooner or
//! later: file frame `k` on output frame `24 000 + L + k`.
//!
//! The oracle is arithmetic on the declared figures: the start frame, the
//! compiler's declared compensation and the file's own frame numbers (a ramp
//! whose every frame is distinct). No render is compared with another.
//!
//! Before cut 2 the session handed the streamer that same table, so the disk
//! tier read `L` frames earlier on top of the compiler's own ring: it lagged
//! the memory tier by `L` (the memory tier, which never pre-rolled, was right).

use std::path::Path;
use tutti_buffers::Interleaved;

use tutti_buffers::InterleavedMut;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_graph::ForkByClone;
use tutti_nodes::testing::Const;
use tutti_nodes::{LimiterConfig, LimiterControls, LimiterNode};
use tutti_sampler::{DiskVoice, MemorySource, StreamPhase, StreamStart, VoiceWindow};
use tutti_session::{InputPort, MainOutput, OutputPort, Session};
use tutti_units::{Beat, Db, SampleRate, Samples};

const RATE: f64 = 48_000.0;
/// Beat 1 at 120 BPM and `RATE`.
const AT: usize = 24_000;
/// File frames, and how many of them the test reads back.
const LEN: usize = 4_800;
const BLOCK: usize = 512;

/// Frame `i` of the file: distinct per frame (a ramp), exactly what an f32
/// WAV hands back; the right channel is its negative.
fn value(i: usize) -> f32 {
    (i as f32 + 1.0) * 1e-5
}

fn write_ramp(path: &Path) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: RATE as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).expect("writes");
    for i in 0..LEN {
        w.write_sample(value(i)).expect("writes");
        w.write_sample(-value(i)).expect("writes");
    }
    w.finalize().expect("writes");
}

/// How the voice under test is made.
#[derive(Clone, Copy, Debug)]
enum Tier {
    Disk,
    Memory,
}

/// Outputs 0 and 1 of the session rendering `tier`'s voice beside a latent
/// path, rolling from beat 0: `AT + 2 000` frames each, and the compensation
/// the compiler declared for output 0.
fn render(tier: Tier) -> (Vec<f32>, Vec<f32>, Samples) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    write_ramp(&path);

    let mut s = Session::builder()
        .sample_rate(SampleRate::of(RATE))
        .outputs(ChannelLayout::QUAD)
        .build()
        .expect("builds");

    // The latent path, on output 2.
    let src = s.graph().add(ForkByClone(Const::mono(1.0)));
    let eff = s.graph().add(LimiterNode::<f32>::from(
        LimiterConfig::new(ChannelTopology::MONO).controls(LimiterControls {
            threshold: Db::new(-1.0),
            ceiling: Db::new(-0.3),
            ..LimiterControls::DEFAULT
        }),
    ));
    s.graph()
        .set_sources(InputPort::new(eff, 0), [OutputPort::new(src, 0)])
        .unwrap();
    s.graph()
        .set_sources(MainOutput::channel(2), [OutputPort::new(eff, 0)])
        .unwrap();

    // The voice, on outputs 0 and 1.
    // The stream's handle is held for the whole render: dropping it would end
    // the stream.
    let (voice, stream) = match tier {
        Tier::Disk => {
            // The app reads the file, and the session puts it in service.
            let opened = tutti_fixtures::block_on(
                s.open_stream(&path, StreamStart::HEAD)
                    .expect("an offline session streams"),
            )
            .expect("the file reads");
            let (stream, reader) = s.apply(opened).expect("the session serves it");
            let voice = DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(1.0)));
            (s.graph().add(voice).erase(), Some(stream))
        }
        Tier::Memory => {
            let mut wave = tutti_buffers::Wave::new(2, tutti_nodes::SampleRate::of(RATE));
            for i in 0..LEN {
                wave.append(Interleaved::new(&[value(i), -value(i)], wave.layout()).unwrap())
                    .unwrap();
            }
            let source =
                MemorySource::placed(std::sync::Arc::new(wave), VoiceWindow::from(Beat::new(1.0)))
                    .expect("a placeable wave");
            (s.graph().add(source).erase(), None)
        }
    };
    s.graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(voice, 0)])
        .unwrap();
    s.graph()
        .set_sources(MainOutput::channel(1), [OutputPort::new(voice, 1)])
        .unwrap();
    let _ = s.update();

    let declared = s.compensation().preroll(0).expect("a plan was sent");
    assert_eq!(
        s.compensation().preroll(1),
        Some(declared),
        "both voice channels carry the same compensation"
    );
    assert_eq!(
        s.compensation().preroll(2),
        Some(Samples::new(0)),
        "the latent path is the one the rest wait for"
    );
    assert!(
        declared.get() > 0,
        "the limiter's lookahead is the compensation, or this tests nothing"
    );

    // The streamer fills the ring on its own thread, and an offline render
    // does not wait for it. A ring that is not yet primed reads silence, which
    // fails the assertions below, so wait for the stream to say it is: its
    // status leaves `Opening` once the ring holds what the streamer reads
    // ahead (the first 4 200 of the file's 4 800 frames, which covers the
    // frames read back).
    if let Some(stream) = &stream {
        tutti_fixtures::wait_until(
            "the stream to be primed",
            tutti_fixtures::HANG_GUARD,
            || stream.status().phase != StreamPhase::Opening,
        );
        let phase = stream.status().phase;
        assert!(
            matches!(phase, StreamPhase::Live | StreamPhase::Ended),
            "the stream is not primed: {phase:?}"
        );
    }

    s.play().expect("room");
    let frames = AT + 2_000;
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let mut block = vec![0.0f32; BLOCK * 4];
    while left.len() < frames {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::QUAD).unwrap());
        left.extend(block.as_chunks::<4>().0.iter().map(|f| f[0]));
        right.extend(block.as_chunks::<4>().0.iter().map(|f| f[1]));
    }
    (left, right, declared)
}

/// **A voice of either tier is heard at its start frame plus the compiler's
/// compensation, once.** Silent before `AT + L`, then file frame `k` on
/// output frame `AT + L + k`, on both channels.
#[test]
fn a_voice_of_either_tier_arrives_at_its_start_plus_the_declared_compensation() {
    for tier in [Tier::Disk, Tier::Memory] {
        let (left, right, declared) = render(tier);
        let first = AT + declared.get();
        assert!(
            left[..first].iter().all(|&x| x == 0.0) && right[..first].iter().all(|&x| x == 0.0),
            "the {tier:?} tier sounded before frame {first} ({AT} + {declared:?})"
        );
        for k in 0..1_000 {
            assert_eq!(
                left[first + k],
                value(k),
                "the {tier:?} tier: left, file frame {k} at output frame {}",
                first + k
            );
            assert_eq!(
                right[first + k],
                -value(k),
                "the {tier:?} tier: right, file frame {k}"
            );
        }
    }
}
