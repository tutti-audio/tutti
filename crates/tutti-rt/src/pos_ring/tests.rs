use super::*;

fn indexed(frames: usize, channels: usize) -> Vec<f32> {
    (0..frames * channels).map(|i| i as f32).collect()
}

/// A ring of `frames` slots of `stride`, keeping `keep` behind.
fn ring(frames: usize, stride: usize, keep: u64) -> (PosWriter, PosReader) {
    PosRing::new(frames, ChannelLayout::clamped(stride), keep).expect("a non-empty layout")
}

/// A slot count of zero or past the maximum is refused, never clamped; the
/// maximum is accepted.
#[test]
fn a_slot_count_outside_one_to_the_maximum_is_refused() {
    let layout = ChannelLayout::clamped(1);
    for frames in [0, MAX_POS_RING_FRAMES + 1] {
        assert!(matches!(
            PosRing::<f32>::new(frames, layout, 0),
            Err(BufferError::CapacityOutOfRange { .. })
        ));
    }
    let (writer, _) = PosRing::<f32>::new(MAX_POS_RING_FRAMES, layout, 0).expect("the maximum");
    assert_eq!(writer.frames(), MAX_POS_RING_FRAMES);
}

/// Pushes whole frames of `samples` and returns how many landed.
fn push(writer: &mut PosWriter, samples: &[f32]) -> usize {
    let frames = Interleaved::new(samples, writer.layout()).expect("whole frames");
    writer.push(frames).expect("the ring's width").get()
}

/// Position `s` through a claim of exactly its frame.
fn frame_at(reader: &mut PosReader, s: u64) -> Vec<f32> {
    let stride = usize::from(reader.layout().count());
    let claim = reader.claim(s, [(s, s + 1), (0, 0)]);
    (0..stride).map(|c| claim.sample(s, c)).collect()
}

/// **The window packs into one word and back**, up to its largest length
/// and far along the line.
#[test]
fn a_window_packs_into_one_word_and_back() {
    let far = 1u64 << 39;
    for (from, to) in [(0, 0), (5, 4_100), (far, far + MAX_POS_RING_FRAMES as u64)] {
        let w = RingWindow { from, to };
        assert_eq!(RingWindow::unpack(w.pack()), w);
    }
}

/// **The slot map is `s mod frames`**, without a divide: for every ring
/// length up to [`MAX_POS_RING_FRAMES`] it agrees with `%` at the edges of
/// each period, at the last position a window can hold (`2^40 - 1`) and at
/// pseudo-random positions between.
#[test]
fn the_slot_map_is_the_position_mod_the_ring() {
    let last = (1u64 << 40) - 1;
    let mut lcg = 0x2545_f491_4f6c_dd1du64;
    let max = MAX_POS_RING_FRAMES as u64;
    for d in [
        1,
        2,
        3,
        5,
        7,
        4_095,
        4_096,
        4_097,
        48_000,
        144_000,
        1_440_000,
        1 << 23,
        max - 2,
        max - 1,
        max,
    ] {
        let c = recip(d);
        let check = |s: u64| assert_eq!(rem(s, c, d), s % d, "{s} mod {d}");
        // The top multiple of `d` a position can reach, and either side.
        let top = last / d * d;
        for s in [
            0,
            1,
            d - 1,
            d,
            d + 1,
            2 * d - 1,
            top - 1,
            top,
            top.saturating_add(1).min(last),
            last - d,
            last - 1,
            last,
        ] {
            check(s);
        }
        for _ in 0..if cfg!(miri) { 16 } else { 4_096 } {
            lcg = lcg.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            check(lcg >> 24);
        }
    }
}

/// **Frames land at the window's end and read back by position**, every
/// channel in its place at six channels.
#[test]
fn frames_land_at_the_windows_end_and_read_back_by_position() {
    let (mut writer, mut reader) = ring(64, 6, 4);
    let data = indexed(10, 6);
    assert_eq!(push(&mut writer, &data), 10);
    assert_eq!(writer.window(), RingWindow { from: 0, to: 10 });
    for s in 0..10 {
        let want: Vec<f32> = (0..6).map(|c| (s as usize * 6 + c) as f32).collect();
        assert_eq!(frame_at(&mut reader, s), want, "frame {s}");
    }
}

/// **A write never goes round the ring onto the reader**: with the reader
/// at 1 000, a ring of 4 096 that keeps 4 behind takes positions up to
/// 1 000 - 4 + 4 096 and no further.
#[test]
fn a_write_never_goes_round_the_ring_onto_the_reader() {
    let (mut writer, mut reader) = ring(4_096, 1, 4);
    let _ = push(&mut writer, &indexed(1_000, 1));
    writer.set_play(1_000);
    assert_eq!(push(&mut writer, &indexed(8_000, 1)), 4_096 - 4);
    assert_eq!(writer.window().to, 1_000 - 4 + 4_096);
    assert_eq!(frame_at(&mut reader, 996), [996.0]);
}

/// **A write never reuses a slot the block in flight reads**: a block
/// reading `[3 997, 4 069)` whose jump copy reads `[100, 200)` keeps both
/// — the writer stops at 4 196, the first position whose slot is 100's —
/// while it writes the block's own positions freely. And the window still
/// holds 100..: the write took out only what it overwrote, not the 204
/// frames a raise for the full 300 asked for would have.
#[test]
fn a_write_never_reuses_a_slot_the_block_in_flight_reads() {
    let (mut writer, mut reader) = ring(4_096, 1, 4);
    let _ = push(&mut writer, &indexed(4_000, 1));
    let claim = reader.claim(3_997, [(3_997, 4_069), (100, 200)]);
    assert_eq!(claim.window(), RingWindow { from: 0, to: 4_000 });
    assert_eq!(push(&mut writer, &[-1.0; 300]), 196, "stops at 4 196");
    assert_eq!(claim.sample(150, 0), 150.0);
    assert!(writer.window().holds(150), "the jump copy left the window");
}

/// **A write cut short takes out of the window only what it overwrote**. A
/// block claims `[10, 14)` and `[2, 5)` of a full ring of 16; a push of 8 is
/// cut to 2 by 2's alias at 18, so the window must still hold 2.. — and a
/// reset then counts 2..5 as stale, so the rewrite at 2 waits for the reader.
/// Raising the start to 8 for the 8 asked for, and keeping it, would leave
/// 2..5 uncounted and let the rewrite land under the claim.
#[test]
fn a_capped_write_keeps_what_it_did_not_overwrite() {
    for retract in [false, true] {
        let (mut writer, mut reader) = ring(16, 1, 0);
        writer.set_play(10);
        assert_eq!(push(&mut writer, &indexed(16, 1)), 16);
        let claim = reader.claim(10, [(10, 14), (2, 5)]);
        assert_eq!(push(&mut writer, &[-1.0; 8]), 2);
        assert_eq!(writer.window(), RingWindow { from: 2, to: 18 });
        if retract {
            writer.retract_to(2);
        } else {
            writer.reset(2);
        }
        let _ = push(&mut writer, &[-9.0; 3]);
        assert_eq!(claim.sample(3, 0), 3.0, "retract {retract}: rewritten");
    }
}

/// **A shrink does not free what a block in flight holds**: a block claims
/// `[600, 680)` in the window `[0, 1 000)`; the writer retracts to 620 and pushes a rewrite — it must not
/// touch 620.. until the reader claims again, after which it may. The
/// same for a reset below the old end.
///
/// A reset that moves the window away from a claim frees the new place at once: what it
/// removed is the old window, not everything below its end (otherwise a seek's refill
/// would wait for the reader's next claim, and reset the window again every cycle).
#[test]
fn a_shrink_does_not_free_what_a_block_in_flight_holds() {
    for reset in [false, true] {
        let (mut writer, mut reader) = ring(4_096, 1, 4);
        let _ = push(&mut writer, &indexed(1_000, 1));
        {
            let claim = reader.claim(600, [(600, 680), (0, 0)]);
            assert!(claim.holds(650));
            if reset {
                writer.reset(620);
            } else {
                writer.retract_to(620);
            }
            assert_eq!(
                push(&mut writer, &[-1.0; 100]),
                0,
                "reset {reset}: wrote under the claim"
            );
            assert_eq!(claim.sample(650, 0), 650.0, "reset {reset}");
        }
        // The next claim sees the shrink: the rewrite may land.
        let claim = reader.claim(600, [(600, 680), (0, 0)]);
        assert!(!claim.holds(650));
        assert_eq!(push(&mut writer, &[-1.0; 100]), 100, "reset {reset}");
    }
    // A reset away from a claim (a seek back, below the old window) frees
    // the new place at once: the old window was `[5 000, 6 000)`.
    let (mut writer, mut reader) = ring(4_096, 1, 4);
    writer.reset(5_000);
    writer.set_play(5_000);
    let _ = push(&mut writer, &indexed(1_000, 1));
    let _claim = reader.claim(1_000, [(997, 1_069), (0, 0)]);
    writer.reset(996);
    writer.set_play(1_000);
    assert_eq!(
        push(&mut writer, &[-1.0; 100]),
        100,
        "a seek's refill was held"
    );
}

/// **Raising the window's start drops the positions below it**, so a
/// reader that jumps back there finds the window without them — and once
/// the reader has claimed since, a reset there refills at once: what the
/// raise removed is no longer counted as held.
#[test]
fn raising_the_start_drops_what_lies_below() {
    let (mut writer, mut reader) = ring(4_096, 1, 4);
    let _ = push(&mut writer, &indexed(1_000, 1));
    writer.raise_from(400);
    assert_eq!(
        writer.window(),
        RingWindow {
            from: 400,
            to: 1_000
        }
    );
    assert_eq!(writer.generation(), 1);
    let _ = reader.claim(500, [(497, 569), (0, 0)]);
    // The reader jumps back below the raise; the writer moves the window.
    let _claim = reader.claim(100, [(97, 169), (0, 0)]);
    writer.reset(96);
    writer.set_play(100);
    assert_eq!(push(&mut writer, &[-1.0; 100]), 100, "the refill was held");
}

/// **A reader that stops claiming holds nothing back**: a block claims `[97, 169)`, the reader goes idle, the
/// writer moves the window to where `97`'s slot comes round again and
/// fills — at once, not after a claim that never comes.
///
/// `idle` also echoes the generation, which only lets the writer forget stale spans at
/// its next shrink rather than at the reader's next claim. With the ranges cleared, a
/// stale span holds nothing back (it counts only where it meets a range), and the next
/// claim echoes anyway, so no single-threaded order, and no loom model here, can tell
/// the echo is missing; it is kept because the reader has, in fact, seen the
/// generation.
#[test]
fn an_idle_reader_holds_nothing_back() {
    let (mut writer, mut reader) = ring(1_024, 1, 4);
    let _ = push(&mut writer, &indexed(1_000, 1));
    let _ = reader.claim(100, [(97, 169), (0, 0)]);
    reader.idle();
    writer.reset(97 + 1_024);
    writer.set_play(97 + 1_024 + 4);
    assert_eq!(
        push(&mut writer, &[-1.0; 500]),
        500,
        "held by an idle reader"
    );
    assert_eq!(writer.in_flight_end(), 0);
}

/// `first_alias` against a brute-force search, over ranges before, around
/// and after the write's start, near and across a lap of the ring — up to
/// twice the ring's length.
///
/// Under miri, every seventh `to` and `a` (still before, inside and past a
/// lap): miri checks the arithmetic for UB, and the full sweep is the native
/// run's.
#[test]
fn first_alias_is_the_first_slot_another_position_of_the_range_holds() {
    let n = 16u64;
    let step = if cfg!(miri) { 7 } else { 1 };
    for to in (0..64u64).step_by(step) {
        for a in (0..64u64).step_by(step) {
            for len in 0..=2 * n {
                let e = a + len;
                let brute = (to..to + 4 * n)
                    .find(|&s| (a..e).any(|q| q != s && q % n == s % n))
                    .unwrap_or(u64::MAX);
                let got = first_alias(to, a, e, n);
                assert!(
                    got == brute || (brute == u64::MAX && got >= to + 4 * n),
                    "to {to} range [{a}, {e}): {got}, brute {brute}"
                );
            }
        }
    }
}

/// **A writer thread and a reader thread, free-running**: the writer
/// pushes, retracts, raises and resets at random; the reader claims random
/// ranges (two, the second often far behind) or idles, and checks every
/// sample it reads in its window is the tag of that position (both
/// channels, twice), so no write ever lands under a claim. Real threads, not
/// a model: it complements `tests/pos_ring_loom.rs` at a scale loom cannot
/// explore.
///
/// The run lasts until the reader has checked `CHECKS` frames (and the
/// writer taken `STEPS` steps), about 0.8 s in a release build and 5 s in
/// a debug one here, not a fixed number of
/// writer steps: those could end before the reader thread was ever
/// scheduled (with six CPU hogs beside it, 1 run in 60 checked 0 frames).
///
/// It is a stress test, so it is a rate, not a certainty; the loom model's
/// `retract_then_push` and `reset_then_push` cover the same property every time.
#[test]
#[cfg_attr(
    miri,
    ignore = "4 000 000 writer steps against a spinning reader never finish under miri's \
              interpreter and cooperative scheduler; the protocol's interleavings are \
              the loom model's (tests/pos_ring_loom.rs), and this native run is its scale"
)]
fn a_writer_and_a_reader_thread_never_tear() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as O};
    use std::time::Duration;
    use tutti_platform::Instant;
    const N: usize = 64;
    /// Frames the reader checks before the writer stops, and steps the
    /// writer takes at least.
    const CHECKS: u64 = 1_000_000;
    const STEPS: u64 = 4_000_000;
    let (mut writer, mut reader) = ring(N, 2, 2);
    let watch = writer.watch();
    let done = std::sync::Arc::new(AtomicBool::new(false));
    // The writer runs until the reader has checked `CHECKS` frames, not for
    // a fixed count alone: under load the reader thread may not be scheduled
    // before a fixed count of steps is over (seen: 0 frames checked).
    let checked = std::sync::Arc::new(AtomicU64::new(0));
    // A tag: the position, the rewrite generation, the channel — kept
    // under 2^24, where an `f32` holds every integer exactly.
    let tag = |pos: u64, generation: u64, c: usize| {
        ((pos % 8_192) * 128 + (generation % 32) * 2 + c as u64) as f32
    };
    let reading = {
        let (done, checked) = (
            std::sync::Arc::clone(&done),
            std::sync::Arc::clone(&checked),
        );
        std::thread::spawn(move || {
            let mut seed = 0x9e37_79b9_u64;
            // Counted here and published once per claim (its one writer, a
            // plain store), so the count adds no shared-line RMW per frame
            // to the interleaving under test.
            let mut total = 0u64;
            while !done.load(O::Relaxed) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                if seed.is_multiple_of(64) {
                    reader.idle();
                    continue;
                }
                let w = watch.window();
                let a = w.from + seed % (w.to - w.from + 1).max(1);
                let e = a + 1 + (seed >> 20) % 8;
                let b = a.saturating_sub((seed >> 32) % (N as u64));
                let f = b + 1 + (seed >> 40) % 4;
                let claim = reader.claim(a, [(a, e), (b, f)]);
                for s in (a..e).chain(b..f) {
                    if !claim.holds(s) {
                        continue;
                    }
                    let (x0, x1, y0) = (claim.sample(s, 0), claim.sample(s, 1), claim.sample(s, 0));
                    let pos = (x0 as u64) / 128;
                    assert_eq!(pos, s % 8_192, "position {s} held another's frame");
                    assert_eq!(x1, x0 + 1.0, "position {s} torn across channels");
                    assert_eq!(y0, x0, "position {s} rewritten under the claim");
                    total += 1;
                }
                checked.store(total, O::Relaxed);
            }
        })
    };
    let mut seed = 0x2545_f491_u64;
    let mut generation = 0u64;
    let mut buf = Vec::new();
    // A bound only for a reader that never runs at all, so that fails
    // rather than hangs.
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut step = 0u64;
    // A reader that stopped (it panicked on a torn frame) ends the run too.
    // The count is read once per 1 024 steps, past the first `STEPS`.
    while step < STEPS
        || !step.is_multiple_of(1_024)
        || (checked.load(O::Relaxed) < CHECKS
            && !reading.is_finished()
            && Instant::now() < deadline)
    {
        step += 1;
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let w = writer.window();
        match seed % 16 {
            0 => {
                generation += 1;
                writer.retract_to(w.from + (seed >> 8) % (w.to - w.from + 1));
            }
            1 => {
                generation += 1;
                let play = writer.play();
                writer.reset(play.saturating_sub(2) + (seed >> 8) % 8);
            }
            2 => {
                generation += 1;
                writer.raise_from(w.from + (seed >> 8) % (w.to - w.from + 1));
            }
            _ => {
                let to = writer.window().to;
                let n = 1 + (seed >> 8) as usize % 16;
                buf.clear();
                for i in 0..n as u64 {
                    buf.push(tag(to + i, generation, 0));
                    buf.push(tag(to + i, generation, 1));
                }
                let _ = push(&mut writer, &buf);
            }
        }
    }
    done.store(true, O::Relaxed);
    reading.join().expect("the reader never saw a torn frame");
    let checked = checked.load(O::Relaxed);
    assert!(
        checked >= CHECKS,
        "the reader checked only {checked} frames in 120 s"
    );
}

/// **A window never passes 2^40**: `push` stops at the limit and lands the
/// short count, and `reset` saturates there, whatever the start or the play
/// position (an unbounded one included).
#[test]
fn a_window_never_passes_the_position_limit() {
    let near = MAX_POS_RING_POSITION - 10;
    let (mut w, mut r) = ring(256, 1, 0);
    w.reset(near);
    drop(r.claim(near, [(0, 0), (0, 0)]));
    assert_eq!(push(&mut w, &[1.0; 100]), 10, "stops at the limit");
    assert_eq!(w.window().to, MAX_POS_RING_POSITION);
    assert_eq!(push(&mut w, &[1.0; 100]), 0);
    assert_eq!(w.window().to, MAX_POS_RING_POSITION);

    w.reset(u64::MAX);
    assert_eq!(
        w.window(),
        RingWindow {
            from: MAX_POS_RING_POSITION,
            to: MAX_POS_RING_POSITION
        }
    );
    drop(r.claim(u64::MAX, [(u64::MAX - 5, u64::MAX), (0, 0)]));
    assert_eq!(
        push(&mut w, &[1.0; 8]),
        0,
        "an unbounded play position lands nothing"
    );
}

/// Pushes `frames` of `stride` channels of `S` valued `value(i)` and reads
/// every one back by position.
fn round_trips<S: RingSample>(stride: usize, value: impl Fn(usize) -> S, bits: impl Fn(S) -> u64) {
    let layout = ChannelLayout::clamped(stride);
    let (mut writer, mut reader) = PosRing::<S>::new(16, layout, 0).expect("a non-empty layout");
    let data: Vec<S> = (0..9 * stride).map(&value).collect();
    let view = Interleaved::new(&data, layout).expect("whole frames");
    assert_eq!(writer.push(view), Ok(Samples::new(9)));
    let claim = reader.claim(0, [(0, 9), (0, 0)]);
    for s in 0..9u64 {
        for c in 0..stride {
            assert_eq!(
                bits(claim.sample(s, c)),
                bits(data[s as usize * stride + c]),
                "position {s} channel {c}"
            );
        }
    }
}

/// **A pushed frame reads back bit-exactly in every format**: an `f32` and an
/// `f64` that no narrower word holds (a payload in the low and the high bits
/// of the mantissa, a negative zero, a signalling-range NaN pattern), and
/// each integer format at its extremes.
#[test]
fn a_pushed_frame_reads_back_bit_exactly_in_every_format() {
    let f32s = [
        0.0,
        -0.0,
        f32::MIN_POSITIVE / 3.0,
        f32::from_bits(0x7fa0_0001),
        1.000_000_1,
        -3.5e30,
    ];
    let f64s = [
        0.0,
        -0.0,
        f64::from_bits(0x0000_0000_0000_0001),
        f64::from_bits(0x7ff4_0000_dead_beef),
        1.0 + f64::EPSILON,
        -3.5e300,
    ];
    let i8s = [i8::MIN, -1, 0, 1, 77, i8::MAX];
    let i16s = [i16::MIN, -1, 0, 1, 12_345, i16::MAX];
    let i24s = [I24::MIN, I24::of(-1), I24::ZERO, I24::of(1), I24::MAX];
    let i32s = [i32::MIN, -1, 0, 1, 0x1234_5678, i32::MAX];
    for stride in [1, 2, 3] {
        round_trips::<f32>(
            stride,
            |i| f32s[i % f32s.len()],
            |x: f32| u64::from(x.to_bits()),
        );
        round_trips::<f64>(stride, |i| f64s[i % f64s.len()], f64::to_bits);
        round_trips::<i8>(stride, |i| i8s[i % i8s.len()], |x: i8| x as u64);
        round_trips::<i16>(stride, |i| i16s[i % i16s.len()], |x: i16| x as u64);
        round_trips::<I24>(stride, |i| i24s[i % i24s.len()], |x: I24| x.get() as u64);
        round_trips::<i32>(stride, |i| i32s[i % i32s.len()], |x: i32| x as u64);
    }
}

/// **A slot no write reached reads silence** in every format.
#[test]
fn an_unwritten_slot_reads_silence() {
    fn silent<S: RingSample>() {
        let (_writer, mut reader) = PosRing::<S>::new(4, ChannelLayout::STEREO, 0).expect("stereo");
        let claim = reader.claim(0, [(0, 0), (0, 0)]);
        let slots = &claim.ring.slots;
        assert!(slots.iter().all(|a| S::load(a) == S::SILENCE));
    }
    silent::<f32>();
    silent::<f64>();
    silent::<i8>();
    silent::<i16>();
    silent::<I24>();
    silent::<i32>();
}

/// **A view of another width is refused and writes nothing**: a stereo view
/// into a mono ring and a mono view into a stereo one both name the two
/// widths and leave the window empty.
#[test]
fn a_view_of_the_wrong_width_is_refused() {
    let data = [1.0f32; 8];
    for (ring_width, view_width) in [
        (ChannelLayout::MONO, ChannelLayout::STEREO),
        (ChannelLayout::STEREO, ChannelLayout::MONO),
        (ChannelLayout::STEREO, ChannelLayout::QUAD),
    ] {
        let (mut writer, _reader) =
            PosRing::<f32>::new(16, ring_width, 0).expect("a non-empty layout");
        let view = Interleaved::new(&data, view_width).expect("whole frames");
        assert_eq!(
            writer.push(view),
            Err(BufferError::WidthMismatch {
                src: view_width,
                dst: ring_width
            })
        );
        assert_eq!(writer.window(), RingWindow { from: 0, to: 0 });
    }
}

/// **The empty layout is refused**: a ring of no channels has no frame to
/// hold, so building one names the zero width rather than making a mono ring.
#[test]
fn an_empty_layout_is_refused() {
    assert_eq!(
        PosRing::<f32>::new(16, ChannelLayout::EMPTY, 0).map(|_| ()),
        Err(BufferError::ZeroWidth)
    );
}
