//! How a participant reaches the state its ops touch.
//!
//! One op code (`Ctx::run_op`) serves both ways of running a block, and the
//! [`Mode`] is the only difference: [`Claimed`] borrows each element through
//! a claim word (`tutti_rt::{SplitMut, SplitRw}`), so participants on several
//! threads cannot alias; [`Exclusive`] borrows through plain `&mut` slices,
//! for the calling thread running a block alone, with no claim to take.

use std::ops::{DerefMut, Range};

use tutti_buffers::Float;
use tutti_rt::{ClaimedRun, Held, SplitMut, SplitRw};

use crate::arena::{Arena, Line};
use crate::slots::{ArenaView, EventSlot, EventsView, SharedFlags, SlotFlags};
use crate::slots::{AudioSlots, EventSlots};

/// A table of `T` an op borrows one element or one run of.
pub(super) trait Table<T> {
    /// One element, held until it drops.
    type One<'g>: DerefMut<Target = T>
    where
        Self: 'g;
    /// A run of elements, held until it drops.
    type Run<'g>: DerefMut<Target = [T]>
    where
        Self: 'g;

    /// Element `i`.
    fn one(&mut self, i: usize) -> Self::One<'_>;
    /// Elements `range`.
    fn run(&mut self, range: Range<usize>) -> Self::Run<'_>;
}

impl<'c, T> Table<T> for &'c SplitMut<'c, T> {
    type One<'g>
        = tutti_rt::Claimed<'g, T>
    where
        Self: 'g;
    type Run<'g>
        = ClaimedRun<'g, T>
    where
        Self: 'g;

    #[inline]
    fn one(&mut self, i: usize) -> Self::One<'_> {
        self.claim(i)
    }
    #[inline]
    fn run(&mut self, range: Range<usize>) -> Self::Run<'_> {
        self.claim_run(range)
    }
}

impl<T> Table<T> for &mut [T] {
    type One<'g>
        = &'g mut T
    where
        Self: 'g;
    type Run<'g>
        = &'g mut [T]
    where
        Self: 'g;

    #[inline(always)]
    fn one(&mut self, i: usize) -> Self::One<'_> {
        &mut self[i]
    }
    #[inline(always)]
    fn run(&mut self, range: Range<usize>) -> Self::Run<'_> {
        &mut self[range]
    }
}

/// The audio slots and their flags an op borrows.
pub(super) trait Audio<F: Float> {
    /// The slots, for one op.
    type Slots<'g>: AudioSlots<F>
    where
        Self: 'g;
    /// The per-slot flags.
    type Flags: SlotFlags + ?Sized;

    /// The slots for one op, and the flags.
    fn parts(&mut self) -> (Self::Slots<'_>, &mut Self::Flags);
}

/// The event slots an op borrows.
pub(super) trait Events {
    /// The slots, for one op.
    type Slots<'g>: EventSlots
    where
        Self: 'g;

    /// The slots for one op.
    fn slots(&mut self) -> Self::Slots<'_>;
}

/// How a participant borrows what an op touches.
pub(super) trait Mode<F: Float> {
    /// A table of `T`.
    type Table<'c, T: 'c>: Table<T>;
    /// The audio slots and flags.
    type Audio<'c>: Audio<F>;
    /// The event slots.
    type Events<'c>: Events;
}

/// Borrows through claims: a block spread over a pool.
pub(super) struct Claimed;

impl<F: Float> Mode<F> for Claimed {
    type Table<'c, T: 'c> = &'c SplitMut<'c, T>;
    type Audio<'c> = ClaimedAudio<'c>;
    type Events<'c> = ClaimedEvents<'c>;
}

/// One participant's claimed audio slots, with the flags every participant
/// shares.
pub(super) struct ClaimedAudio<'c> {
    pub(super) split: &'c SplitRw<'c, Line>,
    /// The slots one op's view holds: this participant's.
    pub(super) held: &'c Held,
    pub(super) flags: SharedFlags<'c>,
}

impl<'c, F: Float> Audio<F> for ClaimedAudio<'c> {
    type Slots<'g>
        = ArenaView<'g>
    where
        Self: 'g;
    type Flags = SharedFlags<'c>;

    #[inline]
    fn parts(&mut self) -> (ArenaView<'_>, &mut SharedFlags<'c>) {
        (
            ArenaView {
                view: self.split.view(self.held),
            },
            &mut self.flags,
        )
    }
}

/// One participant's claimed event slots.
pub(super) struct ClaimedEvents<'c> {
    pub(super) split: &'c SplitRw<'c, EventSlot>,
    /// The slots one op's view holds: this participant's.
    pub(super) held: &'c Held,
}

impl Events for ClaimedEvents<'_> {
    type Slots<'g>
        = EventsView<'g>
    where
        Self: 'g;

    #[inline]
    fn slots(&mut self) -> EventsView<'_> {
        EventsView {
            view: self.split.view(self.held),
        }
    }
}

/// Borrows through plain `&mut` slices: the calling thread alone.
pub(super) struct Exclusive;

impl<F: Float> Mode<F> for Exclusive {
    type Table<'c, T: 'c> = &'c mut [T];
    type Audio<'c> = ExclusiveAudio<'c, F>;
    type Events<'c> = &'c mut [EventSlot];
}

/// The executor's own audio slots and flags.
pub(super) struct ExclusiveAudio<'c, F: Float> {
    pub(super) arena: &'c mut Arena<F>,
    pub(super) flags: &'c mut [u8],
}

impl<F: Float> Audio<F> for ExclusiveAudio<'_, F> {
    type Slots<'g>
        = &'g mut Arena<F>
    where
        Self: 'g;
    type Flags = [u8];

    #[inline(always)]
    fn parts(&mut self) -> (&mut Arena<F>, &mut [u8]) {
        (&mut *self.arena, &mut *self.flags)
    }
}

impl Events for &mut [EventSlot] {
    type Slots<'g>
        = &'g mut [EventSlot]
    where
        Self: 'g;

    #[inline(always)]
    fn slots(&mut self) -> &mut [EventSlot] {
        self
    }
}
