//! The descriptor sweep: every descriptor from 3 up is marked close-on-exec.

use rustix::process::{Resource, getrlimit};

/// The bound of the sweep when the listing cannot be read and the process has
/// no descriptor limit at all.
const UNLIMITED_CEILING: u64 = 1 << 20;

/// How far past the highest listed descriptor the sweep reaches, for
/// descriptors other threads open between the listing and the fork.
const SLACK: u64 = 1024;

/// One past the highest descriptor the sweep reaches, read before the fork,
/// where allocating is fine: the highest number `/dev/fd` lists plus one and
/// [`SLACK`]. The bound follows what is open, not the limit, so a limit of
/// 2^30 does not make the sweep a billion calls. The listing also covers
/// descriptors opened above a soft limit lowered after them. When it cannot be
/// read, the soft `RLIMIT_NOFILE` stands in, then the hard limit, then
/// [`UNLIMITED_CEILING`].
///
/// The one residual: more than [`SLACK`] descriptors opened by other threads
/// between the listing and the fork (each takes the lowest free number, so
/// fewer fall above the bound).
pub(super) fn descriptor_bound() -> i32 {
    let bound = match highest_listed() {
        Some(highest) => highest.saturating_add(1 + SLACK),
        None => {
            let limit = getrlimit(Resource::Nofile);
            limit.current.or(limit.maximum).unwrap_or(UNLIMITED_CEILING)
        }
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
/// descriptor fails with `EBADF`, which is ignored. Allocation-free and
/// lock-free: one `fcntl` per number.
pub(super) fn mark_close_on_exec(bound: i32) {
    for fd in 3..bound {
        // SAFETY: `fcntl` is on POSIX's async-signal-safe list, and with
        // `F_SETFD` it takes three integers and reads no memory; a number that
        // names no descriptor only returns an error.
        let _ = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::io::{FdFlags, fcntl_dupfd_cloexec, fcntl_getfd, fcntl_setfd};
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

    /// **The bound follows the open descriptors, not the limit**: with the
    /// soft limit at the hard one, the bound is still within [`SLACK`] of the
    /// highest listed descriptor.
    #[test]
    fn the_bound_stays_near_the_highest_open_descriptor_under_a_huge_limit() {
        let before = getrlimit(Resource::Nofile);
        setrlimit(
            Resource::Nofile,
            Rlimit {
                current: before.maximum,
                maximum: before.maximum,
            },
        )
        .expect("the soft limit raises to the hard one");

        let bound = u64::try_from(descriptor_bound()).expect("positive");
        let highest = highest_listed().expect("/dev/fd lists");

        setrlimit(Resource::Nofile, before).expect("the limit restores");
        assert!(bound > highest);
        assert!(bound <= highest + 1 + SLACK);
    }
}
