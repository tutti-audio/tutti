//! What ties a spawned server's life to its host's: the process group a
//! server leads (unix), the Job Object the kernel ends it with (Windows).
//!
//! One platform module is chosen here and nowhere else.

#[cfg(unix)]
#[path = "lifetime/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "lifetime/windows.rs"]
mod platform;

pub use platform::{bind_to_host, has_ended, kill_group, own_group};
