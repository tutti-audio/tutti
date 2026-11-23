//! [`SortedEvents`]: the events of one port, sorted and inside the block, and
//! [`SubBlocks`], which splits the block at their offsets.

use tutti_units::Samples;

use crate::event::types::Event;
use crate::node::param_event::ParamEvent;

/// Why a slice is not a valid [`SortedEvents`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EventOrderError {
    /// The event at this index has a smaller offset than the one before it.
    #[error("event {at} is out of order: its offset is smaller than that of the event before it")]
    Unsorted {
        /// Index into the slice.
        at: usize,
    },
    /// The event at this index is at or past the end of the block.
    #[error("event {at} is at or past the end of the block")]
    OutOfBlock {
        /// Index into the slice.
        at: usize,
    },
}

/// A block's events for one port: **sorted by offset, every offset inside the
/// block** — by construction.
///
/// It holds the port's [`Event`]s (MIDI and the rest: what iterating it, its
/// `len` and `is_empty` and its deref to a slice are about) and, beside them,
/// its [`ParamEvent`]s ([`params`](Self::params)), each list sorted
/// separately. [`sub_blocks`](crate::Io::sub_blocks) cuts the block at both.
///
/// The only ways to get one are [`new`](Self::new), which checks,
/// [`sort`](Self::sort), which sorts (stably, so equal offsets keep their
/// order; control side — the sort may allocate), and the executor, whose event
/// slots are sorted and in-block by the rules of [`EventWriter`](crate::EventWriter) and the
/// merge. A node reading a `SortedEvents` never re-checks either property.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SortedEvents<'a> {
    events: &'a [Event],
    params: &'a [ParamEvent],
}

impl<'a> SortedEvents<'a> {
    /// No events.
    pub const EMPTY: SortedEvents<'static> = SortedEvents {
        events: &[],
        params: &[],
    };

    /// `events`, if they are sorted by offset and every offset is below
    /// `frames`.
    ///
    /// # Errors
    ///
    /// [`EventOrderError::OutOfBlock`] or [`EventOrderError::Unsorted`],
    /// naming the first offending index.
    pub fn new(events: &'a [Event], frames: usize) -> Result<Self, EventOrderError> {
        for (i, e) in events.iter().enumerate() {
            if e.offset.index() >= frames {
                return Err(EventOrderError::OutOfBlock { at: i });
            }
            if i > 0 && events[i - 1].offset > e.offset {
                return Err(EventOrderError::Unsorted { at: i });
            }
        }
        Ok(Self {
            events,
            params: &[],
        })
    }

    /// `events` with the param events `params` beside them, if each list is
    /// sorted by offset and every offset is below `frames`.
    ///
    /// # Errors
    ///
    /// [`EventOrderError::OutOfBlock`] or [`EventOrderError::Unsorted`],
    /// naming the first offending index of the list at fault (the events
    /// first).
    pub fn with_both(
        events: &'a [Event],
        params: &'a [ParamEvent],
        frames: usize,
    ) -> Result<Self, EventOrderError> {
        let this = Self::new(events, frames)?;
        for (i, e) in params.iter().enumerate() {
            if e.offset.index() >= frames {
                return Err(EventOrderError::OutOfBlock { at: i });
            }
            if i > 0 && params[i - 1].offset > e.offset {
                return Err(EventOrderError::Unsorted { at: i });
            }
        }
        Ok(Self { params, ..this })
    }

    /// Sorts `events` in place by offset (stably), then views them. Fails only
    /// when an offset is outside the block. Control side: may allocate.
    ///
    /// # Errors
    ///
    /// [`EventOrderError::OutOfBlock`], naming the first event at or past
    /// `frames`; `events` is left unsorted.
    pub fn sort(events: &'a mut [Event], frames: usize) -> Result<Self, EventOrderError> {
        if let Some(at) = events.iter().position(|e| e.offset.index() >= frames) {
            return Err(EventOrderError::OutOfBlock { at });
        }
        events.sort_by_key(|e| e.offset);
        Ok(Self {
            events,
            params: &[],
        })
    }

    /// Wrap slices the executor's own rules keep sorted and in-block.
    /// Checked in debug builds.
    pub(crate) fn trusted(events: &'a [Event], frames: usize) -> Self {
        debug_assert_eq!(Self::new(events, frames).map(|_| ()), Ok(()));
        Self {
            events,
            params: &[],
        }
    }

    /// These events with `params` beside them, which the executor's own rules
    /// keep sorted and in-block. Checked in debug builds.
    pub(crate) fn trusted_params(self, params: &'a [ParamEvent], frames: usize) -> Self {
        debug_assert_eq!(
            Self::with_both(self.events, params, frames).map(|_| ()),
            Ok(())
        );
        Self { params, ..self }
    }

    /// The events.
    #[must_use]
    pub fn as_slice(&self) -> &'a [Event] {
        self.events
    }

    /// The param events (ramps and segments) of the port, sorted by offset.
    #[must_use]
    pub fn params(&self) -> &'a [ParamEvent] {
        self.params
    }

    /// Number of events, the param events not counted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether there are no events. The param events are
    /// [`params`](Self::params), and not counted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The events, in order.
    pub fn iter(&self) -> std::slice::Iter<'a, Event> {
        self.events.iter()
    }
}

/// A block split at its event offsets: the iterator
/// [`Io::sub_blocks`](crate::Io::sub_blocks) returns.
///
/// Yields `(range, events)` pairs that tile the block in order: `range` is a
/// run of frames (slice indices into the block's buffers), and `events` are
/// exactly the events, and the param events, at `range.start` — so a node that
/// applies `events`, then renders `range`, applies every event on its own
/// frame. A block with no events is one chunk; several events on one offset
/// arrive together, in their delivered order; an event on the last frame gets
/// a one-frame chunk.
///
/// Zero allocation: it walks the event slices it was built from.
#[derive(Clone, Debug)]
pub struct SubBlocks<'a> {
    events: &'a [Event],
    params: &'a [ParamEvent],
    pos: usize,
    frames: usize,
}

impl<'a> SortedEvents<'a> {
    /// Split a block of `frames` at these events' offsets. `frames` is the
    /// length these events were checked against.
    pub(crate) fn sub_blocks(self, frames: Samples) -> SubBlocks<'a> {
        SubBlocks {
            events: self.events,
            params: self.params,
            pos: 0,
            frames: frames.get(),
        }
    }
}

impl<'a> Iterator for SubBlocks<'a> {
    type Item = (std::ops::Range<usize>, SortedEvents<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.frames {
            return None;
        }
        // Every event before `pos` was yielded with an earlier chunk, so the
        // ones at `pos` are the prefix; `<=` rather than `==` only so a
        // malformed slice could never stall the walk.
        let here = self
            .events
            .iter()
            .take_while(|e| e.offset.index() <= self.pos)
            .count();
        let here_params = self
            .params
            .iter()
            .take_while(|e| e.offset.index() <= self.pos)
            .count();
        let (now, rest) = self.events.split_at(here);
        let (now_params, rest_params) = self.params.split_at(here_params);
        let next = [
            rest.first().map(|e| e.offset.index()),
            rest_params.first().map(|e| e.offset.index()),
        ]
        .into_iter()
        .flatten()
        .min();
        let end = next.unwrap_or(self.frames).min(self.frames);
        let range = self.pos..end;
        self.events = rest;
        self.params = rest_params;
        self.pos = end;
        Some((
            range,
            SortedEvents {
                events: now,
                params: now_params,
            },
        ))
    }
}

impl std::iter::FusedIterator for SubBlocks<'_> {}

impl<'a> IntoIterator for SortedEvents<'a> {
    type Item = &'a Event;
    type IntoIter = std::slice::Iter<'a, Event>;
    fn into_iter(self) -> Self::IntoIter {
        self.events.iter()
    }
}

impl std::ops::Deref for SortedEvents<'_> {
    type Target = [Event];
    fn deref(&self) -> &[Event] {
        self.events
    }
}
