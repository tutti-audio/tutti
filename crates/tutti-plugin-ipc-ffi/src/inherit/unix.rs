//! Unix without `close_range` (macOS): the sweep runs in the child after the fork.

use super::sweep::{descriptor_bound, mark_close_on_exec};
use std::os::unix::process::CommandExt;
use std::process::Command;

/// Makes the child of `command` inherit only descriptors 0, 1 and 2: every
/// other descriptor the process holds is marked close-on-exec between the fork
/// and the exec. A std `Command` with a `pre_exec` hook spawns with `fork`, not
/// `posix_spawn`.
pub fn inherit_only_stdio(command: &mut Command) {
    let bound = descriptor_bound();
    // SAFETY: the hook runs between `fork` and `exec` and calls only
    // `mark_close_on_exec`, which calls `libc::fcntl`, which POSIX lists as
    // async-signal-safe, up to a bound it was handed before the fork: no
    // allocation, no lock, no `std` I/O, so it is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            mark_close_on_exec(bound);
            Ok(())
        });
    }
}
