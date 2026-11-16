//! The OS monotonic clock, in nanoseconds: the clock a hosted plugin compares
//! its transport's system time against (VST3 `systemTime`, VST2
//! `nanoSeconds`, Core Audio's host time).
//!
//! - Linux and the other Unixes: `CLOCK_MONOTONIC` (a vDSO read on Linux).
//! - macOS: `CLOCK_UPTIME_RAW`, which is `mach_absolute_time` in nanoseconds:
//!   Core Audio's host time (`AudioTimeStamp::mHostTime`, read through
//!   `AudioConvertHostTimeToNanos`) is that clock.
//! - Windows: `QueryPerformanceCounter`, scaled to nanoseconds by its
//!   frequency (fixed at boot).
//!
//! The OS's epoch, not one this process started: a plugin in another process
//! (a plugin server) reads the same timebase without being handed an origin,
//! which is why `std::time::Instant` (an opaque, process-local value) does not
//! do.

/// The OS monotonic clock now, in nanoseconds since the OS's own epoch (see
/// the module docs). Audio-thread safe: no lock, no allocation and no syscall
/// beyond the vDSO's or QPC's. 2^63 ns is 292 years of uptime, so the
/// saturation is unreachable.
///
/// A bare `i64` of nanoseconds, not `Seconds`: `Seconds` is an `f64`, and
/// every reader hands this exact integer count on (VST3's `systemTime`, the
/// plugin wire), where a float would round it.
#[must_use]
pub fn monotonic_ns() -> i64 {
    os::monotonic_ns()
}

#[cfg_attr(unix, path = "clock/unix.rs")]
#[cfg_attr(windows, path = "clock/windows.rs")]
mod os;

#[cfg(test)]
mod tests {
    use super::*;

    /// Two readings never go back.
    #[test]
    fn it_never_goes_back() {
        let a = monotonic_ns();
        let b = monotonic_ns();
        assert!(a > 0 && b >= a, "{a} then {b}");
    }
}
