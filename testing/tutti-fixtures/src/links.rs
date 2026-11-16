//! How a platform publishes a plugin under another name: a symlink on Unix, a
//! copy on Windows.

#[cfg_attr(unix, path = "links/unix.rs")]
#[cfg_attr(windows, path = "links/windows.rs")]
mod platform;

pub(crate) use platform::{is_published, stage};
