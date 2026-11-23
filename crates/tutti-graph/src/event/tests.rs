use tutti_buffers::MaxBlock;
use tutti_ump::{Message, MidiChannel, NoteNumber, U7, Ump, Voice1};
use tutti_units::Samples;

use crate::event::merge::merge_into;
use crate::event::sorted::{EventOrderError, SortedEvents};
use crate::event::types::{Event, EventKind};
use crate::event::writer::{
    EventDensity, EventRejected, EventWriter, MAX_EVENT_PORT_CAPACITY, MIN_EVENT_PORT_CAPACITY,
    PortCount,
};
use crate::node::time::Offset;

fn ev(offset: u32, tag: u32) -> Event {
    Event::midi(Offset::raw(offset), Ump([tag, 0, 0, 0]))
}

/// Ties go to the earlier source; otherwise offset order.
#[test]
fn merge_orders_by_offset_then_source() {
    let a = [ev(0, 1), ev(3, 2), ev(9, 3)];
    let b = [ev(3, 10), ev(4, 11)];
    let mut out = Vec::with_capacity(8);
    let dropped = merge_into(&[&a, &b], &mut out, 8);
    assert_eq!(dropped, 0);
    let tags: Vec<u32> = out
        .iter()
        .map(|e| match e.kind {
            EventKind::Midi(Ump(w)) => w[0],
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(tags, vec![1, 2, 10, 11, 3]);
}

/// The merge refuses past capacity and says how many it refused.
#[test]
fn merge_counts_what_did_not_fit() {
    let a = [ev(0, 1), ev(1, 2), ev(2, 3)];
    let b = [ev(0, 4), ev(5, 5)];
    let mut out = Vec::with_capacity(8);
    assert_eq!(merge_into(&[&a, &b], &mut out, 3), 2);
    assert_eq!(out.len(), 3);
}

/// `SortedEvents::new` refuses what is unsorted or out of the block;
/// `sort` repairs order (stably) and still refuses out-of-block.
#[test]
fn sorted_events_are_sorted_and_in_block() {
    let unsorted = [ev(3, 0), ev(1, 1)];
    assert_eq!(
        SortedEvents::new(&unsorted, 8),
        Err(EventOrderError::Unsorted { at: 1 })
    );
    let late = [ev(1, 0), ev(8, 1)];
    assert_eq!(
        SortedEvents::new(&late, 8),
        Err(EventOrderError::OutOfBlock { at: 1 })
    );
    assert!(SortedEvents::new(&[ev(0, 0), ev(0, 1), ev(7, 2)], 8).is_ok());

    let mut v: Vec<Event> = (0..64).map(|i| ev(i % 3, i)).collect();
    let sorted = SortedEvents::sort(&mut v, 8).expect("in block");
    let tags: Vec<u32> = sorted
        .iter()
        .map(|e| match e.kind {
            EventKind::Midi(Ump(w)) => w[0],
            _ => unreachable!(),
        })
        .collect();
    let mut want: Vec<u32> = (0..64).collect();
    want.sort_by_key(|i| i % 3);
    assert_eq!(tags, want, "stable: equal offsets keep their order");
    let mut late = vec![ev(9, 0)];
    assert!(SortedEvents::sort(&mut late, 8).is_err());
}

/// The writer enforces the three invariants a consumer relies on.
#[test]
fn writer_rejects_out_of_order_out_of_block_and_overflow() {
    let mut buf = Vec::with_capacity(4);
    let dropped = PortCount::default();
    let mut params = Vec::new();
    let mut w = EventWriter::new(&mut buf, &mut params, 2, 16, &dropped);
    assert_eq!(w.push(ev(5, 0)), Ok(()));
    assert_eq!(w.push(ev(4, 0)), Err(EventRejected::OutOfOrder));
    assert_eq!(w.push(ev(16, 0)), Err(EventRejected::OutOfBlock));
    assert_eq!(w.push(ev(5, 0)), Ok(()));
    assert_eq!(w.push(ev(6, 0)), Err(EventRejected::Full));
    assert_eq!(w.len(), 2);
    assert_eq!(dropped.refused.get(), 3);
}

/// The splitter under test, written the obvious way: every distinct
/// offset (and 0) starts a chunk, each chunk runs to the next start, and
/// its events are those whose offset equals its start.
fn hand_split(events: &[Event], frames: usize) -> Vec<(std::ops::Range<usize>, Vec<Event>)> {
    let mut starts: Vec<usize> = std::iter::once(0)
        .chain(events.iter().map(|e| e.offset.index()))
        .collect();
    starts.sort_unstable();
    starts.dedup();
    let ends = starts
        .iter()
        .skip(1)
        .copied()
        .chain(std::iter::once(frames));
    starts
        .iter()
        .zip(ends)
        .map(|(&a, b)| {
            let here = events
                .iter()
                .filter(|e| e.offset.index() == a)
                .copied()
                .collect();
            (a..b, here)
        })
        .collect()
}

fn split(events: &[Event], frames: usize) -> Vec<(std::ops::Range<usize>, Vec<Event>)> {
    SortedEvents::new(events, frames)
        .expect("sorted and in the block")
        .sub_blocks(Samples::new(frames))
        .map(|(r, e)| (r, e.to_vec()))
        .collect()
}

/// The edges `sub_blocks` has to get right, spelled out: an event at
/// offset 0 (no empty leading chunk), several on one offset (one chunk,
/// all of them, in order), one on the last frame (a one-frame chunk), and
/// no events at all (the whole block).
#[test]
fn sub_blocks_split_at_every_offset() {
    let evs = [ev(0, 1), ev(5, 2), ev(5, 3), ev(7, 4)];
    let got = split(&evs, 8);
    assert_eq!(
        got,
        vec![
            (0..5, vec![ev(0, 1)]),
            (5..7, vec![ev(5, 2), ev(5, 3)]),
            (7..8, vec![ev(7, 4)]),
        ]
    );
    assert_eq!(split(&[], 8), vec![(0..8, vec![])]);
    assert_eq!(
        split(&[ev(3, 9)], 8),
        vec![(0..3, vec![]), (3..8, vec![ev(3, 9)])]
    );
    assert_eq!(split(&evs, 8), hand_split(&evs, 8));
}

proptest::proptest! {
    /// Against the hand-rolled splitter, on random sorted offsets — with
    /// offset 0, the last frame and repeats all drawn often — and random
    /// block lengths including 1.
    #[test]
    fn sub_blocks_match_a_hand_rolled_splitter(
        frames in 1usize..40,
        raw in proptest::collection::vec(0u32..6, 0..24),
    ) {
        // Bias toward the edges: 0 → offset 0, 5 → the last frame,
        // anything else → a frame in between (repeats are frequent).
        let last = frames as u32 - 1;
        let mut offsets: Vec<u32> = raw
            .iter()
            .enumerate()
            .map(|(i, &r)| match r {
                0 => 0,
                5 => last,
                _ => (r * 7 + i as u32) % frames as u32,
            })
            .collect();
        offsets.sort_unstable();
        let evs: Vec<Event> = offsets
            .iter()
            .enumerate()
            .map(|(i, &o)| ev(o, i as u32))
            .collect();
        let got = split(&evs, frames);
        proptest::prop_assert_eq!(&got, &hand_split(&evs, frames));
        // The chunks tile the block and every event comes out once.
        let covered: usize = got.iter().map(|(r, _)| r.len()).sum();
        proptest::prop_assert_eq!(covered, frames);
        let n: usize = got.iter().map(|(_, e)| e.len()).sum();
        proptest::prop_assert_eq!(n, evs.len());
    }
}

/// An event is a note-off when its packet is (`Ump::is_note_off` pins the
/// spellings); a note-on and a resync are not.
#[test]
fn only_a_midi_note_off_event_is_a_note_off() {
    let (c, n, v) = (MidiChannel::FIRST, NoteNumber::of(60), U7::of(0x40));
    assert!(Event::midi(Offset::ZERO, Message::midi1(c, Voice1::note_off(n, v))).is_note_off());
    assert!(!Event::midi(Offset::ZERO, Message::midi1(c, Voice1::note_on(n, v))).is_note_off());
    assert!(!Event::resync(Offset::ZERO).is_note_off());
}

fn off(offset: u32) -> Event {
    Event::midi(
        Offset::raw(offset),
        Message::midi1(
            MidiChannel::FIRST,
            Voice1::note_off(NoteNumber::of(60), U7::of(0)),
        ),
    )
}

/// A merge that must cut keeps the note-offs and cuts the others, later
/// ones first, and keeps the merge order. Sources `a = [on@0, on@1,
/// off@2]` and `b = [on@0, off@3]` into room for 3: both note-offs land
/// (2), leaving one other, which is the first in merge order (`a`'s
/// note-on at 0). The three others cut are counted: 2.
#[test]
fn a_merge_that_must_cut_keeps_its_note_offs() {
    let a = [ev(0, 1), ev(1, 2), off(2)];
    let b = [ev(0, 3), off(3)];
    let mut out = Vec::with_capacity(3);
    let dropped = merge_into(&[&a, &b], &mut out, 3);
    assert_eq!(dropped, 2);
    assert_eq!(out, vec![ev(0, 1), off(2), off(3)]);
}

/// A merge that fits is the plain merge, note-offs or not.
#[test]
fn a_merge_that_fits_cuts_nothing() {
    let a = [ev(0, 1), off(2)];
    let b = [off(1), ev(3, 3)];
    let mut out = Vec::with_capacity(4);
    assert_eq!(merge_into(&[&a, &b], &mut out, 4), 0);
    assert_eq!(out, vec![ev(0, 1), off(1), off(2), ev(3, 3)]);
}

/// A writer at its capacity refuses a note-on and takes a note-off, up
/// to `note_off_room`, and every refusal and every skipped event is
/// counted once.
#[test]
fn a_writer_at_capacity_takes_note_offs_to_its_room() {
    let mut buf = Vec::with_capacity(16);
    let dropped = PortCount::default();
    let mut params = Vec::new();
    let mut w = EventWriter::new(&mut buf, &mut params, 2, 16, &dropped);
    w.push(ev(0, 1)).unwrap();
    w.push(ev(1, 2)).unwrap();
    assert_eq!(w.push(ev(2, 3)), Err(EventRejected::Full));
    // Room for 2 + 0 + 8: eight note-offs.
    for k in 0..8 {
        assert_eq!(w.push(off(3 + k)), Ok(()), "note-off {k}");
    }
    assert_eq!(w.push(off(12)), Err(EventRejected::Full));
    w.count_skipped();
    w.recovered();
    assert_eq!((dropped.refused.get(), dropped.recovered.get()), (3, 1));
    assert_eq!(w.len(), 10);
}

/// The capacity a density asks for: the floor on a short block, one per
/// frame on a long one, the cap beyond that, rounded up.
#[test]
fn a_density_sizes_a_port_by_the_block() {
    let at = |density: EventDensity, block| {
        density.capacity(&crate::Prepare::new(
            tutti_units::SampleRate::SR_48K,
            MaxBlock::of(Samples::new(block)),
        ))
    };
    let frame = EventDensity::per_frame(1);
    assert_eq!(at(frame, 1), MIN_EVENT_PORT_CAPACITY);
    assert_eq!(at(frame, 512).get(), 512);
    assert_eq!(at(frame, 8192).get(), 8192);
    assert_eq!(at(frame, 1 << 20), MAX_EVENT_PORT_CAPACITY);
    // A quarter per frame, rounded up: 1000 frames hold 250.
    let quarter = EventDensity::per_kiloframe(256);
    assert_eq!(at(quarter, 4096).get(), 1024);
    assert_eq!(at(quarter, 1001), MIN_EVENT_PORT_CAPACITY);
    assert_eq!(
        at(EventDensity::per_kiloframe(1), 4097),
        MIN_EVENT_PORT_CAPACITY
    );
}
