//! The buffers an op touches, behind two small traits, so one body of op
//! code runs on both ways of running a block: a block on the calling thread
//! alone borrows straight out of the [`Arena`] and the event slots
//! (`split_at_mut`, as `arena.rs` explains), and one spread over a pool out of a per-op view of them whose every slot is
//! **claimed** before use (`tutti_rt::SplitRw`), so two ops the schedule
//! wrongly runs at once panic on the slot they share instead of aliasing
//! it. Neither needs `unsafe` here.
//!
//! The per-slot silence/constant flags go behind a third trait,
//! [`SlotFlags`]: plain bytes alone, shared atomics on a pool.

use std::sync::atomic::{AtomicU8, Ordering};

use tutti_buffers::Float;
use tutti_rt::RwView;

use crate::compile::plan::Direct;
use crate::event::types::Event;
use crate::exec::arena::{Arena, Line, Role, borrow_sorted};
use crate::node::param_event::ParamEvent;

/// The per-slot silence/constant flags an op reads and writes: a plain
/// byte slice for a block alone, and the shared atomics
/// ([`SharedFlags`]) of one spread over a pool. Two impls rather than atomics everywhere: a
/// `Relaxed` atomic is a plain byte move, but the compiler may not forward
/// a store to a later load through it, and on a chain of cheap nodes that
/// measured about 2.5 ns a node.
pub(crate) trait SlotFlags {
    /// Slot `s`'s flags.
    fn get(&self, s: u32) -> u8;
    /// Set slot `s`'s flags.
    fn set(&mut self, s: u32, v: u8);
}

impl SlotFlags for [u8] {
    #[inline(always)]
    fn get(&self, s: u32) -> u8 {
        self[s as usize]
    }
    #[inline(always)]
    fn set(&mut self, s: u32, v: u8) {
        self[s as usize] = v;
    }
}

/// The flags every worker of a parallel block shares. A slot's flag is
/// written by the op that writes the slot and read by the ops that read it,
/// so the slot's claim orders its flag like its samples.
pub(crate) struct SharedFlags<'a>(pub(crate) &'a [AtomicU8]);

impl SlotFlags for SharedFlags<'_> {
    #[inline]
    fn get(&self, s: u32) -> u8 {
        self.0[s as usize].load(Ordering::Relaxed)
    }
    #[inline]
    fn set(&mut self, s: u32, v: u8) {
        self.0[s as usize].store(v, Ordering::Relaxed);
    }
}

/// The audio slots an op borrows: [`Arena`]'s own methods, by name.
pub(crate) trait AudioSlots<F: Float> {
    /// Slot `s`'s first `frames` samples.
    fn slot(&self, s: u32, frames: usize) -> &[F];
    /// Slot `s`'s first `frames` samples, mutably.
    fn slot_mut(&mut self, s: u32, frames: usize) -> &mut [F];
    /// `src` for reading and `dst` for writing, at once. They must differ.
    fn pair(&mut self, src: u32, dst: u32, frames: usize) -> (&[F], &mut [F]);
    /// A [`Direct`] node's buffers (see [`Arena::direct`]).
    fn direct<const I: usize, const O: usize>(
        &mut self,
        frames: usize,
        d: &Direct,
    ) -> ([&[F]; I], [&mut [F]; O]);
    /// A node's buffers by its sorted borrow requests (see
    /// [`Arena::borrow`]).
    fn borrow<'a>(
        &'a mut self,
        frames: usize,
        reqs: &[(u32, Role)],
        ins: &mut [&'a [F]],
        outs: &mut [&'a mut [F]],
    );
    /// Copy slot `src` over slot `dst`.
    fn copy_slot(&mut self, src: u32, dst: u32);
}

impl<F: Float> AudioSlots<F> for Arena<F> {
    #[inline(always)]
    fn slot(&self, s: u32, frames: usize) -> &[F] {
        Arena::slot(self, s, frames)
    }
    #[inline(always)]
    fn slot_mut(&mut self, s: u32, frames: usize) -> &mut [F] {
        Arena::slot_mut(self, s, frames)
    }
    #[inline(always)]
    fn pair(&mut self, src: u32, dst: u32, frames: usize) -> (&[F], &mut [F]) {
        Arena::pair(self, src, dst, frames)
    }
    #[inline(always)]
    fn direct<const I: usize, const O: usize>(
        &mut self,
        frames: usize,
        d: &Direct,
    ) -> ([&[F]; I], [&mut [F]; O]) {
        Arena::direct(self, frames, d)
    }
    #[inline(always)]
    fn borrow<'a>(
        &'a mut self,
        frames: usize,
        reqs: &[(u32, Role)],
        ins: &mut [&'a [F]],
        outs: &mut [&'a mut [F]],
    ) {
        Arena::borrow(self, frames, reqs, ins, outs)
    }
    #[inline(always)]
    fn copy_slot(&mut self, src: u32, dst: u32) {
        Arena::copy_slot(self, src, dst)
    }
}

impl<F: Float, A: AudioSlots<F> + ?Sized> AudioSlots<F> for &mut A {
    #[inline(always)]
    fn slot(&self, s: u32, frames: usize) -> &[F] {
        (**self).slot(s, frames)
    }
    #[inline(always)]
    fn slot_mut(&mut self, s: u32, frames: usize) -> &mut [F] {
        (**self).slot_mut(s, frames)
    }
    #[inline(always)]
    fn pair(&mut self, src: u32, dst: u32, frames: usize) -> (&[F], &mut [F]) {
        (**self).pair(src, dst, frames)
    }
    #[inline(always)]
    fn direct<const I: usize, const O: usize>(
        &mut self,
        frames: usize,
        d: &Direct,
    ) -> ([&[F]; I], [&mut [F]; O]) {
        (**self).direct(frames, d)
    }
    #[inline(always)]
    fn borrow<'a>(
        &'a mut self,
        frames: usize,
        reqs: &[(u32, Role)],
        ins: &mut [&'a [F]],
        outs: &mut [&'a mut [F]],
    ) {
        (**self).borrow(frames, reqs, ins, outs);
    }
    #[inline(always)]
    fn copy_slot(&mut self, src: u32, dst: u32) {
        (**self).copy_slot(src, dst);
    }
}

/// One event slot: the events a port holds in a block, and the param events
/// beside them (empty of room in a slot no param-writing node reaches).
#[derive(Debug, Default)]
pub(crate) struct EventSlot {
    pub(crate) events: Vec<Event>,
    pub(crate) params: Vec<ParamEvent>,
}

impl EventSlot {
    /// A slot with room for `events` events and `params` param events.
    /// Control side: allocates.
    pub(crate) fn with_capacity(events: usize, params: usize) -> Self {
        Self {
            events: Vec::with_capacity(events),
            params: Vec::with_capacity(params),
        }
    }

    /// Empties both lists; their room is kept.
    pub(crate) fn clear(&mut self) {
        self.events.clear();
        self.params.clear();
    }
}

/// The event slots an op borrows.
pub(crate) trait EventSlots {
    /// Slot `s`'s events.
    fn ev(&self, s: u32) -> &Vec<Event>;
    /// Slot `s`'s events, mutably.
    fn ev_mut(&mut self, s: u32) -> &mut Vec<Event>;
    /// Slot `s`'s param events.
    fn pev(&self, s: u32) -> &Vec<ParamEvent>;
    /// Slot `s`.
    fn slot(&self, s: u32) -> &EventSlot;
    /// Slot `s`, mutably.
    fn slot_mut(&mut self, s: u32) -> &mut EventSlot;
    /// Several slots at once, by requests sorted by slot (see
    /// `arena::borrow_sorted`).
    fn borrow_sorted<'a>(
        &'a mut self,
        reqs: &[(u32, Role)],
        read: impl FnMut(u8, &'a EventSlot),
        write: impl FnMut(u8, &'a mut EventSlot),
    );
}

impl EventSlots for [EventSlot] {
    #[inline(always)]
    fn ev(&self, s: u32) -> &Vec<Event> {
        &self[s as usize].events
    }
    #[inline(always)]
    fn ev_mut(&mut self, s: u32) -> &mut Vec<Event> {
        &mut self[s as usize].events
    }
    #[inline(always)]
    fn pev(&self, s: u32) -> &Vec<ParamEvent> {
        &self[s as usize].params
    }
    #[inline(always)]
    fn slot(&self, s: u32) -> &EventSlot {
        &self[s as usize]
    }
    #[inline(always)]
    fn slot_mut(&mut self, s: u32) -> &mut EventSlot {
        &mut self[s as usize]
    }
    #[inline(always)]
    fn borrow_sorted<'a>(
        &'a mut self,
        reqs: &[(u32, Role)],
        mut read: impl FnMut(u8, &'a EventSlot),
        mut write: impl FnMut(u8, &'a mut EventSlot),
    ) {
        borrow_sorted(
            self,
            1,
            reqs,
            |port, v| read(port, &v[0]),
            |port, v| write(port, &mut v[0]),
        );
    }
}

impl<E: EventSlots + ?Sized> EventSlots for &mut E {
    #[inline(always)]
    fn ev(&self, s: u32) -> &Vec<Event> {
        (**self).ev(s)
    }
    #[inline(always)]
    fn ev_mut(&mut self, s: u32) -> &mut Vec<Event> {
        (**self).ev_mut(s)
    }
    #[inline(always)]
    fn pev(&self, s: u32) -> &Vec<ParamEvent> {
        (**self).pev(s)
    }
    #[inline(always)]
    fn slot(&self, s: u32) -> &EventSlot {
        (**self).slot(s)
    }
    #[inline(always)]
    fn slot_mut(&mut self, s: u32) -> &mut EventSlot {
        (**self).slot_mut(s)
    }
    #[inline(always)]
    fn borrow_sorted<'a>(
        &'a mut self,
        reqs: &[(u32, Role)],
        read: impl FnMut(u8, &'a EventSlot),
        write: impl FnMut(u8, &'a mut EventSlot),
    ) {
        (**self).borrow_sorted(reqs, read, write);
    }
}

/// [`EventSlots::borrow_sorted`] for requests built per call: sorted first.
#[inline]
pub(crate) fn borrow_events<'a, E: EventSlots + ?Sized>(
    events: &'a mut E,
    reqs: &mut [(u32, Role)],
    read: impl FnMut(u8, &'a EventSlot),
    write: impl FnMut(u8, &'a mut EventSlot),
) {
    reqs.sort_unstable();
    events.borrow_sorted(reqs, read, write);
}

/// Event slot `src` for reading and `dst` for writing, at once.
pub(crate) fn event_pair<E: EventSlots + ?Sized>(
    events: &mut E,
    src: u32,
    dst: u32,
) -> (&EventSlot, &mut EventSlot) {
    let mut reqs = [(src, Role::Read(0)), (dst, Role::Write(0))];
    let mut input: Option<&EventSlot> = None;
    let mut output: Option<&mut EventSlot> = None;
    borrow_events(
        events,
        &mut reqs,
        |_, v| input = Some(v),
        |_, v| output = Some(v),
    );
    (input.expect("src borrowed"), output.expect("dst borrowed"))
}

fn is_write(r: Role) -> bool {
    matches!(r, Role::Write(_))
}

/// One op's claimed view of the audio arena, for the parallel executor.
pub(crate) struct ArenaView<'v> {
    pub(crate) view: RwView<'v, Line>,
}

#[inline]
fn samples<F: Float>(lines: &[Line], frames: usize) -> &[F] {
    &bytemuck::cast_slice::<Line, F>(lines)[..frames]
}

#[inline]
fn samples_mut<F: Float>(lines: &mut [Line], frames: usize) -> &mut [F] {
    &mut bytemuck::cast_slice_mut::<Line, F>(lines)[..frames]
}

impl<F: Float> AudioSlots<F> for ArenaView<'_> {
    #[inline]
    fn slot(&self, s: u32, frames: usize) -> &[F] {
        samples(self.view.get(s as usize), frames)
    }
    #[inline]
    fn slot_mut(&mut self, s: u32, frames: usize) -> &mut [F] {
        samples_mut(self.view.get_mut(s as usize), frames)
    }
    fn pair(&mut self, src: u32, dst: u32, frames: usize) -> (&[F], &mut [F]) {
        assert_ne!(src, dst, "pair() of one slot");
        let mut s: &[F] = &[];
        let mut d: Option<&mut [F]> = None;
        let reqs = if src < dst {
            [(src, Role::Read(0)), (dst, Role::Write(0))]
        } else {
            [(dst, Role::Write(0)), (src, Role::Read(0))]
        };
        self.view.split(
            &reqs,
            is_write,
            |_, l| s = samples(l, frames),
            |_, l| d = Some(samples_mut(l, frames)),
        );
        (s, d.expect("dst borrowed"))
    }
    fn direct<const I: usize, const O: usize>(
        &mut self,
        frames: usize,
        d: &Direct,
    ) -> ([&[F]; I], [&mut [F]; O]) {
        let n = usize::from(d.count);
        let mut reqs = [(0u32, 0u8); 4];
        for (k, r) in reqs.iter_mut().enumerate().take(n) {
            *r = (d.slots[k], d.role[k]);
        }
        let mut reads: [&[F]; 4] = [&[]; 4];
        let mut outs: [&mut [F]; O] = std::array::from_fn(|_| &mut [][..]);
        // Tag each request with its index into `slots`, so the reads land
        // where `d.input` looks for them.
        let mut tagged = [(0u32, (0u8, 0u8)); 4];
        for (k, t) in tagged.iter_mut().enumerate().take(n) {
            *t = (reqs[k].0, (k as u8, reqs[k].1));
        }
        self.view.split(
            &tagged[..n],
            |(_, role)| role != Direct::READ,
            |(k, _), l| reads[usize::from(k)] = samples(l, frames),
            |(_, role), l| outs[usize::from(role)] = samples_mut(l, frames),
        );
        let ins = std::array::from_fn(|c| match d.input[c] {
            Direct::IN_PLACE => &[][..],
            r => {
                assert_eq!(
                    d.role[usize::from(r)],
                    Direct::READ,
                    "a direct input reads a read slot"
                );
                reads[usize::from(r)]
            }
        });
        (ins, outs)
    }
    fn borrow<'a>(
        &'a mut self,
        frames: usize,
        reqs: &[(u32, Role)],
        ins: &mut [&'a [F]],
        outs: &mut [&'a mut [F]],
    ) {
        self.view.split(
            reqs,
            is_write,
            |r, l| {
                if let Role::Read(port) = r {
                    ins[port as usize] = samples(l, frames);
                }
            },
            |r, l| {
                if let Role::Write(port) = r {
                    outs[port as usize] = samples_mut(l, frames);
                }
            },
        );
    }
    fn copy_slot(&mut self, src: u32, dst: u32) {
        if src == dst {
            return;
        }
        let mut s: &[Line] = &[];
        let mut d: Option<&mut [Line]> = None;
        let reqs = if src < dst {
            [(src, Role::Read(0)), (dst, Role::Write(0))]
        } else {
            [(dst, Role::Write(0)), (src, Role::Read(0))]
        };
        self.view
            .split(&reqs, is_write, |_, l| s = l, |_, l| d = Some(l));
        d.expect("dst borrowed").copy_from_slice(s);
    }
}

/// One op's claimed view of the event slots, for the parallel executor.
pub(crate) struct EventsView<'v> {
    pub(crate) view: RwView<'v, EventSlot>,
}

impl EventSlots for EventsView<'_> {
    #[inline]
    fn ev(&self, s: u32) -> &Vec<Event> {
        &self.view.get(s as usize)[0].events
    }
    #[inline]
    fn ev_mut(&mut self, s: u32) -> &mut Vec<Event> {
        &mut self.view.get_mut(s as usize)[0].events
    }
    #[inline]
    fn pev(&self, s: u32) -> &Vec<ParamEvent> {
        &self.view.get(s as usize)[0].params
    }
    #[inline]
    fn slot(&self, s: u32) -> &EventSlot {
        &self.view.get(s as usize)[0]
    }
    #[inline]
    fn slot_mut(&mut self, s: u32) -> &mut EventSlot {
        &mut self.view.get_mut(s as usize)[0]
    }
    fn borrow_sorted<'a>(
        &'a mut self,
        reqs: &[(u32, Role)],
        mut read: impl FnMut(u8, &'a EventSlot),
        mut write: impl FnMut(u8, &'a mut EventSlot),
    ) {
        self.view.split(
            reqs,
            is_write,
            |r, v| {
                if let Role::Read(port) = r {
                    read(port, &v[0]);
                }
            },
            |r, v| {
                if let Role::Write(port) = r {
                    write(port, &mut v[0]);
                }
            },
        );
    }
}
