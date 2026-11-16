//! The render-thread fault records under `AllocDisabler`.

use tutti_engine::{BackendErrorKind, StreamFaultKind, StreamFaults};

/// **The render thread's fault records allocate and free nothing, however
/// often they run over an owned message or each other**, and what they wrote
/// is read on the control side with its kind and text.
#[test]
fn raising_render_side_faults_repeatedly_allocates_nothing_and_is_read_afterwards() {
    let faults = StreamFaults::new();
    faults.record(BackendErrorKind::Other, &"xrun");
    faults.record_overrun("warm");
    faults.record_panic("warm");

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            faults.record_overrun("the timer fell behind");
            faults.record_panic("the output callback panicked");
        }
    });

    assert_eq!(faults.count(), 1 + 2 + 20_000);
    let last = faults.last().expect("a fault");
    assert_eq!(last.kind, StreamFaultKind::CallbackPanicked);
    assert_eq!(last.message, "the output callback panicked");
    assert!(faults.is_disconnected());
}
