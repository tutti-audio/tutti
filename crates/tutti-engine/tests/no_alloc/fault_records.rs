//! The render-thread fault records under `RtAllocCheck`.

use tutti_platform::Instant;

use tutti_engine::{BackendErrorKind, StreamFaultKind, StreamFaults};

/// **The render thread's fault records allocate and free nothing, however
/// often they run over an owned message or each other**, and what they wrote
/// is read on the control side with its kind and text.
#[test]
fn raising_render_side_faults_repeatedly_allocates_nothing_and_is_read_afterwards() {
    let faults = StreamFaults::new();
    faults.record(BackendErrorKind::Other, &"xrun", Instant::now());
    faults.record_overrun("warm");
    faults.record_panic("warm");

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..10_000 {
            faults.record_overrun("the timer fell behind");
            faults.record_panic("the output callback panicked");
        }
    });

    assert_eq!(faults.count(), 3 + 20_000);
    let last = faults.last().expect("a fault");
    assert_eq!(last.kind, StreamFaultKind::CallbackPanicked);
    assert_eq!(last.message, "the output callback panicked");
    assert!(faults.is_disconnected());
}

/// An error whose `Display` writes in pieces, as a backend's does.
struct Pieces;
impl std::fmt::Display for Pieces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("`alsa::poll()` ")?;
        write!(f, "returned {}", 42)
    }
}

/// **A backend error recorded where the error callback runs (the render
/// thread, on CoreAudio) allocates and frees nothing**: not for a first
/// error, a repeat, nor a disconnect, and
/// the text is read on the control side.
#[test]
fn recording_a_backend_error_allocates_nothing_and_is_read_afterwards() {
    let faults = StreamFaults::new();
    let now = Instant::now();
    faults.record(BackendErrorKind::Other, &Pieces, now);
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..1000 {
            faults.record(BackendErrorKind::Other, &Pieces, now);
        }
        faults.record(BackendErrorKind::DeviceGone, &Pieces, now);
        faults.record(BackendErrorKind::Survivable, &Pieces, now);
    });
    let last = faults.last().expect("a fault");
    assert_eq!(last.kind, StreamFaultKind::Disconnected);
    assert_eq!(last.message, "`alsa::poll()` returned 42");
    assert!(faults.is_disconnected());
}
