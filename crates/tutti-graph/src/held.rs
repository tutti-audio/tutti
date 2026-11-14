//! Keys held on an event output port, so an edge that ends can close them.
//!
//! The executor watches what a node writes to an event output port that
//! carries notes and keeps the set of notes started and not ended, and the
//! channels whose sustain pedal is down. When a commit removes an edge the port
//! still holds keys on, the sink that loses the edge is sent the offs through
//! the injection path that flushes a vanished delay. Nodes know nothing of
//! this.
//!
//! The set is a bitset over the 16-bit [`Key`](tutti_ump::Key), fixed at 8 KiB, allocated when
//! a plan is applied and never grown. A node's op writes only its own unit's
//! sets, so a parallel block needs no new claim.
//!
//! A sink fed by several sources cannot tell whose note a key is: one source's
//! close ends another's note on the same key. So can an owed close that follows a
//! replacement: past about 72 held keys (what a block's slot takes) it can end a
//! note the new unit started on the same key.

use tutti_ump::{Address, KeyBitset, KeyEvent, Lane, LaneSet, Protocol, Ump};

use crate::event::{Event, EventKind};
use crate::time::Offset;

#[derive(Clone, Debug)]
pub(crate) struct Held {
    keys: Box<KeyBitset>,
    /// Sustain pedals down.
    pedals: LaneSet,
}

impl Held {
    pub(crate) fn new() -> Self {
        Self {
            keys: Box::default(),
            pedals: LaneSet::new(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.pedals.is_empty()
    }

    fn pedal(&mut self, protocol: Protocol, address: Address, down: bool) {
        let Some(lane) = Lane::new(protocol, address) else {
            return;
        };
        if down {
            self.pedals.insert(lane);
        } else {
            self.pedals.remove(lane);
        }
    }

    /// Fold in what a block wrote.
    #[inline]
    pub(crate) fn observe(&mut self, events: &[Event]) {
        for e in events {
            let EventKind::Midi(u) = e.kind else { continue };
            match u.key_event() {
                Some(KeyEvent::On(k)) => {
                    self.keys.insert(k);
                }
                Some(KeyEvent::Off(k)) => {
                    self.keys.remove(k);
                }
                Some(KeyEvent::Sustain {
                    protocol,
                    address,
                    down,
                }) => self.pedal(protocol, address, down),
                None => {}
            }
        }
    }

    /// Whether any of `events` is a held-key event.
    pub(crate) fn any_key(events: &[Event]) -> bool {
        events
            .iter()
            .any(|e| matches!(e.kind, EventKind::Midi(u) if u.key_event().is_some()))
    }

    /// Forget everything held.
    fn clear(&mut self) {
        self.keys.clear();
        self.pedals.clear();
    }

    /// Add what `other` holds.
    pub(crate) fn merge(&mut self, other: &Held) {
        self.keys.union_with(&other.keys);
        self.pedals.union_with(&other.pedals);
    }

    /// Write the close of what is held ahead of `events` (at offset 0, so the
    /// block stays sorted), as many as `events`' capacity takes, and forget
    /// those: the rest stays held for the next call. Allocates nothing.
    pub(crate) fn close_ahead(&mut self, events: &mut Vec<Event>) {
        let start = events.len();
        let added = self.drain_into(events);
        debug_assert_eq!(events.len() - start, added);
        events.rotate_right(added);
    }

    /// Append the close of what is held to `events`, pedals up then note
    /// offs, as many as its capacity takes, and forget those. Returns how
    /// many. Allocates nothing.
    pub(crate) fn drain_into(&mut self, events: &mut Vec<Event>) -> usize {
        let start = events.len();
        let room = |e: &Vec<Event>| e.len() < e.capacity();
        for lane in Lane::all() {
            if !self.pedals.contains(lane) {
                continue;
            }
            if !room(events) {
                break;
            }
            events.extend(pedal_up(lane).map(Self::event));
            self.pedals.remove(lane);
        }
        let mut next = 0;
        while let Some(key) = self.keys.next_from(next) {
            if !room(events) {
                break;
            }
            next = u32::from(key.index()) + 1;
            self.keys.remove(key);
            events.push(Self::event(Ump::note_off(key)));
        }
        events.len() - start
    }

    fn event(ump: Ump) -> Event {
        Event {
            offset: Offset::ZERO,
            kind: EventKind::Midi(ump),
        }
    }

    /// The messages that close everything held: pedals up, then every note
    /// off, at offset 0 (ties keep this order).
    pub(crate) fn close_into(&self, out: &mut Vec<Event>) {
        self.for_each_close(|e| out.push(e));
    }

    fn for_each_close(&self, mut f: impl FnMut(Event)) {
        for lane in self.pedals.iter() {
            if let Some(up) = pedal_up(lane) {
                f(Self::event(up));
            }
        }
        for key in self.keys.iter() {
            f(Self::event(Ump::note_off(key)));
        }
    }
}

/// The sustain-pedal-up that closes `lane`'s pedal.
fn pedal_up(lane: Lane) -> Option<Ump> {
    Ump::sustain_up(lane.protocol(), lane.address())
}

/// One table per event output port of a unit; `None` for a port that carries
/// no notes.
pub(crate) type Tables = Vec<Option<Held>>;

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
        let fill = |t: &mut Tables| {
            t.resize_with(ports, || None);
            if notes {
                t.iter_mut().for_each(|h| {
                    h.get_or_insert_with(Held::new);
                });
            }
        };
        fill(&mut self.held);
        // `owed` for every unit that carries notes: a bypass owes what the
        // unit held when it parks (`owe_held`).
        fill(&mut self.owed);
        if waiting {
            fill(&mut self.staged);
        }
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
                !Held::any_key(events),
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
    pub(crate) fn close_port(&mut self, port: usize, events: &mut Vec<Event>) {
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
    pub(crate) fn depart(&mut self, spare: &mut Vec<Tables>) -> Tables {
        let mut gone = std::mem::take(&mut self.held);
        absorb(&mut gone, &mut self.owed);
        spare.push(std::mem::take(&mut self.staged));
        spare.push(std::mem::take(&mut self.owed));
        self.closing = false;
        gone
    }
}
