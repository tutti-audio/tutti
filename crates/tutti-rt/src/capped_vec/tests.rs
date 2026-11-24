use super::*;

/// The push past the cap is refused, keeps the run unchanged and latches the
/// flag; `clear` empties the run and resets the flag with the cap intact.
#[test]
fn a_push_past_the_cap_is_refused_and_latched_until_clear() {
    let mut v = CappedVec::with_capacity(3);
    assert_eq!(v.extend([1, 2, 3, 4, 5]), 3, "the cap stops the extend");
    assert_eq!(v.as_slice(), &[1, 2, 3]);
    assert!(v.is_full() && v.is_overflowed());
    assert_eq!(v.remaining(), 0);
    v.clear();
    assert!(v.is_empty() && !v.is_overflowed());
    assert_eq!((v.capacity(), v.remaining()), (3, 3));
}

/// Filling to the cap and clearing, block after block, keeps the one
/// allocation made at construction: the storage never moves.
#[test]
fn filling_and_clearing_never_reallocate() {
    let mut v = CappedVec::with_capacity(64);
    let storage = v.as_slice().as_ptr();
    for block in 0..10u32 {
        v.extend((0..100).map(|i| block * 100 + i));
        assert_eq!(v.len(), 64);
        assert_eq!(v.as_slice().as_ptr(), storage, "block {block}");
        v.clear();
    }
}

/// The refused push drops its item there and then: a value refused at the cap
/// is not kept anywhere.
#[test]
fn a_refused_push_drops_its_item() {
    use std::rc::Rc;
    let item = Rc::new(());
    let mut v = CappedVec::with_capacity(1);
    assert!(v.push(Rc::clone(&item)));
    assert!(!v.push(Rc::clone(&item)));
    assert_eq!(
        Rc::strong_count(&item),
        2,
        "the original and the one stored"
    );
}

/// A cap of zero refuses the first push.
#[test]
fn a_zero_cap_refuses_every_push() {
    let mut v = CappedVec::with_capacity(0);
    assert!(!v.push(1u8));
    assert!(v.is_overflowed() && v.is_empty() && v.is_full());
}

/// `mark_overflowed` latches the flag without changing the run.
#[test]
fn mark_overflowed_latches_without_pushing() {
    let mut v = CappedVec::with_capacity(4);
    assert!(v.push('a'));
    v.mark_overflowed();
    assert!(v.is_overflowed());
    assert_eq!(v.as_slice(), &['a']);
}

/// `pop` takes the last item and leaves the overflow flag latched.
#[test]
fn pop_takes_the_last_item_and_keeps_the_flag() {
    let mut v = CappedVec::with_capacity(2);
    assert_eq!(v.pop(), None);
    v.extend([1, 2, 3]);
    assert_eq!(v.pop(), Some(2));
    assert_eq!(v.as_slice(), &[1]);
    assert!(v.is_overflowed(), "still latched");
}

/// A refused `push_within_capacity` returns the very item, undropped, latches
/// the flag, and leaves the run as it was; an accepted one is `Ok`.
#[test]
fn push_within_capacity_hands_a_refused_item_back() {
    use std::rc::Rc;
    let item = Rc::new(());
    let mut v = CappedVec::with_capacity(1);
    assert_eq!(v.push_within_capacity(Rc::clone(&item)), Ok(()));
    assert!(!v.is_overflowed());
    let back = v.push_within_capacity(Rc::clone(&item)).unwrap_err();
    assert!(v.is_overflowed());
    assert_eq!(v.len(), 1);
    assert_eq!(Rc::strong_count(&item), 3, "original, stored, handed back");
    drop(back);
    assert_eq!(Rc::strong_count(&item), 2);
}

/// `drain` yields the range in order, shifts the rest down and keeps the
/// storage and the cap, so the room it frees is pushable again.
#[test]
fn drain_yields_in_order_and_keeps_the_storage() {
    let mut v = CappedVec::with_capacity(4);
    v.extend([1, 2, 3, 4]);
    let storage = v.as_slice().as_ptr();
    assert_eq!(v.drain(1..3).collect::<Vec<_>>(), [2, 3]);
    assert_eq!(v.as_slice(), &[1, 4]);
    assert_eq!(v.drain(..).collect::<Vec<_>>(), [1, 4]);
    assert!(v.is_empty());
    assert!(v.push(9) && v.push(8) && v.push(7) && v.push(6) && !v.push(5));
    assert_eq!(v.as_slice().as_ptr(), storage);
}

/// The default has a cap of zero and refuses every push.
#[test]
fn the_default_has_a_zero_cap() {
    let mut v: CappedVec<u8> = CappedVec::default();
    assert_eq!(v.capacity(), 0);
    assert!(!v.push(1));
}

/// `refused` counts one per refused `push`, one per refused
/// `push_within_capacity`, and one per item an `extend` could not take; a
/// push that fits adds nothing; `clear` resets it.
#[test]
fn refused_counts_every_item_each_way_it_is_turned_away() {
    let mut v = CappedVec::with_capacity(2);
    assert_eq!(v.refused(), 0);
    assert!(v.push(1) && v.push(2));
    assert_eq!(v.refused(), 0, "pushes that fit are not refused");
    assert!(!v.push(3));
    assert_eq!(v.refused(), 1);
    assert_eq!(v.push_within_capacity(4), Err(4));
    assert_eq!(v.refused(), 2);
    assert_eq!(v.extend([5, 6, 7]), 0);
    assert_eq!(v.refused(), 5, "each extend item is one refusal");
    assert_eq!(v.as_slice(), &[1, 2]);
    v.clear();
    assert_eq!(v.refused(), 0);
    assert!(!v.is_overflowed());
}

/// An extend of an endless iterator stops at the cap and returns.
#[test]
fn extending_an_endless_iterator_stops_at_the_cap() {
    let mut v = CappedVec::with_capacity(3);
    assert_eq!(v.extend(std::iter::repeat(1)), 3);
    assert!(v.is_overflowed());
}

/// An extend that is partly taken counts only the items past the cap.
#[test]
fn a_partial_extend_counts_only_the_items_past_the_cap() {
    let mut v = CappedVec::with_capacity(3);
    assert_eq!(v.extend(0..5), 3);
    assert_eq!(v.refused(), 2);
    assert!(v.is_overflowed());
}

/// An insert shifts the later items up one, a full collection hands the item
/// back and latches the flag, and the storage never moves.
#[test]
fn insert_within_capacity_shifts_and_refuses_at_the_cap() {
    let mut v = CappedVec::with_capacity(4);
    v.extend([10, 30, 40]);
    let storage = v.as_slice().as_ptr();
    assert_eq!(v.insert_within_capacity(1, 20), Ok(()));
    assert_eq!(v.as_slice(), &[10, 20, 30, 40]);
    assert!(!v.is_overflowed());
    assert_eq!(v.insert_within_capacity(0, 5), Err(5));
    assert_eq!(v.as_slice(), &[10, 20, 30, 40]);
    assert!(v.is_overflowed());
    assert_eq!(v.as_slice().as_ptr(), storage);
}

/// `retain_mut` keeps the accepted items in order, lets the closure edit
/// them, and keeps the storage and the cap.
#[test]
fn retain_mut_keeps_accepted_items_in_order_and_edits_them() {
    let mut v = CappedVec::with_capacity(5);
    v.extend([1, 2, 3, 4, 5]);
    v.retain_mut(|x| {
        *x *= 10;
        *x != 20 && *x != 40
    });
    assert_eq!(v.as_slice(), &[10, 30, 50]);
    v[0] = 7;
    assert_eq!(
        v.as_slice(),
        &[7, 30, 50],
        "an indexed edit goes through the slice"
    );
    assert_eq!(v.capacity(), 5);
    assert_eq!(v.remaining(), 2);
}

/// An index past `len` panics whether or not there is room: the index is
/// checked before the cap.
#[test]
#[should_panic(expected = "past len")]
fn an_insert_past_len_panics_even_when_full() {
    let mut v = CappedVec::with_capacity(2);
    v.extend([1, 2]);
    let _ = v.insert_within_capacity(3, 9);
}

/// `truncate`, `swap_remove` and `remove` take items out without touching the
/// storage, the cap or a latched refusal: `swap_remove` fills the hole with the
/// last item, `remove` shifts the rest down, and a `truncate` past `len` keeps
/// everything.
#[test]
fn removing_items_keeps_the_storage_the_cap_and_the_flag() {
    let mut v = CappedVec::with_capacity(5);
    v.extend([1, 2, 3, 4, 5, 6]);
    assert!(v.is_overflowed());
    let storage = v.as_slice().as_ptr();
    assert_eq!(v.swap_remove(1), 2);
    assert_eq!(v.as_slice(), &[1, 5, 3, 4]);
    assert_eq!(v.remove(1), 5);
    assert_eq!(v.as_slice(), &[1, 3, 4]);
    v.truncate(9);
    assert_eq!(v.as_slice(), &[1, 3, 4]);
    v.truncate(1);
    assert_eq!(v.as_slice(), &[1]);
    assert_eq!((v.capacity(), v.remaining()), (5, 4));
    assert!(v.is_overflowed(), "a removal does not undo a refusal");
    assert_eq!(v.as_slice().as_ptr(), storage);
}

/// An index at `len` panics, as a `Vec`'s does.
#[test]
#[should_panic(expected = "index")]
fn a_swap_remove_at_len_panics() {
    let mut v = CappedVec::with_capacity(2);
    v.extend([1, 2]);
    let _ = v.swap_remove(2);
}

/// A slice that fits is copied whole, after what is there.
#[test]
fn a_slice_that_fits_is_copied_whole() {
    let mut v = CappedVec::with_capacity(5);
    assert!(v.push(9));
    assert_eq!(v.extend_from_slice(&[1, 2, 3, 4]), 4);
    assert_eq!(v.as_slice(), &[9, 1, 2, 3, 4]);
    assert_eq!(v.refused(), 0);
    assert!(!v.is_overflowed());
}

/// A slice that fits in part is copied up to the cap, and the rest counted.
#[test]
fn a_slice_that_fits_in_part_is_cut_and_counted() {
    let mut v = CappedVec::with_capacity(3);
    assert_eq!(v.extend_from_slice(&[1, 2, 3, 4, 5]), 3);
    assert_eq!(v.as_slice(), &[1, 2, 3]);
    assert_eq!(v.refused(), 2);
    assert!(v.is_overflowed());
    assert_eq!(v.extend_from_slice(&[6]), 0);
    assert_eq!(v.refused(), 3);
}

/// An empty slice changes nothing.
#[test]
fn an_empty_slice_changes_nothing() {
    let mut v: CappedVec<u32> = CappedVec::with_capacity(2);
    assert_eq!(v.extend_from_slice(&[]), 0);
    assert!(v.is_empty() && !v.is_overflowed());
}

/// Copying slices block after block keeps the one allocation made at
/// construction.
#[test]
fn copying_slices_never_reallocates() {
    let mut v = CappedVec::with_capacity(64);
    let storage = v.as_slice().as_ptr();
    let src: Vec<u32> = (0..100).collect();
    for block in 0..10 {
        assert_eq!(v.extend_from_slice(&src), 64);
        assert_eq!(v.as_slice().as_ptr(), storage, "block {block}");
        assert_eq!(v.capacity(), 64);
        v.clear();
    }
}
