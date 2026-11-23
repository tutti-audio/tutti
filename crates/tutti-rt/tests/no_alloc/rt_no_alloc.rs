//! Regression gates: what the audio thread runs of each primitive neither
//! allocates nor frees. Everything is built outside the gate, then driven
//! inside `assert_no_alloc`; a primitive that runs on several threads runs
//! every participant inside it, helpers included.

use tutti_rt::{AudioThreadCell, CappedVec, Claim, LastOut, Party, RenderGate, RtVec};

/// `RtVec` reached through `&self`, as `tutti-midi-hardware`'s drain holds it:
/// behind an `AudioThreadCell`.
#[test]
fn rt_vec_steady_state_is_allocation_free() {
    let buf: AudioThreadCell<RtVec<(u32, u32), 64>> = AudioThreadCell::new(RtVec::new());
    // Warm up well within the inline capacity.
    buf.borrow_mut().extend((0..32).map(|i| (32 - i, i)));

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            let mut v = buf.borrow_mut();
            v.clear();
            for i in 0..96u32 {
                let _ = v.push((96 - i, i));
            }
            v.as_mut_slice().sort_by_key(|&(k, _)| k);
            let sum: u32 = v.iter().map(|&(_, x)| x).sum();
            core::hint::black_box(sum);
        }
    });
    assert_eq!(buf.borrow().len(), 64);
    assert!(buf.borrow().is_overflowed());
}

/// `CappedVec` filled past its cap, sorted, read and cleared, block after
/// block, neither allocates nor frees once built: its storage is the one
/// allocation `with_capacity` made.
#[test]
fn capped_vec_steady_state_is_allocation_free() {
    let mut v: CappedVec<(u32, u32)> = CappedVec::with_capacity(64);
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            v.clear();
            for i in 0..96u32 {
                let _ = v.push((96 - i, i));
            }
            v.as_mut_slice().sort_by_key(|&(k, _)| k);
            let sum: u32 = v.iter().map(|&(_, x)| x).sum();
            core::hint::black_box(sum);
        }
    });
    assert_eq!(v.len(), 64);
    assert!(v.is_overflowed());
}

/// Every `AudioThreadCell` borrow path — `borrow_mut`, `borrow`, granted
/// and refused `try_borrow_mut` and `try_borrow`, two shared borrows at once,
/// and each guard's drop — neither allocates nor frees: the always-on check
/// is a compare-exchange and a store or decrement.
///
/// The panic path is left out: it formats a message, and it is a contract
/// violation, not a path the audio thread runs.
#[test]
fn audio_thread_cell_borrows_neither_allocate_nor_free() {
    let cell = AudioThreadCell::new(0u64);
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..10_000u64 {
            *cell.borrow_mut() += i;
            let held = cell.try_borrow_mut().expect("nothing else borrows");
            assert!(cell.try_borrow_mut().is_none());
            assert!(cell.try_borrow().is_none());
            drop(held);
            let a = cell.try_borrow().expect("no writer");
            let b = cell.borrow();
            assert!(cell.try_borrow_mut().is_none());
            core::hint::black_box(*a + *b);
        }
    });
    assert_eq!(*cell.borrow(), (0..10_000u64).sum::<u64>());
}

/// `AtomicRequestFlags::raise` and `take`, and a `ring` push and pop (full and
/// empty answers included), neither allocate nor free: the whole of what a
/// host callback and the block-head drain do.
#[test]
fn request_flags_and_ring_are_allocation_free() {
    use tutti_rt::{AtomicRequestFlags, Overflow, RequestFlag, ring};

    let flags = AtomicRequestFlags::new();
    let (mut writer, mut reader) = ring::<[u32; 3]>(4, Overflow::DropNewest);
    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..1_000u32 {
            flags.raise(RequestFlag::of((round % 32) as u8));
            let taken = flags.take();
            assert_eq!(taken.iter().count(), 1);
            for n in 0..6 {
                let _ = writer.push([round, n, 0]);
            }
            while let Some(edit) = reader.pop() {
                core::hint::black_box(edit);
            }
            assert!(reader.pop().is_none());
        }
    });
    assert!(writer.lost() > 0);
}

/// A reader's whole block on a `PosRing`: `claim`, the window, `holds`,
/// `frame`, `sample`, `get` and `idle`, neither allocating nor freeing. The
/// writer's `push` is the disk thread's and stays outside the claim.
#[test]
fn pos_ring_reader_is_allocation_free() {
    use tutti_buffers::ChannelLayout;
    use tutti_buffers::Interleaved;
    use tutti_rt::PosRing;
    use tutti_units::Samples;

    let (mut writer, mut reader) = PosRing::<f32>::new(64, ChannelLayout::STEREO, 8).unwrap();
    let data: Vec<f32> = (0..2 * 32).map(|i| i as f32).collect();
    let frames = Interleaved::new(&data, ChannelLayout::STEREO).unwrap();
    assert_eq!(writer.push(frames), Ok(Samples::new(32)));
    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..1_000u64 {
            let play = round % 8;
            let claim = reader.claim(play, [(play, play + 8), (0, 0)]);
            assert!(claim.holds(play));
            assert!(claim.window().holds(play));
            let mut sum = 0.0;
            for s in play..play + 8 {
                sum += claim.sample(s, 0) + claim.frame(s).get(1);
            }
            let expect: f32 = (play..play + 8).map(|s| (4 * s + 1) as f32).sum();
            assert_eq!(sum, expect);
            if round % 4 == 0 {
                reader.idle();
            }
        }
    });
}

/// `SeqWriter::write` and `last`, and `SeqReader::read` (a value and the
/// never-written answer), neither allocate nor free.
#[test]
fn seq_cell_write_and_read_are_allocation_free() {
    use tutti_rt::{SeqReader, SeqWriter};

    let mut writer: SeqWriter<[u32; 3]> = SeqWriter::new();
    let reader: SeqReader<[u32; 3]> = writer.reader();
    assert_no_alloc::assert_no_alloc(|| {
        assert_eq!(reader.read(4), Ok(None));
        for round in 0..1_000u32 {
            writer.write([round, round + 1, round + 2]);
            assert_eq!(writer.last(), Some([round, round + 1, round + 2]));
            assert_eq!(reader.read(4), Ok(Some([round, round + 1, round + 2])));
        }
    });
}

/// `SeqSlotsWriter::write` and `new_epoch`, and `SeqSlotsReader::drain`, neither
/// allocate nor free.
#[test]
fn seq_slots_write_and_drain_are_allocation_free() {
    let (mut writer, mut reader) = tutti_rt::seq_slots::<f64>(64);
    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..100u32 {
            for slot in 0..64 {
                writer.write(slot, f64::from(round));
            }
            if round % 10 == 0 {
                writer.new_epoch();
            }
            let mut yielded = 0;
            reader.drain(|_, _| yielded += 1);
            assert_eq!(yielded, if round % 10 == 0 { 0 } else { 64 });
        }
    });
}

/// `AtomicCell::store`, `swap` and `load` neither allocate nor free.
#[test]
fn atomic_cell_store_swap_and_load_are_allocation_free() {
    use tutti_rt::AtomicCell;

    let cell = AtomicCell::new([0u32, 1]);
    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..1_000u32 {
            let pair = [round, !round];
            cell.store(pair);
            assert_eq!(cell.load(), pair);
            assert_eq!(cell.swap([0, 1]), pair);
        }
    });
}

/// A block's claims on a `SplitMut` and a `SplitRw`: a claim, a refused
/// one, a run, a view's reads, writes and `split`, and their releases,
/// neither allocate nor free. The table and the held list are sized outside;
/// each block builds its split inside, as the executor does.
#[test]
fn split_claims_and_views_are_allocation_free() {
    use tutti_rt::{ClaimTable, Held, SplitMut, SplitRw};

    let mut items = vec![0u32; 16];
    let mut chunks = vec![0u32; 32];
    let mut table = ClaimTable::new(16);
    let held = Held::new(8);
    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..1_000u32 {
            let split = SplitMut::new(&mut items, &mut table);
            {
                let mut first = split.claim(0);
                *first += 1;
                assert!(split.try_claim(0).is_err(), "claimed twice");
                let mut run = split.claim_run(1..5);
                run[0] += 1;
            }
            assert_eq!(*split.claim(0), round + 1);

            let rw = SplitRw::new(&mut chunks, 4, &mut table);
            let mut view = rw.view(&held);
            view.get_mut(1)[0] = round;
            assert_eq!(view.get(1)[0], round);
            assert!(view.try_get(2).is_ok());
            let mut seen = 0;
            view.split(
                &[(2, false), (3, true)],
                |write| write,
                |_, read| seen += read.len(),
                |_, write| write[0] = round,
            );
            assert_eq!(seen, 4);
            assert_eq!(view.held(), 3);
        }
    });
}

/// `TaskGraph::begin` and every participant's `work`, the caller's and two
/// helpers', neither allocate nor free; each thread runs its share inside the
/// gate. The four middle tasks wait until all three participants have entered
/// the closure, so no participant can be missing from the gated path; the
/// helpers' leave budget is an hour so none gives up first, and the wait is
/// bounded so a participant that never arrives fails the final assertion
/// instead of hanging.
#[test]
fn task_graph_blocks_are_allocation_free() {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::time::Duration;
    use tutti_platform::Instant;
    use tutti_rt::{BlockWaits, Participant, TaskGraph};

    // 0 -> {1, 2, 3, 4} -> 5.
    let mut graph = TaskGraph::new(
        &[0, 1, 1, 1, 1, 4],
        &[0, 4, 5, 6, 7, 8, 8],
        &[1, 2, 3, 4, 5, 5, 5, 5],
    );
    let waits = BlockWaits {
        helper_leave_after: Duration::from_secs(3600),
        ..BlockWaits::DEFAULT
    };
    let ran: Vec<AtomicU32> = (0..6).map(|_| AtomicU32::new(0)).collect();
    let entered: Vec<AtomicBool> = (0..3).map(|_| AtomicBool::new(false)).collect();
    let timed_out = AtomicBool::new(false);
    for _ in 0..300 {
        for e in &entered {
            e.store(false, Ordering::Relaxed);
        }
        assert_no_alloc::assert_no_alloc(|| graph.begin());
        std::thread::scope(|s| {
            let graph = &graph;
            let (ran, entered, timed_out) = (&ran, &entered, &timed_out);
            let task = move |who: usize, t: u32| {
                entered[who].store(true, Ordering::Release);
                if (1..=4).contains(&t) {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !timed_out.load(Ordering::Relaxed)
                        && !entered.iter().all(|e| e.load(Ordering::Acquire))
                    {
                        if Instant::now() > deadline {
                            timed_out.store(true, Ordering::Relaxed);
                            break;
                        }
                        std::hint::spin_loop();
                    }
                }
                ran[t as usize].fetch_add(1, Ordering::Relaxed);
            };
            for helper in 1..3 {
                s.spawn(move || {
                    assert_no_alloc::assert_no_alloc(|| {
                        graph.work(Participant::Helper(helper), waits, |t| task(helper, t));
                    });
                });
            }
            assert_no_alloc::assert_no_alloc(|| {
                graph.work(Participant::Caller, waits, |t| task(0, t));
            });
        });
        assert!(graph.done());
        assert!(
            !timed_out.load(Ordering::Relaxed),
            "every participant took part"
        );
    }
    for (task, count) in ran.iter().enumerate() {
        assert_eq!(count.load(Ordering::Relaxed), 300, "task {task}");
    }
}

/// `JobGate::run` and two helpers' `help` neither allocate nor free, every
/// thread inside the gate. The job holds participant 0 until both helpers
/// have taken a share of the round, so the helper path is what runs.
#[test]
fn job_gate_run_and_help_are_allocation_free() {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use tutti_rt::JobGate;

    let gate = JobGate::new(3);
    let stop = AtomicBool::new(false);
    let joined = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
    std::thread::scope(|s| {
        for _ in 0..2 {
            s.spawn(|| {
                assert_no_alloc::assert_no_alloc(|| {
                    let mut served = 0;
                    while !stop.load(Ordering::Acquire) {
                        gate.help(&mut served);
                        std::hint::spin_loop();
                    }
                });
            });
        }
        assert_no_alloc::assert_no_alloc(|| {
            for round in 1..=200u32 {
                let joined_all = gate.run(
                    &|i| {
                        joined[i].store(round, Ordering::Release);
                        if i == 0 {
                            while joined[1].load(Ordering::Acquire) != round
                                || joined[2].load(Ordering::Acquire) != round
                            {
                                std::hint::spin_loop();
                            }
                        }
                    },
                    || {},
                );
                assert!(joined_all);
            }
        });
        stop.store(true, Ordering::Release);
    });
}

/// `RtScope::enter`, `check_outside` and the guard's drop
/// neither allocate nor free.
#[test]
fn rt_scope_is_allocation_free() {
    use tutti_rt::RtScope;

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            RtScope::check_outside("a value");
            let guard = RtScope::enter();
            drop(guard);
        }
    });
}

/// `DrainBeat::begin`, and `DrainWatch::mark` and `two_since`, neither allocate
/// nor free: the audio thread's per-block count and the control thread's reads.
#[test]
fn drain_clock_is_allocation_free() {
    let (mut beat, watch) = tutti_rt::drain_clock();
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            let mark = watch.mark();
            assert!(!watch.two_since(mark));
            beat.begin();
            assert!(!watch.two_since(mark));
            beat.begin();
            assert!(watch.two_since(mark));
        }
    });
}

/// The audio end of a `handoff` trades without allocating or freeing: taking
/// a lent value, holding it, and returning it on a recall, with the control
/// end's lend and recall inside the gate too.
#[test]
fn a_handoff_trades_without_allocating() {
    let (mut hand, mut rt) = tutti_rt::handoff::<[u64; 8]>();
    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..1_000u64 {
            assert!(hand.lend([round; 8]).is_ok());
            assert_eq!(rt.poll(|_| {}).map(|v| v[0]), Some(round));
            assert!(hand.recall().is_none());
            assert!(rt.poll(|v| v[1] = 0).is_none());
            let back = hand.take().expect("returned");
            core::hint::black_box(back);
        }
    });
}

/// A `ring` push and pop on every overflow policy (full, empty and
/// disconnected answers included), `drain`,
/// `push_slice`, `pop_slice`, `push_iter`, `pop_each` and the loss reads, neither allocate nor free:
/// the whole of what an audio-thread end does with values that need no drop.
#[test]
fn ring_is_allocation_free() {
    use tutti_rt::{Overflow, ring};

    for overflow in [Overflow::DropNewest, Overflow::Refuse] {
        let (mut writer, mut reader) = ring::<[u32; 3]>(4, overflow);
        let mut out = [[0u32; 3]; 2];
        assert_no_alloc::assert_no_alloc(|| {
            for round in 0..1_000u32 {
                for n in 0..6 {
                    let _ = writer.push([round, n, 0]);
                }
                assert!(writer.vacant() <= 4);
                core::hint::black_box(reader.pop_slice(&mut out));
                core::hint::black_box(writer.push_slice(&out));
                core::hint::black_box(writer.push_iter(out.iter().copied()));
                core::hint::black_box(reader.pop_each(1, |edit| {
                    core::hint::black_box(edit);
                }));
                for edit in reader.drain() {
                    core::hint::black_box(edit);
                }
                core::hint::black_box(reader.peek());
                assert!(reader.pop().is_none());
                core::hint::black_box((writer.take_lost(), reader.lost()));
            }
        });
        drop(reader);
        assert_no_alloc::assert_no_alloc(|| assert!(writer.push([0; 3]).is_err()));
    }
}

/// `RenderGate::enter` (admitted and refused), `Claim::try_claim` (granted and
/// refused) and `LastOut::leave` neither allocate nor free, with their guards' drops.
#[test]
fn gate_claim_and_last_out_audio_paths_neither_allocate_nor_free() {
    let gate = RenderGate::new();
    gate.begin_open().expect("closed").finish();
    let claim = Claim::new();
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            let render = gate.enter().expect("open");
            drop(render);
            let held = claim.try_claim().expect("free");
            assert!(claim.try_claim().is_none());
            drop(held);
        }
    });
    gate.close().expect("open").finish();
    assert_no_alloc::assert_no_alloc(|| assert!(gate.enter().is_none()));

    let out = LastOut::new(7u32);
    assert_no_alloc::assert_no_alloc(|| {
        assert_eq!(out.leave(Party::A), None);
        assert_eq!(out.leave(Party::B), Some(7));
    });
}
