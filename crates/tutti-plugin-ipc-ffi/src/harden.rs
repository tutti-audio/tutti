//! The host's own memory against the processes it spawns.
//!
//! One platform module is chosen here and nowhere else: Linux's dumpable flag,
//! and nothing elsewhere (macOS's equivalent is the app's hardened-runtime
//! signing).

#[cfg(target_os = "linux")]
#[path = "harden/linux.rs"]
mod platform;
#[cfg(not(target_os = "linux"))]
#[path = "harden/other.rs"]
mod platform;

pub use platform::protect_host_memory;
