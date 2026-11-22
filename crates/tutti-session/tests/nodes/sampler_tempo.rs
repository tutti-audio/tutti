//! A sampler node the session's graph adds reads the session's tempo map
//! (doc 020, stage 5), through the builder's engine. Needs the `sampler`
//! feature (`SessionNode` for the sampler's nodes).

use std::sync::Arc;
use tutti_buffers::Interleaved;

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_session::{MainOutput, OutputPort, Session};
use tutti_units::SampleRate;

const RATE: f64 = 48_000.0;

/// An offline session at `RATE`, stereo, graph blocks of up to 1 024 frames.
fn offline() -> Session {
    Session::builder()
        .sample_rate(SampleRate::of(RATE))
        .build()
        .expect("builds")
}

/// `frames` stereo frames of `s` in `block`-frame device blocks, the
/// transport rolling from the first block.
fn run(s: &mut Session, frames: usize, block: usize) -> Vec<f32> {
    s.play().expect("room");
    let mut out = Vec::with_capacity(frames * 2);
    let mut buf = vec![0.0f32; block * 2];
    while out.len() < frames * 2 {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        out.extend_from_slice(&buf);
    }
    out.truncate(frames * 2);
    out
}

/// **A sampler node the graph adds reads the session's tempo map** (doc 020,
/// stage 5): `Graph::add` binds it to the session's reader
/// ([`SessionNode::bind_timeline`](tutti_session::SessionNode::bind_timeline)), so
/// a clip placed at beat 0 plays its samples one a frame through a step in
/// the map instead of being re-seated at the new tempo. The wave is a ramp
/// (sample `i` is `i / 2^17`), 120 BPM and 60 from beat 1 (frame 24 000): at
/// every frame the output is source frame `f` (the map's definition: a clip
/// from frame 0 is at frame `f`), where a clip re-seated at the step would
/// jump from source frame 23 999 to 48 000.
#[test]
fn a_sampler_node_added_to_the_graph_follows_the_sessions_tempo_map() {
    use tutti_sampler::{MemorySource, Playback, SlotId, VoicePool, VoiceWindow};
    use tutti_tempo::{TempoMap, TempoPoint, Transition};
    use tutti_units::{Beat, Bpm};

    let mut wave = tutti_buffers::Wave::new(1, SampleRate::of(RATE));
    for i in 0..RATE as usize * 2 {
        wave.append(Interleaved::new(&[i as f32 / 131_072.0], wave.layout()).unwrap())
            .unwrap();
    }
    let source = MemorySource::placed(Arc::new(wave), VoiceWindow::from(Beat::new(0.0)))
        .expect("a placeable wave");
    let mut pool = VoicePool::new();
    pool.insert(SlotId(1), source, Playback::default())
        .expect("a finite gain and a nonempty width");
    let mut s = offline();
    let map =
        TempoMap::new(
            [(0.0, 120.0), (1.0, 60.0)]
                .into_iter()
                .map(|(beat, bpm)| TempoPoint {
                    beat: Beat::new(beat),
                    tempo: Bpm::of(bpm),
                    transition: Transition::Step,
                }),
        )
        .expect("a map");
    s.set(s.transport_handle().tempo().to(map))
        .expect("not following");
    let voice = s.graph().add(pool);
    for port in 0..2 {
        s.graph()
            .set_sources(MainOutput::channel(port), [OutputPort::new(voice, port)])
            .unwrap()
            .unfaded();
    }
    let _ = s.update();
    let out = run(&mut s, 60_000, 512);
    for f in 0..60_000 {
        let want = f as f32 / 131_072.0;
        assert_eq!(out[2 * f].to_bits(), want.to_bits(), "frame {f}");
    }
}

/// **A sampler pool the graph adds reads the transport's loop**: `Graph::add` binds it
/// ([`SessionNode::bind_timeline`](tutti_session::SessionNode::bind_timeline)), so after the
/// loop is armed over a stretched voice's window and a control tick, the voice carries a
/// spare vocoder, and after the loop is disarmed it does not.
#[test]
fn a_sampler_pool_added_to_the_graph_carries_a_spare_under_the_transports_loop() {
    use tutti_music::LoopRange;
    use tutti_sampler::testing::spares_sent;
    use tutti_sampler::{MemorySource, Playback, SlotId, VoicePool, VoiceWindow};
    use tutti_timeline::LoopRegion;
    use tutti_units::{Beat, StretchFactor};

    let mut wave = tutti_buffers::Wave::new(1, SampleRate::of(RATE));
    for _ in 0..RATE as usize {
        wave.append(Interleaved::new(&[0.0], wave.layout()).unwrap())
            .unwrap();
    }
    let source = MemorySource::placed(Arc::new(wave), VoiceWindow::from(Beat::new(0.0)))
        .expect("a placeable wave");
    let mut s = offline();
    let pool = s.graph().add(VoicePool::new());
    let handle = s
        .graph_ref()
        .handle(&pool)
        .expect("the pool is in the graph")
        .clone();
    handle
        .add(
            SlotId(1),
            source,
            Playback {
                stretch: StretchFactor::new(1.5),
                ..Playback::default()
            },
        )
        .expect("queued");
    let _ = s.update();
    assert_eq!(spares_sent(&handle), 0, "no loop yet");

    let range = LoopRange::try_from(Beat::new(0.5)..Beat::new(1.5)).unwrap();
    let looping = s.transport_handle().looping();
    let mut buf = vec![0.0f32; 512 * 2];
    let mut tick = |s: &mut Session| {
        // The engine applies the edit in a block; the control tick reads it after.
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO).unwrap());
        let _ = s.update();
    };
    s.set(looping.to(LoopRegion::new(range, true))).unwrap();
    tick(&mut s);
    tick(&mut s);
    assert_eq!(spares_sent(&handle), 1, "armed over the voice's window");
    s.set(looping.to(LoopRegion::new(range, false))).unwrap();
    tick(&mut s);
    tick(&mut s);
    assert_eq!(spares_sent(&handle), 0, "disarmed");
}
