//! The descriptor sweep: every descriptor from 3 up is marked close-on-exec.

use rustix::fd::BorrowedFd;
use rustix::io::{FdFlags, fcntl_setfd};
use rustix::process::{Resource, getrlimit};

/// The bound of the sweep when the process has no descriptor limit at all.
const UNLIMITED_CEILING: u64 = 1 << 20;

/// One past the highest descriptor the sweep must reach, read before the fork,
/// where allocating is fine: the larger of the soft `RLIMIT_NOFILE` and one
/// more than the highest number `/dev/fd` lists. The listing covers descriptors
/// opened above a soft limit lowered after them. When it cannot be read, the
/// hard limit stands in, and [`UNLIMITED_CEILING`] when that is infinite too
/// (and likewise for an infinite soft limit).
///
/// The one residual: a descriptor another thread opens after the listing takes
/// the lowest free number, so it is under the bound unless every lower number
/// is taken.
pub(super) fn descriptor_bound() -> i32 {
    let limit = getrlimit(Resource::Nofile);
    let ceiling = limit.maximum.unwrap_or(UNLIMITED_CEILING);
    let soft = limit.current.unwrap_or(ceiling);
    let bound = match highest_listed() {
        Some(highest) => soft.max(highest.saturating_add(1)),
        None => ceiling,
    };
    i32::try_from(bound).unwrap_or(i32::MAX)
}

/// The highest descriptor number `/dev/fd` lists (the listing's own included,
/// which closes when this returns), or `None` when it cannot be read.
fn highest_listed() -> Option<u64> {
    std::fs::read_dir("/dev/fd")
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u64>().ok())
        .max()
}

/// Marks descriptors `3..bound` close-on-exec; a number that names no open
/// descriptor is skipped. Allocation-free and lock-free: one `fcntl` per number.
pub(super) fn mark_close_on_exec(bound: i32) {
    for fd in 3..bound {
        // SAFETY: the borrow lasts for the one `fcntl` below, which only sets a
        // flag on the number and cannot close or use the descriptor's object.
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        let _ = fcntl_setfd(fd, FdFlags::CLOEXEC);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::io::{fcntl_dupfd_cloexec, fcntl_getfd};
    use rustix::process::{Rlimit, setrlimit};

    /// **A descriptor above a lowered soft limit is still swept**: the bound
    /// reaches it through the `/dev/fd` listing, and the sweep marks it
    /// close-on-exec.
    #[test]
    fn a_descriptor_above_a_lowered_soft_limit_is_marked_close_on_exec() {
        let file = std::fs::File::open("/dev/null").expect("/dev/null opens");
        let high = fcntl_dupfd_cloexec(&file, 200).expect("a descriptor at 200 or above");
        fcntl_setfd(&high, FdFlags::empty()).expect("close-on-exec clears");
        let before = getrlimit(Resource::Nofile);
        setrlimit(
            Resource::Nofile,
            Rlimit {
                current: Some(100),
                maximum: before.maximum,
            },
        )
        .expect("the soft limit lowers");

        let bound = descriptor_bound();
        mark_close_on_exec(bound);
        let marked = fcntl_getfd(&high).expect("the descriptor reads");

        setrlimit(Resource::Nofile, before).expect("the limit restores");
        assert!(marked.contains(FdFlags::CLOEXEC));
    }
}
