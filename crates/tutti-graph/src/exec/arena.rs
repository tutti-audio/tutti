//! The audio arena, and the disjoint borrow that hands a node its buffers —
//! without `unsafe`.
//!
//! One `Vec` of 64-byte [`Line`]s holds every slot; a slot is `stride` lines,
//! so every channel a node sees starts on a cache line and its length is
//! padded to a SIMD multiple (one aligned arena per port kind). The lines are
//! plain bytes: an `Arena<F>` reads them as samples of the sample format `F`.
//!
//! # Why no `unsafe`
//!
//! A node needs `&[F]` inputs and `&mut [F]` outputs **from the same
//! arena at once**. The obvious spelling is raw pointers plus a proof that the
//! colouring kept them disjoint — new non-FFI `unsafe`, which the crate
//! avoids whenever a safe construct will do. One will: [`borrow_sorted`] walks the requested slots in order
//! and peels them off with `split_at_mut`, so the borrow checker, not a
//! comment, proves disjointness. If the colouring were ever wrong and asked
//! for a written slot twice, the result is a panic naming the slot — never
//! aliasing. The reinterpretation of `[Line]` as `[F]` is `bytemuck`'s
//! checked `Pod` cast.
//!
//! **Node calls pay no sort.** A node op's requests are sorted once, when the
//! plan is compiled (`NodeTables::lower` in `plan.rs`). The walk still *checks*
//! the order it relies on: a slot behind the cursor is a panic, in every
//! build, never a wrong buffer. Ops whose requests are built per call (event
//! merges) go through `slots::borrow_events`, which sorts first. The port tables
//! are stack arrays sized by a small set of const buckets (see `exec.rs`), so
//! a two-port node does not initialise 128 entries, and the commonest shapes
//! skip the walk entirely (`plan::Form`).

use bytemuck::{Pod, Zeroable};
use std::marker::PhantomData;
use std::mem::size_of;

use tutti_buffers::Float;

use crate::compile::ids::{AudioSlotId, Id};

/// `f32` samples a line holds: 16, filling its 64 bytes.
pub(crate) const LANES: usize = 16;

/// One cache line of samples: 64 bytes, 16 `f32` or 8 `f64`. The alignment is
/// the type's, so every slot the arena hands out starts 64-byte aligned.
#[derive(Clone, Copy, Pod, Zeroable)]
#[repr(C, align(64))]
pub(crate) struct Line([f32; LANES]);

/// A role in one borrow request.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Role {
    Read(u8),
    Write(u8),
}

/// Borrow the slots named by `reqs` out of `items` (slot `s` is
/// `items[s * stride .. (s + 1) * stride]`), handing each read to `read` and
/// each write to `write` along with its port index. The requests must be
/// **sorted** — by the compiler, for node ops; per call, by
/// `slots::borrow_events`, for event merges.
///
/// Several reads of one slot share it; a written slot may appear only once.
///
/// # Panics
///
/// If a written slot is requested twice, or read and written together —
/// either means the plan is unsound, and the verifier rejects such plans in
/// debug builds — and also if `reqs` is not sorted by slot: a slot
/// behind the cursor fails the `checked_sub` and panics, in every build
/// (a debug build names the order earlier, in a `debug_assert!`). Never a
/// wrong or aliased buffer.
#[inline]
pub(crate) fn borrow_sorted<'a, T, S: Id>(
    items: &'a mut [T],
    stride: usize,
    reqs: &[(S, Role)],
    mut read: impl FnMut(u8, &'a [T]),
    mut write: impl FnMut(u8, &'a mut [T]),
) {
    debug_assert!(
        reqs.windows(2).all(|w| w[0] <= w[1]),
        "unsorted borrow requests"
    );
    let mut rest: &'a mut [T] = items;
    let mut base = 0usize;
    let mut i = 0;
    while i < reqs.len() {
        let slot = reqs[i].0.index();
        let mut j = i + 1;
        while j < reqs.len() && reqs[j].0.index() == slot {
            j += 1;
        }
        let tail = std::mem::take(&mut rest);
        // `checked_sub`: an unsorted list must panic here, not wrap to a
        // huge offset that happens to land on a real slot in release.
        let skip = slot.checked_sub(base).expect("borrow requests are sorted");
        let (_, tail) = tail.split_at_mut(skip * stride);
        let (cur, after) = tail.split_at_mut(stride);
        rest = after;
        base = slot + 1;

        let group = &reqs[i..j];
        if let Some(&(_, Role::Write(port))) = group.iter().find(|r| matches!(r.1, Role::Write(_)))
        {
            assert!(
                group.len() == 1,
                "slot {slot} is written and also borrowed elsewhere in one op"
            );
            write(port, cur);
        } else {
            let shared: &'a [T] = cur;
            for &(_, role) in group {
                if let Role::Read(port) = role {
                    read(port, shared);
                }
            }
        }
        i = j;
    }
}

/// The audio arena: `slots × stride` lines.
pub(crate) struct Arena<F: Float> {
    lines: Vec<Line>,
    /// Lines per slot.
    stride: usize,
    format: PhantomData<fn() -> F>,
}

impl<F: Float> Arena<F> {
    /// An arena of `slots` zeroed slots, each holding at least `max_block`
    /// samples. Control thread.
    pub(crate) fn new(slots: usize, max_block: usize) -> Self {
        let stride = (max_block * size_of::<F>())
            .div_ceil(size_of::<Line>())
            .max(1);
        Self {
            lines: vec![Line::zeroed(); slots * stride],
            stride,
            format: PhantomData,
        }
    }

    /// Slot `s`'s first `frames` samples.
    #[inline]
    pub(crate) fn slot(&self, s: AudioSlotId, frames: usize) -> &[F] {
        // `[start..][..stride]` rather than `[start..end]`: two length checks
        // and no `start <= end` one, on the hottest indexing in the executor.
        let lines = &self.lines[s.index() * self.stride..][..self.stride];
        &bytemuck::cast_slice::<Line, F>(lines)[..frames]
    }

    /// Slot `s`'s first `frames` samples, mutably.
    #[inline]
    pub(crate) fn slot_mut(&mut self, s: AudioSlotId, frames: usize) -> &mut [F] {
        let lines = &mut self.lines[s.index() * self.stride..][..self.stride];
        &mut bytemuck::cast_slice_mut::<Line, F>(lines)[..frames]
    }

    /// A [`Direct`](crate::compile::plan::Direct) node's buffers: `I` inputs (an
    /// in-place one empty) and `O` outputs, peeled off in one ascending pass
    /// over its distinct slots, with no request table and no sort.
    ///
    /// # Panics
    ///
    /// If the slots are not strictly ascending, a role names an output past
    /// `O`, or an input names a slot that is not a read. Each is a panic,
    /// never a wrong or aliased buffer.
    #[inline]
    pub(crate) fn direct<const I: usize, const O: usize>(
        &mut self,
        frames: usize,
        d: &crate::compile::plan::Direct,
    ) -> ([&[F]; I], [&mut [F]; O]) {
        use crate::compile::plan::Direct;
        let st = self.stride;
        let mut rest: &mut [Line] = &mut self.lines;
        let mut base = 0usize;
        let mut reads: [&[F]; 4] = [&[]; 4];
        let mut outs: [&mut [F]; O] = std::array::from_fn(|_| &mut [][..]);
        let entries = d.slots.iter().zip(&d.role).zip(reads.iter_mut());
        for ((&slot, &role), read) in entries.take(usize::from(d.count)) {
            let slot = slot.index();
            let skip = slot.checked_sub(base).expect("direct slots are sorted");
            let (_, tail) = std::mem::take(&mut rest).split_at_mut(skip * st);
            let (cur, after) = tail.split_at_mut(st);
            rest = after;
            base = slot + 1;
            match role {
                Direct::READ => {
                    let cur: &[Line] = cur;
                    *read = &bytemuck::cast_slice::<Line, F>(cur)[..frames];
                }
                c => outs[usize::from(c)] = &mut bytemuck::cast_slice_mut::<Line, F>(cur)[..frames],
            }
        }
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

    /// `src` for reading and `dst` for writing, at once. They must differ.
    #[inline]
    pub(crate) fn pair(
        &mut self,
        src: AudioSlotId,
        dst: AudioSlotId,
        frames: usize,
    ) -> (&[F], &mut [F]) {
        assert_ne!(src, dst, "pair() of one slot");
        let st = self.stride;
        let (lo, hi) = (src.min(dst).index(), src.max(dst).index());
        let (a, b) = self.lines.split_at_mut(hi * st);
        let low = &mut a[lo * st..][..st];
        let high = &mut b[..st];
        let (s, d) = if src < dst { (low, high) } else { (high, low) };
        (
            &bytemuck::cast_slice::<Line, F>(s)[..frames],
            &mut bytemuck::cast_slice_mut::<Line, F>(d)[..frames],
        )
    }

    /// The first `outs.len()` slots' first `frames` samples, mutably, one per
    /// entry of `outs` — a crossfade's scratch, whose slots are its outgoing
    /// unit's output channels in order.
    #[inline]
    pub(crate) fn leading_mut<'a>(&'a mut self, frames: usize, outs: &mut [&'a mut [F]]) {
        for (o, lines) in outs
            .iter_mut()
            .zip(self.lines.chunks_exact_mut(self.stride))
        {
            *o = &mut bytemuck::cast_slice_mut::<Line, F>(lines)[..frames];
        }
    }

    /// The arena as one `SplitRw` of its slots, for the parallel executor:
    /// each slot claimed by the op that touches it (`slots.rs`).
    pub(crate) fn split<'a>(
        &'a mut self,
        table: &'a mut tutti_rt::ClaimTable,
    ) -> tutti_rt::SplitRw<'a, Line> {
        tutti_rt::SplitRw::new(&mut self.lines, self.stride, table)
    }

    /// Copy slot `src` over slot `dst` (whole stride). Never allocates.
    pub(crate) fn copy_slot(&mut self, src: AudioSlotId, dst: AudioSlotId) {
        let st = self.stride;
        self.lines
            .copy_within(src.index() * st..(src.index() + 1) * st, dst.index() * st);
    }

    /// Hand the sorted `reqs` to [`borrow_sorted`], as slices of
    /// `frames`.
    #[inline]
    pub(crate) fn borrow<'a>(
        &'a mut self,
        frames: usize,
        reqs: &[(AudioSlotId, Role)],
        ins: &mut [&'a [F]],
        outs: &mut [&'a mut [F]],
    ) {
        let stride = self.stride;
        borrow_sorted(
            &mut self.lines,
            stride,
            reqs,
            |port, lines| {
                ins[port as usize] = &bytemuck::cast_slice::<Line, F>(lines)[..frames];
            },
            |port, lines| {
                outs[port as usize] = &mut bytemuck::cast_slice_mut::<Line, F>(lines)[..frames];
            },
        );
    }

    /// The arena's samples, as one aligned slice — for tests asserting the
    /// alignment claim.
    #[cfg(test)]
    pub(crate) fn base(&self) -> *const F {
        self.lines.as_ptr().cast()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::ids::Id;
    fn slot(s: u32) -> AudioSlotId {
        AudioSlotId::new(s)
    }
    use crate::node::MAX_PORTS;

    /// Every slot starts on a 64-byte boundary, whatever `max_block` is.
    #[test]
    fn slots_are_cache_line_aligned() {
        for max_block in [1, 7, 64, 100, 1000] {
            let mut a = Arena::<f32>::new(5, max_block);
            assert_eq!(a.base() as usize % 64, 0);
            for s in 0..5u32 {
                assert_eq!(a.slot_mut(slot(s), 1).as_ptr() as usize % 64, 0, "slot {s}");
                assert!(a.stride * LANES >= max_block);
            }
        }
    }

    /// The same at `f64`: a line holds 8 samples, so a slot of `stride` lines
    /// holds at least `max_block` of them when `max_block` is no multiple of 8.
    #[test]
    fn f64_slots_are_cache_line_aligned_and_hold_a_block() {
        for max_block in [1, 7, 9, 63, 100, 1001] {
            let mut a = Arena::<f64>::new(5, max_block);
            assert_eq!(a.base() as usize % 64, 0);
            assert!(a.stride * (size_of::<Line>() / size_of::<f64>()) >= max_block);
            for s in 0..5u32 {
                let slot = a.slot_mut(slot(s), max_block);
                assert_eq!(slot.as_ptr() as usize % 64, 0, "slot {s}");
                assert_eq!(slot.len(), max_block);
            }
        }
    }

    /// Reads share, writes are exclusive, and the right port gets the right
    /// slot.
    #[test]
    fn borrow_hands_each_port_its_slot() {
        let mut a = Arena::<f32>::new(6, 16);
        for s in 0..6u32 {
            a.slot_mut(slot(s), 16).fill(s as f32);
        }
        let mut reqs = [
            (slot(4), Role::Read(0)),
            (slot(1), Role::Read(1)),
            (slot(4), Role::Read(2)),
            (slot(3), Role::Write(0)),
            (slot(5), Role::Write(1)),
        ];
        let mut ins: [&[f32]; MAX_PORTS] = [&[]; MAX_PORTS];
        let mut outs: [&mut [f32]; MAX_PORTS] = std::array::from_fn(|_| &mut [][..]);
        // As the compiler hands them over (`NodeTables::lower`).
        reqs.sort_unstable();
        a.borrow(16, &reqs, &mut ins, &mut outs);
        assert_eq!(ins[0][0], 4.0);
        assert_eq!(ins[1][0], 1.0);
        assert_eq!(ins[2][0], 4.0);
        assert_eq!(outs[0][0], 3.0);
        assert_eq!(outs[1][0], 5.0);
        outs[0][0] = 9.0;
        assert_eq!(a.slot(slot(3), 1)[0], 9.0);
    }

    /// A slot requested for writing and reading in one op is refused, never
    /// aliased.
    #[test]
    #[should_panic(expected = "written and also borrowed")]
    fn borrow_refuses_a_write_that_aliases() {
        let mut a = Arena::<f32>::new(3, 16);
        let reqs = [(slot(2), Role::Read(0)), (slot(2), Role::Write(0))];
        let mut ins: [&[f32]; MAX_PORTS] = [&[]; MAX_PORTS];
        let mut outs: [&mut [f32]; MAX_PORTS] = std::array::from_fn(|_| &mut [][..]);
        a.borrow(16, &reqs, &mut ins, &mut outs);
    }

    /// Requests the compiler failed to sort are a panic, never a slot behind
    /// the cursor handed out as the wrong buffer.
    #[test]
    #[should_panic(expected = "sorted")]
    fn borrow_refuses_unsorted_requests() {
        let mut a = Arena::<f32>::new(4, 16);
        let reqs = [(slot(3), Role::Write(0)), (slot(1), Role::Read(0))];
        let mut ins: [&[f32]; MAX_PORTS] = [&[]; MAX_PORTS];
        let mut outs: [&mut [f32]; MAX_PORTS] = std::array::from_fn(|_| &mut [][..]);
        a.borrow(16, &reqs, &mut ins, &mut outs);
    }
}
