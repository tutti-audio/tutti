//! A `loom` model of `PosRing`'s no-tear protocol — the shipped code, not a
//! replica.
//!
//! ```text
//! LOOM_MAX_PREEMPTIONS=4 RUSTFLAGS="--cfg loom" \
//!     cargo test -p tutti-rt --release --test pos_ring_loom
//! ```
//!
//! Under `--cfg loom`, `src/pos_ring.rs` builds its atomics from `loom` (its
//! `sync` module), so every interleaving and reordering loom explores is one
//! the real `claim`/`idle`/`push`/`retract_to`/`raise_from`/`reset` can take.
//! (tutti-sampler, which wraps the ring, cannot build under the flag:
//! `event-listener`, under its async channels, reacts to `cfg(loom)` without
//! depending on loom. That is why the protocol lives here, beside
//! `Publisher`.)
//!
//! # What each model asserts
//!
//! Every model runs at `f32`, the default format (`models!` at the foot). The
//! protocol does not depend on the slot's width: loom explores the same
//! interleavings for a 16-, 32- or 64-bit word, and the per-format conversions
//! are pinned bit-exact by `PosRing`'s readback tests. So one model, a write
//! round the ring under a claim, also runs at `i16`, `I24` and `f64`
//! (`widths!`), which puts each slot mapping's store and load under loom.
//!
//! A ring of 4 slots, 2 channels. (The slot map, `s mod N` by a
//! reciprocal multiply, is a pure function the protocol only needs both
//! sides to share; `PosRing`'s unit test
//! `the_slot_map_is_the_position_mod_the_ring` checks it against `%` for
//! every ring length class, which a 4-slot model cannot.) Every sample the writer stores is a tag of
//! its position, the generation it was written under and its channel. A
//! reader thread makes two claims (one, in the models whose writer does
//! most) of two ranges each; for every position of a claim's ranges its
//! window holds, it reads channel 0, channel 1, then
//! channel 0 again, and asserts they are the tags of *that* position (not
//! another sharing its slot), of one frame (not torn across channels),
//! unchanged across the claim (not rewritten under it). The writer, on the
//! model's main thread:
//!
//! - `a_write_round_the_ring_never_lands_under_a_claim` — appends round the
//!   ring while the reader's second claim reads the slots the first did not;
//! - `a_second_range_is_kept` — appends round the ring onto the slot of a
//!   position only the reader's *second* range holds;
//! - `retract_then_push` — retracts under a claim, then rewrites past the
//!   retraction;
//! - `reset_then_push` — resets below the old end, then rewrites there;
//! - `retract_twice_then_push` — two shrinks before the rewrite (the stale
//!   span kept, or cleared, across them);
//! - `raise_then_reset_then_push` — a raise, then a reset under it;
//! - `reset_then_wrap` — a reset, then appends that go round the ring from
//!   the new place;
//! - `a_capped_push_then_reset_keeps_the_claim` — a push cut to nothing by
//!   the second range's alias, then a reset and a rewrite there;
//! - `a_partly_capped_push_then_reset_keeps_the_claim` — the same with a push
//!   that lands one frame of three.
#![cfg(loom)]

use loom::thread;
use tutti_buffers::ChannelLayout;
use tutti_buffers::Interleaved;
use tutti_rt::{PosReader, PosRing, PosWriter, RingSample};
use tutti_units::Samples;

const STRIDE: usize = 2;
const LAYOUT: ChannelLayout = ChannelLayout::STEREO;

/// The tag's number, in units of `1 / SCALE` of full scale so that every
/// format, an integer one included, holds it exactly.
const SCALE: f64 = 1024.0;

fn tag<S: RingSample>(pos: u64, generation: u64, c: usize) -> S {
    S::from_f64((pos * 100 + generation * 10 + c as u64) as f64 / SCALE)
}

/// `n` frames from position `from`, written under `generation`.
fn frames<S: RingSample>(from: u64, n: u64, generation: u64) -> Vec<S> {
    (from..from + n)
        .flat_map(|p| (0..STRIDE).map(move |c| tag(p, generation, c)))
        .collect()
}

/// Pushes `data` and returns how many frames landed.
fn push<S: RingSample>(writer: &mut PosWriter<S>, data: &[S]) -> Samples {
    let view = Interleaved::new(data, LAYOUT).expect("whole frames");
    writer.push(view).expect("the ring's width")
}

/// A ring holding positions `0..4` written under generation 0, keeping
/// `keep` behind the reader, which the writer says plays at `play`.
fn full_ring<S: RingSample>(keep: u64, play: u64) -> (PosWriter<S>, PosReader<S>) {
    let (mut writer, reader) = PosRing::new(4, LAYOUT, keep).expect("stereo");
    writer.set_play(4);
    assert_eq!(push(&mut writer, &frames(0, 4, 0)), Samples(4));
    writer.set_play(play);
    (writer, reader)
}

type Ranges = [(u64, u64); 2];

/// Claims `(play, ranges)` on a reader thread, checking every held position
/// (module docs).
fn read<S: RingSample, const N: usize>(
    mut reader: PosReader<S>,
    claims: [(u64, Ranges); N],
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for (play, ranges) in claims {
            let claim = reader.claim(play, ranges);
            for s in ranges.iter().flat_map(|&(a, e)| a..e) {
                if !claim.holds(s) {
                    continue;
                }
                let (x0, x1, y0) = (claim.sample(s, 0), claim.sample(s, 1), claim.sample(s, 0));
                let [x0, x1, y0] = [x0, x1, y0].map(|x| x.to_f64() * SCALE);
                assert_eq!((x0 as u64) / 100, s, "position {s} held another's frame");
                assert_eq!(x1, x0 + 1.0, "position {s} torn across channels");
                assert_eq!(y0, x0, "position {s} rewritten under the claim");
            }
        }
    })
}

const NONE: (u64, u64) = (0, 0);

fn a_write_round_the_ring_never_lands_under_a_claim<S: RingSample>() {
    loom::model(|| {
        let (mut writer, reader) = full_ring::<S>(1, 1);
        let reading = read(reader, [(4, [(2, 4), NONE]), (4, [(0, 2), NONE])]);
        writer.set_play(4);
        let _ = push(&mut writer, &frames(4, 3, 0));
        reading.join().expect("the reader");
    });
}

fn a_second_range_is_kept<S: RingSample>() {
    loom::model(|| {
        let (mut writer, reader) = full_ring::<S>(0, 3);
        let reading = read(reader, [(3, [(3, 4), (0, 1)]), (3, [(3, 4), (0, 1)])]);
        let _ = push(&mut writer, &frames(4, 2, 0));
        reading.join().expect("the reader");
    });
}

fn retract_then_push<S: RingSample>() {
    loom::model(|| {
        let (mut writer, reader) = full_ring::<S>(1, 1);
        let reading = read(reader, [(1, [(0, 4), NONE]), (1, [(0, 4), NONE])]);
        writer.retract_to(2);
        let _ = push(&mut writer, &frames(2, 2, 1));
        reading.join().expect("the reader");
    });
}

fn reset_then_push<S: RingSample>() {
    loom::model(|| {
        let (mut writer, reader) = full_ring::<S>(1, 1);
        let reading = read(reader, [(1, [(0, 4), NONE]), (1, [(0, 4), NONE])]);
        writer.reset(1);
        let _ = push(&mut writer, &frames(1, 3, 1));
        reading.join().expect("the reader");
    });
}

fn retract_twice_then_push<S: RingSample>() {
    loom::model(|| {
        let (mut writer, reader) = full_ring::<S>(1, 1);
        let reading = read(reader, [(1, [(1, 3), NONE])]);
        writer.retract_to(3);
        writer.retract_to(1);
        let _ = push(&mut writer, &frames(1, 2, 1));
        reading.join().expect("the reader");
    });
}

fn raise_then_reset_then_push<S: RingSample>() {
    loom::model(|| {
        let (mut writer, reader) = full_ring::<S>(1, 1);
        let reading = read(reader, [(1, [(0, 2), NONE])]);
        writer.raise_from(2);
        writer.reset(0);
        let _ = push(&mut writer, &frames(0, 1, 1));
        reading.join().expect("the reader");
    });
}

fn reset_then_wrap<S: RingSample>() {
    loom::model(|| {
        let (mut writer, reader) = full_ring::<S>(0, 2);
        let reading = read(reader, [(2, [(1, 3), NONE]), (2, [(1, 3), NONE])]);
        writer.reset(2);
        // 2..6 over four slots: 4 and 5 go round onto 0 and 1.
        let _ = push(&mut writer, &frames(2, 4, 1));
        reading.join().expect("the reader");
    });
}

fn a_capped_push_then_reset_keeps_the_claim<S: RingSample>() {
    loom::model(|| {
        let (mut writer, reader) = full_ring::<S>(0, 2);
        let reading = read(reader, [(2, [(2, 3), (0, 1)])]);
        let _ = push(&mut writer, &frames(4, 2, 0));
        writer.reset(0);
        let _ = push(&mut writer, &frames(0, 1, 1));
        reading.join().expect("the reader");
    });
}

/// The write cut short but not to nothing: 4 lands (0's slot), 5 is 1's
/// alias. The window must keep 1..3 — a start left at 3 (the raise, from the
/// ranges read before the fence) would drop 1, the reset would not count it
/// stale, and the rewrite would land on it under the claim.
fn a_partly_capped_push_then_reset_keeps_the_claim<S: RingSample>() {
    loom::model(|| {
        let (mut writer, reader) = full_ring::<S>(0, 3);
        let reading = read(reader, [(3, [(3, 4), (1, 2)])]);
        let _ = push(&mut writer, &frames(4, 3, 0));
        writer.reset(0);
        let _ = push(&mut writer, &frames(0, 3, 1));
        reading.join().expect("the reader");
    });
}

/// Runs each model at `f32`.
macro_rules! models {
    ($($name:ident),* $(,)?) => {
        mod f32_slots {
            $(#[test] fn $name() { super::$name::<f32>() })*
        }
    };
}

/// Runs one model at each other slot mapping: a 16-bit word, a 32-bit word
/// holding an `I24` and a 64-bit float.
macro_rules! widths {
    ($name:ident) => {
        mod other_slots {
            #[test]
            fn i16_slots() {
                super::$name::<i16>()
            }
            #[test]
            fn i24_slots() {
                super::$name::<tutti_buffers::I24>()
            }
            #[test]
            fn f64_slots() {
                super::$name::<f64>()
            }
        }
    };
}

models!(
    a_write_round_the_ring_never_lands_under_a_claim,
    a_second_range_is_kept,
    retract_then_push,
    reset_then_push,
    retract_twice_then_push,
    raise_then_reset_then_push,
    reset_then_wrap,
    a_capped_push_then_reset_keeps_the_claim,
    a_partly_capped_push_then_reset_keeps_the_claim,
);

widths!(a_write_round_the_ring_never_lands_under_a_claim);
