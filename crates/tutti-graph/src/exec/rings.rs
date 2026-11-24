//! The graph's delay lines: `AudioRing` and `EventFifo`, the fixed-length
//! PDC rings for audio and for events.
//!
//! The audio ring is a straight loop over `&[F]` planar slices, so it
//! auto-vectorizes where the shape allows; nothing here is on a hot enough path
//! yet to earn explicit SIMD.
//!
//! # No summing kernel here
//!
//! The audio fan-in kernel is the executor's `sum_op` (`Op::Sum`), an `f32`
//! left fold in the engine's sample format `F`: it accumulates in `f32` in an `f32`
//! engine and in `f64` in an `f64` one, which is the 64-bit path for a many-input bus.
//! The event merge is not arithmetic and has no such question.
//!
//! # Retuning keeps history
//!
//! A ring's state is keyed by its [`AudioDelayKey`](crate::AudioDelayKey) or [`EventDelayKey`](crate::EventDelayKey) and survives a
//! recompile. When the recompile changes its *length* from `n` to `m`, it keeps
//! the most recent `min(n, m)` inputs and zero-fills the older end — exactly
//! the state a ring of length `m` would have had if it had seen the same
//! inputs, as far as the kept history goes.

use std::collections::VecDeque;

use tutti_buffers::Float;
use tutti_rt::CappedVec;
use tutti_units::Samples;

use crate::event::types::Event;
use crate::exec::held::Held;
use crate::exec::slots::EventSlot;
use crate::node::param_event::ParamEvent;
use crate::node::time::Offset;

/// An audio delay line of fixed length.
pub(crate) struct AudioRing<F: Float> {
    buf: Vec<F>,
    pos: usize,
    /// Consecutive silent input frames seen, saturating. Once it reaches the
    /// ring length the ring holds only zeros, so a silent input gives a silent
    /// output.
    quiet: usize,
}

impl<F: Float> AudioRing<F> {
    pub(crate) fn new(len: Samples) -> Self {
        Self {
            buf: vec![F::ZERO; len.get().max(1)],
            pos: 0,
            quiet: usize::MAX,
        }
    }

    /// The ring's inputs, oldest first.
    fn history(&self) -> impl Iterator<Item = F> + '_ {
        self.buf[self.pos..]
            .iter()
            .chain(&self.buf[..self.pos])
            .copied()
    }

    /// Take over `old`'s state into this ring, built zeroed on the control
    /// side for the new length: the most recent `min(old, new)` inputs, the
    /// older end zero-filled. Allocates nothing.
    pub(crate) fn transfer_from(&mut self, old: &AudioRing<F>) {
        let (n, m) = (old.buf.len(), self.buf.len());
        let keep = n.min(m);
        for (slot, x) in self.buf[m - keep..]
            .iter_mut()
            .zip(old.history().skip(n - keep))
        {
            *slot = x;
        }
        self.pos = 0;
        self.quiet = old.quiet;
    }

    /// Delay `src` into `dst`. Returns whether `dst` is known silent.
    pub(crate) fn run(&mut self, src: &[F], dst: &mut [F], src_silent: bool) -> bool {
        let n = self.buf.len();
        for (o, &x) in dst.iter_mut().zip(src) {
            *o = self.buf[self.pos];
            self.buf[self.pos] = x;
            self.pos += 1;
            if self.pos == n {
                self.pos = 0;
            }
        }
        self.account(src.len(), src_silent)
    }

    /// Delay `io` in place. Returns whether it is known silent afterwards.
    pub(crate) fn run_in_place(&mut self, io: &mut [F], src_silent: bool) -> bool {
        let n = self.buf.len();
        for x in io.iter_mut() {
            std::mem::swap(&mut self.buf[self.pos], x);
            self.pos += 1;
            if self.pos == n {
                self.pos = 0;
            }
        }
        self.account(io.len(), src_silent)
    }

    fn account(&mut self, frames: usize, src_silent: bool) -> bool {
        // The output is silent when the input is AND every sample still in
        // the ring from before this block was silent too.
        let silent = src_silent && self.quiet >= self.buf.len();
        self.quiet = if src_silent {
            self.quiet.saturating_add(frames)
        } else {
            0
        };
        silent
    }
}

impl<F: Float> AudioRing<F> {
    /// Copy the oldest `dst.len()` samples, without consuming them — the
    /// read half of a fixed-length feedback delay, whose write half
    /// ([`push`](Self::push)) runs later in the same block. Returns whether
    /// they are known silent.
    pub(crate) fn peek_oldest(&self, dst: &mut [F]) -> bool {
        let n = self.buf.len();
        debug_assert!(dst.len() <= n, "a feedback read is at most one ring long");
        for (i, o) in dst.iter_mut().enumerate() {
            *o = self.buf[(self.pos + i) % n];
        }
        self.quiet >= n
    }

    /// Append `src`, displacing the oldest `src.len()` samples.
    pub(crate) fn push(&mut self, src: &[F], src_silent: bool) {
        let n = self.buf.len();
        for &x in src {
            self.buf[self.pos] = x;
            self.pos += 1;
            if self.pos == n {
                self.pos = 0;
            }
        }
        self.account(src.len(), src_silent);
    }
}

/// An event delay: a FIFO of `(input time, event)` on the delay's own clock.
///
/// Input time rather than due time is stored so a retune can reschedule: an
/// event already past due under the new length is delivered at offset 0 of the
/// next block rather than dropped — a dropped note-off is a stuck note.
///
/// # Capacity, and what is never dropped
///
/// Sized on the control side from a **declared rate**: at most `cap` events
/// per port per `max_block` frames (the same `cap` every event slot holds per
/// block). A delay of `len` frames then holds at most
/// `cap × (⌈len / max_block⌉ + 2)` events; that is the `limit`. Above it,
/// a quarter again is **reserved for note-offs**. Past the limit an incoming
/// non-note-off is dropped (and counted); a note-off uses the reserve, and if
/// even that is full it evicts the oldest non-note-off instead. A note-off is
/// dropped only when the whole FIFO is note-offs.
///
/// **A table of owed closes** (a cut edge's held keys that did not fit when
/// it was cut) is poured into the queue as room frees, never past the limit
/// less one block's input: the closes can neither displace a live note-off
/// (the reserve stays free) nor make a live event of the source refused
/// (a block's input always fits), so nothing live is dropped while a table
/// drains, and a table never overflows: no close is lost.
///
/// The queue never grows past `limit + reserve` ([`bound`](Self::bound)),
/// and the compiler prices the delay's output slot at that same bound
/// (`EventSlotCapacity::fifo`), so everything that falls due in a block —
/// events input across several source blocks, or a backlog a retune made
/// overdue — is delivered in it. A FIFO carried across a recompile that
/// lowered its bound may hold more than the new one until it drains; the
/// editor widens its output slot (and every merge after it) by the most it
/// can hold past the new bound when it stages the commit (`exec/stage.rs`),
/// so those are delivered on their frames too. Delivery never
/// drops: should the slot be full anyway, the due events stay queued and go
/// out at offset 0 of the next block — late, not lost.
pub(crate) struct EventFifo {
    /// The events in flight, with the note-off reserve.
    events: Lane<Event>,
    /// The param events in flight. It has room only in a FIFO built for a
    /// source that writes them ([`with_params`](Self::with_params)); a param
    /// event past its room is dropped and counted, newest first, with no
    /// note-off to keep.
    params: Lane<ParamEvent>,
    clock: DelayClock,
    /// The declared events per block of the port it carries: what one block's
    /// input needs room for.
    rate: usize,
    /// The closes of a cut edge's held keys that the queue had no room for:
    /// poured in, as room frees, ahead of each block's input. Present only
    /// in a FIFO the editor built for a cut edge ([`with_owed`](Self::with_owed)).
    owed: Option<Held>,
}

/// A queued event: what a [`Lane`] orders by offset and keeps, a note-off
/// above the rest.
pub(crate) trait Queued: Copy {
    /// Where in its block.
    fn offset(&self) -> Offset;

    /// This event at `offset`.
    fn at(self, offset: Offset) -> Self;

    /// Whether the lane keeps it before any other when it overflows.
    fn is_note_off(&self) -> bool;
}

impl Queued for Event {
    fn offset(&self) -> Offset {
        self.offset
    }

    fn at(self, offset: Offset) -> Self {
        Self { offset, ..self }
    }

    fn is_note_off(&self) -> bool {
        Event::is_note_off(self)
    }
}

impl Queued for ParamEvent {
    fn offset(&self) -> Offset {
        self.offset
    }

    fn at(self, offset: Offset) -> Self {
        Self { offset, ..self }
    }

    fn is_note_off(&self) -> bool {
        false
    }
}

/// A delay's length and the frame it has reached: the time every queued
/// event's due frame is read against.
struct DelayClock {
    /// The delay, in frames.
    len: u64,
    /// The frame the next block starts at.
    now: u64,
}

impl DelayClock {
    /// The frame an event queued at `t` falls due.
    fn due(&self, t: u64) -> u64 {
        t + self.len
    }

    /// The frames `[start, end)` of the block of `frames` starting now.
    fn block(&self, frames: usize) -> (u64, u64) {
        (self.now, self.now + frames as u64)
    }
}

/// One kind of event in flight through a delay: due time and event, oldest
/// first. Holds at most `limit` events, and a note-off may use `reserve`
/// more, evicting the oldest other event when even that is full. Never grows
/// past the room it was built with.
struct Lane<T> {
    q: VecDeque<(u64, T)>,
    limit: usize,
    reserve: usize,
}

impl<T: Queued> Lane<T> {
    /// A lane with room for `room` events, holding at most `limit` (and
    /// `reserve` more note-offs). Control side: allocates.
    fn with_room(room: usize, limit: usize, reserve: usize) -> Self {
        Self {
            q: VecDeque::with_capacity(room),
            limit,
            reserve,
        }
    }

    /// Re-derive the limit and reserve, clamped to the room the lane has.
    fn fit(&mut self, limit: usize, reserve: usize) {
        let room = self.q.capacity();
        self.limit = limit.min(room);
        self.reserve = reserve.min(room - self.limit);
    }

    /// Queue this block's `input`, which starts at frame `start`. Returns how
    /// many were dropped.
    fn push(&mut self, start: u64, input: &[T]) -> u32 {
        let mut dropped = 0;
        for e in input {
            let item = (start + u64::from(e.offset().get()), *e);
            if self.q.len() < self.limit {
                self.q.push_back(item);
            } else if !e.is_note_off() {
                dropped += 1;
            } else if self.q.len() < self.limit + self.reserve {
                self.q.push_back(item);
            } else if let Some(i) = self.q.iter().position(|(_, x)| !x.is_note_off()) {
                // `remove` shifts in place and never reallocates, but it is
                // O(n) in the queue length, as is the `position` scan. That
                // is paid only on overflow — the rate the FIFO was sized for
                // has already been exceeded — so it buys note-off safety at
                // the one moment it is needed, not every block.
                self.q.remove(i);
                self.q.push_back(item);
                dropped += 1;
            } else {
                dropped += 1;
            }
        }
        dropped
    }

    /// Emit into `out` what falls due in the block of `frames` starting at
    /// the clock. Stops, without dropping, when `out` is full.
    fn pop_due(&mut self, clock: &DelayClock, out: &mut CappedVec<T>, frames: usize) {
        let (start, end) = clock.block(frames);
        while let Some(&(t, e)) = self.q.front() {
            let due = clock.due(t);
            if due >= end || out.is_full() {
                break;
            }
            self.q.pop_front();
            // `start <= … < end`: inside this block by the loop condition.
            out.push(e.at(Offset::raw(due.saturating_sub(start) as u32)));
        }
    }

    /// Take `old`'s queue into this lane, which was built with room for all
    /// of it. Should the room fall short, what does not fit is lost,
    /// non-note-offs first (a note-off evicts the oldest of them, as
    /// [`push`](Self::push) does): returns how many were lost.
    fn take_from(&mut self, old: &mut Self) -> usize {
        let room = self.q.capacity();
        let mut lost = 0;
        while let Some(item) = old.q.pop_front() {
            if self.q.len() < room {
                self.q.push_back(item);
            } else if item.1.is_note_off()
                && let Some(i) = self.q.iter().position(|(_, x)| !x.is_note_off())
            {
                self.q.remove(i);
                self.q.push_back(item);
                lost += 1;
            } else {
                lost += 1;
            }
        }
        lost
    }
}

impl EventFifo {
    /// A FIFO for a delay of `len`, sized from the declared rate (see the
    /// type docs). Control side: allocates.
    #[cfg(test)]
    pub(crate) fn sized(len: Samples, cap: usize, max_block: usize) -> Self {
        Self::sized_for(len, cap, max_block, 0)
    }

    /// As `sized`, with room for at least `room` events: a
    /// FIFO that takes over another's queue must hold all of it. Control
    /// side: allocates.
    pub(crate) fn sized_for(len: Samples, cap: usize, max_block: usize, room: usize) -> Self {
        let (limit, reserve) = Self::bounds(len.get(), cap, max_block);
        Self {
            events: Lane::with_room((limit + reserve).max(room), limit, reserve),
            params: Lane::with_room(0, 0, 0),
            clock: DelayClock {
                len: len.get() as u64,
                now: 0,
            },
            rate: cap.max(1),
            owed: None,
        }
    }

    /// This FIFO with room for param events: as many as it holds events (its
    /// limit), for a source that writes them. Control side: allocates.
    pub(crate) fn with_params(mut self) -> Self {
        self.params = Lane::with_room(self.events.limit, self.events.limit, 0);
        self
    }

    /// This FIFO with a table for owed closes. Control side: allocates.
    pub(crate) fn with_owed(mut self) -> Self {
        self.owed = Some(Held::new());
        self
    }

    /// The table of owed closes, taken out: the FIFO goes away and what it
    /// owes moves to its sink.
    pub(crate) fn take_owed(&mut self) -> Option<Held> {
        if self.owed.as_ref().is_some_and(|t| !t.is_empty()) {
            self.owed.take()
        } else {
            None
        }
    }

    /// Owe the closes of `held` to the FIFO's sink: they enter the queue as
    /// room frees, a pedal up first. Returns whether the FIFO has a table.
    pub(crate) fn owe(&mut self, held: &Held) -> bool {
        match &mut self.owed {
            Some(t) => {
                t.merge(held);
                true
            }
            None => false,
        }
    }

    /// Queue what is owed, at the clock: behind everything in flight, ahead
    /// of this block's input, as far as the queue stays a block's input short
    /// of its limit. The room left holds the live input of the block (so a
    /// table draining never makes a source's own events refused) and the
    /// note-off reserve stays free, so a live note-off always finds room.
    /// Allocates nothing.
    pub(crate) fn pour(&mut self) {
        let room = self.events.limit.saturating_sub(self.rate);
        let Some(owed) = &mut self.owed else {
            return;
        };
        while self.events.q.len() < room {
            let Some(e) = owed.pop_close() else {
                break;
            };
            self.events.q.push_back((self.clock.now, e));
        }
    }

    /// The most a FIFO for a delay of `len` at this rate ever holds: its
    /// limit plus the note-off reserve. What its output slot is sized for.
    pub(crate) fn bound(len: usize, cap: usize, max_block: usize) -> usize {
        let (limit, reserve) = Self::bounds(len, cap, max_block);
        limit + reserve
    }

    /// `⌈len / max_block⌉ + 2`: how many declared blocks' worth of events a
    /// delay of `len` holds at most (see the type docs). The compiler prices
    /// with it (`EventSlotCapacity::fifo`), so it and [`bounds`](Self::bounds)
    /// are one formula.
    pub(crate) fn blocks(len: usize, max_block: usize) -> usize {
        len.div_ceil(max_block.max(1)) + 2
    }

    /// `(limit, note-off reserve)` for a delay of `len` at the declared rate.
    fn bounds(len: usize, cap: usize, max_block: usize) -> (usize, usize) {
        let limit = cap.max(1) * Self::blocks(len, max_block);
        (limit, limit / 4 + 8)
    }

    pub(crate) fn retune(&mut self, len: Samples) {
        self.clock.len = len.get() as u64;
    }

    /// Re-derive the limit for the current length at this rate and maximum
    /// block — after a retune, a re-prepare that shrank `MaxBlock` (more
    /// blocks fit in the same delay, so more events may be in flight), or a
    /// source that now declares a different rate (`Shape::event_capacity`,
    /// a unit hard-replaced by one with another declaration). Keeps every
    /// queued event. Never allocates: the FIFO was built with room for the
    /// new bound (`exec/stage.rs`), and a bound past the room it has is
    /// clamped to it.
    ///
    /// The limit is the new bound, not the new bound plus what is queued:
    /// while the backlog is at or past the new limit, a new non-note-off is
    /// refused (dropped, and counted in the executor's dropped events) and a
    /// note-off takes the reserve or evicts the oldest non-note-off, as
    /// [`push`](Self::push) does at any full FIFO. Queued events past a smaller
    /// new bound are kept, and the output slot is widened by the most they
    /// can be, so they still go out on their frames even if they all fall
    /// due at once.
    pub(crate) fn resize(&mut self, cap: usize, max_block: usize) {
        let (limit, reserve) = Self::bounds(self.clock.len as usize, cap, max_block);
        self.events.fit(limit, reserve);
        self.params.fit(limit, 0);
        self.rate = cap.max(1);
        // A table pours up to `limit - rate`: staging sizes the room for the
        // new bound, so the clamp above never leaves a FIFO that carries
        // events no room to pour into.
        debug_assert!(
            cap == 0 || self.events.limit > self.rate,
            "an event FIFO's limit leaves a pour no room"
        );
    }

    /// How many events are queued.
    pub(crate) fn queued(&self) -> usize {
        self.events.q.len() + self.params.q.len()
    }

    /// Take over `old`'s queue and clock into this FIFO, which was built on
    /// the control side for the new length and rate with room for all of
    /// it: what a retune and a [`resize`](Self::resize) leave. Allocates
    /// nothing. Should the room fall short, what does not fit is lost,
    /// non-note-offs first (a note-off evicts the oldest of them, as
    /// [`push`](Self::push) does): returns how many were lost, for the
    /// caller to count.
    pub(crate) fn transfer_from(&mut self, old: &mut EventFifo) -> usize {
        let mut lost = self.events.take_from(&mut old.events);
        lost += self.params.take_from(&mut old.params);
        self.clock.now = old.clock.now;
        // What was owed stays owed (the old table goes back with the FIFO).
        if let Some(owed) = old.owed.as_ref().filter(|t| !t.is_empty())
            && !self.owe(owed)
        {
            debug_assert!(false, "a FIFO that owes is built with a table");
            lost += 1;
        }
        lost
    }

    /// Every event still queued, oldest first.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> impl Iterator<Item = Event> + '_ {
        self.events.q.iter().map(|&(_, e)| e)
    }

    /// The queued events for a flush, when the delay itself goes away: each
    /// at its due time relative to the earliest, so a note-on/note-off pair
    /// keeps its length, appended to `out` as far as its capacity takes.
    /// Returns how many did not fit. Allocates nothing.
    pub(crate) fn flush_into(&self, out: &mut CappedVec<Event>) -> usize {
        let first = self.events.q.front().map_or(0, |&(t, _)| self.clock.due(t));
        // The param events in flight go no further: a sink that outlives its
        // delay takes up the source's own segments again (each is absolute).
        let mut lost = self.params.q.len();
        for &(t, e) in &self.events.q {
            if out.is_full() {
                lost += 1;
                continue;
            }
            out.push(Event {
                offset: Offset::raw((self.clock.due(t) - first).min(u64::from(u32::MAX)) as u32),
                ..e
            });
        }
        lost
    }

    /// Queue this block's `input`, events and param events. Returns how many
    /// were dropped.
    pub(crate) fn push(&mut self, input: &EventSlot) -> u32 {
        let start = self.clock.now;
        self.events.push(start, &input.events) + self.params.push(start, &input.params)
    }

    /// Emit into `out` what falls due in the block of `frames`, events and
    /// param events.
    pub(crate) fn pop_due(&mut self, out: &mut EventSlot, frames: usize) {
        self.events.pop_due(&self.clock, &mut out.events, frames);
        self.params.pop_due(&self.clock, &mut out.params, frames);
    }

    /// Move the clock past a block of `frames`.
    pub(crate) fn advance(&mut self, frames: usize) {
        self.clock.now += frames as u64;
    }

    /// A PDC delay's whole block: emit what was already queued and falls
    /// due, queue the block's input, emit what of it falls due too, advance.
    ///
    /// The backlog goes out **before** the input is queued: the limit bounds
    /// events in flight, and an event leaving in this very block is not. A
    /// FIFO a recompile retuned shorter holds a backlog that is all due at
    /// once; queued first, the input would have been refused against room
    /// the backlog was about to free. Order is kept: every queued event was
    /// input before this block's, at the same length, so it falls due no
    /// later.
    pub(crate) fn run(&mut self, input: &EventSlot, out: &mut EventSlot, frames: usize) -> u32 {
        self.pop_due(out, frames);
        self.pour();
        let dropped = self.push(input);
        self.pop_due(out, frames);
        self.advance(frames);
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::param_event::ParamRamp;
    use tutti_ump::{Message, MidiChannel, NoteNumber, U7, Ump, Voice1};

    /// [`EventFifo::push`] of a list of events.
    fn push_events(f: &mut EventFifo, events: &[Event]) -> u32 {
        f.push(&EventSlot {
            events: filled(events),
            params: CappedVec::default(),
        })
    }

    /// `events` in a buffer with room for just them.
    fn filled(events: &[Event]) -> CappedVec<Event> {
        let mut buf = CappedVec::with_capacity(events.len());
        buf.extend(events.iter().copied());
        buf
    }

    /// [`EventFifo::pop_due`] into the events `out`, which keeps its capacity.
    fn pop_due_events(f: &mut EventFifo, out: &mut CappedVec<Event>, frames: usize) {
        let mut slot = EventSlot {
            events: std::mem::take(out),
            params: CappedVec::default(),
        };
        f.pop_due(&mut slot, frames);
        *out = slot.events;
    }

    /// [`EventFifo::run`] over a list of events and the `out` it fills.
    fn run(f: &mut EventFifo, input: &[Event], out: &mut CappedVec<Event>, frames: usize) -> u32 {
        let input = EventSlot {
            events: filled(input),
            params: CappedVec::default(),
        };
        let mut slot = EventSlot {
            events: std::mem::take(out),
            params: CappedVec::default(),
        };
        let lost = f.run(&input, &mut slot, frames);
        *out = slot.events;
        lost
    }

    fn param(offset: u32, target: f64) -> ParamEvent {
        ParamEvent::ramp(
            Offset::raw(offset),
            ParamRamp::by_id(tutti_units::ParamId::new(1), target, Samples::new(0)),
        )
    }

    /// A ring of `n` outputs its input `n` samples later, across blocks.
    #[test]
    fn ring_delays_by_its_length() {
        let mut r = AudioRing::<f32>::new(Samples::new(3));
        let src: Vec<f32> = (1..=8).map(|x| x as f32).collect();
        let mut dst = vec![0.0; 8];
        r.run(&src[..5], &mut dst[..5], false);
        r.run(&src[5..], &mut dst[5..], false);
        assert_eq!(dst, vec![0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
    }

    /// A ring built for another length takes the newest inputs and
    /// zero-pads the oldest end.
    #[test]
    fn transfer_keeps_recent_history() {
        let mut r = AudioRing::<f32>::new(Samples::new(4));
        let mut sink = vec![0.0; 4];
        r.run(&[1.0, 2.0, 3.0, 4.0], &mut sink, false);
        let mut shorter = AudioRing::<f32>::new(Samples::new(2));
        shorter.transfer_from(&r);
        let mut out = vec![0.0; 2];
        shorter.run(&[0.0, 0.0], &mut out, false);
        assert_eq!(out, vec![3.0, 4.0]);
        let mut longer = AudioRing::<f32>::new(Samples::new(4));
        longer.transfer_from(&shorter);
        let mut out = vec![0.0; 4];
        longer.run(&[0.0; 4], &mut out, false);
        assert_eq!(out, vec![0.0, 0.0, 0.0, 0.0]);
    }

    /// A FIFO built with too little room takes what fits, evicts a
    /// non-note-off for a note-off, counts every event it cannot keep and
    /// never grows its queue.
    #[test]
    fn a_transfer_into_too_small_a_fifo_counts_its_loss_and_never_grows() {
        let mut old = EventFifo::sized(Samples::new(64), 4, 8);
        let queued: Vec<Event> = (0..4).map(|i| note(0, false, i)).collect();
        push_events(&mut old, &queued);
        push_events(&mut old, &[note(0, true, 10)]);
        let mut new = EventFifo::sized(Samples::new(64), 4, 8);
        // Room for three: shrink it by building one that holds three.
        new.events.q = VecDeque::with_capacity(3);
        let room = new.events.q.capacity();
        assert!(room < 5);
        let lost = new.transfer_from(&mut old);
        assert_eq!(new.events.q.capacity(), room, "the queue did not grow");
        assert_eq!(lost, 5 - room);
        assert_eq!(new.events.q.len(), room);
        assert!(old.events.q.is_empty());
        assert!(
            new.pending().any(|e| e.is_note_off()),
            "the note-off evicted a note-on"
        );
    }

    /// A queue that is all note-offs has none to evict: the one that does not
    /// fit is counted lost, and the queue does not grow. (A FIFO the editor
    /// stages holds all of the queue it takes, so this is a bug's backstop.)
    #[test]
    fn a_transfer_into_a_queue_of_note_offs_counts_what_does_not_fit() {
        let mut old = EventFifo::sized(Samples::new(64), 4, 8);
        push_events(
            &mut old,
            &[note(0, true, 1), note(0, true, 2), note(0, true, 3)],
        );
        let mut new = EventFifo::sized(Samples::new(64), 4, 8);
        new.events.q = VecDeque::with_capacity(2);
        let room = new.events.q.capacity();
        let lost = new.transfer_from(&mut old);
        assert_eq!(lost, 3 - room);
        assert_eq!(new.events.q.capacity(), room);
        assert!(new.pending().all(|e| e.is_note_off()));
    }

    /// An event is delivered `len` frames later, in whichever block that is.
    #[test]
    fn fifo_delays_events_across_blocks() {
        let mut f = EventFifo::sized(Samples::new(6), 8, 8);
        let mut out = CappedVec::with_capacity(8);
        run(
            &mut f,
            &[Event::midi(Offset::raw(2), Ump([7, 0, 0, 0]))],
            &mut out,
            8,
        );
        assert!(out.is_empty(), "due at 8, which is the next block");
        run(&mut f, &[], &mut out, 8);
        assert_eq!(
            out.as_slice(),
            [Event::midi(Offset::ZERO, Ump([7, 0, 0, 0]))]
        );
    }

    /// A backlog due this block frees its room before the block's input is
    /// queued: a FIFO holding its limit's worth of overdue events (a retune
    /// made them due at once) takes a full block of new events without
    /// refusing one, and delivers all of them in order.
    #[test]
    fn a_due_backlog_makes_room_for_the_blocks_input() {
        let mut f = EventFifo::sized(Samples::new(64), 2, 64);
        let limit = f.events.limit;
        let backlog: Vec<Event> = (0..limit as u32).map(|i| note(i, false, i)).collect();
        push_events(&mut f, &backlog);
        // Shorter: the whole backlog is overdue.
        f.retune(Samples::new(1));
        f.resize(2, 64);
        let input = [note(0, false, 100), note(1, false, 101)];
        let mut out = CappedVec::with_capacity(EventFifo::bound(1, 2, 64));
        let dropped = run(&mut f, &input, &mut out, 64);
        assert_eq!(dropped, 0, "nothing refused");
        let tags: Vec<u32> = out
            .iter()
            .map(|e| match e.kind {
                crate::event::types::EventKind::Midi(Ump(w)) => (w[0] >> 8) & 0x7f,
                _ => unreachable!("notes only"),
            })
            .collect();
        let want: Vec<u32> = (0..limit as u32).chain([100, 101]).collect();
        assert_eq!(
            tags, want,
            "the backlog first, then the input, all this block"
        );
    }

    /// A param event is delayed as an event is: `len` frames later, on the
    /// frame it was due, in whichever block that is, and in order.
    #[test]
    fn fifo_delays_param_events_across_blocks() {
        let mut f = EventFifo::sized(Samples::new(6), 8, 8).with_params();
        let block = |params: &[ParamEvent]| {
            let mut slot = EventSlot::with_capacity(0, params.len());
            slot.params.extend(params.iter().copied());
            slot
        };
        let mut out = EventSlot::with_capacity(8, 8);
        f.run(&block(&[param(2, 0.25), param(5, 0.5)]), &mut out, 8);
        // Due at 8 and 11: neither in this block.
        assert!(out.params.is_empty());
        out.clear();
        f.run(&block(&[]), &mut out, 8);
        assert_eq!(out.params.as_slice(), [param(0, 0.25), param(3, 0.5)]);
    }

    /// A FIFO built without room for param events drops what it is given and
    /// counts it: the compiler gives room only to a source that declares it.
    #[test]
    fn a_fifo_with_no_param_room_drops_param_events() {
        let mut f = EventFifo::sized(Samples::new(6), 8, 8);
        let mut input = EventSlot::with_capacity(0, 1);
        input.params.push(param(0, 1.0));
        let mut out = EventSlot::with_capacity(8, 8);
        assert_eq!(f.run(&input, &mut out, 8), 1);
        f.run(&EventSlot::default(), &mut out, 8);
        assert!(out.params.is_empty());
    }

    /// A feedback ring read before it is written delays by exactly its
    /// length, whatever the block sizes.
    #[test]
    fn a_feedback_ring_delays_by_its_length_across_ragged_blocks() {
        let mut r = AudioRing::<f32>::new(Samples::new(8));
        let src: Vec<f32> = (1..=40).map(|x| x as f32).collect();
        let mut got = Vec::new();
        let mut at = 0;
        for n in [8, 3, 1, 8, 5, 7, 8] {
            let mut out = vec![0.0; n];
            r.peek_oldest(&mut out);
            r.push(&src[at..at + n], false);
            got.extend(out);
            at += n;
        }
        let want: Vec<f32> = (0..at)
            .map(|i| if i < 8 { 0.0 } else { src[i - 8] })
            .collect();
        assert_eq!(got, want);
    }

    fn note(offset: u32, off: bool, tag: u32) -> Event {
        // MIDI 1.0, group 0, channel 0, note `tag`, velocity 100.
        let (channel, note, velocity) = (
            MidiChannel::FIRST,
            NoteNumber::masked(tag as u8),
            U7::of(100),
        );
        Event::midi(
            Offset::raw(offset),
            if off {
                Ump::from(Message::midi1(channel, Voice1::note_off(note, velocity)))
            } else {
                Ump::from(Message::midi1(channel, Voice1::note_on(note, velocity)))
            },
        )
    }

    /// A note-off survives an overflow: past the limit it takes the reserve,
    /// and when even that is full it evicts the oldest non-note-off.
    #[test]
    fn a_note_off_survives_overflow() {
        let mut f = EventFifo::sized(Samples::new(4), 1, 4);
        let limit = f.events.limit;
        let cap = f.events.q.capacity();
        let ons: Vec<Event> = (0..cap as u32 + 5).map(|i| note(0, false, i)).collect();
        let dropped = push_events(&mut f, &ons);
        assert_eq!(f.events.q.len(), limit, "note-ons stop at the limit");
        assert_eq!(dropped as usize, ons.len() - limit);
        // Fill the reserve with note-offs, then one more.
        let offs: Vec<Event> = (0..(cap - limit) as u32 + 1)
            .map(|i| note(0, true, i))
            .collect();
        push_events(&mut f, &offs);
        let kept_offs = f.pending().filter(Event::is_note_off).count();
        assert_eq!(kept_offs, offs.len(), "every note-off is kept");
        assert!(f.events.q.len() <= cap, "and the FIFO never grew");
    }

    /// The queue never passes its bound, however much room its allocation
    /// has: a FIFO resized from a longer delay keeps its larger allocation,
    /// but its note-off reserve is counted against the new bound, which is
    /// what its output slot is priced at.
    #[test]
    fn a_fifo_never_holds_more_than_its_bound() {
        let mut f = EventFifo::sized(Samples::new(64), 4, 8);
        f.retune(Samples::new(1));
        f.resize(4, 8);
        let offs: Vec<Event> = (0..200).map(|i| note(0, true, i)).collect();
        push_events(&mut f, &offs);
        assert_eq!(f.pending().count(), EventFifo::bound(1, 4, 8));
    }

    /// Delivery never drops: when the output slot is full, due events stay
    /// queued and go out at offset 0 of the next block — late, not lost.
    #[test]
    fn a_full_output_slot_delays_events_rather_than_dropping_them() {
        let mut f = EventFifo::sized(Samples::new(1), 16, 8);
        let burst: Vec<Event> = (0..5)
            .map(|i| Event::midi(Offset::ZERO, Ump([i, 0, 0, 0])))
            .collect();
        push_events(&mut f, &burst);
        let mut out = CappedVec::with_capacity(2);
        pop_due_events(&mut f, &mut out, 8);
        f.advance(8);
        assert_eq!(out.len(), 2, "the slot takes two");
        let mut got = out.to_vec();
        for _ in 0..3 {
            out.clear();
            pop_due_events(&mut f, &mut out, 8);
            f.advance(8);
            assert!(
                out.iter().all(|e| e.offset == Offset::ZERO),
                "late events land at 0"
            );
            got.extend(out.iter().copied());
        }
        let tags: Vec<u32> = got
            .iter()
            .map(|e| match e.kind {
                crate::event::types::EventKind::Midi(Ump(w)) => w[0],
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(tags, vec![0, 1, 2, 3, 4], "all five, in order");
    }
}
