//! Regression gate: sampler hot paths must not allocate per-buffer.
//!
//! Each gate here aborts the process (SIGABRT) on a regression rather than
//! failing an assertion, because an allocation inside a node's `process` (which
//! `Direct` runs under an `RtScope`) is caught where it happens rather than
//! reported afterwards.
//!
//! Covered: in-memory `MemorySource` (the workhorse playback node) in full
//! blocks and in one-frame blocks, the voice-pool command drain, and a
//! stretching voice, whose phase vocoder is built on the control side and whose
//! ratio changes between blocks
//! (`a_stretching_voice_with_a_changing_ratio_is_allocation_free`).
//!
//! The sampler's nodes are graph nodes: each is driven through
//! `tutti_graph::contract::Direct`, whose buffers are built once, under a
//! rolling transport moved after every block as a host moves it. A node has
//! no per-sample entry point, so the sample-accurate gates drive one-frame
//! blocks.
//!
//! `DiskVoice` is not covered here: it needs a crate-private ring, so its
//! non-allocation (steady state, a jump's scratch copy and fade) is guarded
//! in-crate, in `voice::disk_voice`'s tests.

use tutti_sampler::testing::{
    controlled_pool, free_source, loop_frames, placed_config, placed_source, voice_node,
};

use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_buffers::Wave;
use tutti_graph::contract::Direct;
use tutti_graph::{Node, Published};
use tutti_rt::Reclaim;
use tutti_sampler::testing::MockTransport;
use tutti_sampler::{
    MemorySource, MemorySourceConfig, Placed, Playback, Row, SlotId, VoiceNode, VoiceNodeConfig,
    VoicePool, VoicePoolConfig, VoicePoolControls, VoiceWindow,
};
use tutti_units::{
    Amplitude, Beat, BeatDuration, Bpm, Cents, PlaybackRate, SamplePosition, SampleRate, Seconds,
    StretchFactor,
};

const RATE: SampleRate = SampleRate::SR_48K;

/// `node` driven by hand in blocks of `frames`, under a transport rolling
/// from beat 0 at 120 BPM and moved after each block. Built outside every
/// guard; a block allocates nothing of the rig's own.
struct Rig<N> {
    direct: Direct<N>,
    t: Arc<MockTransport>,
    frames: usize,
}

impl<N: Node> Rig<N> {
    fn new(node: N, frames: usize) -> Self {
        Self {
            direct: Direct::new(node, RATE, frames),
            t: MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0)),
            frames,
        }
    }

    fn block(&mut self) {
        let env = self.t.env(self.frames, RATE);
        self.direct.block_in(&env);
        self.t.advance(self.frames as i64, RATE);
    }

    fn node(&mut self) -> &mut N {
        &mut self.direct.node
    }
}

/// Build a stereo `Wave` of `duration_secs` filled with a 440 Hz sine.
fn sine_wave(duration_secs: f64, sample_rate: f64) -> Arc<Wave> {
    let mut wave = Wave::zero(2, SampleRate::of(sample_rate), Seconds::new(duration_secs));
    let len = wave.len().get();
    for i in 0..len {
        let t = i as f64 / sample_rate;
        let s = (t * 440.0 * core::f64::consts::TAU).sin() as f32 * 0.5;
        wave.set(0, i, s);
        wave.set(1, i, s);
    }
    Arc::new(wave)
}

#[test]
fn memory_source_process_is_allocation_free() {
    let mut rig = Rig::new(free_source(sine_wave(2.0, 48_000.0)), 64);

    // Warm-up — drains the position update branch.
    for _ in 0..16 {
        rig.block();
    }

    for _ in 0..256 {
        rig.block();
    }
    // The render ran, at unit rate: one source frame per output frame.
    assert_eq!(rig.node().position(), SamplePosition::new(272.0 * 64.0));
}

/// One-frame blocks: the sample-accurate caller's path.
#[test]
fn memory_source_one_frame_blocks_are_allocation_free() {
    let mut rig = Rig::new(free_source(sine_wave(2.0, 48_000.0)), 1);
    for _ in 0..256 {
        rig.block();
    }

    for _ in 0..2_048 {
        rig.block();
    }
    assert_eq!(rig.node().position(), SamplePosition::new(2_304.0));
}

/// A stretching voice whose ratio keeps changing renders without allocating.
///
/// The row owns its vocoder, built on the control side with the voice, so a change of
/// the factor or the pitch is a value the block applies in place: each block here (a
/// retarget of the vocoder's controls, the input it asks for read at the cursor, and a
/// hop of frames) is guarded. The ratio steps every block, over stretches both sides of
/// unity and a pitch shift, so the vocoder's input count varies from block to block.
#[test]
fn a_stretching_voice_with_a_changing_ratio_is_allocation_free() {
    let wave = sine_wave(4.0, 48_000.0);
    let row = |stretch: f64, cents: f64| {
        Row::from(placed_parts(&wave, 1.5))
            .stretching(StretchFactor::new(stretch), Cents::new(cents))
    };
    let (pool, controls) = controlled_pool(VoicePool::from(config_of(
        ChannelLayout::STEREO,
        [(1, row(1.5, 0.0))],
    )));
    let mut rig = Rig::new(pool, 64);
    // The first block installs the voice, and the vocoder fills past its latency.
    for _ in 0..64 {
        rig.block();
    }

    for step in 0..400u32 {
        let stretch = 0.8 + 0.1 * f64::from(step % 7);
        let cents = 100.0 * f64::from(step % 5);
        controls.replace(config_of(ChannelLayout::STEREO, [(1, row(stretch, cents))]).table());
        // The block applies the new values and renders the voice.
        rig.block();
        controls.reclaim();
    }
}

/// A pool of one stretching memory voice (a wave of 4 s, stretched 1.5, shifted
/// 300 cents) placed at beat 0 for `duration` beats (`None`: open-ended), driven in
/// blocks of `block`, and its controls. Warmed outside any guard until the voice sounds.
fn stretching_rig(
    block: usize,
    duration: Option<f64>,
) -> (Rig<VoicePool<f32>>, VoicePoolControls<f32>) {
    let window = VoiceWindow {
        start: Beat::new(0.0),
        duration: duration.map(BeatDuration::new),
    };
    let voice = VoiceNodeConfig::placed(placed_config(sine_wave(4.0, 48_000.0), window))
        .expect("a finite gain and a nonempty width")
        .stretch(StretchFactor::new(1.5))
        .pitch(Cents::new(300.0));
    let (pool, controls) = controlled_pool(VoicePool::from(config_of(
        ChannelLayout::STEREO,
        [(1, voice.into())],
    )));
    let mut rig = Rig::new(pool, block);
    for _ in 0..(16_384 / block).max(2) {
        rig.block();
    }
    (rig, controls)
}

/// `blocks` blocks of `rig`.
fn guarded<N: Node>(rig: &mut Rig<N>, blocks: usize) {
    for _ in 0..blocks {
        rig.block();
    }
}

/// A loop set while a stretching voice plays allocates nothing: the controls' tick
/// (outside the guard) sends the voice a spare vocoder once a block has carried the
/// loop, and setting the loop only changes what the blocks read; the priming, the swap
/// at the wraps and the reset of the vocoder the swap replaces all run in `process`.
#[test]
fn a_loop_set_while_a_stretching_voice_plays_does_not_allocate() {
    use tutti_graph::TransportPosition;
    use tutti_music::{FrameClock, LoopRange};
    let region = LoopRange::new(Beat::new(0.125), Beat::new(0.376)).expect("a loop");
    let wave = sine_wave(4.0, 48_000.0);
    let (pool, controls) = controlled_pool(VoicePool::from(config_of(
        ChannelLayout::STEREO,
        [(0, placed_parts(&wave, 1.5).into())],
    )));
    let node = tutti_graph::contract::prepared(pool, RATE, 64);
    let mut direct = Direct::new(node, RATE, 64);
    let mut clock = FrameClock::new(Beat::new(0.0), Bpm::of(120.0), RATE);
    let mut block = |looping: Option<LoopRange>, direct: &mut Direct<VoicePool<f32>>| {
        direct.set_transport(TransportPosition::new(
            true,
            Bpm::of(120.0),
            clock.beat(),
            looping,
        ));
        direct.block();
        controls.reclaim();
        clock.advance(tutti_units::Samples::new(64), looping);
    };
    // Past the vocoder's priming, no loop yet.
    for _ in 0..80 {
        block(None, &mut direct);
    }
    for i in 0..256 {
        block((i >= 10).then_some(region), &mut direct);
    }
}

/// A spare vocoder arrives and leaves without the audio thread allocating or freeing: the
/// controls build it on their tick once a block has carried the loop (outside the guard),
/// the drain moves it into the slot and, when the loop moves off, out to the retirement
/// ring, and the controls free it.
#[test]
fn a_spare_vocoder_arrives_and_retires_without_allocating() {
    use tutti_music::LoopRange;
    let region = LoopRange::new(Beat::new(0.125), Beat::new(0.376)).expect("a loop");
    let (mut rig, controls) = stretching_rig(64, None);
    for _ in 0..3 {
        rig.t.set_loop(Some(region));
        guarded(&mut rig, 4);
        controls.reclaim();
        guarded(&mut rig, 4);
        assert_eq!(tutti_sampler::testing::spare_count(rig.node()), 1);
        assert_eq!(controls.spares(), 1);
        rig.t.set_loop(None);
        guarded(&mut rig, 4);
        controls.reclaim();
        guarded(&mut rig, 4);
        assert_eq!(tutti_sampler::testing::spare_count(rig.node()), 0);
        assert_eq!(controls.spares(), 0);
    }
}

/// A stretching voice allocates nothing wherever it seats: its priming (several
/// blocks of vocoder work, in the scratch the owner built) runs in `process`, at
/// a seek mid-play, at a stop and a start, at a window's end and its re-entry, and in
/// one-frame blocks and in a block past any scratch of a few thousand frames.
#[test]
fn a_stretching_voice_does_not_allocate_when_it_seats() {
    // A seek mid-play, twice (the second lands in the first's priming).
    let (mut rig, _controls) = stretching_rig(64, None);
    rig.t.seek(Beat::new(0.3));
    guarded(&mut rig, 4);
    rig.t.seek(Beat::new(0.9));
    guarded(&mut rig, 40);

    // A stop and a start: the stop unseats, the start primes again.
    rig.t.set_rolling(false);
    guarded(&mut rig, 8);
    rig.t.set_rolling(true);
    guarded(&mut rig, 40);

    // A window's end and its re-entry: 0.5 beat is 12 000 frames at 120 BPM; the
    // blocks run past it, then the playhead goes back inside.
    let (mut rig, _controls) = stretching_rig(64, Some(0.5));
    guarded(&mut rig, 220);
    rig.t.seek(Beat::new(0.1));
    guarded(&mut rig, 40);

    // One-frame blocks: a seat primes over hundreds of them.
    let (mut rig, _controls) = stretching_rig(1, None);
    rig.t.seek(Beat::new(0.2));
    guarded(&mut rig, 700);

    // A block of 8 492 frames, past a few thousand of scratch, across a seek.
    let (mut rig, _controls) = stretching_rig(8_192 + 300, None);
    rig.t.seek(Beat::new(1.0));
    guarded(&mut rig, 3);
}

/// A stretching disk voice allocates nothing wherever it seats either: the priming (the
/// ring read for the vocoder's history, the first analysis frame, the discarded frames) runs
/// in `process`, at the start (from a window ahead), at a seek mid-play, and at a stop and
/// a start. The streamer is stepped by hand between blocks, outside the guard.
#[test]
fn a_stretching_disk_voice_does_not_allocate_when_it_seats() {
    use tutti_sampler::testing::wait_for_open;
    use tutti_sampler::{DiskStreamer, DiskVoice, StreamStart};
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("tone.wav");
    let tone: Vec<f32> = (0..2 * 96_000)
        .map(|i| (0.3 * (std::f64::consts::TAU * 440.0 * (i / 2) as f64 / 48_000.0).sin()) as f32)
        .collect();
    tutti_fixtures::signal::write_wav_f32(&path, RATE, 2, &tone);
    let mut streamer =
        DiskStreamer::<_, f32>::manual(RATE, Default::default()).expect("streamer builds");
    let opening = streamer.open_stream(&path, StreamStart::HEAD);
    let (stream, reader) = wait_for_open(&mut streamer, opening).expect("the file opens");
    assert!(streamer.step_until_idle(1_000) < 1_000, "the ring primes");
    let voice = DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.5)));
    let streamed = VoiceNodeConfig::streaming(
        voice,
        stream,
        Amplitude::new(1.0),
        StretchFactor::new(1.5),
        Cents::new(300.0),
    )
    .expect("a finite gain and a nonempty width");
    let pool = VoicePool::from(config_of(ChannelLayout::STEREO, [(1, streamed.into())]));
    let mut rig = Rig::new(pool, 64);
    let mut run = |rig: &mut Rig<VoicePool<f32>>, blocks: usize| {
        for _ in 0..blocks {
            rig.block();
            let _ = streamer.step();
        }
    };
    // The window opens at beat 0.5 (frame 12 000): the voice primes ahead of it.
    run(&mut rig, 250);
    rig.t.seek(Beat::new(1.2));
    run(&mut rig, 60);
    rig.t.set_rolling(false);
    run(&mut rig, 8);
    rig.t.set_rolling(true);
    run(&mut rig, 60);
}

/// A stretching voice allocates nothing while it primes where a stopped transport stands:
/// the stopped blocks that prime, a locate, and the blocks that play on from it.
#[test]
fn a_stretching_voice_does_not_allocate_while_priming_on_a_stopped_transport() {
    let wave = sine_wave(4.0, 48_000.0);
    let pool = VoicePool::from(config_of(
        ChannelLayout::STEREO,
        [(1, placed_parts(&wave, 1.5).into())],
    ));
    let mut rig = Rig::new(pool, 64);
    rig.t.set_rolling(false);
    rig.block();
    guarded(&mut rig, 80);
    rig.t.seek(Beat::new(0.5));
    guarded(&mut rig, 80);
    rig.t.set_rolling(true);
    guarded(&mut rig, 40);
}

/// Removing a stretching voice and replacing its wave allocate nothing in the drain:
/// the voice is retired for the control thread to free, and a voice that is added
/// again primes in `process`.
#[test]
fn removing_and_replacing_a_stretching_voice_does_not_allocate() {
    let (mut rig, controls) = stretching_rig(64, None);
    let window = VoiceWindow::from(Beat::new(0.0));
    let stretching = |wave: Arc<Wave>, stretch: f64| {
        let voice = VoiceNodeConfig::placed(placed_config(wave, window))
            .expect("a finite gain and a nonempty width")
            .stretch(StretchFactor::new(stretch));
        config_of(ChannelLayout::STEREO, [(1, voice.into())])
    };
    controls.replace(stretching(sine_wave(3.0, 48_000.0), 1.5).table());
    guarded(&mut rig, 20);
    controls.reclaim();
    controls.replace(config_of(ChannelLayout::STEREO, []).table());
    guarded(&mut rig, 4);
    controls.reclaim();
    controls.replace(stretching(sine_wave(2.0, 48_000.0), 0.8).table());
    guarded(&mut rig, 40);
}

// ---------------------------------------------------------------------------
// VoicePool — the live-graph workhorse. One node per track, mixing
// down every voice's `MemorySource` each buffer, under the crate's mock
// transport so voices see a running playhead.
// ---------------------------------------------------------------------------

/// Steady-state mixdown must be allocation-free: two voices already installed in
/// the slot list, the per-buffer `process()` sums them with no heap traffic.
///
/// The voices are the config's rows, installed by the first block, so the guarded
/// loop measures ONLY the per-buffer mixdown.
#[test]
fn voice_pool_process_steady_state_is_allocation_free() {
    let wave = sine_wave(2.0, 48_000.0);

    let unit = VoicePool::from(config_of(
        ChannelLayout::STEREO,
        (0..2u128).map(|i| (i, placed_parts(&wave, 1.0).into())),
    ));

    let mut rig = Rig::new(unit, 64);

    // Warm-up: the first block installs the voices, then the first position reads.
    for _ in 0..16 {
        rig.block();
    }
    assert_eq!(rig.node().voice_count(), 2);

    for _ in 0..256 {
        rig.block();
    }
}

/// A forked pool with a stretching voice renders its blocks without allocating.
///
/// Every other pool test drives a freshly constructed node, so this is the pool's RT
/// contract across a fork: the fork holds its own copies of the voices, vocoder
/// included, and builds its own block lanes.
#[test]
fn forked_voice_pool_with_stretch_is_allocation_free() {
    use tutti_graph::{IntoNode, NodeParts, RenderMode};
    let wave = sine_wave(2.0, 48_000.0);

    // Actually stretching: a unity voice has no vocoder, so the read would never
    // touch one and the test would pass either way.
    let original = VoicePool::from(config_of(
        ChannelLayout::STEREO,
        [(0, placed_parts(&wave, 2.0).into())],
    ));
    let NodeParts { fork, .. } = original.into_parts();
    let forked = fork
        .expect("a pool forks")
        .fork(RenderMode::Realtime)
        .expect("a memory pool forks")
        .node;

    let mut direct = Direct::boxed(forked, RATE, 64);
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut block = || {
        direct.block_in(&t.env(64, RATE));
        t.advance(64, RATE);
    };

    // Warm past the vocoder's fill-up so `process` is on its steady-state path.
    for _ in 0..64 {
        block();
    }

    for _ in 0..256 {
        block();
    }
}

/// Same steady-state guard in one-frame blocks: the sample-accurate caller's
/// path.
#[test]
fn voice_pool_one_frame_blocks_are_allocation_free() {
    let wave = sine_wave(2.0, 48_000.0);

    let unit = VoicePool::from(config_of(
        ChannelLayout::STEREO,
        (0..2u128).map(|i| (i, placed_parts(&wave, 1.0).into())),
    ));

    let mut rig = Rig::new(unit, 1);
    for _ in 0..256 {
        rig.block();
    }

    for _ in 0..2_048 {
        rig.block();
    }
}

/// Block lengths from one frame to the maximum, straddling 64.
const RAGGED: [usize; 10] = [1, 37, 100, 511, 512, 64, 200, 129, 3, 65];

/// `node` prepared at 64 frames, then re-prepared at 512 (the scratch
/// resized on the control thread), warmed, then 2 000 blocks of
/// [`RAGGED`] lengths, on a transport that moves and winds
/// back to beat 0 at beat 3 (inside the test waves).
fn ragged_at_512<N: Node>(node: N) {
    let node = tutti_graph::contract::prepared(node, RATE, 64);
    let mut direct = Direct::new(node, RATE, 512);
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let step = |i: usize, direct: &mut Direct<N>| {
        let len = RAGGED[i % RAGGED.len()];
        direct.block_in(&t.env(len, RATE));
        t.advance(len as i64, RATE);
        if t.beat().get() >= 3.0 {
            t.seek(Beat::new(0.0));
        }
    };
    for i in 0..64 {
        step(i, &mut direct);
    }
    for i in 0..2_000 {
        step(i, &mut direct);
    }
}

/// A stereo pool config of `channels` over `rows`, each under its key.
fn config_of(
    channels: impl Into<ChannelLayout>,
    rows: impl IntoIterator<Item = (u128, Row<f32>)>,
) -> VoicePoolConfig<f32> {
    rows.into_iter().fold(
        VoicePoolConfig::<f32>::new(channels).expect("a width the sampler reads"),
        |config, (key, row)| config.row(SlotId(key), row).expect("a free key"),
    )
}

/// The voice of `wave` placed at beat 0, stretched by `stretch`.
fn placed_parts(wave: &Arc<Wave>, stretch: f64) -> VoiceNodeConfig<f32, Placed> {
    placed_voice(placed_config(
        wave.clone(),
        VoiceWindow::from(Beat::new(0.0)),
    ))
    .stretch(StretchFactor::new(stretch))
}

/// The placed voice over `source`.
fn placed_voice(source: MemorySourceConfig<f32>) -> VoiceNodeConfig<f32, Placed> {
    VoiceNodeConfig::placed(source).expect("a finite gain and a nonempty width")
}

/// [`placed_parts`] as a standalone voice node.
fn placed_node(wave: &Arc<Wave>, stretch: f64) -> VoiceNode<f32> {
    VoiceNode::from(placed_parts(wave, stretch))
}

/// Every memory voice node, re-prepared at a 512-frame `max_block`,
/// renders ragged blocks up to it without allocating: a pool (two plain
/// voices and a stretched one), a stretched `VoiceNode`, and a
/// `MemorySource` placed and free-running, and a pool of looped voices and
/// a silent one — each block in one piece
/// through scratch sized in `prepare`. (The disk voice's gate is in-crate:
/// `clip_reader_ragged_blocks_at_max_block_512_are_allocation_free`.)
#[test]
fn voice_nodes_at_max_block_512_in_ragged_blocks_are_allocation_free() {
    let wave = sine_wave(2.0, 48_000.0);

    let pool = VoicePool::from(config_of(
        ChannelLayout::STEREO,
        [
            (0, placed_parts(&wave, 1.0).into()),
            (1, placed_parts(&wave, 1.0).into()),
            (2, placed_parts(&wave, 2.0).into()),
        ],
    ));
    ragged_at_512(pool);

    // Looped, at unit rate and varispeed (the block read's looped lanes,
    // straight from the wave and gathered), and one placed past the
    // transport (a silent voice the pool skips).
    let looped = |speed: f64| {
        placed_voice(
            placed_config(wave.clone(), VoiceWindow::from(Beat::new(0.0)))
                .looped(loop_frames(1_000, 30_000, 256))
                .speed(PlaybackRate::of(speed)),
        )
    };
    let late = placed_voice(placed_config(
        wave.clone(),
        VoiceWindow::from(Beat::new(1_000.0)),
    ));
    let pool = VoicePool::from(config_of(
        ChannelLayout::STEREO,
        [
            (0, looped(1.0).into()),
            (1, looped(1.37).into()),
            (2, late.into()),
        ],
    ));
    ragged_at_512(pool);

    ragged_at_512(placed_node(&wave, 0.5));

    ragged_at_512(placed_source(
        wave.clone(),
        VoiceWindow::from(Beat::new(0.0)),
    ));

    let free = free_source(wave.clone());
    free.trigger();
    ragged_at_512(free);
}

/// A node placed at beat 0 under a transport looping beats 0.125 to 0.376,
/// in ragged blocks that cut the loop's wraps (and so the crossfade) at
/// every offset: nothing allocates, and the crossfade ran: no step across a
/// wrap is a cut's (a 440 Hz tone steps 0.49 there).
/// `smooth`: the wrap is also free of a cut (a stretched voice reseats at a wrap and is
/// silent while it primes, so it is not).
fn wraps_without_allocating<N: Node>(node: N, smooth: bool) {
    use tutti_graph::TransportPosition;
    use tutti_music::LoopRange;
    const WRAPS: usize = 4;
    let region = tutti_music::LoopRange::new(Beat::new(0.125), Beat::new(0.376)).expect("a loop");
    let looping = LoopRange::new(Beat::new(0.125), Beat::new(0.376)).expect("a loop");
    let node = tutti_graph::contract::prepared(node, RATE, 64);
    let mut direct = Direct::new(node, RATE, 512);
    let mut clock = tutti_music::FrameClock::new(Beat::new(0.0), Bpm::of(120.0), RATE);
    let mut heard = Vec::with_capacity(70_000);
    let mut step = |i: usize, direct: &mut Direct<N>, heard: &mut Vec<f32>| -> bool {
        let len = RAGGED[i % RAGGED.len()];
        let before = clock.beat();
        direct.set_transport(TransportPosition::new(
            true,
            Bpm::of(120.0),
            before,
            Some(looping),
        ));
        direct.set_block_len(len);
        direct.block();
        heard.extend_from_slice(direct.output().plane(0).samples());
        clock.advance(tutti_units::Samples::new(len), Some(region));
        clock.beat() < before
    };
    for i in 0..64 {
        step(i, &mut direct, &mut Vec::with_capacity(512));
    }
    let mut wraps = 0;
    for i in 0..400 {
        wraps += usize::from(step(i, &mut direct, &mut heard));
    }
    assert!(wraps >= WRAPS, "the run wraps ({wraps})");
    if !smooth {
        return;
    }
    let widest = heard
        .windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f32::max);
    assert!(widest < 0.06, "a step of {widest}: the wrap is a cut");
}

/// A loop wrap's crossfade does not allocate, through a pool, a
/// `VoiceNode` and a bare `MemorySource`: the tail lanes are sized in
/// `prepare`.
#[test]
fn a_loop_wrap_crossfade_is_allocation_free() {
    let wave = sine_wave(2.0, 48_000.0);
    let pooled = |stretch: f64| {
        VoicePool::from(config_of(
            ChannelLayout::STEREO,
            [(0, placed_parts(&wave, stretch).into())],
        ))
    };
    wraps_without_allocating(pooled(1.0), true);
    wraps_without_allocating(placed_node(&wave, 1.0), true);
    // Stretched: every wrap is a reseat, which primes in `process`.
    wraps_without_allocating(pooled(1.5), false);
    wraps_without_allocating(placed_node(&wave, 1.5), false);
    wraps_without_allocating(
        placed_source(wave.clone(), VoiceWindow::from(Beat::new(0.0))),
        true,
    );
}

/// A table's change of stretch, and a row that begins to stretch, drain
/// allocation-free.
///
/// A row that stretches owns its vocoder, built with its voice on the control side:
/// a change of the factor or the pitch is a value the block writes in place, and a
/// row that begins to stretch is a new voice, whose vocoder arrives built, moved in by
/// the drain. Every one of those is a store or a move; nothing is constructed here.
/// The old slot of the row leaves through the retirement ring instead of being
/// dropped here, because dropping a vocoder frees its rings.
///
/// The `RtAllocCheck` global allocator *aborts* the process (SIGABRT via
/// `handle_alloc_error`) on a violation — it cannot be caught with
/// `catch_unwind`. An allocation here would take the whole test binary down,
/// hiding every other test's result. So the actual `RtScope`-armed
/// drain runs in a re-exec'd CHILD of this test binary
/// (`RT_NO_ALLOC_STRETCH_CHILD=1`), and the parent asserts the child exited
/// cleanly. A regression that reintroduces allocation on the drain aborts the
/// child → this test FAILS.
#[test]
fn voice_pool_update_stretch_drain_is_allocation_free() {
    // Child arm: run the guarded drain. Aborts only on a regression.
    if std::env::var_os("RT_NO_ALLOC_STRETCH_CHILD").is_some() {
        run_stretch_drain_under_guard();
        // Reached because the guard did NOT abort — the drain is alloc-free.
        std::process::exit(0);
    }

    // Parent arm: re-exec ourselves running only this one test, in the child
    // role, and inspect how it exited.
    let exe = std::env::current_exe().expect("current_exe");
    let status = std::process::Command::new(exe)
        .arg("--exact")
        .arg("voice_pool_update_stretch_drain_is_allocation_free")
        .arg("--nocapture")
        .env("RT_NO_ALLOC_STRETCH_CHILD", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn child test process");

    assert!(
        status.success(),
        "The stretch drain allocated on the audio thread (child exited {status}). \
         The drain must only write the voice's stretch and move a vocoder in — never allocate."
    );
}

/// The body that runs inside the re-exec'd child: enqueue an `UpdateStretch`
/// and drain it under an `RtScope`. Exits cleanly today (drain is
/// alloc-free); a regression that allocates on the drain aborts it.
fn run_stretch_drain_under_guard() {
    let wave = sine_wave(2.0, 48_000.0);

    let (unit, controls) = controlled_pool(VoicePool::from(config_of(
        ChannelLayout::STEREO,
        [(1, placed_parts(&wave, 1.0).into())],
    )));
    let mut unit = Rig::new(unit, 64);

    // The first block installs the voice, outside the guard of the changes below.
    for _ in 0..16 {
        unit.block();
    }

    // The row at unity does not stretch, so a row with a stretch of 2 is a new
    // voice, and it is the table that has to deliver its vocoder. It is built
    // here, on this (control) thread; the guarded drain below only moves it into
    // the slot.
    controls.replace(
        config_of(
            ChannelLayout::STEREO,
            [(1, placed_parts(&wave, 2.0).into())],
        )
        .table(),
    );
    unit.block();

    // The same voice at another factor and pitch: values, applied in place.
    controls.replace(
        config_of(
            ChannelLayout::STEREO,
            [(
                1,
                Row::from(placed_parts(&wave, 2.0))
                    .stretching(StretchFactor::new(1.25), Cents::new(200.0)),
            )],
        )
        .table(),
    );
    unit.block();
}

// ---------------------------------------------------------------------------
// Width-generic (N-channel) allocation gates.
//
// The stereo gates above cannot see a `vec![0.0; channels]` written as a
// "temporary" frame buffer on the width-generic path, because at width 2 the
// old fixed-size stack arrays are still in play. These run the same code at
// width 6, where a runtime-sized frame is the tempting (and wrong) choice.
// ---------------------------------------------------------------------------

/// A 6-channel wave whose channel `c` carries a distinct amplitude, so a
/// wrong-channel read is a wrong value rather than a plausible one.
fn surround_wave(duration_secs: f64, sample_rate: f64) -> Arc<Wave> {
    let mut wave = Wave::zero(6, SampleRate::of(sample_rate), Seconds::new(duration_secs));
    let len = wave.len().get();
    for i in 0..len {
        let t = i as f64 / sample_rate;
        let s = (t * 440.0 * core::f64::consts::TAU).sin() as f32 * 0.5;
        for c in 0..6 {
            wave.set(c, i, s * (c + 1) as f32 * 0.1);
        }
    }
    Arc::new(wave)
}

#[test]
fn memory_source_process_is_allocation_free_at_six_channels() {
    let node = MemorySource::from(
        MemorySourceConfig::new(surround_wave(2.0, 48_000.0), 6u16).expect("a placeable wave"),
    );
    assert_eq!(node.shape().audio_out().count(), 6);
    let mut rig = Rig::new(node, 64);

    for _ in 0..16 {
        rig.block();
    }

    for _ in 0..256 {
        rig.block();
    }
}

/// One-frame blocks at width 6.
#[test]
fn memory_source_one_frame_blocks_are_allocation_free_at_six_channels() {
    let node = MemorySource::from(
        MemorySourceConfig::new(surround_wave(2.0, 48_000.0), 6u16).expect("a placeable wave"),
    );
    let mut rig = Rig::new(node, 1);
    for _ in 0..256 {
        rig.block();
    }

    for _ in 0..2_048 {
        rig.block();
    }
}

/// The MISMATCHED-width path is different code from the matched path — it runs
/// the fold — and only executes when the wave's width differs from the node's.
/// Neither the stereo gates nor the matched 6-channel gates above reach it.
#[test]
fn memory_source_process_is_allocation_free_when_folding_six_to_two() {
    // 6-channel wave, 2-channel node.
    let node = free_source(surround_wave(2.0, 48_000.0));
    assert_eq!(node.shape().audio_out().count(), 2);
    let mut rig = Rig::new(node, 64);

    for _ in 0..16 {
        rig.block();
    }

    for _ in 0..256 {
        rig.block();
    }
}

/// End-to-end: a 6-channel wave placed on a timeline must reach all six outputs
/// of a 6-wide reader, through `process` (the planar path), with no allocation.
///
/// The per-unit tests each cover one hop; this is the only one that exercises
/// the whole in-memory chain at width 6 — `read_frame` -> `MemorySource` ->
/// `PlaybackSlot` -> `VoicePool` -> planar outputs — and so the only one that
/// would catch a width being dropped at a seam rather than inside a node.
#[test]
fn six_channel_clip_reaches_six_reader_outputs_without_allocating() {
    let wave = surround_wave(2.0, 48_000.0);
    let sampler = VoiceNodeConfig::placed(
        MemorySourceConfig::new(wave, ChannelLayout::from(6u16))
            .expect("a placeable wave")
            .window(VoiceWindow {
                start: Beat::new(0.0),
                duration: None,
            }),
    )
    .expect("a finite gain and a nonempty width");
    let reader = VoicePool::from(config_of(6u16, [(1, sampler.into())]));
    assert_eq!(reader.shape().audio_out().count(), 6);
    let mut rig = Rig::new(reader, 64);

    for _ in 0..16 {
        rig.block();
    }

    // Every channel carries the material, not just the front pair.
    for c in 0..6 {
        let energy: f32 = rig
            .direct
            .output()
            .plane(c)
            .samples()
            .iter()
            .map(|s| s.abs())
            .sum();
        assert!(
            energy > 0.0,
            "channel {c} produced silence — a width was dropped at a seam"
        );
    }

    for _ in 0..256 {
        rig.block();
    }
}

// ---------------------------------------------------------------------------
// Table-drain allocation gates.
//
// The block applies a changed table from `process`, so anything it builds is built
// in the audio callback. Two paths are the standing temptation: a new row needs a
// stretch vocoder (an FFT setup plus two block-scratch buffers PER CHANNEL), and a
// loop needs a crossfade. Both are kept off the callback: the controls build every
// voice, and the loop crossfade reads its fade from the wave in place.
//
// These abort (SIGABRT) rather than fail if they regress, like every other gate
// in this file.
// ---------------------------------------------------------------------------

/// A row of the 6-channel `wave` placed at beat 0, stretched by 2: it owns a vocoder,
/// built by the controls.
fn surround_row(wave: &Arc<Wave>) -> Row<f32> {
    VoiceNodeConfig::placed(
        MemorySourceConfig::new(wave.clone(), ChannelLayout::from(6u16))
            .expect("a placeable wave")
            .window(VoiceWindow {
                start: Beat::new(0.0),
                duration: None,
            }),
    )
    .expect("a finite gain and a nonempty width")
    .stretch(StretchFactor::new(2.0))
    .into()
}

/// New stretching rows arriving mid-playback must not build their filters in the drain.
///
/// The filter is built by the controls on this thread, with the voice; the drain only
/// moves the voice into the roster. Width 6 so the cost would be 6 FFT setups and 12
/// scratch buffers if it ever moved back. The drain runs under the guard: a voice
/// arrives boxed and is moved, never freed or rebuilt there.
#[test]
fn a_new_rows_voice_drain_is_allocation_free_at_six_channels() {
    let wave = surround_wave(2.0, 48_000.0);
    let (reader, controls) = controlled_pool(VoicePool::from(config_of(6u16, [])));
    let mut reader = Rig::new(reader, 64);

    // Warm up the drain machinery before the guard.
    for _ in 0..16 {
        reader.block();
    }

    // Write the table OUTSIDE the guard (building the voices allocates, deliberately)
    // and drain it INSIDE it.
    controls.replace(config_of(6u16, (0..8u128).map(|i| (i, surround_row(&wave)))).table());

    reader.block();
    assert_eq!(
        reader.node().voice_count(),
        8,
        "every voice must have arrived"
    );

    // Steady state — nothing new — must be clean. This is what catches a
    // per-block allocation sneaking into the width-generic mix path.
    for _ in 0..64 {
        reader.block();
    }
}

/// Removing stretching rows must not FREE on the audio thread.
///
/// A table without a row retires its slot from `process` — the audio callback. A
/// slot dropped there takes its vocoder with it, and the vocoder owns its
/// vocoders and block scratch (~100 KB per channel), which would be freed
/// inside the callback.
///
/// `RtAllocCheck` traps deallocation as well as allocation, so this is the
/// direct guard. That the control side then frees what was retired, and the wave
/// it held, is `tests/rt_no_free.rs`.
#[test]
fn remove_voice_drain_does_not_free_on_the_audio_thread() {
    let wave = surround_wave(2.0, 48_000.0);
    // Stretching rows, built OUTSIDE the guard: building the filters allocates,
    // deliberately, on the control thread. A filter is what owns the bank this
    // test is about.
    let (reader, controls) = controlled_pool(VoicePool::from(config_of(
        6u16,
        (0..4u128).map(|i| (i, surround_row(&wave))),
    )));
    let mut reader = Rig::new(reader, 64);
    for _ in 0..16 {
        reader.block();
    }
    assert_eq!(reader.node().voice_count(), 4, "the rows must have arrived");

    // Write the removal outside, drain it inside.
    controls.replace(config_of(6u16, []).table());
    reader.block();
    assert_eq!(reader.node().voice_count(), 0, "removes must have drained");
}

/// Changing a loop range mid-playback must not allocate in the drain.
///
/// The loop crossfade holds no buffer: it reads its fade from the wave in place
/// (`LoopSpan`), so a loop change is a store. (It once copied a pre-loop
/// tail into a buffer reserved up front, which this pinned stayed in place.)
#[test]
fn update_loop_drain_is_allocation_free_at_six_channels() {
    let wave = surround_wave(2.0, 48_000.0);
    let placed = || {
        MemorySourceConfig::new(wave.clone(), ChannelLayout::from(6u16))
            .expect("a placeable wave")
            .window(VoiceWindow {
                start: Beat::new(0.0),
                duration: None,
            })
    };
    let table = |looped: Option<tutti_sampler::Loop>| {
        let source = match looped {
            Some(looped) => placed().looped(looped),
            None => placed(),
        };
        let voice = VoiceNodeConfig::placed(source).expect("a finite gain and a nonempty width");
        config_of(6u16, [(1, voice.into())])
    };
    let (reader, controls) = controlled_pool(VoicePool::from(table(None)));
    let mut reader = Rig::new(reader, 64);

    for _ in 0..16 {
        reader.block();
    }

    // Several loop changes, including the on -> off -> on cycle that must
    // reclaim and reuse the same buffer.
    for i in 0..8u64 {
        controls.replace(
            table(Some(loop_frames(
                i as usize * 8,
                i as usize * 8 + 4096,
                256,
            )))
            .table(),
        );
        reader.block();
        controls.replace(table(None).table());
        reader.block();
    }
}

// ---------------------------------------------------------------------------
// VoiceNode's command drain — the newest thing running inside the callback.
// ---------------------------------------------------------------------------

/// Reading a published placement allocates nothing, and frees nothing.
///
/// `VoiceNode::drain_commands` runs at the top of every `process`, which
/// is the audio callback. The pool's drain is careful for a reason its own
/// `AddVoice` doc spells out — a command that carries a `Voice` or a stretch
/// filter has to be *built* on the control thread, or the callback pays for it.
///
/// A node's placement is a `Copy` window in a `Publisher` cell, read once
/// per block and applied as a field write when it changed, so this should be
/// free. "Should be" is exactly the kind of claim this file exists to convert
/// into a measurement.
///
/// Both halves are covered — a block that applies a new window (the eight
/// publishes before the gate leave the last one to apply), and the unchanged
/// read that runs on every other block.
#[test]
fn voice_node_command_drain_does_not_allocate() {
    let source = placed_config(
        sine_wave(1.0, 48_000.0),
        VoiceWindow {
            start: Beat::new(0.0),
            duration: Some(BeatDuration::new(4.0)),
        },
    );

    let (node, handle) =
        tutti_sampler::testing::controlled(voice_node(source, Playback::default()));
    let mut rig = Rig::new(node, 64);

    // Warm-up outside the gate: the first blocks touch lazily-sized state, and
    // the armed `process` would attribute that to the drain.
    for _ in 0..16 {
        rig.block();
    }

    // Queue more than one block's worth, so the drain loop runs repeatedly
    // rather than taking its empty fast path once.
    for i in 0..8u64 {
        handle
            .window_cell()
            .expect("a placed voice")
            .publish(VoiceWindow::span(
                Beat::new(i as f64 * 0.25),
                BeatDuration::new(4.0),
            ));
    }

    // First block drains the eight queued commands...
    rig.block();
    // ...and the rest take the empty path, which is what every steady-state
    // block does.
    for _ in 0..64 {
        rig.block();
    }
}
