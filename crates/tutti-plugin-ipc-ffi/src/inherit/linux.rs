//! Linux: `close_range` marks every descriptor from 3 up close-on-exec.

use super::sweep::{descriptor_bound, mark_close_on_exec};
use std::os::unix::process::CommandExt;
use std::process::Command;

/// Makes the child of `command` inherit only descriptors 0, 1 and 2: every
/// other descriptor the process holds is marked close-on-exec between the fork
/// and the exec (`close_range` with `CLOSE_RANGE_CLOEXEC`, from Linux 5.11;
/// before it, a sweep of `fcntl` calls). Marking, not closing, keeps std's
/// close-on-exec pipe open until the exec, so a failed exec is still reported
/// by `spawn`.
pub fn inherit_only_stdio(command: &mut Command) {
    let bound = descriptor_bound();
    // SAFETY: the hook runs between `fork` and `exec` and calls only the raw
    // `close_range` system call and, on its failure, `mark_close_on_exec`
    // (`fcntl` system calls through `rustix` on a number read before the
    // fork): no allocation, no lock, no `std` I/O, so it is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            // SAFETY: `close_range` takes three integers and reads no memory.
            let done = libc::syscall(
                libc::SYS_close_range,
                3 as libc::c_uint,
                libc::c_uint::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            );
            if done != 0 {
                // ENOSYS (before 5.9) or EINVAL (before 5.11, no flag).
                mark_close_on_exec(bound);
            }
            Ok(())
        });
    }
}
