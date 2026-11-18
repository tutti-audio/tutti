use super::*;

/// A push fills the slots in order, refuses the one past the capacity and
/// hands the value back; equality ignores what is in the dead slots.
#[test]
fn a_push_past_the_capacity_is_refused_with_the_value() {
    let mut list: Inline<u8, 2> = Inline::new();
    assert_eq!(list.push(1), Ok(()));
    assert_eq!(list.push(2), Ok(()));
    assert_eq!(list.push(3), Err(Full(3)));
    assert_eq!(&*list, &[1, 2]);
}

/// `from_slice` and `filled` refuse what does not fit, and equality compares
/// the live items only.
#[test]
fn construction_refuses_what_does_not_fit_and_equality_reads_live_items() {
    assert!(Inline::<u8, 2>::from_slice(&[1, 2, 3]).is_none());
    assert!(Inline::<u8, 2>::filled(7, 3).is_none());
    let a = Inline::<u8, 3>::from_slice(&[1, 2]).unwrap();
    let mut b = Inline::<u8, 3>::from_slice(&[1, 2, 9]).unwrap();
    assert_ne!(a, b);
    b = Inline::<u8, 3>::from_slice(&[1, 2]).unwrap();
    assert_eq!(a, b);
}

/// `Debug` shows the live items and nothing of the dead slots.
#[test]
fn debug_prints_only_the_live_slots() {
    let list = Inline::<u8, 4>::from_slice(&[1, 2]).unwrap();
    assert_eq!(format!("{list:?}"), "[1, 2]");
}
