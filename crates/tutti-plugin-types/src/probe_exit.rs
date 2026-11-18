//! How the probe process ends: the statuses it exits with, and how the host
//! reads the status of one that left no result.
//!
//! The probe's own exits are [`ProbeExit`], one enum for the probe that picks a
//! code and the host that reads it back. A process that ends without a result
//! and without one of those codes was ended by something else: a signal, a Rust
//! panic, or a plugin's own `exit`. That is [`ProbeDeath`].

use std::process::ExitStatus;

#[cfg(unix)]
#[path = "probe_exit/unix.rs"]
mod platform;
#[cfg(not(unix))]
#[path = "probe_exit/other.rs"]
mod platform;

/// The status a Rust program ends with when its main thread panics.
const PANIC_STATUS: i32 = 101;

/// A status `plugin-probe` exits with on purpose.
///
/// Only what cannot be written to the result file has a code: a fault the probe
/// can describe (a bad command line, a thread that is not the main one) is a
/// written [`ProbeRefusal`](crate::ProbeRefusal). The codes are few and far from
/// the small ones (1, 2, 3 and 4) a plugin's own `exit` or a runtime uses for
/// its failures, so a plugin that ends the process by `exit` is read as the
/// plugin's doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
#[non_exhaustive]
pub enum ProbeExit {
    /// The result file is written. A plugin that would not load is a result, not
    /// an error.
    Written = 0,
    /// The result file could not be written, or the command line named none:
    /// 75, the "temporary failure" status of the BSD `sysexits` convention, which a
    /// plugin has no reason to use.
    ResultUnwritable = 75,
}

impl ProbeExit {
    const ALL: [Self; 2] = [Self::Written, Self::ResultUnwritable];

    /// The process status this exit is.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// The exit a process status is, if the probe picks it.
    #[must_use]
    pub fn from_code(code: i32) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|exit| i32::from(exit.code()) == code)
    }
}

/// How a probe that left no result ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ProbeDeath {
    /// The OS ended it with a signal: a crash in the plugin, or a kill by another
    /// process, which reads the same (a `SIGKILL`, 9, is also the OOM killer's and
    /// the probe's own end with its host).
    #[error("killed by signal {0}")]
    Signal(i32),
    /// A Rust panic ended it.
    #[error("panicked")]
    Panicked,
    /// It exited by a code of its own ([`ProbeExit`]) and wrote no result.
    #[error("exited ({0:?}) without writing a result")]
    Exited(ProbeExit),
    /// It exited with a status no probe picks: a plugin's own `exit`, or a crash
    /// code of a platform without signals.
    #[error("exited with status {} without writing a result", platform::status_text(*.0))]
    Status(i32),
    /// The platform gave neither a status nor a signal.
    #[error("ended without a status")]
    Unknown,
}

impl ProbeDeath {
    /// How the process whose end is `status` died.
    #[must_use]
    pub fn of(status: ExitStatus) -> Self {
        if let Some(signal) = platform::signal(status) {
            return Self::Signal(signal);
        }
        match status.code() {
            Some(PANIC_STATUS) => Self::Panicked,
            Some(code) => ProbeExit::from_code(code).map_or(Self::Status(code), Self::Exited),
            None => Self::Unknown,
        }
    }

    /// Whether the probe's own code ended it for a reason of the probe or its
    /// environment (a result that could not be written), which says nothing about
    /// the plugin. A plugin's own `exit` is [`Status`](Self::Status), whatever its
    /// code.
    #[must_use]
    pub const fn is_the_probes_own(self) -> bool {
        matches!(self, Self::Exited(ProbeExit::ResultUnwritable))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    /// Every probe code reads back as the exit that wrote it, and no other does:
    /// the small statuses a plugin's own `exit` uses are not the probe's.
    #[test]
    fn a_probe_exit_reads_back_from_its_code() {
        for exit in ProbeExit::ALL {
            assert_eq!(ProbeExit::from_code(i32::from(exit.code())), Some(exit));
        }
        for plugins in [1, 2, 3, 4, PANIC_STATUS] {
            assert_eq!(ProbeExit::from_code(plugins), None, "{plugins}");
        }
    }

    /// A status tells a signal, a panic, a probe's own exit and a plugin's exit
    /// apart (`from_raw` takes a wait status: the exit code in the high byte, a
    /// signal in the low).
    #[test]
    fn a_status_is_told_a_signal_from_a_panic_from_an_exit() {
        let exited = |code: i32| ProbeDeath::of(ExitStatus::from_raw(code << 8));
        assert_eq!(
            ProbeDeath::of(ExitStatus::from_raw(11)),
            ProbeDeath::Signal(11)
        );
        assert_eq!(exited(101), ProbeDeath::Panicked);
        assert_eq!(exited(75), ProbeDeath::Exited(ProbeExit::ResultUnwritable));
        assert_eq!(exited(0), ProbeDeath::Exited(ProbeExit::Written));
        assert_eq!(exited(7), ProbeDeath::Status(7));
    }

    /// **A plugin's own `exit` never reads as the probe's**: the codes a plugin
    /// is likely to use (1 to 4, which the probe once used for a bad command line
    /// and a thread fault) are `Status`, which a scan blacklists. Only the probe's
    /// unwritable result is its own.
    #[test]
    fn a_plugins_exit_is_never_the_probes_own() {
        let exited = |code: i32| ProbeDeath::of(ExitStatus::from_raw(code << 8));
        for plugins in 1..=4 {
            let death = exited(plugins);
            assert_eq!(death, ProbeDeath::Status(plugins));
            assert!(!death.is_the_probes_own(), "exit({plugins})");
        }
        assert!(ProbeDeath::Exited(ProbeExit::ResultUnwritable).is_the_probes_own());
        for plugins in [
            ProbeDeath::Exited(ProbeExit::Written),
            ProbeDeath::Signal(11),
            ProbeDeath::Panicked,
            ProbeDeath::Status(7),
        ] {
            assert!(!plugins.is_the_probes_own(), "{plugins:?}");
        }
    }
}
