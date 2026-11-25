//! Keys held on an event output port, so an edge that ends can close them.
//!
//! The executor watches what a node writes to an event output port that
//! carries notes and keeps what it started and has not ended: the notes down,
//! and the hold pedals (CC 64, 66, 67, 69) and pitch bends off their normal
//! position. When a commit removes an edge the port still holds something on,
//! the sink that loses the edge is sent the offs, the pedal-ups and the
//! centred bends through the injection path that flushes a vanished delay.
//! Nodes know nothing of this.
//!
//! The tables are [`tutti_ump::Held`]: a bitset over the 15-bit
//! [`NoteKey`](tutti_ump::NoteKey) (4 KiB) and a word per lane (1 KiB),
//! allocated on the control side with the commit that places the unit and never
//! grown. A node's op writes only its own unit's
//! tables, so a parallel block needs no new claim.
//!
//! A sink fed by several sources cannot tell whose note a key is: one source's
//! close ends another's note on the same key. So can an owed close that follows a
//! replacement: past about 72 held keys (what a block's slot takes) it can end a
//! note the new unit started on the same key.

use tutti_rt::CappedVec;
use tutti_ump::Ump;

use crate::event::types::{Event, EventKind};
use crate::node::time::Offset;

/// What a port has been told is held ([`tutti_ump::Held`]: the keys down, the
/// hold pedals down and the bends off centre) and how it is closed into a block.
#[derive(Clone, Debug)]
pub(crate) struct Held(tutti_ump::Held);

impl Held {
    pub(crate) fn new() -> Self {
        Self(tutti_ump::Held::new())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Fold in what a block wrote.
    #[inline]
    pub(crate) fn observe(&mut self, events: &[Event]) {
        for e in events {
            if let EventKind::Midi(u) = e.kind {
                self.0.observe(u);
            }
        }
    }

    /// Forget everything held.
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    /// Add what `other` holds.
    pub(crate) fn merge(&mut self, other: &Held) {
        self.0.union_with(&other.0);
    }

    /// Write the close of what is held ahead of `events` (at offset 0, so the
    /// block stays sorted), as many as `events`' capacity takes, and forget
    /// those: the rest stays held for the next call. Allocates nothing.
    pub(crate) fn close_ahead(&mut self, events: &mut CappedVec<Event>) {
        let start = events.len();
        let added = self.drain_into(events);
        debug_assert_eq!(events.len() - start, added);
        events.rotate_right(added);
    }

    /// Append the close of what is held to `events`, controllers returned then
    /// note offs, as many as its capacity takes, and forget those. Returns how
    /// many. Allocates nothing.
    pub(crate) fn drain_into(&mut self, events: &mut CappedVec<Event>) -> usize {
        self.drain_upto(events, usize::MAX)
    }

    /// [`drain_into`](Self::drain_into) writing at most `max` events.
    pub(crate) fn drain_upto(&mut self, events: &mut CappedVec<Event>, max: usize) -> usize {
        let start = events.len();
        let room = |e: &CappedVec<Event>| !e.is_full() && e.len() - start < max;
        let released = self.0.controllers_mut().release_while(None, |ump| {
            if !room(events) {
                return false;
            }
            events.push(Self::event(ump));
            true
        });
        if !released {
            return events.len() - start;
        }
        let keys = self.0.keys_mut();
        let mut next = 0;
        while let Some(key) = keys.next_from(next) {
            if !room(events) {
                break;
            }
            next = u32::from(key.index()) + 1;
            keys.remove(key);
            events.push(Self::event(Ump::note_off(key)));
        }
        events.len() - start
    }

    /// The next message that closes what is held, forgotten as it is taken:
    /// a controller's return first, then the lowest key's note off, at offset
    /// 0. `None` once nothing is held. Allocates nothing.
    pub(crate) fn pop_close(&mut self) -> Option<Event> {
        if let Some(ump) = self.0.controllers_mut().pop() {
            return Some(Self::event(ump));
        }
        let keys = self.0.keys_mut();
        let key = keys.next_from(0)?;
        keys.remove(key);
        Some(Self::event(Ump::note_off(key)))
    }

    fn event(ump: Ump) -> Event {
        Event {
            offset: Offset::ZERO,
            kind: EventKind::Midi(ump),
        }
    }

    /// Calls `f` with the messages that close everything held: controllers
    /// returned, then every note off, at offset 0 (ties keep this order).
    pub(crate) fn close_each(&self, mut f: impl FnMut(Event)) {
        for ump in self.0.controllers().closes() {
            f(Self::event(ump));
        }
        for key in self.0.keys().iter() {
            f(Self::event(Ump::note_off(key)));
        }
    }
}

/// One table per event output port of a unit; `None` for a port that carries
/// no notes.
pub(crate) type Tables = Vec<Option<Held>>;

/// `ports` tables, one for each port when `notes`: allocates what is missing.
fn fill_tables(t: &mut Tables, ports: usize, notes: bool) {
    t.resize_with(ports, || None);
    if notes {
        t.iter_mut().for_each(|h| {
            h.get_or_insert_with(Held::new);
        });
    }
}

/// Add every table of `from` to the matching table of `into`, and empty
/// `from` (its tables stay allocated).
fn absorb(into: &mut Tables, from: &mut Tables) {
    if from.is_empty() {
        return;
    }
    debug_assert!(
        into.len() == from.len()
            && into
                .iter()
                .zip(from.iter())
                .all(|(a, b)| a.is_some() == b.is_some()),
        "tables of one unit have one shape"
    );
    for (a, b) in into.iter_mut().zip(from.iter_mut()) {
        if let (Some(a), Some(b)) = (a, b) {
            a.merge(b);
            b.clear();
        }
    }
}

/// A unit's held-key tables, in one of these shapes:
///
/// - running: `held` is the unit now writing the port; the others are empty.
/// - waiting: a replace with a fade is queued behind a running one. `held`
///   is still the running unit's, and `staged` (empty) is for the waiting
///   one.
/// - closing: a unit stopped writing and `owed` holds what it left; the next
///   blocks write the closes ahead of the port's events. Independent of the
///   other two: `owed` is only ever added to and drained.
///
/// Every way out closes or hands over the keys, none drops them:
///
/// | event | `held` | `staged` | `owed` |
/// |---|---|---|---|
/// | the waiting unit takes over (`promote`) | becomes `staged`, emptied | becomes `held` | gains the old `held` |
/// | the unit is retired or hard-replaced (`depart`) | closed at the commit | freed | closed with it |
/// | a cut displaces the running unit (`displace`) | the waiting unit's | taken | closed at the commit |
/// | a re-prepare checks the unit out (`park`) | kept | taken | gains the displaced unit's |
/// | the unit comes back (restore) | as parked | `size` re-makes it if one waits | as parked |
/// | it does not come back | closed (`depart`) | freed | closed with it |
/// | an edge is cut with keys owed | closed on that edge | | closed on that edge too; the rest still drain to the other sinks. Cutting and re-adding the edge while a close is owed can send the sink an extra orphan off: harmless, but not "closed once" |
#[derive(Default)]
pub(crate) struct Keys {
    held: Tables,
    staged: Tables,
    owed: Tables,
    closing: bool,
}

impl Keys {
    /// Size the tables for a unit with `ports` event outputs. A unit that
    /// carries notes gets a table per port, and while a replace waits, its
    /// `staged` and `owed` too. Allocates: control side, or applying a commit.
    pub(crate) fn size(&mut self, ports: usize, notes: bool, waiting: bool) {
        fill_tables(&mut self.held, ports, notes);
        // `owed` for every unit that carries notes: a bypass owes what the
        // unit held when it parks (`owe_held`).
        fill_tables(&mut self.owed, ports, notes);
        if waiting {
            fill_tables(&mut self.staged, ports, notes);
        }
    }

    /// The tables a waiting replace's unit writes into (`staged`), built on
    /// the control side for [`wait`](Self::wait).
    pub(crate) fn staging(ports: usize, notes: bool) -> Tables {
        let mut t = Tables::new();
        fill_tables(&mut t, ports, notes);
        t
    }

    /// A replace waits behind a running one: `staged` is `tables` unless it
    /// is already sized. Returns the tables not used, to free on the control
    /// side. Allocates nothing.
    pub(crate) fn wait(&mut self, tables: Tables) -> Tables {
        if self.staged.len() < self.held.len() {
            std::mem::replace(&mut self.staged, tables)
        } else {
            tables
        }
    }

    /// Hands every table to `spare`, to be freed on the control side.
    pub(crate) fn retire_into(self, spare: &mut CappedVec<Tables>) {
        for tables in [self.held, self.staged, self.owed] {
            crate::exec::place(spare, tables);
        }
    }

    /// A replace with a crossfade departed the running unit's keys (it
    /// stays in the store, fading out, and its keys are closed at the
    /// commit): take the tables of `fresh`, sized for the unit, where it has
    /// none now. Every table not used goes to `spare`. Allocates nothing.
    pub(crate) fn refill(&mut self, mut fresh: Keys, spare: &mut CappedVec<Tables>) {
        if self.held.len() < fresh.held.len() {
            std::mem::swap(&mut self.held, &mut fresh.held);
        }
        if self.owed.len() < fresh.owed.len() {
            std::mem::swap(&mut self.owed, &mut fresh.owed);
        }
        fresh.retire_into(spare);
    }

    pub(crate) fn held(&self) -> &Tables {
        &self.held
    }

    /// Fold in what the running unit wrote to `port`.
    #[inline]
    pub(crate) fn observe(&mut self, port: usize, events: &[Event]) {
        match self.held.get_mut(port) {
            Some(Some(h)) => h.observe(events),
            Some(None) => debug_assert!(
                !events.iter().any(|e| matches!(
                    e.kind,
                    EventKind::Midi(u) if u.note_key_event().is_some()
                )),
                "a node that declared no note events wrote one"
            ),
            None => {}
        }
    }

    /// What `port` owes and has not yet been sent.
    pub(crate) fn owed(&self, port: u16) -> Option<&Held> {
        self.owed.get(usize::from(port))?.as_ref()
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.closing
    }

    /// Write what `port` owes ahead of `events`, as far as they take.
    pub(crate) fn close_port(&mut self, port: usize, events: &mut CappedVec<Event>) {
        if let Some(Some(h)) = self.owed.get_mut(port) {
            h.close_ahead(events);
        }
    }

    /// After a block's closes: whether any are still owed.
    pub(crate) fn settle(&mut self) {
        self.closing = self.owed.iter().flatten().any(|h| !h.is_empty());
    }

    /// The unit stops writing (a bypass parks it): what it holds is owed,
    /// and the next blocks write the closes ahead of the port's events.
    pub(crate) fn owe_held(&mut self) {
        absorb(&mut self.owed, &mut self.held);
        self.settle();
    }

    /// The waiting unit takes over; the unit that stopped leaves its keys owed.
    pub(crate) fn promote(&mut self) {
        debug_assert!(
            self.staged.len() == self.held.len(),
            "promoting with nothing waiting"
        );
        std::mem::swap(&mut self.held, &mut self.staged);
        absorb(&mut self.owed, &mut self.staged);
        self.closing = true;
    }

    /// The running unit is displaced by the waiting one at once (a cut):
    /// returns what it held and owed, to close at the commit.
    pub(crate) fn displace(&mut self) -> Tables {
        debug_assert!(
            self.staged.len() == self.held.len(),
            "displacing with nothing waiting"
        );
        let mut stale = std::mem::replace(&mut self.held, std::mem::take(&mut self.staged));
        absorb(&mut stale, &mut self.owed);
        self.closing = false;
        stale
    }

    /// A displaced unit's keys are owed from the next block: a re-prepare
    /// cannot close them at the commit, the units are out.
    pub(crate) fn park(&mut self, mut stale: Tables) -> Tables {
        absorb(&mut self.owed, &mut stale);
        self.closing = true;
        stale
    }

    /// The unit is gone: returns every key it held or owed, to close at the
    /// commit, and pushes the emptied tables to `spare`.
    pub(crate) fn depart(&mut self, spare: &mut CappedVec<Tables>) -> Tables {
        let mut gone = std::mem::take(&mut self.held);
        absorb(&mut gone, &mut self.owed);
        crate::exec::place(spare, std::mem::take(&mut self.staged));
        crate::exec::place(spare, std::mem::take(&mut self.owed));
        self.closing = false;
        gone
    }
}
