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

use crate::exec::arena::{Arena, Line};
use crate::exec::slots::{ArenaView, EventSlot, EventsView, SharedFlags, SlotFlags};
use crate::exec::slots::{AudioSlots, EventSlots};

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

/// How a participant borrows what an op touches.
pub(super) trait Mode<F: Float> {
    /// A table of `T`.
    type Table<'c, T: 'c>: Table<T>;
    /// The audio slots and flags a participant holds.
    type Audio<'c>;
    /// The event slots a participant holds.
    type Events<'c>;
    /// The audio slots, for one op.
    type AudioSlots<'g>: AudioSlots<F>;
    /// The per-slot flags.
    type Flags<'c>: SlotFlags + ?Sized;
    /// The event slots, for one op.
    type EventSlots<'g>: EventSlots;

    /// The audio slots for one op, and the flags.
    fn audio<'g, 'c: 'g>(
        a: &'g mut Self::Audio<'c>,
    ) -> (Self::AudioSlots<'g>, &'g mut Self::Flags<'c>);
    /// The event slots for one op.
    fn events<'g, 'c: 'g>(e: &'g mut Self::Events<'c>) -> Self::EventSlots<'g>;
}

/// Borrows through claims: a block spread over a pool.
pub(super) struct Claimed;

impl<F: Float> Mode<F> for Claimed {
    type Table<'c, T: 'c> = &'c SplitMut<'c, T>;
    type Audio<'c> = ClaimedAudio<'c>;
    type Events<'c> = ClaimedEvents<'c>;
    type AudioSlots<'g> = ArenaView<'g>;
    type Flags<'c> = SharedFlags<'c>;
    type EventSlots<'g> = EventsView<'g>;

    #[inline]
    fn audio<'g, 'c: 'g>(a: &'g mut ClaimedAudio<'c>) -> (ArenaView<'g>, &'g mut SharedFlags<'c>) {
        (
            ArenaView {
                view: a.split.view(a.held),
            },
            &mut a.flags,
        )
    }

    #[inline]
    fn events<'g, 'c: 'g>(e: &'g mut ClaimedEvents<'c>) -> EventsView<'g> {
        EventsView {
            view: e.split.view(e.held),
        }
    }
}

/// One participant's claimed audio slots, with the flags every participant
/// shares.
pub(super) struct ClaimedAudio<'c> {
    pub(super) split: &'c SplitRw<'c, Line>,
    /// The slots one op's view holds: this participant's.
    pub(super) held: &'c Held,
    pub(super) flags: SharedFlags<'c>,
}

/// One participant's claimed event slots.
pub(super) struct ClaimedEvents<'c> {
    pub(super) split: &'c SplitRw<'c, EventSlot>,
    /// The slots one op's view holds: this participant's.
    pub(super) held: &'c Held,
}

/// Borrows through plain `&mut` slices: the calling thread alone.
pub(super) struct Exclusive;

impl<F: Float> Mode<F> for Exclusive {
    type Table<'c, T: 'c> = &'c mut [T];
    type Audio<'c> = ExclusiveAudio<'c, F>;
    type Events<'c> = &'c mut [EventSlot];
    type AudioSlots<'g> = &'g mut Arena<F>;
    type Flags<'c> = [crate::exec::Flags];
    type EventSlots<'g> = &'g mut [EventSlot];

    #[inline(always)]
    fn audio<'g, 'c: 'g>(
        a: &'g mut ExclusiveAudio<'c, F>,
    ) -> (&'g mut Arena<F>, &'g mut [crate::exec::Flags]) {
        (&mut *a.arena, &mut *a.flags)
    }

    #[inline(always)]
    fn events<'g, 'c: 'g>(e: &'g mut &'c mut [EventSlot]) -> &'g mut [EventSlot] {
        e
    }
}

/// The executor's own audio slots and flags.
pub(super) struct ExclusiveAudio<'c, F: Float> {
    pub(super) arena: &'c mut Arena<F>,
    pub(super) flags: &'c mut [crate::exec::Flags],
}
