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

/// `drain` yields the run in order, empties the list and clears the overflow
/// flag, and the cap still holds afterwards.
#[test]
fn drain_yields_the_run_and_empties_the_list() {
    let mut v = CappedVec::with_capacity(2);
    v.extend([1, 2, 3]);
    assert!(v.is_overflowed());
    assert_eq!(v.drain().collect::<Vec<_>>(), [1, 2]);
    assert!(v.is_empty() && !v.is_overflowed());
    assert!(v.push(4) && v.push(5) && !v.push(6));
}
