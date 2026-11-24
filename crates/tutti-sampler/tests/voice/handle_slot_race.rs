//! A pool handle's commands name the voice that holds the slot when they are
//! queued, whichever clone of the handle replaces that voice meanwhile.
//!
//! Thread A removes a streaming voice and adds another at the same id, round after
//! round, on one clone of the handle; thread B steers the slot on another clone:
//! loops, directions and wave swaps. Every call is stamped from one atomic
//! counter before and after, so a call that began after a transition returned,
//! or ended before one began, is ordered against it without a clock. The pool is
//! drained between the threads' rounds, as an audio callback drains it, and
//! nothing here waits on time: the threads take turns with the drain.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
use std::time::{Duration, Instant};

use tutti_sampler::testing::{MockTransport, loop_frames, wait_for_open};
use tutti_sampler::{
    DiskStreamer, DiskStreamerConfig, DiskVoice, Loop, Manual, PlayDirection, Playback, SendError,
    SlotId, StreamHandle, StreamStart, VoicePool, VoicePoolHandle, VoiceWindow,
};
use tutti_units::{Beat, Bpm, SampleRate};

const SR: f64 = 48_000.0;
const RATE: SampleRate = SampleRate::of(SR);
const BLOCK: usize = 64;
/// Frames in the file; every loop B publishes ends inside it.
const LEN: usize = 4_096;
/// Voices A swaps in: the slot holds `ROUNDS + 1` streams over the test.
const ROUNDS: usize = 200;
/// Calls B makes between two drains: with A's two commands, far below the ring.
const PER_DRAIN: usize = 8;
/// Longest the test waits on anything before it calls the run hung.
const HANG: Duration = Duration::from_secs(120);

const SLOT: SlotId = SlotId(7);

/// What B asked for; a loop is `[0, n)` and a direction is Reverse for odd `n`,
/// so the call's `n` names the value it published.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Loop,
    Direction,
    ReplaceWave,
}

#[derive(Clone, Copy, Debug)]
struct Call {
    kind: Kind,
    n: usize,
    /// The counter before the call began and after it returned.
    began: u64,
    ended: u64,
    result: Result<(), SendError>,
}

/// A's counters around the round that removed stream `k` and added `k + 1`.
#[derive(Clone, Copy, Debug)]
struct Round {
    removing: u64,
    removed: u64,
    adding: u64,
    added: u64,
}

fn direction_of(n: usize) -> PlayDirection {
    if n % 2 == 1 {
        PlayDirection::Reverse
    } else {
        PlayDirection::Forward
    }
}

/// Spins until `blocks` passes `seen`: the pool has drained since.
fn next_drain(blocks: &AtomicU64, seen: u64, since: Instant) {
    while blocks.load(SeqCst) == seen {
        assert!(since.elapsed() < HANG, "the drain never ran");
        std::thread::yield_now();
    }
}

fn open_streams(
    streamer: &mut DiskStreamer<Manual<f32>, f32>,
    path: &std::path::Path,
    count: usize,
) -> Vec<(DiskVoice<f32>, StreamHandle<f32>)> {
    (0..count)
        .map(|_| {
            let opening = streamer.open_stream(path, StreamStart::HEAD);
            let (stream, reader) = wait_for_open(streamer, opening).expect("the file opens");
            let window = VoiceWindow::from(Beat::new(0.0));
            (DiskVoice::from_reader(reader, window), stream)
        })
        .collect()
}

/// **A command for a slot reaches the voice that holds it when the command is
/// queued, and none reaches a voice removed.** A clone removes the slot's
/// voice and adds another while another clone loops, reverses and swaps the
/// slot's wave. After the threads join and the pool drains, each stream's loop
/// and direction equal what they were when its `remove` returned (nothing
/// reached it later), name a call that began before that `remove` returned and
/// ended after its `add` began, and are no earlier than the last call that ran
/// wholly while the stream held the slot; a call wholly inside a hold never
/// answers `NoSuchSlot`, one wholly inside the gap between two voices always
/// does, and a wave swap never answers `Ok`.
#[test]
fn a_command_reaches_the_voice_holding_the_slot_when_it_is_queued() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("ramp.wav");
    let samples: Vec<f32> = (0..LEN).map(|i| i as f32 / LEN as f32).collect();
    tutti_fixtures::signal::write_wav_f32(&path, RATE, 1, &samples);
    let mut streamer =
        DiskStreamer::manual(RATE, DiskStreamerConfig::default()).expect("streamer builds");
    let mut voices = open_streams(&mut streamer, &path, ROUNDS + 1);
    // Each stream's cells, kept by the test: a cell outlives its stream's drop.
    let cells: Vec<_> = voices
        .iter()
        .map(|(_, stream)| {
            (
                stream.looping_cell().clone(),
                stream.direction_cell().clone(),
            )
        })
        .collect();

    let (pool, handle) =
        tutti_sampler::VoicePool::with_channels(tutti_buffers::ChannelLayout::MONO)
            .expect("mono is a width")
            .with_handle();
    let mut pool: VoicePool<f32> = tutti_sampler::testing::prepared(pool, RATE, BLOCK);
    let (first, first_stream) = voices.remove(0);
    handle
        .add_streaming(SLOT, first, first_stream, Playback::default())
        .expect("room");

    let clock = AtomicU64::new(1);
    let blocks = AtomicU64::new(0);
    let a_done = AtomicBool::new(false);
    let b_done = AtomicBool::new(false);
    let since = Instant::now();
    let wave = std::sync::Arc::new(tutti_buffers::Wave::new(1, RATE));

    let (rounds, calls, rests) = std::thread::scope(|scope| {
        let a = scope.spawn({
            let handle: VoicePoolHandle<f32> = handle.clone();
            let (clock, blocks, a_done, cells) = (&clock, &blocks, &a_done, &cells);
            move || {
                let mut rounds = Vec::new();
                let mut rests = Vec::new();
                for (k, (voice, stream)) in voices.into_iter().enumerate() {
                    let seen = blocks.load(SeqCst);
                    let removing = clock.fetch_add(1, SeqCst);
                    handle.remove(SLOT).expect("the slot holds A's voice");
                    let removed = clock.fetch_add(1, SeqCst);
                    // The stream's cells as `remove` left them.
                    rests.push((*cells[k].0.current(), *cells[k].1.current()));
                    let adding = clock.fetch_add(1, SeqCst);
                    handle
                        .add_streaming(SLOT, voice, stream, Playback::default())
                        .map_err(|e| e.error)
                        .expect("the slot is free");
                    let added = clock.fetch_add(1, SeqCst);
                    rounds.push(Round {
                        removing,
                        removed,
                        adding,
                        added,
                    });
                    next_drain(blocks, seen, since);
                }
                a_done.store(true, SeqCst);
                (rounds, rests)
            }
        });
        let b = scope.spawn({
            let handle: VoicePoolHandle<f32> = handle.clone();
            let (clock, blocks, a_done, b_done) = (&clock, &blocks, &a_done, &b_done);
            let wave = &wave;
            move || {
                let mut calls = Vec::new();
                let mut n = 0;
                let mut call = |kind: Kind, n: usize, send: &dyn Fn() -> Result<(), SendError>| {
                    let began = clock.fetch_add(1, SeqCst);
                    let result = send();
                    let ended = clock.fetch_add(1, SeqCst);
                    calls.push(Call {
                        kind,
                        n,
                        began,
                        ended,
                        result,
                    });
                };
                while !a_done.load(SeqCst) {
                    let seen = blocks.load(SeqCst);
                    for _ in 0..PER_DRAIN {
                        n += 1;
                        assert!(n < LEN, "B outran the file");
                        let setting = Some(loop_frames(0, n, 0));
                        call(Kind::Loop, n, &|| handle.set_loop(SLOT, setting));
                        call(Kind::Direction, n, &|| {
                            handle.set_direction(SLOT, direction_of(n))
                        });
                        call(Kind::ReplaceWave, n, &|| {
                            handle.replace_wave(SLOT, wave.clone())
                        });
                    }
                    next_drain(blocks, seen, since);
                }
                b_done.store(true, SeqCst);
                calls
            }
        });

        // The audio side: drains every block, and tends the streamer.
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0));
        while !(a_done.load(SeqCst) && b_done.load(SeqCst)) {
            assert!(since.elapsed() < HANG, "the threads never finished");
            tutti_sampler::testing::block(&mut pool, &transport, RATE, BLOCK);
            handle.collect_retired();
            let _ = streamer.step_until_idle(1_000);
            blocks.fetch_add(1, SeqCst);
        }
        let (rounds, rests) = a.join().expect("A ran");
        (rounds, b.join().expect("B ran"), rests)
    });
    tutti_sampler::testing::block(
        &mut pool,
        &MockTransport::rolling(Beat::new(0.0), Bpm::of(120.0)),
        RATE,
        BLOCK,
    );
    handle.collect_retired();
    assert_eq!(pool.voice_count(), 1, "the slot holds the last voice");
    assert_eq!(handle.drops(), tutti_sampler::PoolDrops::default());

    check(&rounds, &calls, &rests, &cells);
}

/// The settings the stream `k` holds, against what B's calls could have given it.
fn check(
    rounds: &[Round],
    calls: &[Call],
    rests: &[(Option<Loop>, PlayDirection)],
    cells: &[(
        tutti_sampler::ControlCell<Option<Loop>>,
        tutti_sampler::ControlCell<PlayDirection>,
    )],
) {
    // Stream k holds the slot from after its add returned (stream 0: from the
    // start) to the moment its remove began (the last stream: to the end).
    let held = |k: usize| {
        let from = if k == 0 { 0 } else { rounds[k - 1].added };
        (from, rounds.get(k).map_or(u64::MAX, |r| r.removing))
    };
    // It may have held it from when its add began to when its remove returned.
    let may_hold = |k: usize| {
        let from = if k == 0 { 0 } else { rounds[k - 1].adding };
        (from, rounds.get(k).map_or(u64::MAX, |r| r.removed))
    };
    // No voice holds the slot from a round's remove returning to its add starting.
    let gaps: Vec<(u64, u64)> = rounds.iter().map(|r| (r.removed, r.adding)).collect();

    for call in calls {
        let inside = |(from, to): (u64, u64)| call.began > from && call.ended < to;
        let in_hold = (0..=rounds.len()).any(|k| inside(held(k)));
        let in_gap = gaps.iter().any(|&gap| inside(gap));
        match (call.kind, call.result) {
            (Kind::ReplaceWave, Err(SendError::DiskVoice(_) | SendError::NoSuchSlot(_))) => {}
            (Kind::Loop | Kind::Direction, Ok(()) | Err(SendError::NoSuchSlot(_))) => {}
            (_, result) => panic!("{call:?} answered {result:?}"),
        }
        if in_hold {
            let want = if call.kind == Kind::ReplaceWave {
                matches!(call.result, Err(SendError::DiskVoice(_)))
            } else {
                call.result == Ok(())
            };
            assert!(want, "a call inside a hold missed the voice: {call:?}");
        }
        if in_gap {
            assert!(
                matches!(call.result, Err(SendError::NoSuchSlot(_))),
                "a call inside a gap found a voice: {call:?}"
            );
        }
    }

    for (k, (looping, direction)) in cells.iter().enumerate() {
        let (now_loop, now_direction) = (*looping.current(), *direction.current());
        if let Some((rest_loop, rest_direction)) = rests.get(k) {
            assert_eq!(
                now_loop, *rest_loop,
                "stream {k}: a loop reached it after its remove returned"
            );
            assert_eq!(
                now_direction, *rest_direction,
                "stream {k}: a direction reached it after its remove returned"
            );
        }
        let (hold_from, hold_to) = held(k);
        let (may_from, may_to) = may_hold(k);
        for kind in [Kind::Loop, Kind::Direction] {
            let ok = |call: &&Call| call.kind == kind && call.result == Ok(());
            // The calls that could have reached stream k, and those that must have.
            let could: Vec<&Call> = calls
                .iter()
                .filter(ok)
                .filter(|c| c.ended > may_from && c.began < may_to)
                .collect();
            let must_last = calls
                .iter()
                .filter(ok)
                .filter(|c| c.began > hold_from && c.ended < hold_to)
                .map(|c| c.n)
                .max();
            let candidates: Vec<usize> = could
                .iter()
                .map(|c| c.n)
                .filter(|&n| must_last.is_none_or(|last| n >= last))
                .collect();
            match kind {
                Kind::Loop => match now_loop {
                    Some(l) => assert!(
                        candidates.contains(&l.end().get()),
                        "stream {k}: its loop {l:?} is no call that could have reached it"
                    ),
                    None => assert!(
                        must_last.is_none(),
                        "stream {k}: a loop set while it held the slot is lost"
                    ),
                },
                _ => {
                    let initial = must_last.is_none() && now_direction == PlayDirection::Forward;
                    assert!(
                        initial || candidates.iter().any(|&n| direction_of(n) == now_direction),
                        "stream {k}: its direction is no call that could have reached it"
                    );
                }
            }
        }
    }
}
