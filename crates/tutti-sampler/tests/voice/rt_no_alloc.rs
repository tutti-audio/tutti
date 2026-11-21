//! Regression gate: sampler hot paths must not allocate per-buffer.
//!
//! Each gate here aborts the process (SIGABRT) on a regression rather than
//! failing an assertion, because an allocation inside `assert_no_alloc`'s guard
//! is caught where it happens rather than reported afterwards.
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
//! `DiskSource` is not covered here: it needs a crate-private ring, so its
//! non-allocation (steady state, a jump's scratch copy and fade) is guarded
//! in-crate, in `voice::disk_voice`'s tests.

use tutti_sampler::testing::loop_frames;

use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_buffers::Wave;
use tutti_graph::Node;
use tutti_graph::contract::Direct;
use tutti_sampler::testing::MockTransport;
use tutti_sampler::{
    Direction, MemorySource, MemorySourceConfig, Playback, SendError, SlotId, Voice, VoiceNode,
    VoicePool, VoicePoolHandle, VoiceSource, VoiceWindow,
};
use tutti_units::{
    Amplitude, Beat, BeatDuration, Bpm, Cents, PlaybackRate, SampleRate, Seconds, StretchFactor,
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
    let mut rig = Rig::new(
        MemorySource::new(sine_wave(2.0, 48_000.0)).expect("a placeable wave"),
        64,
    );

    // Warm-up — drains the position update branch.
    for _ in 0..16 {
        rig.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            rig.block();
        }
    });
}

/// One-frame blocks: the sample-accurate caller's path.
#[test]
fn memory_source_one_frame_blocks_are_allocation_free() {
    let mut rig = Rig::new(
        MemorySource::new(sine_wave(2.0, 48_000.0)).expect("a placeable wave"),
        1,
    );
    for _ in 0..256 {
        rig.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            rig.block();
        }
    });
}

/// A stretching voice whose ratio keeps changing renders without allocating.
///
/// The vocoder is built on the control side when the command is sent, and the
/// drain only moves it in, so each block here (a retarget of the vocoder's
/// controls, the input it asks for read at the cursor, and a hop of frames) is
/// guarded. The ratio steps every block, over stretches both sides of unity
/// and a pitch shift, so the vocoder's input count varies from block to block.
#[test]
fn a_stretching_voice_with_a_changing_ratio_is_allocation_free() {
    let wave = sine_wave(4.0, 48_000.0);
    let (pool, handle) = VoicePool::new().with_handle();
    let mut rig = Rig::new(pool, 64);
    let (sampler, play) = placed_parts(&wave, 1.5);
    handle
        .add(SlotId(1), sampler, play)
        .expect("the command queue has room in a test");
    // The add drains, and the vocoder fills past its latency.
    for _ in 0..64 {
        rig.block();
    }

    for step in 0..400u32 {
        let stretch = 0.8 + 0.1 * f64::from(step % 7);
        let cents = 100.0 * f64::from(step % 5);
        handle
            .set_stretch(SlotId(1), StretchFactor::new(stretch), Cents::new(cents))
            .expect("the command queue has room in a test");
        // The block drains the update and renders the voice; the surplus
        // vocoder the update carried leaves through the retirement ring.
        assert_no_alloc::assert_no_alloc(|| rig.block());
        handle.collect_retired();
    }
}

/// A pool of one stretching memory voice (a wave of 4 s, stretched 1.5, shifted
/// 300 cents) placed at beat 0 for `duration` beats (`None`: open-ended), driven in
/// blocks of `block`, and its handle. Warmed outside any guard until the voice sounds.
fn stretching_rig(
    block: usize,
    duration: Option<f64>,
) -> (Rig<VoicePool<f32>>, VoicePoolHandle<f32>) {
    let (pool, handle) = VoicePool::new().with_handle();
    let mut rig = Rig::new(pool, block);
    let source = MemorySource::placed(
        sine_wave(4.0, 48_000.0),
        VoiceWindow {
            start: Beat::new(0.0),
            duration: duration.map(BeatDuration::new),
        },
    )
    .expect("a placeable wave");
    handle
        .add(
            SlotId(1),
            source,
            Playback {
                stretch: StretchFactor::new(1.5),
                pitch: Cents::new(300.0),
                ..Playback::default()
            },
        )
        .expect("the command queue has room in a test");
    for _ in 0..(16_384 / block).max(2) {
        rig.block();
    }
    (rig, handle)
}

/// `blocks` guarded blocks of `rig`.
fn guarded<N: Node>(rig: &mut Rig<N>, blocks: usize) {
    for _ in 0..blocks {
        assert_no_alloc::assert_no_alloc(|| rig.block());
    }
}

/// A loop set while a stretching voice plays allocates nothing: the spare vocoder that
/// primes for the wrap is built with the voice, so setting the loop only changes what the
/// blocks read, and the priming, the swap at the wraps and the reset of the vocoder the
/// swap replaces all run inside the guard.
#[test]
fn a_loop_set_while_a_stretching_voice_plays_does_not_allocate() {
    use tutti_graph::TransportPosition;
    use tutti_music::{FrameClock, LoopRange};
    let region = LoopRange::new(Beat::new(0.125), Beat::new(0.376)).expect("a loop");
    let wave = sine_wave(4.0, 48_000.0);
    let mut pool = VoicePool::new();
    insert(&mut pool, 0, placed_parts(&wave, 1.5));
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
        clock.advance(tutti_units::Samples::new(64), looping);
    };
    // Past the vocoder's priming, no loop yet.
    for _ in 0..80 {
        block(None, &mut direct);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..600 {
            block((i >= 10).then_some(region), &mut direct);
        }
    });
}

/// A stretching voice allocates nothing wherever it seats: its priming (several
/// blocks of vocoder work, in the scratch the owner built) runs inside the guard, at
/// a seek mid-play, at a stop and a start, at a window's end and its re-entry, and in
/// one-frame blocks and in a block past any scratch of a few thousand frames.
#[test]
fn a_stretching_voice_does_not_allocate_when_it_seats() {
    // A seek mid-play, twice (the second lands in the first's priming).
    let (mut rig, _handle) = stretching_rig(64, None);
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
    let (mut rig, _handle) = stretching_rig(64, Some(0.5));
    guarded(&mut rig, 220);
    rig.t.seek(Beat::new(0.1));
    guarded(&mut rig, 40);

    // One-frame blocks: a seat primes over hundreds of them.
    let (mut rig, _handle) = stretching_rig(1, None);
    rig.t.seek(Beat::new(0.2));
    guarded(&mut rig, 700);

    // A block of 8 492 frames, past a few thousand of scratch, across a seek.
    let (mut rig, _handle) = stretching_rig(8_192 + 300, None);
    rig.t.seek(Beat::new(1.0));
    guarded(&mut rig, 3);
}

/// A stretching voice allocates nothing while it primes where a stopped transport stands:
/// the stopped blocks that prime, a locate, and the blocks that play on from it.
#[test]
fn a_stretching_voice_does_not_allocate_while_priming_on_a_stopped_transport() {
    let (pool, handle) = VoicePool::new().with_handle();
    let mut rig = Rig::new(pool, 64);
    rig.t.set_rolling(false);
    let source = MemorySource::placed(sine_wave(4.0, 48_000.0), VoiceWindow::from(Beat::new(0.0)))
        .expect("a placeable wave");
    handle
        .add(
            SlotId(1),
            source,
            Playback {
                stretch: StretchFactor::new(1.5),
                ..Playback::default()
            },
        )
        .expect("the command queue has room in a test");
    rig.block();
    guarded(&mut rig, 80);
    rig.t.seek(Beat::new(0.5));
    guarded(&mut rig, 80);
    rig.t.set_rolling(true);
    guarded(&mut rig, 40);
}

/// Removing a stretching voice and replacing its wave allocate nothing in the drain:
/// the voice is retired for the control thread to free, and a voice that is added
/// again primes inside the guard.
#[test]
fn removing_and_replacing_a_stretching_voice_does_not_allocate() {
    let (mut rig, handle) = stretching_rig(64, None);
    handle
        .replace_wave(SlotId(1), sine_wave(3.0, 48_000.0))
        .expect("the command queue has room in a test");
    guarded(&mut rig, 20);
    handle.collect_retired();
    handle
        .remove(SlotId(1))
        .expect("the command queue has room in a test");
    guarded(&mut rig, 4);
    handle.collect_retired();
    let source = MemorySource::placed(sine_wave(2.0, 48_000.0), VoiceWindow::from(Beat::new(0.0)))
        .expect("a placeable wave");
    handle
        .add(
            SlotId(1),
            source,
            Playback {
                stretch: StretchFactor::new(0.8),
                ..Playback::default()
            },
        )
        .expect("the command queue has room in a test");
    guarded(&mut rig, 40);
}

// ---------------------------------------------------------------------------
// VoicePool — the live-graph workhorse. One node per track, mixing
// down every voice's `MemorySource` each buffer, under the crate's mock
// transport so voices see a running playhead.
// ---------------------------------------------------------------------------

/// Steady-state mixdown must be allocation-free: two voices already present in
/// the slot list, the per-buffer `process()` sums them with no heap traffic.
///
/// Voices are seeded via `insert_voice` (the synchronous, channel-less Populate
/// path) so the guarded loop measures ONLY the per-buffer mixdown — no `Add`
/// command drain, which is a separate concern below.
#[test]
fn voice_pool_process_steady_state_is_allocation_free() {
    let wave = sine_wave(2.0, 48_000.0);

    let mut unit = VoicePool::new();

    for i in 0..2u128 {
        let sampler = MemorySource::placed(wave.clone(), VoiceWindow::from(Beat::new(0.0)))
            .expect("a placeable wave");
        {
            let play = Playback {
                gain: sampler.gain(),
                speed: sampler.speed(),
                loop_: sampler.loop_(),
                direction: Direction::Forward,
                stretch: StretchFactor::new(1.0),
                pitch: Cents::new(0.0),
            };
            unit.insert(SlotId(i), sampler, play)
                .expect("a finite gain and a nonempty width");
        };
    }
    assert_eq!(unit.voice_count(), 2);

    let mut rig = Rig::new(unit, 64);

    // Warm-up — the empty command drain + first position reads.
    for _ in 0..16 {
        rig.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            rig.block();
        }
    });
}

/// A cloned pool with a stretching voice renders its blocks without
/// allocating.
///
/// Every other pool test drives a freshly constructed node, and the two
/// stretch tests never clone a pool, so this is the pool's RT contract across
/// a clone. The clone needs no preparation first: its filters build their
/// own scratch, and the pool builds its own block lanes
/// (the slot reads through the vocoder into them). (The
/// graph never clones a node — a pool's fork is a fresh, empty pool —
/// so a clone is what a host holding a template makes.)
#[test]
fn cloned_voice_pool_with_stretch_is_allocation_free() {
    let wave = sine_wave(2.0, 48_000.0);

    let mut original = VoicePool::new();

    let sampler = MemorySource::placed(wave.clone(), VoiceWindow::from(Beat::new(0.0)))
        .expect("a placeable wave");
    {
        let play = Playback {
            gain: sampler.gain(),
            speed: sampler.speed(),
            loop_: sampler.loop_(),
            direction: Direction::Forward,
            // Actually stretching: a unity voice has no vocoder, so the
            // read would never touch one and the test would pass either way.
            stretch: StretchFactor::new(2.0),
            pitch: Cents::new(0.0),
        };
        original
            .insert(SlotId(0), sampler, play)
            .expect("a finite gain and a nonempty width");
    };

    let mut rig = Rig::new(original.clone(), 64);

    // Warm past the vocoder's fill-up so `process` is on its steady-state path.
    for _ in 0..64 {
        rig.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            rig.block();
        }
    });
}

/// Same steady-state guard in one-frame blocks: the sample-accurate caller's
/// path.
#[test]
fn voice_pool_one_frame_blocks_are_allocation_free() {
    let wave = sine_wave(2.0, 48_000.0);

    let mut unit = VoicePool::new();

    for i in 0..2u128 {
        let sampler = MemorySource::placed(wave.clone(), VoiceWindow::from(Beat::new(0.0)))
            .expect("a placeable wave");
        {
            let play = Playback {
                gain: sampler.gain(),
                speed: sampler.speed(),
                loop_: sampler.loop_(),
                direction: Direction::Forward,
                stretch: StretchFactor::new(1.0),
                pitch: Cents::new(0.0),
            };
            unit.insert(SlotId(i), sampler, play)
                .expect("a finite gain and a nonempty width");
        };
    }

    let mut rig = Rig::new(unit, 1);
    for _ in 0..256 {
        rig.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            rig.block();
        }
    });
}

/// Block lengths from one frame to the maximum, straddling 64.
const RAGGED: [usize; 10] = [1, 37, 100, 511, 512, 64, 200, 129, 3, 65];

/// `node` prepared at 64 frames, then re-prepared at 512 (the scratch
/// resized on the control thread), warmed, then 2 000 blocks of
/// [`RAGGED`] lengths inside the guard, on a transport that moves and winds
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
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..2_000 {
            step(i, &mut direct);
        }
    });
}

/// A memory source reading `wave` placed at beat 0, and how it plays:
/// `stretch`.
fn placed_parts(wave: &Arc<Wave>, stretch: f64) -> (MemorySource<f32>, Playback) {
    let sampler = MemorySource::placed(wave.clone(), VoiceWindow::from(Beat::new(0.0)))
        .expect("a placeable wave");
    let play = Playback {
        gain: sampler.gain(),
        speed: sampler.speed(),
        loop_: sampler.loop_(),
        direction: Direction::Forward,
        stretch: StretchFactor::new(stretch),
        pitch: Cents::new(0.0),
    };
    (sampler, play)
}

/// [`placed_parts`] as a voice.
fn placed_voice(wave: &Arc<Wave>, stretch: f64) -> Voice<f32> {
    let (sampler, play) = placed_parts(wave, stretch);
    Voice {
        source: VoiceSource::Memory(sampler),
        play,
    }
}

/// Inserts `parts` in `pool` at `id`.
fn insert(pool: &mut VoicePool<f32>, id: u128, (source, play): (MemorySource<f32>, Playback)) {
    pool.insert(SlotId(id), source, play)
        .expect("a finite gain and a nonempty width");
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

    let mut pool = VoicePool::new();
    insert(&mut pool, 0, placed_parts(&wave, 1.0));
    insert(&mut pool, 1, placed_parts(&wave, 1.0));
    insert(&mut pool, 2, placed_parts(&wave, 2.0));
    ragged_at_512(pool);

    // Looped, at unit rate and varispeed (the block read's looped lanes,
    // straight from the wave and gathered), and one placed past the
    // transport (a silent voice the pool skips).
    let looped = |speed: f64| {
        let (source, mut play) = placed_parts(&wave, 1.0);
        play.loop_ = Some(loop_frames(1_000, 30_000, 256));
        play.speed = PlaybackRate::of(speed);
        (source, play)
    };
    let mut pool = VoicePool::new();
    insert(&mut pool, 0, looped(1.0));
    insert(&mut pool, 1, looped(1.37));
    let late = MemorySource::placed(wave.clone(), VoiceWindow::from(Beat::new(1_000.0)))
        .expect("a placeable wave");
    let (_, silent) = placed_parts(&wave, 1.0);
    insert(&mut pool, 2, (late, silent));
    ragged_at_512(pool);

    ragged_at_512(
        VoiceNode::new(placed_voice(&wave, 0.5)).expect("a finite gain and a nonempty width"),
    );

    ragged_at_512(
        MemorySource::placed(wave.clone(), VoiceWindow::from(Beat::new(0.0)))
            .expect("a placeable wave"),
    );

    let free = MemorySource::new(wave.clone()).expect("a placeable wave");
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
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..400 {
            wraps += usize::from(step(i, &mut direct, &mut heard));
        }
    });
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
    let mut pool = VoicePool::new();
    insert(&mut pool, 0, placed_parts(&wave, 1.0));
    wraps_without_allocating(pool, true);
    wraps_without_allocating(
        VoiceNode::new(placed_voice(&wave, 1.0)).expect("a finite gain and a nonempty width"),
        true,
    );
    // Stretched: every wrap is a reseat, which primes inside the guard.
    let mut stretched = VoicePool::new();
    insert(&mut stretched, 0, placed_parts(&wave, 1.5));
    wraps_without_allocating(stretched, false);
    wraps_without_allocating(
        VoiceNode::new(placed_voice(&wave, 1.5)).expect("a finite gain and a nonempty width"),
        false,
    );
    wraps_without_allocating(
        MemorySource::placed(wave.clone(), VoiceWindow::from(Beat::new(0.0)))
            .expect("a placeable wave"),
        true,
    );
}

/// The `VoiceCommand::UpdateStretch` drain is allocation-free.
///
/// Draining an `UpdateStretch` calls `PlaybackSlot::set_stretch`, which writes
/// the voice's stretch and pitch and — when the slot has no vocoder yet — moves
/// in the one the sender built. Every one of those is a store or a move; nothing
/// is constructed here. So the per-block command drain stays
/// alloc-free even when a stretch update lands on it.
///
/// The drain must not *free* either: a surplus vocoder (one that arrived for a slot
/// that already had one) goes to the retirement channel instead of being dropped
/// here, because dropping a vocoder frees its rings. That half
/// is pinned by
/// `a_redundant_stretch_filter_is_retired_not_freed_on_the_audio_thread`; this
/// test would not catch it, since `assert_no_alloc` counts allocations, and a free
/// is not one.
///
/// The `RtAllocCheck` global allocator *aborts* the process (SIGABRT via
/// `handle_alloc_error`) on a violation — it cannot be caught with
/// `catch_unwind`. An allocation here would take the whole test binary down,
/// hiding every other test's result. So the actual `assert_no_alloc`-guarded
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
        "UpdateStretch drain allocated on the audio thread (child exited {status}). \
         The drain must only write the voice's stretch and move a vocoder in — never allocate."
    );
}

/// The body that runs inside the re-exec'd child: enqueue an `UpdateStretch`
/// and drain it inside `assert_no_alloc`. Exits cleanly today (drain is
/// alloc-free); a regression that allocates on the drain aborts it.
fn run_stretch_drain_under_guard() {
    let wave = sine_wave(2.0, 48_000.0);

    let (unit, handle) = VoicePool::new().with_handle();
    let mut unit = Rig::new(unit, 64);

    let sampler = MemorySource::placed(wave.clone(), VoiceWindow::from(Beat::new(0.0)))
        .expect("a placeable wave");
    handle
        .add(SlotId(1), sampler, Playback::default())
        .expect("the command queue has room in a test");

    // Drain the Add first, outside the guard.
    for _ in 0..16 {
        unit.block();
    }

    // Enqueue a stretch update. The voice spawned at `Playback::default()` —
    // unity factor, zero cents — so it has NO resident vocoder, and this is the
    // command that has to deliver one. `send` builds it here, on this (control)
    // thread; the guarded drain below only moves it into the slot.
    //
    // That makes this the interesting case rather than a trivial one: before the
    // filter was carried on the command, the drain had nothing to install and
    // this test passed by doing nothing at all.
    handle
        .set_stretch(SlotId(1), StretchFactor::new(2.0), Cents::new(0.0))
        .expect("the command queue has room in a test");

    assert_no_alloc::assert_no_alloc(|| {
        unit.block();
    });
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
    let node =
        MemorySource::with_channels(surround_wave(2.0, 48_000.0), 6u16).expect("a placeable wave");
    assert_eq!(node.shape().audio_out().count(), 6);
    let mut rig = Rig::new(node, 64);

    for _ in 0..16 {
        rig.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            rig.block();
        }
    });
}

/// One-frame blocks at width 6.
#[test]
fn memory_source_one_frame_blocks_are_allocation_free_at_six_channels() {
    let node =
        MemorySource::with_channels(surround_wave(2.0, 48_000.0), 6u16).expect("a placeable wave");
    let mut rig = Rig::new(node, 1);
    for _ in 0..256 {
        rig.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            rig.block();
        }
    });
}

/// The MISMATCHED-width path is different code from the matched path — it runs
/// the fold — and only executes when the wave's width differs from the node's.
/// Neither the stereo gates nor the matched 6-channel gates above reach it.
#[test]
fn memory_source_process_is_allocation_free_when_folding_six_to_two() {
    // 6-channel wave, 2-channel node.
    let node = MemorySource::new(surround_wave(2.0, 48_000.0)).expect("a placeable wave");
    assert_eq!(node.shape().audio_out().count(), 2);
    let mut rig = Rig::new(node, 64);

    for _ in 0..16 {
        rig.block();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            rig.block();
        }
    });
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
    let mut reader = VoicePool::with_channels(6u16).expect("a width the sampler reads");
    // `placement: None` on the Playback record, so the sampler's OWN placement
    // is what gates playback.
    let sampler = MemorySource::with_config(
        wave,
        MemorySourceConfig {
            placement: Some(VoiceWindow {
                start: Beat::new(0.0),
                duration: None,
            }),
            channels: ChannelLayout::from(6u16),
            ..Default::default()
        },
    )
    .expect("a placeable wave");
    {
        let play = Playback {
            gain: sampler.gain(),
            speed: sampler.speed(),
            loop_: sampler.loop_(),
            direction: Direction::Forward,
            stretch: StretchFactor::new(1.0),
            pitch: Cents::new(0.0),
        };
        reader
            .insert(SlotId(1), sampler, play)
            .expect("a finite gain and a nonempty width");
    };
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

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            rig.block();
        }
    });
}

// ---------------------------------------------------------------------------
// Command-drain allocation gates.
//
// `drain_commands` runs from `process`, so anything it builds is built
// in the audio callback. Two paths are the standing temptation: `AddVoice`
// needs a stretch vocoder (an FFT setup plus two block-scratch buffers PER
// CHANNEL), and `UpdateLoop` needs a pre-loop crossfade plus a read buffer.
// Both are kept off the callback — the sender builds the filter, and the
// crossfade buffer is resident and re-pointed in place.
//
// These abort (SIGABRT) rather than fail if they regress, like every other gate
// in this file.
// ---------------------------------------------------------------------------

/// Adding a STRETCHING voice mid-playback must not build its filter in the drain.
///
/// The filter is built by `VoicePoolHandle::send` on this thread; the
/// drain only moves it into the slot. Width 6 so the cost would be 6 FFT setups
/// and 12 scratch buffers if it ever moved back.
///
/// # What this test can and cannot assert
///
/// It cannot wrap the drain in `assert_no_alloc`, and the reason is worth
/// recording rather than working around: `VoiceCommand::AddVoice` carries a
/// `Box<Voice>`, so draining it *drops a Box* — and `assert_no_alloc`
/// instruments `dealloc` as well as `alloc`, so the free alone is a violation.
/// That box predates this work and is deliberate (a `Voice` is far larger than
/// the other variants; boxing keeps the bounded queue small).
///
/// So the assertion is structural instead: after draining, every stretching
/// slot must ALREADY hold its filter. If construction moved back into the
/// drain, the filter would still be present and this would still pass — which
/// is why `add_voice_drain_does_not_build_the_stretch_filter` below tests the
/// same property the other way, by checking that a filter which the sender did
/// NOT build stays absent.
#[test]
fn add_voice_drain_is_allocation_free_at_six_channels() {
    let wave = surround_wave(2.0, 48_000.0);
    let (reader, handle) = VoicePool::with_channels(6u16)
        .expect("a width the sampler reads")
        .with_handle();
    let mut reader = Rig::new(reader, 64);

    // Warm up the drain machinery before the guard.
    for _ in 0..16 {
        reader.block();
    }

    // Queue the adds OUTSIDE the guard (send allocates, deliberately) and drain
    // them INSIDE it.
    for i in 0..8u128 {
        let sampler = MemorySource::with_config(
            wave.clone(),
            MemorySourceConfig {
                placement: Some(VoiceWindow {
                    start: Beat::new(0.0),
                    duration: None,
                }),
                channels: ChannelLayout::from(6u16),
                ..Default::default()
            },
        )
        .expect("a placeable wave");
        handle
            .add(
                SlotId(i),
                sampler,
                Playback {
                    // Non-unity: this is the branch that needs a filter.
                    stretch: StretchFactor::new(2.0),
                    ..Default::default()
                },
            )
            .expect("the command queue has room in a test");
    }

    reader.block();
    assert_eq!(reader.node().voice_count(), 8, "all adds must have drained");

    // Steady state — an empty queue — must be clean. This is what catches a
    // per-block allocation sneaking into the width-generic mix path.
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..64 {
            reader.block();
        }
    });
}

/// Removing a stretching voice must not FREE on the audio thread.
///
/// `VoiceCommand::Remove` is handled by `voices.retain(...)` inside
/// `drain_commands`, which runs from `process` — the audio callback. A
/// slot dropped there takes its vocoder with it, and the vocoder owns its
/// vocoders and block scratch (~100 KB per channel), which would be freed
/// inside the callback.
///
/// `assert_no_alloc` traps deallocation as well as allocation, so this is the
/// direct guard. It was missing: every other drain test here covers `AddVoice`,
/// `UpdateStretch`, or `UpdateLoop`.
#[test]
fn remove_voice_drain_does_not_free_on_the_audio_thread() {
    let wave = surround_wave(2.0, 48_000.0);
    let (reader, handle) = VoicePool::with_channels(6u16)
        .expect("a width the sampler reads")
        .with_handle();
    let mut reader = Rig::new(reader, 64);

    for _ in 0..16 {
        reader.block();
    }

    // Add stretching voices OUTSIDE the guard — building the filters allocates,
    // deliberately, on the control thread.
    for i in 0..4u128 {
        let sampler = MemorySource::with_config(
            wave.clone(),
            MemorySourceConfig {
                placement: Some(VoiceWindow {
                    start: Beat::new(0.0),
                    duration: None,
                }),
                channels: ChannelLayout::from(6u16),
                ..Default::default()
            },
        )
        .expect("a placeable wave");
        handle
            .add(
                SlotId(i),
                sampler,
                Playback {
                    // Non-unity: this is what makes a filter resident, and the
                    // filter is what owns the bank this test is about.
                    stretch: StretchFactor::new(2.0),
                    ..Default::default()
                },
            )
            .expect("the command queue has room in a test");
    }
    reader.block();
    assert_eq!(reader.node().voice_count(), 4, "adds must have drained");

    // Queue the removes outside, drain them inside.
    for i in 0..4u128 {
        handle
            .remove(SlotId(i))
            .expect("the command queue has room in a test");
    }
    assert_no_alloc::assert_no_alloc(|| {
        reader.block();
    });
    assert_eq!(reader.node().voice_count(), 0, "removes must have drained");
}

/// The deferred free must actually happen on the control thread.
///
/// The RT guard above proves the callback does not free. That alone would also
/// be satisfied by never freeing at all — a leak. This pins the other half:
/// `collect_retired` returns the slots and drops them here, off the callback.
#[test]
fn collect_retired_frees_the_removed_slots_on_the_control_thread() {
    let wave = surround_wave(2.0, 48_000.0);
    let (reader, handle) = VoicePool::with_channels(6u16)
        .expect("a width the sampler reads")
        .with_handle();
    let mut reader = Rig::new(reader, 64);

    for i in 0..3u128 {
        let sampler = MemorySource::with_config(
            wave.clone(),
            MemorySourceConfig {
                placement: Some(VoiceWindow {
                    start: Beat::new(0.0),
                    duration: None,
                }),
                channels: ChannelLayout::from(6u16),
                ..Default::default()
            },
        )
        .expect("a placeable wave");
        handle
            .add(
                SlotId(i),
                sampler,
                Playback {
                    stretch: StretchFactor::new(2.0),
                    ..Default::default()
                },
            )
            .expect("the command queue has room in a test");
    }
    reader.block();
    assert_eq!(reader.node().voice_count(), 3);

    // Nothing retired yet.
    assert_eq!(handle.collect_retired(), 0, "no removes have been drained");

    for i in 0..3u128 {
        handle
            .remove(SlotId(i))
            .expect("the command queue has room in a test");
    }
    reader.block();
    assert_eq!(reader.node().voice_count(), 0, "removes must have drained");

    // The slots are now parked in the channel, still allocated. Collecting
    // frees them here, on this thread.
    assert_eq!(
        handle.collect_retired(),
        3,
        "the removed slots must reach the control thread to be freed"
    );
    assert_eq!(handle.collect_retired(), 0, "and only once");
}

/// **A pool's owner sweeps its retired slots through `Reclaim`**: the session's
/// reclaim phase frees what the callback parked, so the retire ring never
/// fills and sends a free back into the callback.
#[test]
fn reclaim_frees_the_removed_slots_a_callback_parked() {
    let wave = surround_wave(2.0, 48_000.0);
    let (reader, handle) = VoicePool::with_channels(6u16)
        .expect("a width the sampler reads")
        .with_handle();
    let mut reader = Rig::new(reader, 64);
    let sampler = MemorySource::with_config(
        wave,
        MemorySourceConfig {
            placement: Some(VoiceWindow {
                start: Beat::new(0.0),
                duration: None,
            }),
            channels: ChannelLayout::from(6u16),
            ..Default::default()
        },
    )
    .expect("a placeable wave");
    handle
        .add(SlotId(1), sampler, Playback::default())
        .expect("room in the queue");
    reader.block();
    handle.remove(SlotId(1)).expect("room in the queue");
    reader.block();
    assert_eq!(reader.node().voice_count(), 0, "the remove drained");

    assert_eq!(
        tutti_rt::Reclaim::reclaim(&handle),
        0,
        "nothing stays pinned"
    );
    assert_eq!(
        handle.collect_retired(),
        0,
        "the sweep already freed the parked slot"
    );
}

/// An `AddVoice` past the pool's 64 slots neither grows the slot vector
/// nor frees the voice in the callback.
///
/// The slot vector is reserved to `MAX_RESIDENT_VOICES` (64), and the drain
/// runs inside `process`. A push past the reservation would reallocate the
/// vector there, and dropping the voice instead would free its box, its
/// source and the stretch vocoder the sender built for it. The drain refuses
/// the add, hands the voice back through the retirement channel, and counts
/// it; the control thread frees it in `collect_retired`.
///
/// The refused voice is a stretched one (it carries a filter, the largest
/// thing a refusal could free) over a wave only it holds, so a `Weak` to
/// the wave says where it was freed.
#[test]
fn an_add_past_the_voice_cap_is_refused_without_allocating() {
    const CAP: usize = 64;
    let wave = sine_wave(0.5, 48_000.0);
    let (pool, handle) = VoicePool::new().with_handle();
    let mut rig = Rig::new(pool, 64);

    let voice_over = |wave: Arc<Wave>, play: Playback| {
        let source = MemorySource::new(wave)
            .expect("a placeable wave")
            .placed_at(VoiceWindow {
                start: Beat::new(0.0),
                duration: None,
            });
        (source, play)
    };

    // Fill the pool to its reservation, outside the guard, in batches the
    // command queue holds.
    for batch in 0..(CAP / 32) {
        for i in 0..32 {
            let (source, play) = voice_over(wave.clone(), Playback::default());
            handle
                .add(SlotId((batch * 32 + i) as u128), source, play)
                .expect("the command queue has room in a test");
        }
        rig.block();
    }
    for _ in 0..4 {
        rig.block();
    }
    assert_eq!(rig.node().voice_count(), CAP, "the pool is full");
    assert_eq!(handle.drops().voices_refused, 0);

    // One more, at a new id: stretched, so `send` builds it a filter here.
    let extra = sine_wave(0.5, 48_000.0);
    let extra_weak = Arc::downgrade(&extra);
    let (source, play) = voice_over(
        extra,
        Playback {
            stretch: StretchFactor::new(2.0),
            ..Default::default()
        },
    );
    handle
        .add(SlotId(CAP as u128), source, play)
        .expect("the command queue has room in a test");

    assert_no_alloc::assert_no_alloc(|| {
        rig.block();
    });

    assert_eq!(rig.node().voice_count(), CAP, "the add was refused");
    assert_eq!(handle.drops().voices_refused, 1, "and counted");
    assert!(
        extra_weak.upgrade().is_some(),
        "the refused voice waits in the retirement channel"
    );
    assert_eq!(
        handle.set_gain(SlotId(CAP as u128), Amplitude::new(0.5)),
        Ok(()),
        "until collected, the handle counts the refused id as live"
    );
    assert_eq!(handle.collect_retired(), 1, "it is handed back");
    assert!(
        extra_weak.upgrade().is_none(),
        "and freed here, on the control thread"
    );
    assert_eq!(
        handle.set_gain(SlotId(CAP as u128), Amplitude::new(0.5)),
        Err(SendError::NoSuchSlot(SlotId(CAP as u128))),
        "the refused add holds no slot, so its id is free again"
    );
}

/// A refusal for an id that was removed and added again since does not free
/// the new voice's id: the collect frees an id only when it is still the add
/// that was refused.
#[test]
fn a_stale_refusal_leaves_a_readded_id_live() {
    const CAP: u128 = 64;
    let wave = sine_wave(0.5, 48_000.0);
    let (pool, handle) = VoicePool::new().with_handle();
    let mut rig = Rig::new(pool, 64);
    let add = |id: u128| {
        let source = MemorySource::new(wave.clone())
            .expect("a placeable wave")
            .placed_at(VoiceWindow::default());
        handle
            .add(SlotId(id), source, Playback::default())
            .expect("room");
    };
    for batch in 0..2 {
        for i in 0..32 {
            add(batch * 32 + i);
        }
        rig.block();
    }
    add(CAP);
    rig.block();
    assert_eq!(handle.drops().voices_refused, 1, "the pool was full");

    // Room, then the refused id again: this add is taken.
    handle.remove(SlotId(0)).expect("room");
    handle.remove(SlotId(CAP)).expect("still counted live");
    add(CAP);
    rig.block();
    assert_eq!(rig.node().voice_count(), CAP as usize, "the re-add landed");

    assert_eq!(handle.collect_retired(), 2, "the old slot and the refusal");
    assert_eq!(
        handle.set_gain(SlotId(CAP), Amplitude::new(0.5)),
        Ok(()),
        "the refusal was the earlier add's: the re-added voice stays live"
    );
}

/// Changing a loop range mid-playback must not allocate in the drain.
///
/// The loop crossfade holds no buffer: it reads its fade from the wave in
/// place (`LoopSpan`), so a loop change is a store. (It once copied a pre-loop
/// tail into a buffer reserved up front, which this pinned stayed in place.)
#[test]
fn update_loop_drain_is_allocation_free_at_six_channels() {
    let wave = surround_wave(2.0, 48_000.0);
    let (reader, handle) = VoicePool::with_channels(6u16)
        .expect("a width the sampler reads")
        .with_handle();
    let mut reader = Rig::new(reader, 64);

    let sampler = MemorySource::with_config(
        wave,
        MemorySourceConfig {
            placement: Some(VoiceWindow {
                start: Beat::new(0.0),
                duration: None,
            }),
            channels: ChannelLayout::from(6u16),
            ..Default::default()
        },
    )
    .expect("a placeable wave");
    handle
        .add(SlotId(1), sampler, Playback::default())
        .expect("the command queue has room in a test");

    for _ in 0..16 {
        reader.block();
    }

    // Several loop changes, including the on -> off -> on cycle that must
    // reclaim and reuse the same buffer.
    for i in 0..8u64 {
        handle
            .set_loop(
                SlotId(1),
                Some(loop_frames(i as usize * 8, i as usize * 8 + 4096, 256)),
            )
            .expect("the command queue has room in a test");
        handle
            .set_loop(SlotId(1), None)
            .expect("the command queue has room in a test");
    }

    assert_no_alloc::assert_no_alloc(|| {
        reader.block();
    });
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
    let source = MemorySource::new(sine_wave(1.0, 48_000.0))
        .expect("a placeable wave")
        .placed_at(VoiceWindow {
            start: Beat::new(0.0),
            duration: Some(BeatDuration::new(4.0)),
        });
    source.play();

    let (node, handle) = tutti_sampler::VoiceNode::with_channels(
        Voice {
            source: VoiceSource::Memory(source),
            play: Playback::default(),
        },
        ChannelLayout::STEREO,
    )
    .expect("a finite gain and a nonempty width")
    .with_handle();
    let mut rig = Rig::new(node, 64);

    // Warm-up outside the gate: the first blocks touch lazily-sized state, and
    // `assert_no_alloc` would attribute that to the drain.
    for _ in 0..16 {
        rig.block();
    }

    // Queue more than one block's worth, so the drain loop runs repeatedly
    // rather than taking its empty fast path once.
    for i in 0..8u64 {
        handle.window_cell().publish(Some(VoiceWindow::span(
            Beat::new(i as f64 * 0.25),
            BeatDuration::new(4.0),
        )));
    }

    assert_no_alloc::assert_no_alloc(|| {
        // First block drains the eight queued commands...
        rig.block();
        // ...and the rest take the empty path, which is what every steady-state
        // block does.
        for _ in 0..64 {
            rig.block();
        }
    });
}

/// A `ReplaceWave` drain frees no wave on the audio thread: the wave it
/// lets go of leaves through the retirement channel and is freed by
/// `collect_retired`, on the control side.
///
/// Both waves this sheds are the last reference to their buffer, so dropping
/// either in the drain would free a decoded `Wave` inside the guard: the old
/// wave a memory slot swaps out (its only owner is the source), and the new
/// wave of a `ReplaceWave` aimed at a slot that does not exist (its only owner
/// is the command). A `Weak` to each tells where it was freed: alive after
/// the drain, gone after `collect_retired`.
#[test]
fn replacing_a_wave_retires_the_old_one_rather_than_freeing_it_in_the_drain() {
    let (pool, handle) = VoicePool::new().with_handle();
    let mut rig = Rig::new(pool, 64);

    let old = sine_wave(0.5, 48_000.0);
    let old_weak = Arc::downgrade(&old);
    // `old` moves into the voice: the source holds its only reference.
    let (source, play) = {
        let parts = placed_parts(&old, 1.0);
        drop(old);
        parts
    };
    handle
        .add(SlotId(1), source, play)
        .expect("the command queue has room in a test");
    rig.block();
    assert_eq!(rig.node().voice_count(), 1, "the add must have landed");
    assert_eq!(
        old_weak.strong_count(),
        1,
        "the source must hold the old wave's only reference"
    );

    // Swap it out inside the guard.
    let new = sine_wave(0.5, 48_000.0);
    handle
        .replace_wave(SlotId(1), new)
        .expect("the command queue has room in a test");
    assert_no_alloc::assert_no_alloc(|| rig.block());
    assert_eq!(
        old_weak.strong_count(),
        1,
        "the old wave must still be alive after the drain: retired, not freed"
    );
    assert_eq!(handle.collect_retired(), 1, "the old wave must be retired");
    assert_eq!(
        old_weak.strong_count(),
        0,
        "collect_retired must free the old wave, on the control side"
    );

    // A swap for a slot that does not exist is refused at the call: the wave
    // is dropped by the caller's own call, and nothing is queued.
    let orphan = sine_wave(0.5, 48_000.0);
    let orphan_weak = Arc::downgrade(&orphan);
    assert_eq!(
        handle.replace_wave(SlotId(99), orphan),
        Err(SendError::NoSuchSlot(SlotId(99)))
    );
    assert_eq!(orphan_weak.strong_count(), 0, "freed here, never queued");
    assert_no_alloc::assert_no_alloc(|| rig.block());
    assert_eq!(handle.collect_retired(), 0, "nothing reached the pool");
}
