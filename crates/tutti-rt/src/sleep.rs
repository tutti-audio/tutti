//! [`sleep_until_ns`]: block the calling thread until an absolute time on the
//! [`monotonic_ns`](crate::monotonic_ns) clock.
//!
//! A thread that paces itself against deadlines (the session's system clock,
//! one block per deadline) sleeps *to* each deadline rather than *for* an
//! interval, so a late wake-up does not push every later deadline back and the
//! error of one sleep is not added to the next.
//!
//! - Linux and Android: `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME)`, the
//!   same clock `monotonic_ns` reads, so the kernel wakes the thread at the
//!   deadline itself.
//! - Every other target: the time left, slept through `std::thread::sleep` and
//!   re-read until the deadline has passed. Windows' std sleeps on a
//!   high-resolution waitable timer where the OS has one; macOS sleeps on
//!   `nanosleep`.
//!
//! Control-side thread code: it blocks, so it is never for the audio callback.

#[cfg_attr(
    any(target_os = "linux", target_os = "android"),
    path = "sleep/absolute.rs"
)]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "android")),
    path = "sleep/relative.rs"
)]
mod os;

/// Blocks the calling thread until [`monotonic_ns`](crate::monotonic_ns)
/// reads at least `deadline_ns`. A deadline already past returns at once.
///
/// Never returns early: a signal that interrupts the sleep resumes it. It may
/// return late, by the OS's scheduling and timer granularity.
///
/// ```
/// use tutti_rt::{monotonic_ns, sleep_until_ns};
///
/// let deadline = monotonic_ns() + 1_000_000;
/// sleep_until_ns(deadline);
/// assert!(monotonic_ns() >= deadline);
/// ```
pub fn sleep_until_ns(deadline_ns: i64) {
    os::sleep_until_ns(deadline_ns);
}

#[cfg(test)]
mod tests;
