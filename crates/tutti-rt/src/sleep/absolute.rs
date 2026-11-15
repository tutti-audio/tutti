//! The absolute sleep of a kernel that has one: `clock_nanosleep` on
//! `CLOCK_MONOTONIC`, the clock `monotonic_ns` reads there.

const NANOS: i64 = 1_000_000_000;

pub(super) fn sleep_until_ns(deadline_ns: i64) {
    // Both fields are platform-sized; a deadline in range of `i64` nanoseconds
    // has whole seconds that fit `time_t` and a remainder below one second.
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    let ts = libc::timespec {
        tv_sec: deadline_ns.div_euclid(NANOS) as libc::time_t,
        tv_nsec: deadline_ns.rem_euclid(NANOS) as _,
    };
    loop {
        // SAFETY: `ts` is a valid `timespec` that outlives the call, the clock
        // is one every Linux provides, and the remaining-time pointer is null,
        // which an absolute sleep permits.
        let status = unsafe {
            libc::clock_nanosleep(
                libc::CLOCK_MONOTONIC,
                libc::TIMER_ABSTIME,
                &ts,
                std::ptr::null_mut(),
            )
        };
        // `EINTR`: a signal interrupted the sleep, and the absolute deadline
        // needs no recomputing to resume it. Anything else ends the sleep.
        if status != libc::EINTR {
            return;
        }
    }
}
