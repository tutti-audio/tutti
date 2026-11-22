//! What ties a spawned server's life to its host's: the process group a
//! server leads (unix), the Job Object the kernel ends it with (Windows).
//!
//! One platform module is chosen here and nowhere else. Windows' assignment to
//! the job (`bind_to_host` there) is the spawn's alone.

#[cfg(unix)]
#[path = "lifetime/unix.rs"]
pub(crate) mod platform;
#[cfg(windows)]
#[path = "lifetime/windows.rs"]
pub(crate) mod platform;

pub use platform::{has_ended, kill_group};
