//! IPC transport — sync wire framing for host↔server message pairs.
//!
//! Framing: u32 big-endian length prefix + postcard payload. The payload's
//! schema and its version constant are `tutti_plugin_ipc`'s; this module
//! only moves bytes.
//!
use interprocess::local_socket::{ListenerOptions, traits::Listener as _};
use std::io::Read;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tutti_platform::Instant;
use tutti_plugin_ipc::{
    AUDIO_REPLY_FRAME_BYTES, AUDIO_REQUEST_FRAME_BYTES, AudioReply, AudioRequest, ControlFrame,
    FrameBuf, Framed, Request, read_frame, write_frame,
};
use tutti_plugin_types::{BridgeResult, Deadline};

use crate::main_loop::Bell;
use crate::main_loop::{Listener as PlatformListener, Stream as PlatformStream};

/// The longest request the audio connection takes: the protocol's own cap for
/// a block's request ([`AUDIO_REQUEST_FRAME_BYTES`]). A block's request carries
/// automation at the host's point budget for the activated block and queue
/// count, which is megabytes at a large block, so the buffer holds the whole
/// bound rather than a guess. The memory is zeroed pages the OS maps on first
/// touch: a connection that never carries a large block never makes them
/// resident.
const AUDIO_REQUEST_BYTES: usize = AUDIO_REQUEST_FRAME_BYTES;

/// The longest reply the audio connection sends, in bytes: a block's reply
/// with its MIDI at the cap, each event at the most its varints take.
pub(crate) const AUDIO_REPLY_BYTES: usize = AUDIO_REPLY_FRAME_BYTES;

/// The audio connection's transport: blocks in, block replies out.
pub(crate) type AudioTransport = dyn Transport<AudioRequest, AudioReply>;

/// The control connection's transport: requests in, answers and news out.
pub(crate) type ControlTransport = dyn Transport<Request, ControlFrame>;

/// Writes a frame to a connection from a thread other than the one serving
/// it ([`Transport::pulser`]).
pub(crate) type Pulser<Out> = Box<dyn Fn(&Out) -> BridgeResult<()> + Send + Sync>;

/// One framed, blocking message channel to the host.
///
/// A trait rather than the concrete socket so [`crate::session::Session`] can be
/// driven from a test double without a real socket pair.
pub(crate) trait Transport<In, Out>: Send {
    /// Block until one complete host message arrives.
    ///
    /// # Errors
    ///
    /// Returns an error on EOF (the host disconnected), a short read, a length
    /// prefix over the lane's frame cap (a peer that cannot frame), or a payload
    /// postcard cannot decode — the last meaning a protocol-version mismatch the
    /// `Ready` handshake failed to catch.
    fn recv(&mut self) -> BridgeResult<In>;

    /// Wait up to `timeout` (`None`: for ever) for the host to have something to
    /// read: `Ok(true)` when a [`recv`](Self::recv) now will find a frame begun
    /// (or the host gone, which `recv` reports as the error it is), `Ok(false)`
    /// on a timeout, or when `wake` rang with nothing to read.
    ///
    /// This is what lets one thread serve the socket and the editor's idle tick
    /// ([`crate::main_loop`]): it returns **the instant** a byte arrives, so a
    /// block's request is read as soon as it was when the loop blocked in
    /// `recv`.
    ///
    /// # Errors
    ///
    /// The OS refuses the wait.
    fn wait_readable(
        &mut self,
        timeout: Option<std::time::Duration>,
        wake: &Bell,
    ) -> BridgeResult<bool>;

    /// The connection's own OS stream, for a wait that also covers descriptors
    /// of the plugin's ([`crate::main_loop`]); `None` for a transport that is
    /// not one (an in-memory pair), which is waited on through
    /// [`wait_readable`](Self::wait_readable) alone.
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
    fn send(&mut self, msg: &Out) -> BridgeResult<()>;

    /// A writer for the connection that another thread may call while this one
    /// is serving a request, to say the server is at work on it: its frames
    /// and this transport's own never interleave. `None` for a transport with
    /// no such writer (a test's script).
    fn pulser(&self) -> Option<Pulser<Out>> {
        None
    }
}

/// How long the rest of an audio frame may take to arrive once its first byte
/// has: a frame is written by one `write`, so a host that stops part-way has
/// stopped, and the render thread must not wait on it for ever. Less than the
/// patience the server gives a stopping render thread.
const PARTIAL_FRAME_PATIENCE: Duration = Duration::from_secs(2);

/// A reader that, after its first read, waits for the stream to be readable
/// within a [`Deadline`] before each further one: a frame that stops part-way
/// ends in `TimedOut` and not in a read that never returns. A trickle of bytes
/// does not extend it, as the deadline is the frame's and not the read's.
/// `wait` and `now` are the readiness wait and the clock, so the rule is a
/// function of the instants it is given.
struct FrameReader<'a, R, W, N> {
    inner: &'a mut R,
    began: bool,
    deadline: Deadline,
    wait: W,
    now: N,
}

impl<R, W, N> Read for FrameReader<'_, R, W, N>
where
    R: Read,
    W: FnMut(&R, Option<Duration>) -> std::io::Result<bool>,
    N: FnMut() -> Instant,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // The first read follows the readable wait that began the frame.
        if std::mem::replace(&mut self.began, true) {
            let now = (self.now)();
            if self.deadline.passed(now) || !(self.wait)(self.inner, self.deadline.left(now))? {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
        }
        self.inner.read(buf)
    }
}

/// [`Transport`] over the platform's local socket (Unix domain socket, or a
/// Windows named pipe).
pub(crate) struct SocketTransport<In, Out> {
    stream: Arc<PlatformStream>,
    /// Held while a frame is written, by [`send`](Transport::send) and by the
    /// [`pulser`](Transport::pulser) alike, so two writers never interleave
    /// the bytes of their frames.
    writing: Arc<Mutex<()>>,
    /// The audio connection's frame buffers, of fixed capacity, so a block's
    /// frames allocate nothing on the render thread; `None` for a connection
    /// that is served off it.
    frames: Option<FrameBuf>,
    /// Never rung: the wake argument of the wait for the rest of a frame, which
    /// only its deadline ends.
    quiet: Option<Bell>,
    lane: PhantomData<fn(Out) -> In>,
}

impl<In, Out> SocketTransport<In, Out> {
    /// Wrap an already-accepted stream. The stream must be in blocking mode;
    /// see [`TransportListener::accept`] for why that is not incidental.
    pub(crate) fn new(stream: PlatformStream) -> Self {
        Self {
            stream: Arc::new(stream),
            writing: Arc::default(),
            frames: None,
            quiet: None,
            lane: PhantomData,
        }
    }
}

impl SocketTransport<AudioRequest, AudioReply> {
    /// Wraps the audio connection's stream, which the render thread reads and
    /// writes through buffers built here: a frame is never read into, or
    /// serialized into, memory that is sized on that thread.
    ///
    /// # Errors
    ///
    /// The OS refused the bell of the frame wait.
    pub(crate) fn audio(stream: PlatformStream) -> std::io::Result<Self> {
        Ok(Self {
            stream: Arc::new(stream),
            writing: Arc::default(),
            frames: Some(FrameBuf::new(AUDIO_REQUEST_BYTES, AUDIO_REPLY_BYTES)),
            quiet: Some(Bell::new()?),
            lane: PhantomData,
        })
    }
}

impl<In: Framed, Out: Framed> Transport<In, Out> for SocketTransport<In, Out> {
    fn recv(&mut self) -> BridgeResult<In> {
        // The frame is bounded before it is allocated (see
        // [`CONTROL_FRAME_BYTES`](tutti_plugin_ipc::CONTROL_FRAME_BYTES)). The server is the more exposed end: it loads
        // untrusted plugin binaries, and one that corrupts its memory could
        // otherwise turn a bad length into a 4 GiB allocation.
        //
        // The verdict differs from the host's on purpose. The host reads an
        // oversize length as `ProcessCrashed`; here the peer is the host,
        // which has not crashed, so the error names the frame and the session
        // ends on it either way.
        match (self.frames.as_mut(), self.quiet.as_ref()) {
            (Some(frames), Some(quiet)) => {
                let mut stream: &PlatformStream = &self.stream;
                let mut reader = FrameReader {
                    inner: &mut stream,
                    began: false,
                    deadline: Deadline::after(Instant::now(), PARTIAL_FRAME_PATIENCE),
                    wait: |stream: &&PlatformStream, timeout| {
                        crate::main_loop::wait_readable(stream, timeout, quiet)
                            .map_err(std::io::Error::other)
                    },
                    now: Instant::now,
                };
                frames.read(&mut reader)
            }
            _ => read_frame(&mut &*self.stream),
        }
    }

    /// Waits for the host's next frame on the platform's wait
    /// ([`crate::main_loop`], which owns what each OS waits with).
    fn wait_readable(
        &mut self,
        timeout: Option<std::time::Duration>,
        wake: &Bell,
    ) -> BridgeResult<bool> {
        crate::main_loop::wait_readable(&self.stream, timeout, wake)
    }

    fn stream(&self) -> Option<&PlatformStream> {
        Some(&self.stream)
    }

    fn send(&mut self, msg: &Out) -> BridgeResult<()> {
        // Reachable with an oversize body through `StateData`, whose chunk is
        // whatever the plugin hands back from `get_state`.
        let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        match self.frames.as_mut() {
            Some(frames) => frames.write(&mut &*self.stream, msg),
            None => write_frame(&mut &*self.stream, msg),
        }
    }

    fn pulser(&self) -> Option<Pulser<Out>> {
        let (stream, writing) = (Arc::clone(&self.stream), Arc::clone(&self.writing));
        Some(Box::new(move |msg| {
            let _writing = writing.lock().unwrap_or_else(PoisonError::into_inner);
            write_frame(&mut &*stream, msg)
        }))
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

    /// Wait for the host to connect.
    ///
    /// The accept is blocking, and the listener is never put into non-blocking
    /// mode: that is what keeps the accepted stream from inheriting
    /// `O_NONBLOCK` (on BSD-derived systems, macOS included, the socket
    /// `accept(2)` returns inherits the listener's flag, and every read on it
    /// then fails with `WouldBlock`). A host that dies while this waits is the
    /// host watch's to end the process ([`crate::server`]).
    pub(crate) fn accept(&self) -> BridgeResult<SocketTransport<Request, ControlFrame>> {
        Ok(SocketTransport::new(self.accept_stream()?))
    }

    /// [`accept`](Self::accept) for the audio connection, whose transport reads
    /// and writes through fixed buffers ([`SocketTransport::audio`]).
    pub(crate) fn accept_audio(&self) -> BridgeResult<SocketTransport<AudioRequest, AudioReply>> {
        Ok(SocketTransport::audio(self.accept_stream()?)?)
    }

    fn accept_stream(&self) -> BridgeResult<PlatformStream> {
        Ok(self.listener.accept()?)
    }
}

#[cfg(all(test, unix))]
mod recv_tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use tutti_plugin_ipc::CONTROL_FRAME_BYTES;
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
        let mut transport: SocketTransport<Request, ControlFrame> =
            SocketTransport::new(PlatformStream::from(OwnedFd::from(server)));
        let len = u32::try_from(CONTROL_FRAME_BYTES + 1).expect("the cap fits a u32 prefix");
        host.write_all(&len.to_be_bytes()).expect("write prefix");
        match transport.recv() {
            Err(BridgeError::FrameTooLarge(e)) => assert_eq!(e.len, CONTROL_FRAME_BYTES + 1),
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

    fn pair() -> (UnixStream, SocketTransport<Request, ControlFrame>) {
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

        let wake = Bell::new().expect("a bell");
        assert!(
            !transport
                .wait_readable(Some(Duration::ZERO), &wake)
                .unwrap()
        );

        host.write_all(&[0]).expect("write");
        assert!(
            transport
                .wait_readable(Some(Duration::from_secs(600)), &wake)
                .unwrap()
        );
    }

    /// A host that hangs up is readable: `recv` reports the end as the error
    /// it is, rather than the loop ticking on for ever.
    #[test]
    fn a_closed_host_is_readable() {
        let (host, mut transport) = pair();
        drop(host);
        let wake = Bell::new().expect("a bell");
        assert!(
            transport
                .wait_readable(Some(Duration::from_secs(600)), &wake)
                .unwrap()
        );
        assert!(transport.recv().is_err());
    }

    /// **A ring of the wake ends a wait on a quiet socket, readable to no one**:
    /// the render thread's wait returns for a command the main thread queued.
    /// The wait's timeout outlasts the test harness's limit, so a wait the ring
    /// does not end fails.
    #[test]
    fn a_ring_ends_a_wait_on_a_quiet_socket() {
        let (_host, mut transport) = pair();
        let wake = Bell::new().expect("a bell");
        let ringer = wake.clone();
        let rung = std::thread::spawn(move || ringer.ring());
        assert!(
            !transport
                .wait_readable(Some(Duration::from_secs(600)), &wake)
                .unwrap(),
            "woken, with nothing to read"
        );
        rung.join().expect("the ringer");
        assert!(wake.take());
    }
}

#[cfg(test)]
mod audio_frame_tests {
    use super::*;
    use tutti_plugin_ipc::ProcessAudioData;
    use tutti_plugin_types::{ParameterChanges, ParameterQueue, SampleRate};
    use tutti_units::ParamId;

    /// **The audio connection accepts the largest request the host may send.**
    /// A block of 65536 frames with 256 queues each at the whole chunk room (the
    /// most the host's point pool lets 256 queues carry) encodes to more than
    /// 4 MiB, past what a small fixed cap would hold, and it is read through the
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
        let block = AudioRequest::ProcessAudio(ProcessAudioData {
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
        let AudioRequest::ProcessAudio(back) = frames.read(&mut wire.as_slice()).unwrap() else {
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

#[cfg(test)]
mod frame_reader_tests {
    use super::*;

    /// **A frame that stops part-way ends in `TimedOut` at its deadline, and the
    /// wait for each further read is for the time that is left**: a trickle that
    /// keeps every read short does not extend the frame's one deadline. The
    /// clock is constructed, a second per look, and the wait never finds more
    /// bytes.
    #[test]
    fn a_frame_that_stops_part_way_times_out_at_its_deadline() {
        let start = Instant::now();
        let ticks = std::cell::Cell::new(0_u64);
        let waits = std::cell::RefCell::new(Vec::new());
        let mut source: &[u8] = &[1, 2, 3, 4, 5, 6];
        let mut reader = FrameReader {
            inner: &mut source,
            began: false,
            deadline: Deadline::after(start, Duration::from_secs(3)),
            wait: |_: &&[u8], left: Option<Duration>| {
                waits.borrow_mut().push(left);
                Ok(true)
            },
            now: || {
                ticks.set(ticks.get() + 1);
                start + Duration::from_secs(ticks.get() - 1)
            },
        };
        let mut one = [0_u8; 1];

        // The first read is the frame's first byte: no wait, no look at the clock.
        assert_eq!(reader.read(&mut one).expect("the first byte"), 1);
        assert!(waits.borrow().is_empty());
        // One byte a look, a second per look: the waits ask for what is left.
        assert_eq!(reader.read(&mut one).expect("at 0 s"), 1);
        assert_eq!(reader.read(&mut one).expect("at 1 s"), 1);
        assert_eq!(reader.read(&mut one).expect("at 2 s"), 1);
        let late = reader
            .read(&mut one)
            .expect_err("the deadline passed at 3 s");

        assert_eq!(late.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(
            *waits.borrow(),
            [
                Some(Duration::from_secs(3)),
                Some(Duration::from_secs(2)),
                Some(Duration::from_secs(1)),
            ]
        );
    }

    /// A wait that finds nothing readable within the time left is a timeout
    /// too, however much time the deadline has.
    #[test]
    fn a_wait_that_finds_nothing_readable_times_out() {
        let start = Instant::now();
        let mut source: &[u8] = &[1, 2];
        let mut reader = FrameReader {
            inner: &mut source,
            began: true,
            deadline: Deadline::after(start, Duration::from_secs(3)),
            wait: |_: &&[u8], _| Ok(false),
            now: || start,
        };

        let mut one = [0_u8; 1];
        let late = reader.read(&mut one).expect_err("nothing came");
        assert_eq!(late.kind(), std::io::ErrorKind::TimedOut);
    }
}
