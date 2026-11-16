//! macOS and iOS: `CLOCK_UPTIME_RAW`, `mach_absolute_time` in nanoseconds.

pub(super) const CLOCK: libc::clockid_t = libc::CLOCK_UPTIME_RAW;
