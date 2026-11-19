//! The absolute sleep of a kernel that has one: `clock_nanosleep` on
//! `CLOCK_MONOTONIC`, the clock `monotonic_ns` reads there.

const NANOS: i64 = 1_000_000_000;

/// Sleeps to `deadline_ns`, resuming on `EINTR`. `Err` when the kernel refuses
/// the sleep for any other reason: the caller sleeps another way.
pub(super) fn sleep_until_ns(deadline_ns: i64) -> Result<(), ()> {
    // `CLOCK_MONOTONIC` never reads below zero, so a negative deadline has
    // passed.
    if deadline_ns < 0 {
        return Ok(());
    }
    // The whole seconds, clamped to what `time_t` holds (32 bits on some
    // targets): a clamped deadline is still far past any real wait.
    let tv_sec = libc::time_t::try_from(deadline_ns / NANOS).unwrap_or(libc::time_t::MAX);
    // `tv_nsec` is below one second, which every `c_long` holds.
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    let ts = libc::timespec {
        tv_sec,
        tv_nsec: (deadline_ns % NANOS) as _,
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
        match status {
            0 => return Ok(()),
            // A signal interrupted the sleep; the absolute deadline needs no
            // recomputing to resume it.
            libc::EINTR => {}
            _ => return Err(()),
        }
    }
}
