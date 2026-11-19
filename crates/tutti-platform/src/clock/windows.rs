//! Windows: `QueryPerformanceCounter` scaled by its frequency.

pub(super) fn monotonic_ns() -> i64 {
    use std::sync::atomic::{AtomicI64, Ordering};
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

    // The frequency is fixed at boot: read once, then cached (0 is unread).
    static FREQUENCY: AtomicI64 = AtomicI64::new(0);
    let mut freq = FREQUENCY.load(Ordering::Relaxed);
    if freq <= 0 {
        // SAFETY: a valid, writable `i64`. It cannot fail on Windows XP and
        // later; a failure leaves it 0, handled below.
        let _ = unsafe { QueryPerformanceFrequency(&mut freq) };
        if freq <= 0 {
            return 0;
        }
        FREQUENCY.store(freq, Ordering::Relaxed);
    }
    let mut count = 0i64;
    // SAFETY: as above.
    let _ = unsafe { QueryPerformanceCounter(&mut count) };
    // Whole seconds and the remainder apart, so the scaling cannot overflow.
    let (secs, rest) = (count / freq, count % freq);
    secs.saturating_mul(1_000_000_000)
        .saturating_add(rest * 1_000_000_000 / freq)
}
