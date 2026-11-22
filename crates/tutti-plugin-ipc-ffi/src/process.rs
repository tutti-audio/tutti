//! A spawned process the host holds.
//!
//! One platform module is chosen here and nowhere else: std's `Child` on unix,
//! the process handle `CreateProcessW` returned on Windows.

#[cfg(unix)]
#[path = "process/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "process/windows.rs"]
mod platform;

pub use platform::Process;
