extern crate std;
use super::*;
use std::vec::Vec;

#[test]
fn push_fills_then_refuses_at_the_cap() {
    let mut v: RtVec<u32, 3> = RtVec::new();
    assert!(v.push(1));
    assert!(v.push(2));
    assert!(v.push(3));
    assert!(!v.push(4), "push past N must be refused");
    assert_eq!(v.as_slice(), &[1, 2, 3]);
    assert_eq!(v.len(), 3);
    assert!(v.is_full());
    assert_eq!(v.remaining(), 0);
}

#[test]
fn overflow_is_reported_and_latches_for_the_block() {
    let mut v: RtVec<u32, 2> = RtVec::new();
    assert!(!v.is_overflowed());
    v.push(1);
    v.push(2);
    assert!(!v.is_overflowed(), "at capacity but nothing dropped yet");
    v.push(3);
    assert!(v.is_overflowed(), "a refused push latches the flag");
    assert!(
        v.is_overflowed(),
        "and it stays set for the rest of the block"
    );
}

#[test]
fn clear_resets_contents_and_the_flag() {
    let mut v: RtVec<u32, 2> = RtVec::new();
    v.extend(0..10);
    assert!(v.is_overflowed());
    v.clear();
    assert!(v.is_empty());
    assert!(
        !v.is_overflowed(),
        "the flag describes the current block, not history"
    );
}

#[test]
fn extend_reports_how_many_it_wrote() {
    let mut v: RtVec<u32, 4> = RtVec::new();
    assert_eq!(v.extend(0..2), 2, "everything fits");
    assert!(!v.is_overflowed());
    assert_eq!(v.extend(100..200), 2, "only the remaining room");
    assert!(v.is_overflowed());
    assert_eq!(v.as_slice(), &[0, 1, 100, 101]);
}

#[test]
fn as_slice_serves_a_borrowed_return() {
    // The shape the plugin-host pools need: fill, then hand `&[T]` back.
    fn lend(v: &RtVec<u32, 8>) -> &[u32] {
        v.as_slice()
    }
    let mut v: RtVec<u32, 8> = RtVec::new();
    v.extend([7, 8, 9]);
    assert_eq!(lend(&v), &[7, 8, 9]);
}

#[test]
fn as_mut_slice_allows_in_place_fixups() {
    let mut v: RtVec<u32, 8> = RtVec::new();
    v.extend([3, 1, 2]);
    v.as_mut_slice().sort_unstable();
    assert_eq!(v.as_slice(), &[1, 2, 3]);
}

#[test]
fn deref_and_iter_read_the_filled_run_only() {
    let mut v: RtVec<u32, 8> = RtVec::new();
    v.extend([4, 5]);
    assert_eq!(v.len(), 2, "not the inline capacity");
    assert_eq!(v.first(), Some(&4), "slice methods via Deref");
    let seen: Vec<u32> = v.iter().copied().collect();
    assert_eq!(seen, std::vec![4, 5]);
}

#[test]
fn refilling_past_the_cap_every_block_stays_at_the_cap() {
    // The regression this type exists to prevent: driven past `N` on every
    // block, the collection must never grow. Checked via `remaining`,
    // which `no_std` can assert without a heap probe.
    let mut v: RtVec<u32, 4> = RtVec::new();
    for _ in 0..1_000 {
        v.clear();
        v.extend(0..64);
        assert_eq!(v.len(), 4);
        assert_eq!(v.remaining(), 0);
        assert!(v.is_overflowed());
    }
}

/// `pop` takes the last item and leaves the overflow flag latched: a pop does
/// not undo a refused push.
#[test]
fn pop_takes_the_last_item_and_keeps_the_flag() {
    let mut v: RtVec<u8, 2> = RtVec::new();
    assert_eq!(v.pop(), None);
    v.extend([1, 2, 3]);
    assert!(v.is_overflowed());
    assert_eq!(v.pop(), Some(2));
    assert_eq!(v.as_slice(), &[1]);
    assert!(v.is_overflowed(), "still latched");
    assert!(v.push(9));
    assert_eq!(v.as_slice(), &[1, 9]);
}

/// Counts its drops in a shared cell.
struct Counted<'a>(&'a core::cell::Cell<usize>);

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[test]
fn every_stored_item_drops_exactly_once_through_pop_clear_and_drop() {
    let drops = core::cell::Cell::new(0);
    let mut v: RtVec<Counted<'_>, 4> = RtVec::new();
    for _ in 0..4 {
        assert!(v.push(Counted(&drops)));
    }
    assert!(
        !v.push(Counted(&drops)),
        "the refused item is dropped at once"
    );
    assert_eq!(drops.get(), 1);
    drop(v.pop());
    assert_eq!(drops.get(), 2, "a popped item drops with its owner");
    v.clear();
    assert_eq!(drops.get(), 5, "clear drops the three left");
    assert!(v.push(Counted(&drops)));
    assert!(v.push(Counted(&drops)));
    drop(v);
    assert_eq!(
        drops.get(),
        7,
        "dropping the collection drops what it holds"
    );
}

#[test]
fn a_boxed_collection_is_empty_and_works_as_new() {
    let mut v = RtVec::<u32, 3>::boxed();
    assert!(v.is_empty() && !v.is_overflowed());
    assert_eq!(v.capacity(), 3);
    assert_eq!(v.extend([1, 2, 3, 4]), 3);
    assert_eq!(v.as_slice(), &[1, 2, 3]);
    assert!(v.is_overflowed());
}

/// `boxed` builds in the box: a collection eight times the thread's stack is
/// made and filled on that thread without overflowing it.
#[test]
fn boxed_builds_a_collection_larger_than_the_stack() {
    const STACK: usize = 64 * 1024;
    let handle = std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(|| {
            let mut v = RtVec::<[u8; 64], { 8 * STACK / 64 }>::boxed();
            assert!(core::mem::size_of_val(&*v) > 8 * STACK - 1);
            while v.push([7; 64]) {}
            v.len()
        })
        .expect("spawn the small-stack thread");
    assert_eq!(handle.join().expect("no stack overflow"), 8 * STACK / 64);
}

/// Drops into a shared counter, and panics on its drop when told to.
struct Bomb<'a> {
    drops: &'a core::cell::Cell<usize>,
    panics: bool,
}

impl Drop for Bomb<'_> {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
        assert!(!self.panics, "this item's drop panics");
    }
}

#[test]
fn a_panicking_item_drop_in_clear_still_drops_the_others_once_and_empties() {
    let drops = core::cell::Cell::new(0);
    let mut v: RtVec<Bomb<'_>, 4> = RtVec::new();
    for i in 0..4 {
        assert!(v.push(Bomb {
            drops: &drops,
            panics: i == 1,
        }));
    }
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| v.clear()));
    assert!(unwound.is_err(), "the panic reaches the caller");
    assert_eq!(drops.get(), 4, "each of the four items dropped once");
    assert_eq!(v.len(), 0);
    drop(v);
    assert_eq!(
        drops.get(),
        4,
        "dropping the emptied collection drops none twice"
    );
}
