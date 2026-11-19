//! The Unix clock: `clock_gettime` on the platform's monotonic clock id.

#[cfg_attr(target_vendor = "apple", path = "unix/apple.rs")]
#[cfg_attr(not(target_vendor = "apple"), path = "unix/posix.rs")]
mod id;

pub(super) fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable `timespec`, and the clock id is one
    // every Unix (or, on Apple, macOS 10.12+) provides. It cannot fail with
    // those, and a failure would leave `ts` zero, a valid reading.
    unsafe { libc::clock_gettime(id::CLOCK, &mut ts) };
    // The `timespec` field types are platform-sized; widen to `i64`.
    #[allow(clippy::unnecessary_cast, clippy::useless_conversion)]
    let (secs, nanos) = (i64::from(ts.tv_sec as i64), i64::from(ts.tv_nsec as i64));
    secs.saturating_mul(1_000_000_000).saturating_add(nanos)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The clock is the OS's**, not one this process started: on Unix a
    /// reading sits between two direct reads of the clock it names.
    #[test]
    fn the_clock_is_the_os_monotonic_clock() {
        let direct = || {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: a valid, writable `timespec`.
            unsafe { libc::clock_gettime(id::CLOCK, &mut ts) };
            #[allow(clippy::unnecessary_cast)]
            let ns = ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64;
            ns
        };
        let a = direct();
        let x = monotonic_ns();
        let b = direct();
        assert!(a <= x && x <= b, "{a} <= {x} <= {b}");
    }
}
