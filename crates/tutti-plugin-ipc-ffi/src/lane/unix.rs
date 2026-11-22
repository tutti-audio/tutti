//! The audio lane on unix: a socket pair, one end sent as a descriptor.

use crate::descriptor;
use crate::{Connection, SlabError};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

/// What `OpenLane` carries of a lane: nothing, as the server's end travels as a
/// descriptor beside the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct LaneWire;

impl LaneWire {
    /// The wire of a new lane: nothing to name on unix.
    ///
    /// # Errors
    ///
    /// Never on unix.
    #[allow(clippy::unnecessary_wraps)]
    pub fn fresh() -> Result<Self, SlabError> {
        Ok(Self)
    }
}

/// A fresh duplex stream's two ends, the host's first: neither is inherited by
/// a child process.
///
/// # Errors
///
/// The OS refused the socket pair.
pub fn lane_pair() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let (host, server) = UnixStream::pair()?;
    Ok((host.into(), server.into()))
}

/// Sends `end`, the server's end of a lane, over `conn` as the message that
/// follows the frame announcing it.
///
/// # Errors
///
/// The OS error of the send, a send timeout included.
pub fn send_lane(conn: Connection<'_>, end: BorrowedFd<'_>) -> std::io::Result<()> {
    descriptor::send(conn, end)
}

/// Receives the lane end a frame announced, over `conn`.
///
/// # Errors
///
/// [`SlabError::NoDescriptor`] when none arrives, [`SlabError::ExtraDescriptors`]
/// when more than one does, and [`SlabError::Transfer`] for an OS error.
pub fn receive_lane(conn: Connection<'_>, _: &LaneWire) -> Result<OwnedFd, SlabError> {
    crate::descriptor::receive(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;

    /// **A lane end sent over a broker is a live duplex stream**: the broker's
    /// bytes before and after the descriptor arrive in order, and the received end
    /// talks to the end the sender kept, both ways.
    #[test]
    fn a_lane_end_sent_over_the_broker_is_a_live_duplex_stream() {
        let (mut broker_host, broker_server) = UnixStream::pair().unwrap();
        let (host_end, server_end) = lane_pair().unwrap();
        broker_host.write_all(b"before").unwrap();
        send_lane(broker_host.as_fd(), server_end.as_fd()).unwrap();
        broker_host.write_all(b"after").unwrap();
        drop(server_end);

        let mut head = [0u8; 6];
        (&broker_server).read_exact(&mut head).unwrap();
        assert_eq!(&head, b"before");
        let got = receive_lane(broker_server.as_fd(), &LaneWire).unwrap();
        let mut tail = [0u8; 5];
        (&broker_server).read_exact(&mut tail).unwrap();
        assert_eq!(&tail, b"after");

        let (mut host, mut server) = (UnixStream::from(host_end), UnixStream::from(got));
        host.write_all(b"ping").unwrap();
        let mut ping = [0u8; 4];
        server.read_exact(&mut ping).unwrap();
        assert_eq!(&ping, b"ping");
        server.write_all(b"pong").unwrap();
        let mut pong = [0u8; 4];
        host.read_exact(&mut pong).unwrap();
        assert_eq!(&pong, b"pong");
    }

    /// **A lane's two ends close with their holders**: after both drop, the
    /// process has the descriptors it began with.
    #[test]
    fn the_descriptors_of_a_lane_return_to_baseline_after_both_ends_drop() {
        let (a, b) = UnixStream::pair().unwrap();
        let before = open_fds();
        for _ in 0..16 {
            let (host, server) = lane_pair().unwrap();
            send_lane(a.as_fd(), server.as_fd()).unwrap();
            drop(server);
            let got = receive_lane(b.as_fd(), &LaneWire).unwrap();
            drop((host, got));
        }
        assert_eq!(open_fds(), before);
    }

    /// A lane's ends are closed on `exec`, so a spawned child inherits neither.
    #[test]
    fn the_ends_of_a_lane_close_on_exec() {
        use rustix::io::{FdFlags, fcntl_getfd};
        let (host, server) = lane_pair().unwrap();
        for end in [&host, &server] {
            assert!(fcntl_getfd(end).unwrap().contains(FdFlags::CLOEXEC));
        }
    }

    fn open_fds() -> usize {
        std::fs::read_dir("/dev/fd").unwrap().count()
    }
}
