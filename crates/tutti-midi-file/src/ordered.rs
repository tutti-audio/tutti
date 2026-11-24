//! [`Ordered`]: the run of `(Tick, event)` pairs every file part holds, in tick
//! order, with the tick it ends at.

use crate::{OutOfOrder, Tick};

/// Events in tick order, each at an absolute [`Tick`], and where the run ends.
/// The run is the body of a Standard MIDI File's track, of a Clip File's
/// sequence and of its configuration header: each part wraps one and shows the
/// builders its own kind has.
///
/// The ticks do not decrease and the end is never before the last event, so a
/// run is ordered by construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ordered<E> {
    events: Vec<(Tick, E)>,
    end: Tick,
}

impl<E> Default for Ordered<E> {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            end: Tick(0),
        }
    }
}

impl<E> Ordered<E> {
    /// A run a decoder or converter assembled: in order by construction (each
    /// tick is the sum of the deltas before it, and `end` is at or after the
    /// last event). Not checked in release, so only those call it.
    pub(crate) fn from_read(events: Vec<(Tick, E)>, end: Tick) -> Self {
        debug_assert!(events.is_sorted_by_key(|&(tick, _)| tick));
        debug_assert!(events.last().is_none_or(|&(tick, _)| tick <= end));
        Self { events, end }
    }

    pub(crate) fn events(&self) -> &[(Tick, E)] {
        &self.events
    }

    pub(crate) fn end(&self) -> Tick {
        self.end
    }

    /// Places `event` at `tick`, after the events already at that tick, and
    /// moves the end to `tick` if it is past it.
    pub(crate) fn place(&mut self, tick: Tick, event: E) {
        let at = self.events.partition_point(|&(t, _)| t <= tick);
        self.events.insert(at, (tick, event));
        self.end = self.end.max(tick);
    }

    /// Inserts `event` at `tick` as the event number `index`, which keeps the
    /// order: the event before it is not later and the event after it is not
    /// earlier.
    pub(crate) fn insert_at(&mut self, index: usize, tick: Tick, event: E) {
        debug_assert!(
            index
                .checked_sub(1)
                .is_none_or(|i| self.events[i].0 <= tick)
        );
        debug_assert!(self.events.get(index).is_none_or(|&(t, _)| tick <= t));
        self.events.insert(index, (tick, event));
        self.end = self.end.max(tick);
    }

    /// Appends `event` at `tick`, which is not before the last event's.
    pub(crate) fn push(&mut self, tick: Tick, event: E) -> Result<(), OutOfOrder> {
        if let Some(&(last, _)) = self.events.last()
            && tick < last
        {
            return Err(OutOfOrder { last, at: tick });
        }
        self.events.push((tick, event));
        self.end = self.end.max(tick);
        Ok(())
    }

    /// The run ends at `end`, or where it ends already if that is later.
    pub(crate) fn end_at_least(&mut self, end: Tick) {
        self.end = self.end.max(end);
    }

    /// Adds the events, then puts the run in tick order (a stable sort, so
    /// events at one tick stay in the order added) and moves its end to the
    /// last event's if that is past it.
    pub(crate) fn extend(&mut self, events: impl IntoIterator<Item = (Tick, E)>) {
        self.events.extend(events);
        self.events.sort_by_key(|&(tick, _)| tick);
        if let Some(&(last, _)) = self.events.last() {
            self.end = self.end.max(last);
        }
    }
}
