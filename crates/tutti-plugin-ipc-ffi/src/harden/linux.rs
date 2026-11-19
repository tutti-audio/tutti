//! Linux: a process that is not dumpable cannot be `ptrace`d, nor have its
//! memory written through `/proc`, by another process of the same user.

use rustix::process::DumpableBehavior;

/// Clears this process's dumpable flag (PR_SET_DUMPABLE 0). The flag is this
/// process's own: a process it spawns is dumpable again after its `exec`.
///
/// It also stops this process dumping core and, for an unprivileged debugger
/// or crash handler of the same user, ptrace attach: a host calls it only when
/// its app chose that.
///
/// # Errors
///
/// The OS error of `prctl`.
pub fn protect_host_memory() -> std::io::Result<()> {
    rustix::process::set_dumpable_behavior(DumpableBehavior::NotDumpable)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The flag reads back cleared**, from the kernel's own report.
    #[test]
    fn the_kernel_reports_the_flag_cleared() {
        // The flag is per process, and nextest runs each test in its own.
        protect_host_memory().expect("prctl accepts the flag");
        let dumpable = rustix::process::dumpable_behavior().expect("prctl reads it back");
        assert_eq!(dumpable, DumpableBehavior::NotDumpable);
    }
}
