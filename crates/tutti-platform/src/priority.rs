//! [`request_priority`]: ask the OS to schedule the calling thread ahead of
//! ordinary ones, and say what it granted.
//!
//! The call is a request, not a promise. It only asks for a higher priority
//! within the OS's ordinary scheduler (on Linux `SCHED_OTHER`, so a lower nice
//! value), never a real-time class, and the OS may refuse it: Linux with
//! `EPERM`/`EACCES` below the process's `RLIMIT_NICE` (an unprivileged user,
//! a container, most CI), and the web, which has no thread priorities, always refuses.
//! What each request means per OS: Linux nice -20 (`Highest`) and nice -10
//! (`Raised`), Windows `HIGHEST` and `ABOVE_NORMAL`, macOS and iOS the
//! `SCHED_OTHER` maximum and a quarter of the way down from it.
//! The result is read back as a [`PriorityOutcome`], so a caller logs what it got,
//! rather than the name of what it asked for.
//!
//! This is control-side code, run once when a thread starts; it allocates
//! (the refusal's message) and is not for the audio callback.

use std::fmt;

/// What a thread asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PriorityRequest {
    /// The highest priority the scheduler grants the thread's current
    /// policy: nice -20 on Linux, `HIGHEST` on Windows. For a thread with a
    /// deadline, such as a plugin's audio phase.
    Highest,
    /// A step above ordinary threads, never real time: about nice -10 on
    /// Linux, `ABOVE_NORMAL` on Windows, between the default and the maximum
    /// on macOS. For a thread that should keep ahead of the machine's
    /// background work but not of the audio threads, such as disk streaming.
    Raised,
}

/// The scheduling class the thread ended up in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scheduling {
    /// A real-time policy (`SCHED_FIFO`/`SCHED_RR`): the thread already was
    /// one, and kept it. This call never moves a thread into one.
    Realtime,
    /// The OS's ordinary time-sharing scheduler, with its priority raised.
    TimeSharing,
}

/// What [`request_priority`] got. `Display` is the line to log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PriorityOutcome {
    /// The OS accepted the request.
    Granted {
        /// What was asked for.
        requested: PriorityRequest,
        /// The class the thread is scheduled in.
        scheduling: Scheduling,
    },
    /// The OS refused; the thread's priority is unchanged.
    Refused {
        /// What was asked for.
        requested: PriorityRequest,
        /// The OS error, as text.
        error: String,
    },
}

impl PriorityOutcome {
    /// Whether the OS accepted the request.
    #[must_use]
    pub fn is_granted(&self) -> bool {
        matches!(self, PriorityOutcome::Granted { .. })
    }
}

impl fmt::Display for PriorityOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PriorityOutcome::Granted {
                requested,
                scheduling,
            } => write!(
                f,
                "thread priority {requested:?} granted, scheduled {}",
                match scheduling {
                    Scheduling::Realtime => "real time",
                    Scheduling::TimeSharing => "time-sharing, not real time",
                }
            ),
            PriorityOutcome::Refused { requested, error } => write!(
                f,
                "thread priority {requested:?} refused ({error}); the thread keeps its priority"
            ),
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), path = "priority/native.rs")]
#[cfg_attr(target_arch = "wasm32", path = "priority/wasm.rs")]
mod os;

/// Ask the OS to raise the **calling thread**'s priority, and report what it
/// did. A refusal changes nothing and is harmless to the caller.
///
/// ```
/// use tutti_platform::{PriorityRequest, request_priority};
///
/// // Unprivileged, this is a refusal; privileged, a grant. Either is an answer.
/// let outcome = request_priority(PriorityRequest::Raised);
/// println!("{outcome}");
/// ```
#[must_use]
pub fn request_priority(request: PriorityRequest) -> PriorityOutcome {
    os::request(request)
}
