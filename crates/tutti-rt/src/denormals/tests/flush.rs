use super::super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};

#[cfg_attr(target_arch = "x86_64", path = "reg_x86_64.rs")]
#[cfg_attr(target_arch = "aarch64", path = "reg_aarch64.rs")]
mod reg;

use reg::{FLUSH, read_reg};

fn flushing() -> bool {
    read_reg() & FLUSH == FLUSH
}

// Miri cannot execute x86 SSE intrinsics, and these tests are about
// nothing else: they read and write MXCSR directly. Ignored under miri
// rather than cfg'd out, so `cargo miri test` reports them as skipped
// instead of silently covering a smaller surface than it appears to.
#[test]
#[cfg_attr(miri, ignore = "reads MXCSR; miri has no x86 SSE intrinsics")]
fn restores_state() {
    let before = read_reg();
    assert!(!flushing());
    let out = with_no_denormals(|| {
        assert!(flushing());
        7
    });
    assert_eq!(out, 7, "the closure's value is returned");
    assert_eq!(before, read_reg());
}

#[test]
#[cfg_attr(miri, ignore = "reads MXCSR; miri has no x86 SSE intrinsics")]
fn nested() {
    let original = read_reg();
    with_no_denormals(|| {
        let outer = read_reg();
        with_no_denormals(|| assert!(flushing()));
        assert!(flushing());
        assert_eq!(outer, read_reg(), "the inner call restores the outer mode");
    });
    assert_eq!(original, read_reg());
}

#[test]
#[cfg_attr(miri, ignore = "reads MXCSR; miri has no x86 SSE intrinsics")]
fn after_a_panic() {
    let before = read_reg();
    let r = catch_unwind(AssertUnwindSafe(|| {
        with_no_denormals(|| {
            assert!(flushing());
            panic!("inside");
        })
    }));
    assert!(r.is_err());
    assert_eq!(before, read_reg());
}

#[test]
#[cfg_attr(miri, ignore = "reads MXCSR; miri has no x86 SSE intrinsics")]
fn nested_panic() {
    let before = read_reg();
    with_no_denormals(|| {
        let outer = read_reg();
        let r = catch_unwind(AssertUnwindSafe(|| {
            with_no_denormals(|| panic!("inner"));
        }));
        assert!(r.is_err());
        assert_eq!(
            outer,
            read_reg(),
            "the outer mode survives the inner unwind"
        );
        assert!(flushing());
    });
    assert_eq!(before, read_reg());
}

#[test]
#[cfg_attr(miri, ignore = "reads MXCSR; miri has no x86 SSE intrinsics")]
fn a_subnormal_result_flushes_only_inside() {
    // The product of the smallest normal and a half is subnormal, so
    // IEEE gives a nonzero value and FTZ gives zero.
    use std::hint::black_box;
    let product = || black_box(f32::MIN_POSITIVE) * black_box(0.5_f32);
    assert!(product() > 0.0, "subnormal results survive by default");
    assert_eq!(with_no_denormals(product), 0.0);
    assert!(product() > 0.0, "and again after the call");
}
