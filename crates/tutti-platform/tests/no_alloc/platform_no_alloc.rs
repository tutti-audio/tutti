//! The platform edges' allocation gates.

/// `with_no_denormals` neither allocates nor frees, and returns its
/// closure's value.
#[test]
fn with_no_denormals_is_allocation_free() {
    use tutti_platform::with_no_denormals;

    assert_no_alloc::assert_no_alloc(|| {
        for round in 0..1_000u32 {
            assert_eq!(with_no_denormals(|| round + 1), round + 1);
        }
    });
}

/// The OS clock, from its first read: on Windows that first read also queries
/// and caches the counter's frequency, so it runs inside the gate too.
#[test]
fn the_clock_is_allocation_free_from_its_first_read() {
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1_000 {
            core::hint::black_box(tutti_platform::monotonic_ns());
        }
    });
}
