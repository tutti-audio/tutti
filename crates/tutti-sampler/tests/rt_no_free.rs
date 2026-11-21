//! Regression gate: the pool's command drain must not free on the audio
//! thread, and a disk stream's ring is freed on the control thread.
//!
//! `tests/rt_no_alloc.rs` guards allocation, and says why it cannot see a
//! free: `assert_no_alloc` counts allocations only. This file counts
//! deallocations, with a global allocator that tallies every `dealloc` made
//! while a thread-local flag is up, so the drain's frees are counted and
//! nothing else's (the test harness's own threads included).

use tutti_buffers::Mono;
use tutti_sampler::VoiceWindow;

use tutti_sampler::testing::loop_frames;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use tutti_buffers::Wave;
use tutti_graph::contract::Direct;
use tutti_sampler::testing::MockTransport;
use tutti_sampler::testing::wait_for_open;
use tutti_sampler::{
    DiskStreamer, DiskVoice, Manual, MemorySource, Playback, SendError, SlotId, StreamHandle,
    StreamStart, VoicePool,
};
use tutti_units::{Beat, Bpm, SampleRate, Samples, StretchFactor};

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

/// Adding voices, and removing one, frees nothing in the drain. Two stretched
/// adds (their filters built by the sender), a remove of the first and an add
/// at its id again are drained by one `process`, counting frees on this
/// thread. The removed slot comes back through the retirement channel, where
/// the control thread frees it.
#[test]
fn the_add_voice_drain_frees_nothing() {
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let wave = Arc::new(Wave::from_mono(
        SampleRate::SR_48K,
        Mono::new(&vec![0.5f32; 48_000]),
    ));
    let (pool, handle) = VoicePool::new().with_handle();
    let mut rig = Direct::new(pool, SampleRate::SR_48K, 64);
    let env = t.env(64, SampleRate::SR_48K);
    let voice = || {
        (
            MemorySource::placed(Arc::clone(&wave), VoiceWindow::from(Beat::new(0.0)))
                .expect("a placeable wave"),
            Playback {
                stretch: StretchFactor::new(2.0),
                ..Playback::default()
            },
        )
    };
    for id in [1, 2] {
        let (source, play) = voice();
        handle
            .add(SlotId(id), source, play)
            .expect("the command queue has room");
    }
    handle
        .remove(SlotId(1))
        .expect("the command queue has room");
    let (source, play) = voice();
    handle
        .add(SlotId(1), source, play)
        .expect("the command queue has room");
    let frees = frees_during(|| {
        rig.block_in(&env);
    });
    assert_eq!(
        rig.node.voice_count(),
        2,
        "the last add took the id the remove freed"
    );
    assert_eq!(
        frees, 0,
        "the drain freed {frees} time(s) on the audio thread; a box or a \
         replaced slot must travel to the control thread instead"
    );
    assert_eq!(
        handle.collect_retired(),
        1,
        "the removed slot is retired to the control thread"
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

/// A pool with the disk voice of a stream whose handle has been dropped and
/// closed, so the voice's reader is the ring's **last holder**: its drop
/// frees the ring wherever it happens.
fn pool_holding_the_last_holder(
    streamer: DiskStreamer<Manual<f32>, f32>,
    stream: StreamHandle<f32>,
    reader: tutti_sampler::StreamReader<f32>,
) -> (VoicePool<f32>, tutti_sampler::VoicePoolHandle<f32>) {
    let (pool, handle) = VoicePool::new().with_handle();
    let voice = DiskVoice::from_reader(reader, VoiceWindow::from(Beat::new(0.0)));
    handle
        .add_streaming(SlotId(1), voice, stream, Playback::default())
        .expect("room");
    // The streamer is gone: its side of the ring goes with it, and the voice
    // (whose handle keeps only the stream's control side) holds the ring last.
    drop(streamer);
    (pool, handle)
}

/// **The remove path: a voice's last holder of a ring is never dropped on the
/// audio thread.** The voice is removed through the handle; the drain frees
/// nothing (the slot goes over the retirement channel) and the control thread's
/// `collect_retired` frees the ring. A disk voice's drop is on the
/// control thread.
#[test]
fn a_removed_disk_voice_frees_its_ring_on_the_control_thread() {
    let (_dir, streamer, stream, reader) = stream();
    let (pool, handle) = pool_holding_the_last_holder(streamer, stream, reader);
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut rig = Direct::new(pool, SampleRate::SR_48K, 64);
    let env = t.env(64, SampleRate::SR_48K);
    rig.block_in(&env);
    assert_eq!(rig.node.voice_count(), 1, "setup: the voice is in");

    handle.remove(SlotId(1)).expect("room");
    let (frees, _) = watch(|| {
        rig.block_in(&env);
    });
    assert_eq!(rig.node.voice_count(), 0);
    assert_eq!(
        frees, 0,
        "the drain freed {frees} time(s) on the audio thread"
    );

    let (_, biggest) = watch(|| {
        assert_eq!(handle.collect_retired(), 1);
    });
    assert!(
        biggest >= RING_BYTES,
        "the control thread's collection freed at most {biggest} bytes: no ring"
    );
}

/// **A full retirement channel refuses the command; the callback never frees.**
/// The handle reserves a retirement slot for every command that might retire a
/// value. With the channel full of uncollected voices the next `remove` of the
/// disk voice (the ring's last holder), and an add that the pool could refuse,
/// fail with `SendError::RetirementFull` and queue nothing, so the drain frees
/// nothing; after `collect_retired` the same `remove` goes through and the
/// ring is freed on the control side.
#[test]
fn a_full_retirement_channel_refuses_a_remove_and_the_callback_frees_nothing() {
    let (_dir, streamer, stream, reader) = stream();
    let (pool, handle) = pool_holding_the_last_holder(streamer, stream, reader);
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut rig = Direct::new(pool, SampleRate::SR_48K, 64);
    let env = t.env(64, SampleRate::SR_48K);
    rig.block_in(&env);

    let wave = Arc::new(Wave::from_mono(
        SampleRate::SR_48K,
        Mono::new(&vec![0.5f32; 480]),
    ));
    let memory = || {
        MemorySource::placed(Arc::clone(&wave), VoiceWindow::from(Beat::new(0.0)))
            .expect("a placeable wave")
    };
    // The pool holds 64 voices: the disk voice and 63 memory ones. Removing
    // the 63 (and one more added and removed after) puts 64 values in the
    // retirement channel, none collected.
    for id in 2..=64 {
        handle
            .add(SlotId(id), memory(), Playback::default())
            .expect("room");
    }
    rig.block_in(&env);
    for id in 2..=64 {
        handle.remove(SlotId(id)).expect("room");
    }
    rig.block_in(&env);
    handle
        .add(SlotId(100), memory(), Playback::default())
        .expect("room");
    rig.block_in(&env);
    handle.remove(SlotId(100)).expect("room");
    rig.block_in(&env);

    // Full: the remove of the last holder, and an add that could be refused
    // at the voice cap and retired, are both refused at the call.
    assert_eq!(
        handle.remove(SlotId(1)),
        Err(SendError::RetirementFull),
        "the remove is refused"
    );
    assert_eq!(
        handle.add(SlotId(101), memory(), Playback::default()),
        Err(SendError::RetirementFull),
        "an add that might be refused is too"
    );
    let (frees, _) = watch(|| {
        rig.block_in(&env);
    });
    assert_eq!(rig.node.voice_count(), 1, "nothing was queued");
    assert_eq!(frees, 0, "the callback freed {frees} time(s)");

    assert_eq!(handle.collect_retired(), 64);
    handle.remove(SlotId(1)).expect("room after collecting");
    let (frees, _) = watch(|| {
        rig.block_in(&env);
    });
    assert_eq!(rig.node.voice_count(), 0);
    assert_eq!(frees, 0, "the callback freed {frees} time(s)");
    let (_, biggest) = watch(|| {
        assert_eq!(handle.collect_retired(), 1);
    });
    assert!(
        biggest >= RING_BYTES,
        "the control thread's collection freed at most {biggest} bytes: no ring"
    );
}

/// **A retirement after every handle is gone is freed with the pool.** The
/// handle is dropped with a remove of the ring's last holder still queued;
/// the block applies it, the callback frees nothing (the voice is parked in
/// the pool), and dropping the pool, on the control side, frees the ring.
#[test]
fn a_retirement_after_the_handle_is_gone_is_freed_when_the_pool_drops() {
    let (_dir, streamer, stream, reader) = stream();
    let (pool, handle) = pool_holding_the_last_holder(streamer, stream, reader);
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut rig = Direct::new(pool, SampleRate::SR_48K, 64);
    let env = t.env(64, SampleRate::SR_48K);
    rig.block_in(&env);

    handle.remove(SlotId(1)).expect("room");
    drop(handle);
    let (frees, _) = watch(|| {
        rig.block_in(&env);
    });
    assert_eq!(rig.node.voice_count(), 0, "the remove was applied");
    assert_eq!(frees, 0, "the callback freed {frees} time(s)");

    let (_, biggest) = watch(|| drop(rig));
    assert!(
        biggest >= RING_BYTES,
        "dropping the pool freed at most {biggest} bytes: the ring was leaked"
    );
}

/// **A second handle starts with every retirement slot.** Reservations belong
/// to one handle's ring: a first handle with 64 reservations does not use up
/// the second's room, and the first's `collect_retired` (on its own, dead
/// ring) does not give the second's slots back.
#[test]
fn a_second_handle_has_its_own_retirement_room() {
    let wave = Arc::new(Wave::from_mono(
        SampleRate::SR_48K,
        Mono::new(&vec![0.5f32; 480]),
    ));
    let memory = || {
        MemorySource::placed(Arc::clone(&wave), VoiceWindow::from(Beat::new(0.0)))
            .expect("a placeable wave")
    };
    let (pool, first) = VoicePool::<f32>::new().with_handle();
    first
        .add(SlotId(1), memory(), Playback::default())
        .expect("room");
    for _ in 0..63 {
        first
            .replace_wave(SlotId(1), Arc::clone(&wave))
            .expect("room");
    }
    let (pool, second) = pool.with_handle();
    assert_eq!(
        first.replace_wave(SlotId(1), Arc::clone(&wave)),
        Err(SendError::RetirementFull),
        "setup: the first handle still holds its own 64"
    );
    let t = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
    let mut rig = Direct::new(pool, SampleRate::SR_48K, 64);
    let env = t.env(64, SampleRate::SR_48K);
    second
        .add(SlotId(1), memory(), Playback::default())
        .expect("room");
    rig.block_in(&env);
    // Every replaced wave is retired: 64 fill the second handle's channel.
    for _ in 0..64 {
        second
            .replace_wave(SlotId(1), Arc::clone(&wave))
            .expect("the second handle has all 64 slots");
    }
    rig.block_in(&env);
    assert_eq!(first.collect_retired(), 0);
    assert_eq!(
        second.replace_wave(SlotId(1), Arc::clone(&wave)),
        Err(SendError::RetirementFull),
        "the first handle's collect gave nothing back to the second"
    );
    assert_eq!(second.collect_retired(), 64);
    second
        .replace_wave(SlotId(1), wave)
        .expect("room after collecting");
}
