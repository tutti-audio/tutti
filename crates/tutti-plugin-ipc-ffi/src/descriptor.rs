//! A descriptor over a connected Unix socket: one byte whose ancillary data
//! carries it.
//!
//! The byte is a message of its own, sent after the frame that announces it
//! and received by the exact read that follows that frame, so no read of the
//! stream's frames can swallow it: a plain `read` that crosses a message
//! carrying descriptors discards them.

use crate::SlabError;
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};
use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::time::Duration;

/// The one payload byte of a descriptor message.
const MARK: u8 = 0xFD;

/// Sends `fd` over `socket` as one byte carrying it.
///
/// # Errors
///
/// The OS error of `sendmsg`, a send timeout included.
pub(crate) fn send(socket: BorrowedFd<'_>, fd: BorrowedFd<'_>) -> std::io::Result<()> {
    let fds = [fd];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    let pushed = control.push(SendAncillaryMessage::ScmRights(&fds));
    debug_assert!(pushed, "the buffer is sized for one descriptor");
    let sent = sendmsg(
        socket,
        &[IoSlice::new(&[MARK])],
        &mut control,
        SendFlags::empty(),
    )?;
    if sent == 1 {
        Ok(())
    } else {
        Err(std::io::ErrorKind::WriteZero.into())
    }
}

/// Receives the one descriptor [`send`] sends, waiting up to `patience` for
/// its byte to begin. `flags` are the platform's `recvmsg` flags (the one that
/// closes the descriptor on `exec`, where the platform has it).
///
/// Any descriptor that arrives beside the wrong message is closed.
///
/// # Errors
///
/// [`SlabError::NoDescriptor`] if the byte arrives without a descriptor or the
/// patience ends; [`SlabError::ExtraDescriptors`] for more than one or a
/// control message the buffer could not hold; [`SlabError::Transfer`] for an
/// OS error, the peer closing included.
pub(crate) fn recv(
    socket: BorrowedFd<'_>,
    flags: RecvFlags,
    patience: Duration,
) -> Result<OwnedFd, SlabError> {
    let transfer = |source| SlabError::Transfer { source };
    if !crate::wait_readable(socket, Some(patience)).map_err(transfer)? {
        return Err(SlabError::NoDescriptor);
    }
    // Room for two descriptors: a second is seen, and refused, not truncated away.
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let mut byte = [0u8; 1];
    let got = recvmsg(
        socket,
        &mut [IoSliceMut::new(&mut byte)],
        &mut control,
        flags,
    )
    .map_err(|e| transfer(e.into()))?;
    // Every descriptor that arrived is owned from here, so each path closes it.
    let mut fds: Vec<OwnedFd> = control
        .drain()
        .flat_map(|message| match message {
            RecvAncillaryMessage::ScmRights(fds) => fds.collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect();
    if got.bytes == 0 {
        return Err(transfer(std::io::ErrorKind::UnexpectedEof.into()));
    }
    if byte[0] != MARK {
        // Not the byte a descriptor rides on: the stream is out of step.
        return Err(SlabError::BadMark { found: byte[0] });
    }
    if got.flags.contains(ReturnFlags::CTRUNC) || fds.len() > 1 {
        return Err(SlabError::ExtraDescriptors);
    }
    fds.pop().ok_or(SlabError::NoDescriptor)
}

/// The connection a slab is sent over: the socket's descriptor.
pub type Connection<'a> = BorrowedFd<'a>;

/// What `SetupSharedMemory` carries of a slab: nothing, as the descriptor
/// travels beside the frame.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SlabWire;

/// `op` failed with an OS error, as a slab error.
pub(crate) fn file_error(op: &'static str) -> impl Fn(rustix::io::Errno) -> SlabError {
    move |errno| SlabError::File {
        op,
        source: errno.into(),
    }
}

/// A second descriptor of `fd`'s file, to map.
pub(crate) fn duplicate(fd: &OwnedFd) -> Result<std::fs::File, SlabError> {
    fd.try_clone()
        .map(std::fs::File::from)
        .map_err(|source| SlabError::File {
            op: "duplicate",
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MappingWriter, SLAB_HEADER_BYTES, SlabHandle};
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage};
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    const SIZE: usize = SLAB_HEADER_BYTES + 64;

    /// **Sending a slab maps the same memory in the receiver**: a write by the
    /// sender is read through the received handle, and the stream's bytes before
    /// and after the descriptor still arrive in order.
    #[test]
    fn a_sent_slab_is_the_same_memory_and_the_stream_stays_in_step() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let (mut host, handle) = MappingWriter::create(SIZE).unwrap();
        a.write_all(b"before").unwrap();
        handle.send(a.as_fd()).unwrap();
        a.write_all(b"after").unwrap();
        let mut head = [0u8; 6];
        b.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"before");
        let got = SlabHandle::receive(b.as_fd(), handle.wire()).unwrap();
        let mut tail = [0u8; 5];
        b.read_exact(&mut tail).unwrap();
        assert_eq!(&tail, b"after");

        host.write(SLAB_HEADER_BYTES, &[9u8; 16]);
        let peer = MappingWriter::open(&got, SIZE).unwrap();
        let mut back = [0u8; 16];
        peer.mapping().read(SLAB_HEADER_BYTES, &mut back);
        assert_eq!(back, [9u8; 16]);
    }

    /// A byte that is not the descriptor's mark is refused by name, with its
    /// descriptor closed: the stream is out of step with the sender.
    #[test]
    fn a_byte_that_is_not_the_mark_is_refused_and_its_descriptor_closed() {
        let (a, b) = UnixStream::pair().unwrap();
        let (x, _x) = UnixStream::pair().unwrap();
        let fds = [x.as_fd()];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        assert!(control.push(SendAncillaryMessage::ScmRights(&fds)));
        sendmsg(
            &a,
            &[IoSlice::new(&[MARK ^ 1])],
            &mut control,
            SendFlags::empty(),
        )
        .unwrap();
        let before = open_fds();
        let err = recv(b.as_fd(), RecvFlags::empty(), Duration::from_secs(5))
            .expect_err("the wrong byte");
        assert!(
            matches!(err, SlabError::BadMark { found } if found == MARK ^ 1),
            "{err}"
        );
        assert_eq!(open_fds(), before, "the refused descriptor stayed open");
    }

    /// A byte with no descriptor beside it is refused by name.
    #[test]
    fn a_byte_without_a_descriptor_is_refused() {
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(&[MARK]).unwrap();
        let err =
            recv(b.as_fd(), RecvFlags::empty(), Duration::from_secs(5)).expect_err("no descriptor");
        assert!(matches!(err, SlabError::NoDescriptor), "{err}");
    }

    /// A peer that sent nothing ends the wait in `NoDescriptor` at its patience,
    /// not in a read that never returns.
    #[test]
    fn a_silent_peer_ends_in_no_descriptor() {
        let (_a, b) = UnixStream::pair().unwrap();
        let err = recv(b.as_fd(), RecvFlags::empty(), Duration::ZERO).expect_err("silent");
        assert!(matches!(err, SlabError::NoDescriptor), "{err}");
    }

    /// A peer that closed mid-message is a transfer error, not a hang.
    #[test]
    fn a_closed_peer_is_a_transfer_error() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(a);
        let err = recv(b.as_fd(), RecvFlags::empty(), Duration::from_secs(5)).expect_err("closed");
        assert!(matches!(err, SlabError::Transfer { .. }), "{err}");
    }

    /// Two descriptors in one message are refused, and both are closed.
    #[test]
    fn two_descriptors_are_refused_and_closed() {
        let (a, b) = UnixStream::pair().unwrap();
        let (x, _x) = UnixStream::pair().unwrap();
        let (y, _y) = UnixStream::pair().unwrap();
        let fds = [x.as_fd(), y.as_fd()];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        assert!(control.push(SendAncillaryMessage::ScmRights(&fds)));
        sendmsg(
            &a,
            &[IoSlice::new(&[MARK])],
            &mut control,
            SendFlags::empty(),
        )
        .unwrap();
        let before = open_fds();
        let err = recv(b.as_fd(), RecvFlags::empty(), Duration::from_secs(5))
            .expect_err("two descriptors");
        assert!(matches!(err, SlabError::ExtraDescriptors), "{err}");
        assert_eq!(open_fds(), before, "a refused descriptor stayed open");
    }

    /// **No descriptor leaks across setups**: creating, sending, receiving,
    /// mapping and dropping a slab many times leaves the process with the
    /// descriptors it began with.
    #[test]
    fn many_setups_leak_no_descriptors() {
        let (a, b) = UnixStream::pair().unwrap();
        let before = open_fds();
        for _ in 0..64 {
            let (host, handle) = MappingWriter::create(SIZE).unwrap();
            handle.send(a.as_fd()).unwrap();
            let got = SlabHandle::receive(b.as_fd(), handle.wire()).unwrap();
            let peer = MappingWriter::open(&got, SIZE).unwrap();
            drop((host, handle, got, peer));
        }
        assert_eq!(open_fds(), before);
    }

    /// The process's open descriptors, from the kernel's list.
    fn open_fds() -> usize {
        std::fs::read_dir("/dev/fd").unwrap().count()
    }
}
