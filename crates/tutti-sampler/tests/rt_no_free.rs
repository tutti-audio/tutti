//! Regression gate: the pool's audio thread frees nothing, and a disk stream's
//! ring is freed on the control thread.
//!
//! `tests/voice/rt_no_alloc.rs` guards allocation, and says why it cannot see a
//! free: `assert_no_alloc` counts allocations only. This file counts
//! deallocations, with a global allocator that tallies every `dealloc` made
//! while a thread-local flag is up, so the callback's frees are counted and
//! nothing else's (the test harness's own threads included). A removed row's
//! slot, wave and vocoder are freed by `Reclaim::reclaim` on the control side.

use tutti_buffers::Mono;
use tutti_sampler::VoiceWindow;

use tutti_sampler::testing::{controlled_pool, loop_frames, placed_config};

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use tutti_buffers::{ChannelLayout, Wave};
use tutti_graph::Published;
use tutti_graph::contract::Direct;
use tutti_rt::Reclaim;
use tutti_sampler::testing::MockTransport;
use tutti_sampler::testing::wait_for_open;
use tutti_sampler::{
    DiskStreamer, DiskVoice, Manual, Row, SlotId, StreamHandle, StreamStart, VoiceCount,
    VoiceNodeConfig, VoicePool, VoicePoolConfig, VoicePoolControls,
};
use tutti_units::{Amplitude, Beat, Bpm, Cents, SampleRate, Samples, StretchFactor};

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static FREES: Cell<usize> = const { Cell::new(0) };
    /// The largest single free made while watching, in bytes.
    static BIGGEST: Cell<usize> = const { Cell::new(0) };
}

struct CountFrees;

unsafe impl GlobalAlloc for CountFrees {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // `try_with`: a thread tearing down its locals still frees.
        let _ = WATCHING.try_with(|w| {
            if w.get() {
                let _ = FREES.try_with(|f| f.set(f.get() + 1));
                let _ = BIGGEST.try_with(|b| b.set(b.get().max(layout.size())));
            }
        });
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountFrees = CountFrees;

/// Frees made on this thread while `f` runs.
fn frees_during(f: impl FnOnce()) -> usize {
    watch(f).0
}

/// How many frees, and the largest one in bytes, made on this thread while `f`
/// runs.
fn watch(f: impl FnOnce()) -> (usize, usize) {
    FREES.with(|c| c.set(0));
    BIGGEST.with(|c| c.set(0));
    WATCHING.with(|w| w.set(true));
    f();
    WATCHING.with(|w| w.set(false));
    (FREES.with(|c| c.get()), BIGGEST.with(|c| c.get()))
}

/// A stereo pool config over `rows`.
fn config_of(rows: impl IntoIterator<Item = (u128, Row<f32>)>) -> VoicePoolConfig<f32> {
    rows.into_iter().fold(
        VoicePoolConfig::<f32>::new(ChannelLayout::STEREO).expect("stereo is a pool width"),
        |config, (key, row)| config.row(SlotId(key), row).expect("a free key"),
    )
}

/// A mono wave of 0.5 over `frames` frames: a new allocation each call, so two calls are
/// two identities.
fn flat(frames: usize) -> Arc<Wave> {
    Arc::new(Wave::from_mono(
        SampleRate::SR_48K,
        Mono::new(&vec![0.5f32; frames]),
    ))
}

/// The row of `wave` placed at beat 0, stretched by 2 (a vocoder, built by the sender).
fn stretched(wave: &Arc<Wave>) -> Row<f32> {
    Row::from(
        VoiceNodeConfig::placed(placed_config(
            Arc::clone(wave),
            VoiceWindow::from(Beat::new(0.0)),
        ))
        .expect("a finite gain and a nonempty width")
        .stretch(StretchFactor::new(2.0)),
    )
}

/// A change of the table frees nothing in the callback. Two stretched rows (their
/// vocoders built by the sender) are installed by the first block; a table without the
/// first row, then a table with it back over another wave, are each applied by one
/// block, counting frees on this thread. The removed slot goes through the retirement
/// ring, and `reclaim` on the control side frees it, and the wave it held.
#[test]
fn a_table_change_drain_frees_nothing() {
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let first = flat(48_000);
    let first_weak = Arc::downgrade(&first);
    let second = flat(48_000);
    let (pool, controls) = controlled_pool(VoicePool::from(config_of([
        (1, stretched(&first)),
        (2, stretched(&second)),
    ])));
    let mut rig = Direct::new(pool, SampleRate::SR_48K, 64);
    let env = t.env(64, SampleRate::SR_48K);
    let frees = frees_during(|| {
        rig.block_in(&env);
    });
    assert_eq!(
        rig.node.voice_count(),
        2,
        "the first block installs both rows"
    );
    assert_eq!(
        frees, 0,
        "installing freed {frees} time(s) on the audio thread"
    );

    let without_first = config_of([(2, stretched(&second))]);
    controls.replace(without_first.table());
    let frees = frees_during(|| {
        rig.block_in(&env);
    });
    assert_eq!(rig.node.voice_count(), 1, "the removed row's slot is gone");
    assert_eq!(
        frees, 0,
        "the drain freed {frees} time(s) on the audio thread; a slot must travel to \
         the control thread instead"
    );

    let again = flat(48_000);
    controls.replace(config_of([(1, stretched(&again)), (2, stretched(&second))]).table());
    let frees = frees_during(|| {
        rig.block_in(&env);
    });
    assert_eq!(rig.node.voice_count(), 2, "the row is back under its key");
    assert_eq!(
        frees, 0,
        "the drain freed {frees} time(s) on the audio thread"
    );

    drop(first);
    assert!(
        first_weak.upgrade().is_some(),
        "the removed slot, parked in the retirement ring, still holds its wave"
    );
    controls.reclaim();
    assert!(
        first_weak.upgrade().is_none(),
        "the sweep freed the removed slot, and its wave, on the control side"
    );
}

/// **A table with another wave under the same key frees the old wave on the control
/// side.** The row's voice is a new voice (a wave is structure): the old slot, whose
/// source is the wave's last holder, leaves through the retirement ring in the block
/// that applies the table, which frees nothing, and `reclaim` frees the wave.
#[test]
fn a_table_with_another_wave_under_the_same_key_frees_the_old_wave_on_the_control_side() {
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let old = flat(24_000);
    let old_weak = Arc::downgrade(&old);
    let (pool, controls) = controlled_pool(VoicePool::from(config_of([(1, stretched(&old))])));
    drop(old);
    let mut rig = Direct::new(pool, SampleRate::SR_48K, 64);
    let env = t.env(64, SampleRate::SR_48K);
    rig.block_in(&env);
    assert_eq!(rig.node.voice_count(), 1, "setup: the row is installed");
    assert!(old_weak.upgrade().is_some(), "the slot holds the old wave");

    controls.replace(config_of([(1, stretched(&flat(24_000)))]).table());
    let frees = frees_during(|| {
        rig.block_in(&env);
    });
    assert_eq!(rig.node.voice_count(), 1, "the new voice replaced the old");
    assert_eq!(
        frees, 0,
        "the drain freed {frees} time(s) on the audio thread"
    );
    assert!(
        old_weak.upgrade().is_some(),
        "the old wave is alive after the drain: retired, not freed"
    );
    controls.reclaim();
    assert!(
        old_weak.upgrade().is_none(),
        "the sweep freed the old wave, on the control side"
    );
}

// ---------------------------------------------------------------------------
// A disk stream's ring: who frees it.
// ---------------------------------------------------------------------------

/// A ring is at least 4 096 frames of the file's width in 4-byte slots, so a
/// free of this many bytes or more, in a block or a step, is a ring (the
/// writer's decoder and the voice's buffers are all smaller than this for a
/// stereo file under the default config).
const RING_BYTES: usize = 4_096 * 2 * 4;

/// A stereo WAV two seconds long.
fn write_wav(path: &std::path::Path) {
    let interleaved: Vec<f32> = (0..96_000)
        .flat_map(|i| {
            let s = (i as f32 * 0.01).sin() * 0.25;
            [s, s]
        })
        .collect();
    tutti_fixtures::signal::write_wav_f32(path, tutti_units::SampleRate::SR_48K, 2, &interleaved);
}

/// A hand-stepped streamer streaming a fresh file, and the stream's handle and
/// reader.
fn stream() -> (
    tempfile::TempDir,
    DiskStreamer<Manual<f32>, f32>,
    StreamHandle<f32>,
    tutti_sampler::StreamReader<f32>,
) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("two_seconds.wav");
    write_wav(&path);
    let mut streamer =
        DiskStreamer::manual(SampleRate::SR_48K, Default::default()).expect("builds");
    let opening = streamer.open_stream(&path, StreamStart::HEAD);
    let (stream, reader) = wait_for_open(&mut streamer, opening).expect("the file opens");
    (dir, streamer, stream, reader)
}

/// **Dropping a `StreamHandle` sends; the streamer frees.** With the voice's reader
/// already gone (so the streamer's writer is the ring's last holder), dropping
/// the handle on the control thread frees no ring, and the ring is freed in the
/// streamer's step that handles the close: the thread that steps, never the one
/// that drops the handle.
#[test]
fn dropping_a_stream_frees_the_ring_in_the_streamers_step() {
    let (_dir, mut streamer, stream, reader) = stream();
    drop(reader);

    let (_, biggest) = watch(|| drop(stream));
    assert!(
        biggest < RING_BYTES,
        "dropping the handle freed a {biggest}-byte block: the ring, on the dropping thread"
    );
    let (_, biggest) = watch(|| {
        let _ = streamer.step();
    });
    assert!(
        biggest >= RING_BYTES,
        "the close's step freed at most {biggest} bytes: the ring was not freed there"
    );
}

/// **A `Block` frees nothing when it ends** (the reader's
/// half). A block lends the stream's published control and holds it to its
/// end; the streamer publishes a new one meanwhile (a loop edit), which
/// retires the old one *behind* the block's loan. Dropping the block, on the
/// audio thread, returns the loan and frees nothing; the old control is freed
/// by the streamer's next sweep (`reclaim_control`), on its own thread. So a
/// `StreamReader` and its blocks are free-detecting-clean: the audio thread
/// never owns published state.
#[test]
fn a_block_that_ends_after_a_publish_frees_nothing() {
    let (_dir, mut streamer, stream, mut reader) = stream();
    let none = || Samples::new(0)..Samples::new(0);
    let block = reader.block(Samples::new(0), [none(), none()]);
    stream
        .looping_cell()
        .publish(Some(loop_frames(1_000, 3_000, 0)));
    let _ = streamer.step();

    let frees = frees_during(|| drop(block));
    assert_eq!(
        frees, 0,
        "ending the block freed {frees} time(s) on the audio thread"
    );
    let frees = frees_during(|| {
        let _ = streamer.step();
    });
    assert!(
        frees > 0,
        "the streamer's sweep freed nothing: the retired control leaked"
    );
}

/// A pool of one streamed row over a stream whose handle has been dropped and
/// closed, so the voice's reader is the ring's **last holder**: its drop
/// frees the ring wherever it happens.
fn pool_holding_the_last_holder(
    streamer: DiskStreamer<Manual<f32>, f32>,
    stream: StreamHandle<f32>,
    reader: tutti_sampler::StreamReader<f32>,
    voices: VoiceCount,
    memory: &[(u128, Row<f32>)],
) -> (VoicePool<f32>, VoicePoolControls<f32>) {
    let voice = DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0)));
    let streamed = VoiceNodeConfig::streaming(
        voice,
        stream,
        Amplitude::new(1.0),
        StretchFactor::UNITY,
        Cents::new(0.0),
    )
    .expect("a finite gain and a nonempty width");
    let config = memory
        .iter()
        .cloned()
        .fold(
            VoicePoolConfig::<f32>::new(ChannelLayout::STEREO)
                .expect("stereo is a pool width")
                .voices(voices)
                .expect("the voices hold the rows"),
            |config, (key, row)| config.row(SlotId(key), row).expect("a free key"),
        )
        .row(SlotId(1), streamed)
        .expect("a free key");
    // The streamer is gone: its side of the ring goes with it, and the voice
    // (whose row keeps only the stream's control side) holds the ring last.
    drop(streamer);
    controlled_pool(VoicePool::from(config))
}

/// **The remove path: a voice's last holder of a ring is never dropped on the
/// audio thread.** The row is removed by a table without it; the drain frees
/// nothing (the slot goes over the retirement ring) and the control thread's
/// `reclaim` frees the ring.
#[test]
fn a_removed_disk_voice_frees_its_ring_on_the_control_thread() {
    let (_dir, streamer, stream, reader) = stream();
    let (pool, controls) =
        pool_holding_the_last_holder(streamer, stream, reader, VoiceCount::DEFAULT, &[]);
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut rig = Direct::new(pool, SampleRate::SR_48K, 64);
    let env = t.env(64, SampleRate::SR_48K);
    rig.block_in(&env);
    assert_eq!(rig.node.voice_count(), 1, "setup: the voice is in");

    controls.replace(config_of([]).table());
    let (frees, _) = watch(|| {
        rig.block_in(&env);
    });
    assert_eq!(rig.node.voice_count(), 0);
    assert_eq!(
        frees, 0,
        "the drain freed {frees} time(s) on the audio thread"
    );

    let (_, biggest) = watch(|| {
        controls.reclaim();
    });
    assert!(
        biggest >= RING_BYTES,
        "the control thread's collection freed at most {biggest} bytes: no ring"
    );
}

/// **A retirement room with no place left defers an install; the callback never
/// frees.** The controls reserve a retirement slot for every voice they send, held
/// until it is collected. A pool of two voices has six: with them used by uncollected
/// voices, a row whose voice is new is not sent, so the table's other changes (the
/// removal of the disk voice, the ring's last holder) still apply and the drain frees
/// nothing; after `reclaim` the deferred voice is sent and the ring is freed on the
/// control side.
#[test]
fn a_full_retirement_room_defers_an_install_and_the_callback_frees_nothing() {
    let (_dir, streamer, stream, reader) = stream();
    let wave = flat(480);
    let memory = |wave: &Arc<Wave>| {
        Row::from(
            VoiceNodeConfig::placed(placed_config(
                Arc::clone(wave),
                VoiceWindow::from(Beat::new(0.0)),
            ))
            .expect("a finite gain and a nonempty width"),
        )
    };
    let (pool, controls) = pool_holding_the_last_holder(
        streamer,
        stream,
        reader,
        VoiceCount::of(2),
        &[(2, memory(&wave))],
    );
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut rig = Direct::new(pool, SampleRate::SR_48K, 64);
    let env = t.env(64, SampleRate::SR_48K);
    rig.block_in(&env);
    assert_eq!(rig.node.voice_count(), 2, "setup: both voices are in");

    // Each wave under key 2 is a new voice: with the two first, they use the six
    // places, none collected.
    let current = controls.current();
    let disk = current
        .rows()
        .iter()
        .find(|(key, _)| *key == SlotId(1))
        .map(|(_, row)| row.clone())
        .expect("the disk row");
    drop(current);
    let with = |other: &Arc<Wave>| {
        VoicePoolConfig::<f32>::new(ChannelLayout::STEREO)
            .expect("stereo is a pool width")
            .voices(VoiceCount::of(2))
            .expect("two voices")
            .row(SlotId(1), disk.clone())
            .expect("a free key")
            .row(SlotId(2), memory(other))
            .expect("a free key")
    };
    for _ in 0..4 {
        controls.replace(with(&flat(480)).table());
        rig.block_in(&env);
    }
    assert_eq!(rig.node.voice_count(), 2, "setup: six voices sent");

    // The seventh is deferred: the memory row's old slot leaves and no new one comes.
    let waiting = flat(480);
    controls.replace(with(&waiting).table());
    rig.block_in(&env);
    assert_eq!(rig.node.voice_count(), 1, "only the disk voice sounds");

    // Remove the ring's last holder: it applies, and the callback frees nothing. The
    // test lets go of its own copies of the row first.
    drop(disk);
    controls.replace(config_of([(2, memory(&waiting))]).table());
    let (frees, _) = watch(|| {
        rig.block_in(&env);
    });
    assert_eq!(rig.node.voice_count(), 0, "the disk voice is gone");
    assert_eq!(frees, 0, "the callback freed {frees} time(s)");

    let (_, biggest) = watch(|| {
        controls.reclaim();
    });
    assert!(
        biggest >= RING_BYTES,
        "the control thread's collection freed at most {biggest} bytes: no ring"
    );
    let (frees, _) = watch(|| {
        rig.block_in(&env);
    });
    assert_eq!(rig.node.voice_count(), 1, "the deferred voice arrived");
    assert_eq!(frees, 0, "the callback freed {frees} time(s)");
}

/// **A retirement after the controls are gone is freed with the pool.** The
/// controls are dropped with a removal of the ring's last holder applied by the next
/// block; the callback frees nothing (the voice is parked in the pool), and
/// dropping the pool, on the control side, frees the ring.
#[test]
fn a_retirement_after_the_controls_are_gone_is_freed_when_the_pool_drops() {
    let (_dir, streamer, stream, reader) = stream();
    let (pool, controls) =
        pool_holding_the_last_holder(streamer, stream, reader, VoiceCount::DEFAULT, &[]);
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut rig = Direct::new(pool, SampleRate::SR_48K, 64);
    let env = t.env(64, SampleRate::SR_48K);
    rig.block_in(&env);

    controls.replace(config_of([]).table());
    drop(controls);
    let (frees, _) = watch(|| {
        rig.block_in(&env);
    });
    assert_eq!(rig.node.voice_count(), 0, "the removal was applied");
    assert_eq!(frees, 0, "the callback freed {frees} time(s)");

    let (_, biggest) = watch(|| drop(rig));
    assert!(
        biggest >= RING_BYTES,
        "dropping the pool freed at most {biggest} bytes: the ring was leaked"
    );
}
