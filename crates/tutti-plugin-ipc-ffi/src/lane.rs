//! The audio lane's connection: how the host hands the server one end of a
//! fresh duplex stream over the broker connection.
//!
//! On unix the lane is a socket pair, and the server's end travels as a
//! descriptor beside the frame that announces it (the slab's way). On Windows
//! the lane is a fresh named pipe the server listens on, and the frame carries
//! its name. The platform's module is chosen once below.

#[cfg(unix)]
#[path = "lane/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "lane/windows.rs"]
mod platform;

pub use platform::*;
