//! Host-side transports between this crate and the plugin-server subprocess.
//!
//! Two transports:
//! - [`control`] — typed length-prefixed postcard over a local socket for
//!   commands and responses.
//! - [`shm`] — named shared-memory audio storage for bulk sample transfer.
//!
pub mod control;
pub mod shm;

// The control stream's platform difference (socket naming, waiting for bytes)
// is `sys`'s: this `mod` line carries the transport's only platform `cfg`.
#[cfg(unix)]
#[path = "sys/unix.rs"]
mod sys;
#[cfg(not(unix))]
#[path = "sys/windows.rs"]
mod sys;

pub(crate) use sys::PendingLane;
#[cfg(any(test, feature = "testing"))]
pub use sys::broker_pair;
/// What a mock server receives a slab over.
#[cfg(test)]
pub(crate) use sys::connection;
pub(crate) use sys::socket_path_in;
