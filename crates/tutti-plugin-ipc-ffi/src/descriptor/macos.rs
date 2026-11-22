//! Receiving a descriptor on macOS: `recvmsg` has no close-on-exec flag, so it is
//! set right after.

use super::{DESCRIPTOR_PATIENCE, recv};
use crate::{Connection, SlabError};
use rustix::io::{FdFlags, fcntl_setfd};
use rustix::net::RecvFlags;
use std::os::fd::OwnedFd;

/// Receives the one descriptor a frame announced, over `conn`, closed on `exec`
/// as soon as it is received (the platform's `recvmsg` cannot do it atomically).
///
/// A thread that forks between the receive and the flag can hand the descriptor
/// to its child. Nothing reads a lane's or a slab's end-of-file as the peer's
/// death: the host learns that from the process's exit wait.
///
/// # Errors
///
/// As [`recv`], and the OS error of setting the flag.
pub(crate) fn receive(conn: Connection<'_>) -> Result<OwnedFd, SlabError> {
    let fd = recv(conn, RecvFlags::empty(), DESCRIPTOR_PATIENCE)?;
    fcntl_setfd(&fd, FdFlags::CLOEXEC).map_err(|e| SlabError::Transfer { source: e.into() })?;
    Ok(fd)
}
