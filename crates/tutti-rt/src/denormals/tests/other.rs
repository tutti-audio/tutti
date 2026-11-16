use super::super::*;
use std::panic::catch_unwind;

#[test]
fn restores_state() {
    assert_eq!(with_no_denormals(|| 7), 7);
}

#[test]
fn nested() {
    assert_eq!(with_no_denormals(|| with_no_denormals(|| 7)), 7);
}

#[test]
fn after_a_panic() {
    assert!(catch_unwind(|| with_no_denormals(|| panic!("inside"))).is_err());
}

#[test]
fn nested_panic() {
    with_no_denormals(|| {
        assert!(catch_unwind(|| with_no_denormals(|| panic!("inner"))).is_err());
    });
}
