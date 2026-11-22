//! Receiving a descriptor on Linux: `recvmsg` closes it on `exec` as it arrives.

use super::{DESCRIPTOR_PATIENCE, recv};
use crate::{Connection, SlabError};
use rustix::net::RecvFlags;
use std::os::fd::OwnedFd;

/// Receives the one descriptor a frame announced, over `conn`, closed on `exec`
/// from the moment it arrives.
///
/// # Errors
///
/// As [`recv`].
pub(crate) fn receive(conn: Connection<'_>) -> Result<OwnedFd, SlabError> {
    recv(conn, RecvFlags::CMSG_CLOEXEC, DESCRIPTOR_PATIENCE)
}
