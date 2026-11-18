//! IPC transport — sync wire framing for host↔server message pairs.
//!
//! Framing: u32 big-endian length prefix + postcard payload. The payload's
//! schema and its version constant are `tutti_plugin_ipc`'s; this module
//! only moves bytes.
//!
//! The other duty here is **not outliving the host**. A subprocess parked in
//! `accept` has nothing to interrupt it, so both [`wait_for_connection`] and
//! `parent_is_alive` (the backend's) exist to bound that wait by whether there is still
//! anyone to serve.

use interprocess::local_socket::{ListenerOptions, traits::Listener as _};
use tutti_plugin_ipc::{
    BridgeMessage, FrameBuf, HostMessage, MAX_FRAME_BYTES, read_frame, write_frame,
};
use tutti_plugin_types::{BridgeResult, MIDI_STACK_CAPACITY};

use crate::main_loop::{
    Listener as PlatformListener, Stream as PlatformStream, wait_for_connection,
};

/// The longest request the audio connection takes: whatever the protocol
/// allows ([`MAX_FRAME_BYTES`]). A block's request carries automation at the
/// host's point budget for the activated block and queue count, which is
/// megabytes at a large block, so the buffer holds the protocol's whole bound
/// rather than a guess. The memory is zeroed pages the OS maps on first touch:
/// a connection that never carries a large block never makes them resident.
const AUDIO_REQUEST_BYTES: usize = MAX_FRAME_BYTES;

/// The longest reply the audio connection sends, in bytes: a block's reply
/// with its MIDI at the cap, each event at the most its varints take.
pub(crate) const AUDIO_REPLY_BYTES: usize = MIDI_STACK_CAPACITY * 32 + 256;

/// One framed, blocking message channel to the host.
///
/// A trait rather than the concrete socket so [`crate::session::Session`] can be
/// driven from a test double without a real socket pair.
pub(crate) trait Transport: Send {
    /// Block until one complete host message arrives.
    ///
    /// # Errors
    ///
    /// Returns an error on EOF (the host disconnected), a short read, a length
    /// prefix over `MAX_FRAME_BYTES` (a peer that cannot frame), or a payload
    /// postcard cannot decode — the last meaning a protocol-version mismatch the
    /// `Ready` handshake failed to catch.
    fn recv(&mut self) -> BridgeResult<HostMessage>;

    /// Wait up to `timeout` for the host to have something to read: `Ok(true)`
    /// when a [`recv`](Self::recv) now will find a frame begun (or the host gone,
    /// which `recv` reports as the error it is), `Ok(false)` on a timeout.
    ///
    /// This is what lets one thread serve the socket and the editor's idle tick
    /// ([`crate::main_loop`]): it returns **the instant** a byte arrives, so a
    /// block's request is read as soon as it was when the loop blocked in
    /// `recv`.
    ///
    /// # Errors
    ///
    /// The OS refuses the wait.
    fn wait_readable(&mut self, timeout: std::time::Duration) -> BridgeResult<bool>;

    /// The connection's own OS stream, for a wait that also covers descriptors
    /// of the plugin's ([`crate::main_loop`]); `None` for a transport that is
    /// not one (an in-memory pair), which is waited on through
    /// [`wait_readable`](Self::wait_readable) alone.
    // Only the unix backend polls it with the plugin's descriptors; elsewhere
    // no plugin has any to add.
    #[cfg_attr(not(unix), allow(dead_code))]
    fn stream(&self) -> Option<&PlatformStream> {
        None
    }

    /// Write one length-prefixed message. Returns once it is handed to the OS,
    /// which is not a guarantee the host has read it.
    ///
    /// # Errors
    ///
    /// Returns an error if the payload cannot be serialized, or if the write
    /// fails — a broken pipe here means the host went away.
    fn send(&mut self, msg: &BridgeMessage) -> BridgeResult<()>;
}

/// [`Transport`] over the platform's local socket (Unix domain socket, or a
/// Windows named pipe).
pub(crate) struct SocketTransport {
    stream: PlatformStream,
    /// The audio connection's frame buffers, of fixed capacity, so a block's
    /// frames allocate nothing on the render thread; `None` for a connection
    /// that is served off it.
    frames: Option<FrameBuf>,
}

impl SocketTransport {
    /// Wrap an already-accepted stream. The stream must be in blocking mode;
    /// see [`TransportListener::accept`] for why that is not incidental.
    pub(crate) fn new(stream: PlatformStream) -> Self {
        Self {
            stream,
            frames: None,
        }
    }

    /// Wraps the audio connection's stream, which the render thread reads and
    /// writes through buffers built here: a frame is never read into, or
    /// serialized into, memory that is sized on that thread.
    pub(crate) fn audio(stream: PlatformStream) -> Self {
        Self {
            stream,
            frames: Some(FrameBuf::new(AUDIO_REQUEST_BYTES, AUDIO_REPLY_BYTES)),
        }
    }
}

impl Transport for SocketTransport {
    fn recv(&mut self) -> BridgeResult<HostMessage> {
        // The frame is bounded before it is allocated (see
        // [`MAX_FRAME_BYTES`]). The server is the more exposed end: it loads
        // untrusted plugin binaries, and one that corrupts its memory could
        // otherwise turn a bad length into a 4 GiB allocation.
        //
        // The verdict differs from the host's on purpose. The host reads an
        // oversize length as `ProcessCrashed`; here the peer is the host,
        // which has not crashed, so the error names the frame and the session
        // ends on it either way.
        match self.frames.as_mut() {
            Some(frames) => frames.read(&mut self.stream),
            None => read_frame(&mut self.stream),
        }
    }

    /// Waits for the host's next frame on the platform's wait
    /// ([`crate::main_loop`], which owns what each OS waits with).
    fn wait_readable(&mut self, timeout: std::time::Duration) -> BridgeResult<bool> {
        crate::main_loop::wait_readable(&self.stream, timeout)
    }

    fn stream(&self) -> Option<&PlatformStream> {
        Some(&self.stream)
    }

    fn send(&mut self, msg: &BridgeMessage) -> BridgeResult<()> {
        // Reachable with an oversize body through `StateData`, whose chunk is
        // whatever the plugin hands back from `get_state`.
        match self.frames.as_mut() {
            Some(frames) => frames.write(&mut self.stream, msg),
            None => write_frame(&mut self.stream, msg),
        }
    }
}

/// The listening endpoint the host connects to, twice: once for the handshake
/// phase and once for the audio phase.
pub(crate) struct TransportListener {
    listener: PlatformListener,
}

impl TransportListener {
    /// Create the socket at `socket_path`, removing any stale file there first.
    ///
    /// # Errors
    ///
    /// Returns an error if the path is not a valid socket name, or if the
    /// socket cannot be created — a directory that does not exist, or one this
    /// process may not write to.
    pub(crate) fn bind(socket_path: &std::path::Path) -> BridgeResult<Self> {
        // A no-op on Windows, where the endpoint is a pipe rather than a file.
        let _ = std::fs::remove_file(socket_path);
        // Must agree with the host's `connect`, which is why both go through the
        // one helper: a listener bound to a filesystem path and a client dialling
        // a pipe name would simply never meet.
        let name = tutti_plugin_ipc::socket_name(socket_path)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let listener = ListenerOptions::new().name(name).create_sync_as()?;
        Ok(Self { listener })
    }

    /// Wait for the host to connect, giving up if the host dies first.
    ///
    /// The accept itself is blocking, and the listener is never put into
    /// non-blocking mode — that is what keeps the accepted stream from
    /// inheriting `O_NONBLOCK`. The bail-out lives in [`wait_for_connection`],
    /// which runs *before* the accept and can notice a dead host.
    ///
    /// Without that check, a host killed between the two connections this server
    /// accepts — handshake, then audio — strands the subprocess permanently. The
    /// audio loop exits cleanly on EOF, but it is never reached: the server is
    /// parked in the second accept with nothing to interrupt it. One orphaned
    /// process per loaded plugin, every time a DAW crashes or is force-quit.
    pub(crate) fn accept(&self) -> BridgeResult<SocketTransport> {
        Ok(SocketTransport::new(self.accept_stream()?))
    }

    /// [`accept`](Self::accept) for the audio connection, whose transport reads
    /// and writes through fixed buffers ([`SocketTransport::audio`]).
    pub(crate) fn accept_audio(&self) -> BridgeResult<SocketTransport> {
        Ok(SocketTransport::audio(self.accept_stream()?))
    }

    fn accept_stream(&self) -> BridgeResult<PlatformStream> {
        // Held across the accept: where the wait cannot interrupt a blocked
        // accept, this is what notices a host that dies during it.
        let _watching = wait_for_connection(&self.listener)?;
        Ok(self.listener.accept()?)
    }
}

#[cfg(all(test, unix))]
mod recv_tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use tutti_plugin_ipc::MAX_FRAME_BYTES;
    use tutti_plugin_types::BridgeError;

    /// A length prefix over the cap is a framing violation by the host, not
    /// this process crashing: the error names the frame and the limit, and is
    /// refused before any allocation (no body is ever sent).
    #[test]
    fn an_oversize_inbound_frame_is_a_protocol_error_not_a_crash() {
        let (mut host, server) = UnixStream::pair().expect("socket pair");
        // A regression that waits for a body fails on this timeout rather
        // than hanging the suite.
        server
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("read timeout");
        let mut transport = SocketTransport::new(PlatformStream::from(OwnedFd::from(server)));
        let len = u32::try_from(MAX_FRAME_BYTES + 1).expect("the cap fits a u32 prefix");
        host.write_all(&len.to_be_bytes()).expect("write prefix");
        match transport.recv() {
            Err(BridgeError::FrameTooLarge(e)) => assert_eq!(e.len, MAX_FRAME_BYTES + 1),
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }
}

#[cfg(all(test, unix))]
mod wait_tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    fn pair() -> (UnixStream, SocketTransport) {
        let (host, server) = UnixStream::pair().expect("socket pair");
        (
            host,
            SocketTransport::new(PlatformStream::from(OwnedFd::from(server))),
        )
    }

    /// **A quiet socket's wait ends unreadable when nothing arrives, and a byte
    /// ends the wait.** The loop's idle tick is the first; a block's request reaching
    /// the plugin as it arrives is the second: the readable wait's timeout
    /// outlasts the test harness's limit, so a wait that ran it out fails.
    #[test]
    fn a_quiet_wait_ends_unreadable_and_a_byte_ends_it() {
        let (mut host, mut transport) = pair();

        assert!(!transport.wait_readable(Duration::from_millis(40)).unwrap());

        host.write_all(&[0]).expect("write");
        assert!(transport.wait_readable(Duration::from_secs(600)).unwrap());
    }

    /// A host that hangs up is readable: `recv` reports the end as the error
    /// it is, rather than the loop ticking on for ever.
    #[test]
    fn a_closed_host_is_readable() {
        let (host, mut transport) = pair();
        drop(host);
        assert!(transport.wait_readable(Duration::from_secs(10)).unwrap());
        assert!(transport.recv().is_err());
    }
}

#[cfg(test)]
mod audio_frame_tests {
    use super::*;
    use tutti_plugin_ipc::ProcessAudioData;
    use tutti_plugin_types::{ParameterChanges, ParameterQueue, SampleRate};
    use tutti_types::ParamId;

    /// **The audio connection accepts the largest request the host may send.**
    /// A block of 65536 frames with 256 queues each at the whole chunk room (the
    /// most the host's point pool lets 256 queues carry) encodes to more than
    /// 4 MiB, a size an earlier fixed cap refused, and it is read through the
    /// buffer the audio connection is built with.
    #[test]
    fn the_largest_legal_block_request_is_accepted() {
        const QUEUES: u32 = 256;
        const FRAMES: usize = 65536;
        let room = ParameterQueue::chunk_room(FRAMES);
        let mut changes = ParameterChanges::reserved(QUEUES as usize, room);
        for queue in 0..QUEUES {
            for point in 0..room {
                changes.add_change(
                    ParamId::new(0xF000_0000 + queue),
                    i32::try_from(point * 28).unwrap(),
                    point as f64 / room as f64,
                );
            }
        }
        let block = HostMessage::ProcessAudio(ProcessAudioData {
            seq: 1,
            num_samples: FRAMES,
            param_changes: changes,
            ..ProcessAudioData::empty(SampleRate::SR_48K)
        });
        let mut wire = Vec::new();
        write_frame(&mut wire, &block).unwrap();
        assert!(
            wire.len() > 4 * 1024 * 1024,
            "the case is past the old cap, {} bytes",
            wire.len()
        );
        let mut frames = FrameBuf::new(AUDIO_REQUEST_BYTES, AUDIO_REPLY_BYTES);
        let HostMessage::ProcessAudio(back) = frames.read(&mut wire.as_slice()).unwrap() else {
            panic!("a block");
        };
        let points: usize = back
            .param_changes
            .queues
            .iter()
            .map(ParameterQueue::len)
            .sum();
        assert_eq!(points, QUEUES as usize * room);
    }
}
