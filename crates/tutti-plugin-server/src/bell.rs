//! [`Bell`]: how a plugin's call on any thread makes the main loop run now.
//!
//! The main loop sleeps in one wait over the control socket, the plugin's own
//! descriptors and the next tick. A plugin's `request_process` may be called
//! from its audio thread, which can neither take a lock nor write the control
//! socket, yet its answer (`ProcessRequested`) must leave the server at once,
//! not on the next tick. [`Bell::ring`] sets a flag and makes the platform's
//! wait end; it takes no lock, allocates nothing and never waits. The flag is
//! the one record of the request: the main loop looks at it with
//! [`Bell::rung`] after each wait, and the pass that tells the host takes it
//! with [`Bell::take`]. A `Bell` is built once on the main thread, with the
//! session, and a plugin's wake holds a clone. How the wait ends is the
//! platform module's: this `mod` line carries the bell's only platform `cfg`.

#[cfg(unix)]
#[path = "bell/unix.rs"]
mod sys;
#[cfg(not(unix))]
#[path = "bell/windows.rs"]
mod sys;

pub(crate) use sys::Bell;
