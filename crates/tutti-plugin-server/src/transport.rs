//! IPC transport — sync wire framing for host↔server message pairs.
//!
//! Framing: u32 big-endian length prefix + postcard payload. The payload's
//! schema and its version constant are `tutti_plugin_ipc`'s; this module
//! only moves bytes.
//!
use interprocess::local_socket::{ListenerOptions, traits::Listener as _};
use std::io::Read;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tutti_platform::Instant;
use tutti_plugin_ipc::{
    AUDIO_REPLY_FRAME_BYTES, AUDIO_REQUEST_FRAME_BYTES, AudioReply, AudioRequest, BlobWire,
    ControlFrame, ControlReply, ControlRequest, FrameBuf, Framed, ReqId, Request, SlabHandle,
    SlabLayout, read_frame, write_frame,
};
use tutti_plugin_ipc_ffi::{Blob, Lease};
use tutti_plugin_types::{
    Deadline, IpcError, IpcResult, MAX_STATE_BYTES, ServerError, SlabError, StateUse,
};

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
pub(crate) type ControlTransport = dyn Transport<Incoming, ControlFrame>;

/// What the control connection delivers: a request, or a slab setup or a state
/// with the object it announced.
///
/// A `SetupSharedMemory`, `LoadState` or `StageState` frame is not delivered as
/// a [`Request`]: the transport reads the object that follows it and delivers
/// both, so a setup or a state without its object cannot reach the session.
#[derive(Debug)]
pub(crate) enum Incoming {
    /// Any request but a slab setup.
    Request(Request),
    /// The host's `SetupSharedMemory` and the slab that came with it.
    SetupSlab {
        /// The request's id, which the answer echoes.
        id: ReqId,
        /// The region geometry the host sized the slab for.
        layout: SlabLayout,
        /// The slab, as received.
        slab: SlabHandle,
    },
    /// The host's `LoadState` and the state in the object that came with it.
    LoadState {
        /// The request's id, which the answer echoes.
        id: ReqId,
        /// What the state is restored for.
        state_use: StateUse,
        /// The state: the object's bytes, checked against the declared length.
        state: Vec<u8>,
    },
    /// The host's `StageState` and the state in the object that came with it.
    StageState {
        /// The request's id, which the answer echoes.
        id: ReqId,
        /// The state: the object's bytes, checked against the declared length.
        state: Vec<u8>,
    },
    /// A `LoadState` or `StageState` whose object was refused: it was read off
    /// the stream, so the stream is in step, and the host is answered.
    StateRefused {
        /// The request's id, which the answer echoes.
        id: ReqId,
        /// Whether it was a `StageState`.
        staging: bool,
        /// Why the object was refused.
        error: String,
    },
}

/// A message a connection delivers, built from the frame it reads and whatever
/// follows that frame on the stream.
pub(crate) trait Inbound: Sized {
    /// The frame the message is read from.
    type Frame: Framed;

    /// Completes the message of `frame`, reading from `stream` what follows it.
    ///
    /// # Errors
    ///
    /// What follows the frame did not arrive as the frame announced.
    fn complete(frame: Self::Frame, stream: &PlatformStream) -> IpcResult<Self>;
}

impl Inbound for AudioRequest {
    type Frame = Self;

    fn complete(frame: Self, _: &PlatformStream) -> IpcResult<Self> {
        Ok(frame)
    }
}

impl Inbound for Incoming {
    type Frame = Request;

    fn complete(Request { id, body }: Request, stream: &PlatformStream) -> IpcResult<Self> {
        match body {
            ControlRequest::SetupSharedMemory { slab, layout } => {
                // The slab is the next message on the stream: an exact read
                // right after the frame, so no read ahead can have taken it.
                let slab = SlabHandle::receive(crate::main_loop::connection(stream), slab)?;
                Ok(Self::SetupSlab { id, layout, slab })
            }
            ControlRequest::LoadState {
                state_use,
                len,
                blob,
            } => Ok(match take_state(stream, len, blob)? {
                Ok(state) => Self::LoadState {
                    id,
                    state_use,
                    state,
                },
                Err(e) => Self::StateRefused {
                    id,
                    staging: false,
                    error: e.to_string(),
                },
            }),
            ControlRequest::StageState { len, blob } => Ok(match take_state(stream, len, blob)? {
                Ok(state) => Self::StageState { id, state },
                Err(e) => Self::StateRefused {
                    id,
                    staging: true,
                    error: e.to_string(),
                },
            }),
            body => Ok(Self::Request(Request { id, body })),
        }
    }
}

/// The state in the object that follows a state frame, which declared `len`.
///
/// The outer error is a stream that did not carry the object (the connection
/// cannot go on); the inner is an object that arrived and was refused (too
/// long, of another length, not sealed), with the stream in step.
fn take_state(
    stream: &PlatformStream,
    len: u64,
    blob: BlobWire,
) -> IpcResult<Result<Vec<u8>, SlabError>> {
    let object = Blob::receive(crate::main_loop::connection(stream), blob)?;
    Ok(object.read(len, MAX_STATE_BYTES))
}

/// Answers request `id` with `state`: a `StateReady` frame and the object that
/// holds the bytes. An object the server cannot make is answered as an `Error`,
/// as a plugin that cannot save is.
///
/// # Errors
///
/// The connection failed.
pub(crate) fn send_state(
    transport: &mut ControlTransport,
    id: ReqId,
    state: &[u8],
) -> IpcResult<()> {
    match Blob::create(state) {
        Ok(object) => {
            let reply = ControlReply::StateReady {
                len: state.len() as u64,
                blob: object.wire(),
            };
            transport.send_attached(&ControlFrame::Reply { id, reply }, object)
        }
        Err(e) => transport.send(&ControlFrame::Reply {
            id,
            reply: ControlReply::Error {
                error: ServerError::StateNotShared {
                    reason: e.to_string(),
                },
            },
        }),
    }
}

/// Writes a frame to a connection from a thread other than the one serving
/// it ([`Transport::pulser`]).
pub(crate) type Pulser<Out> = Box<dyn Fn(&Out) -> IpcResult<()> + Send + Sync>;

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
    fn recv(&mut self) -> IpcResult<In>;

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
    ) -> IpcResult<bool>;

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
    fn send(&mut self, msg: &Out) -> IpcResult<()>;

    /// Write `msg`, which announces `object`, and then the object as the message
    /// that follows it on the stream, both under one hold of the connection's
    /// writer. Where the object is read by name (Windows) it is kept until the
    /// next one is sent.
    ///
    /// # Errors
    ///
    /// As [`send`](Self::send), and a connection that carries no objects.
    fn send_attached(&mut self, msg: &Out, object: Blob) -> IpcResult<()> {
        let _ = (msg, object);
        Err(IpcError::ProtocolError(
            "this connection carries no objects",
        ))
    }

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

/// How a connection's frames are written: by one writer ([`Alone`]), or by
/// two that must not interleave their bytes ([`Shared`]).
pub(crate) trait Writing: Send + Sync {
    /// Held while a frame is written.
    fn hold(&self) -> Option<MutexGuard<'_, ()>>;

    /// The lock a second writer takes to write a frame, when there is one.
    fn shared(&self) -> Option<Arc<Mutex<()>>>;
}

/// The audio connection's writing: its render thread is the only writer, and
/// there is no lock for it to take.
pub(crate) struct Alone;

impl Writing for Alone {
    fn hold(&self) -> Option<MutexGuard<'_, ()>> {
        None
    }

    fn shared(&self) -> Option<Arc<Mutex<()>>> {
        None
    }
}

/// The control connection's writing: the thread serving it, and the one that
/// tells the host a load is at work ([`Transport::pulser`]), each hold the lock
/// for the length of a frame.
#[derive(Default)]
pub(crate) struct Shared(Arc<Mutex<()>>);

impl Writing for Shared {
    fn hold(&self) -> Option<MutexGuard<'_, ()>> {
        Some(self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn shared(&self) -> Option<Arc<Mutex<()>>> {
        Some(Arc::clone(&self.0))
    }
}

/// [`Transport`] over the platform's local socket (Unix domain socket, or a
/// Windows named pipe).
pub(crate) struct SocketTransport<In, Out, W = Shared> {
    stream: Arc<PlatformStream>,
    writing: W,
    /// The audio connection's frame buffers, of fixed capacity, so a block's
    /// frames allocate nothing on the render thread; `None` for a connection
    /// that is served off it.
    frames: Option<FrameBuf>,
    /// Never rung: the wake argument of the wait for the rest of a frame, which
    /// only its deadline ends.
    quiet: Option<Bell>,
    /// What keeps the last object sent readable until the next is sent.
    kept: Option<Lease>,
    lane: PhantomData<fn(Out) -> In>,
}

impl<In, Out> SocketTransport<In, Out, Shared> {
    /// Wrap an already-accepted stream. The stream must be in blocking mode;
    /// see [`TransportListener::accept`] for why that is not incidental.
    pub(crate) fn new(stream: PlatformStream) -> Self {
        Self {
            stream: Arc::new(stream),
            writing: Shared::default(),
            frames: None,
            quiet: None,
            kept: None,
            lane: PhantomData,
        }
    }
}

impl SocketTransport<AudioRequest, AudioReply, Alone> {
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
            writing: Alone,
            frames: Some(FrameBuf::new(AUDIO_REQUEST_BYTES, AUDIO_REPLY_BYTES)),
            quiet: Some(Bell::new()?),
            kept: None,
            lane: PhantomData,
        })
    }
}

impl<In: Inbound, Out: Framed, W: Writing> Transport<In, Out> for SocketTransport<In, Out, W> {
    fn recv(&mut self) -> IpcResult<In> {
        // The frame is bounded before it is allocated (see
        // [`CONTROL_FRAME_BYTES`](tutti_plugin_ipc::CONTROL_FRAME_BYTES)). The server is the more exposed end: it loads
        // untrusted plugin binaries, and one that corrupts its memory could
        // otherwise turn a bad length into a 4 GiB allocation.
        //
        // The verdict differs from the host's on purpose. The host reads an
        // oversize length as `ProcessCrashed`; here the peer is the host,
        // which has not crashed, so the error names the frame and the session
        // ends on it either way.
        let frame = match (self.frames.as_mut(), self.quiet.as_ref()) {
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
                frames.read(&mut reader)?
            }
            _ => read_frame(&mut &*self.stream)?,
        };
        In::complete(frame, &self.stream)
    }

    /// Waits for the host's next frame on the platform's wait
    /// ([`crate::main_loop`], which owns what each OS waits with).
    fn wait_readable(
        &mut self,
        timeout: Option<std::time::Duration>,
        wake: &Bell,
    ) -> IpcResult<bool> {
        crate::main_loop::wait_readable(&self.stream, timeout, wake)
    }

    fn stream(&self) -> Option<&PlatformStream> {
        Some(&self.stream)
    }

    fn send(&mut self, msg: &Out) -> IpcResult<()> {
        // Reachable with an oversize body through `StateData`, whose chunk is
        // whatever the plugin hands back from `get_state`.
        let _writing = self.writing.hold();
        match self.frames.as_mut() {
            Some(frames) => frames.write(&mut &*self.stream, msg),
            None => write_frame(&mut &*self.stream, msg),
        }
    }

    fn send_attached(&mut self, msg: &Out, object: Blob) -> IpcResult<()> {
        let _writing = self.writing.hold();
        write_frame(&mut &*self.stream, msg)?;
        self.kept = Some(object.send(crate::main_loop::connection(&self.stream))?);
        Ok(())
    }

    fn pulser(&self) -> Option<Pulser<Out>> {
        let (stream, writing) = (Arc::clone(&self.stream), self.writing.shared()?);
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
    pub(crate) fn bind(socket_path: &std::path::Path) -> IpcResult<Self> {
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
    pub(crate) fn accept(&self) -> IpcResult<SocketTransport<Incoming, ControlFrame>> {
        Ok(SocketTransport::new(self.accept_stream()?))
    }

    /// [`accept`](Self::accept) for the audio connection, whose transport reads
    /// and writes through fixed buffers ([`SocketTransport::audio`]).
    pub(crate) fn accept_audio(
        &self,
    ) -> IpcResult<SocketTransport<AudioRequest, AudioReply, Alone>> {
        Ok(SocketTransport::audio(self.accept_stream()?)?)
    }

    fn accept_stream(&self) -> IpcResult<PlatformStream> {
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
    use tutti_plugin_types::IpcError;

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
        let mut transport: SocketTransport<Incoming, ControlFrame> =
            SocketTransport::new(PlatformStream::from(OwnedFd::from(server)));
        let len = u32::try_from(CONTROL_FRAME_BYTES + 1).expect("the cap fits a u32 prefix");
        host.write_all(&len.to_be_bytes()).expect("write prefix");
        match transport.recv() {
            Err(IpcError::FrameTooLarge(e)) => assert_eq!(e.len, CONTROL_FRAME_BYTES + 1),
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }
}

/// A state crosses the control connection as an object in both directions, over
/// a real socket and descriptor, and the server's receive refuses an object that
/// is not what its frame declared.
#[cfg(all(test, unix))]
mod state_object_tests {
    use super::*;
    use std::os::fd::{AsFd, OwnedFd};
    use std::os::unix::net::UnixStream;

    const CAP_PLUS: u64 = MAX_STATE_BYTES as u64 + 1;

    fn pair() -> (UnixStream, SocketTransport<Incoming, ControlFrame>) {
        let (host, server) = UnixStream::pair().expect("socket pair");
        (
            host,
            SocketTransport::new(PlatformStream::from(OwnedFd::from(server))),
        )
    }

    /// The host side of a state request: its frame, then `object`.
    fn host_sends(
        host: &mut UnixStream,
        body: impl FnOnce(u64, BlobWire) -> ControlRequest,
        declared: u64,
        object: Blob,
    ) {
        let request = Request {
            id: ReqId::new(5),
            body: body(declared, object.wire()),
        };
        write_frame(host, &request).expect("the frame");
        let _lease = object.send(host.as_fd()).expect("the object");
    }

    fn load(declared: u64) -> impl FnOnce(u64, BlobWire) -> ControlRequest {
        move |_, blob| ControlRequest::LoadState {
            state_use: StateUse::Project,
            len: declared,
            blob,
        }
    }

    /// More than the largest chunk the old framing sent, with a pattern a dropped
    /// or reordered piece would break.
    fn big() -> Vec<u8> {
        (0..9 * 1024 * 1024 + 7).map(|i| (i % 251) as u8).collect()
    }

    /// **A state from the host arrives as the bytes it sent**, for a load and for
    /// a stage, through the real descriptor.
    #[test]
    fn a_state_from_the_host_arrives_whole() {
        let (mut host, mut transport) = pair();
        let bytes = big();
        host_sends(
            &mut host,
            load(bytes.len() as u64),
            bytes.len() as u64,
            Blob::create(&bytes).unwrap(),
        );
        match transport.recv().expect("a load") {
            Incoming::LoadState { id, state, .. } => {
                assert_eq!(id, ReqId::new(5));
                assert!(state == bytes, "the state arrived changed");
            }
            other => panic!("expected the load, got {other:?}"),
        }
        host_sends(
            &mut host,
            |len, blob| ControlRequest::StageState { len, blob },
            bytes.len() as u64,
            Blob::create(&bytes).unwrap(),
        );
        match transport.recv().expect("a stage") {
            Incoming::StageState { state, .. } => assert!(state == bytes),
            other => panic!("expected the stage, got {other:?}"),
        }
    }

    /// **A state to the host is a `StateReady` frame and an object**: the host
    /// reads the frame, takes the object and gets the bytes the server sent.
    #[test]
    fn a_state_to_the_host_arrives_whole() {
        let (mut host, mut transport) = pair();
        let bytes = big();
        send_state(&mut transport, ReqId::new(9), &bytes).expect("sent");
        let frame: ControlFrame = read_frame(&mut host).expect("the frame");
        let ControlFrame::Reply {
            id,
            reply: ControlReply::StateReady { len, blob },
        } = frame
        else {
            panic!("expected StateReady, got {frame:?}");
        };
        assert_eq!((id, len), (ReqId::new(9), bytes.len() as u64));
        let got = Blob::receive(host.as_fd(), blob)
            .expect("the object")
            .read(len, MAX_STATE_BYTES)
            .expect("its bytes");
        assert!(got == bytes, "the state arrived changed");
    }

    /// **An object shorter than its frame declared is refused as a refused
    /// state, not delivered**, for a load and for a stage; the stream stays in
    /// step. (The claim is past a page, so the platform that rounds objects up
    /// refuses it too; the claim of less than the object holds is the ffi
    /// crate's test.)
    #[test]
    fn a_declared_length_that_is_not_the_objects_is_refused_and_the_stream_goes_on() {
        let (mut host, mut transport) = pair();
        for declared in [1u64 << 20, 2 << 20] {
            host_sends(
                &mut host,
                load(declared),
                declared,
                Blob::create(&[1u8; 100]).unwrap(),
            );
            match transport.recv().expect("a refusal, not an end") {
                Incoming::StateRefused {
                    staging: false,
                    error,
                    ..
                } => {
                    assert!(error.contains("declared"), "{error}");
                }
                other => panic!("expected the refusal, got {other:?}"),
            }
        }
        host_sends(
            &mut host,
            |len, blob| ControlRequest::StageState { len, blob },
            1 << 20,
            Blob::create(&[1u8; 100]).unwrap(),
        );
        assert!(matches!(
            transport.recv().expect("a refusal"),
            Incoming::StateRefused { staging: true, .. }
        ));
        // In step: the next state still arrives.
        host_sends(&mut host, load(3), 3, Blob::create(&[7, 8, 9]).unwrap());
        assert!(matches!(
            transport.recv().expect("a load"),
            Incoming::LoadState { state, .. } if state == [7, 8, 9]
        ));
    }

    /// **A declared length past `MAX_STATE_BYTES` is refused**, before the object
    /// is read.
    #[test]
    fn a_declared_length_past_the_cap_is_refused() {
        let (mut host, mut transport) = pair();
        host_sends(
            &mut host,
            load(CAP_PLUS),
            CAP_PLUS,
            Blob::create(&[0u8; 16]).unwrap(),
        );
        match transport.recv().expect("a refusal") {
            Incoming::StateRefused { error, .. } => {
                assert!(error.contains("limit"), "{error}");
            }
            other => panic!("expected the refusal, got {other:?}"),
        }
    }

    /// **A state frame with no object after it ends the connection by name**, and
    /// does not wait for ever.
    #[test]
    fn a_missing_object_is_an_error() {
        let (mut host, mut transport) = pair();
        let blob = Blob::create(&[1, 2, 3]).unwrap();
        let request = Request {
            id: ReqId::new(1),
            body: load(3)(3, blob.wire()),
        };
        write_frame(&mut host, &request).unwrap();
        assert!(transport.recv().is_err(), "a state with no object arrived");
    }

    /// **A state request is beaten over the real control connection**: while the
    /// work runs, the host reads `Working` frames for the request (as `Restoring`
    /// for a load, `Opening` for a save), and the work waits on two of them, so
    /// a beat that never came leaves it waiting.
    #[test]
    fn working_beats_a_state_request_over_the_connection() {
        use tutti_plugin_ipc::Working;
        for restoring in [true, false] {
            let (mut host, transport) = pair();
            let executor = tutti_platform::Executor::new();
            let seen =
                crate::server::working(&executor, &transport, ReqId::new(9), restoring, || {
                    (0..2)
                        .map(
                            |_| match read_frame::<_, ControlFrame>(&mut host).expect("a beat") {
                                ControlFrame::Working { id, stage } => {
                                    assert_eq!(id, ReqId::new(9));
                                    stage
                                }
                                other => panic!("a beat sends only `Working`, sent {other:?}"),
                            },
                        )
                        .collect::<Vec<_>>()
                });
            let want = if restoring {
                Working::Restoring
            } else {
                Working::Opening
            };
            assert_eq!(seen, [want; 2]);
        }
    }

    /// **No descriptor leaks across states**: sending and receiving many states,
    /// both ways, leaves the process with the descriptors it began with.
    #[test]
    fn many_states_leak_no_descriptors() {
        // Counts this process's descriptors, so it relies on nextest's one process per test.
        let open_fds = || std::fs::read_dir("/dev/fd").unwrap().count();
        let (mut host, mut transport) = pair();
        let before = open_fds();
        for i in 0..64u8 {
            host_sends(
                &mut host,
                load(1000),
                1000,
                Blob::create(&[i; 1000]).unwrap(),
            );
            assert!(matches!(
                transport.recv().unwrap(),
                Incoming::LoadState { .. }
            ));
            send_state(&mut transport, ReqId::new(1), &[i; 1000]).unwrap();
            let frame: ControlFrame = read_frame(&mut host).unwrap();
            let ControlFrame::Reply {
                reply: ControlReply::StateReady { len, blob },
                ..
            } = frame
            else {
                panic!("expected StateReady");
            };
            let got = Blob::receive(host.as_fd(), blob)
                .unwrap()
                .read(len, MAX_STATE_BYTES)
                .unwrap();
            assert_eq!(got, vec![i; 1000]);
        }
        assert_eq!(open_fds(), before);
    }
}

#[cfg(all(test, unix))]
mod wait_tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    fn pair() -> (UnixStream, SocketTransport<Incoming, ControlFrame>) {
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
    /// **Only the control connection has a second writer, and so a lock**: the
    /// audio connection's transport (the render thread's) offers none and takes
    /// none, by its type, and the control connection's frames written through
    /// its pulser arrive whole, in order with its own.
    #[test]
    fn only_the_control_connection_has_a_shared_writer() {
        let (host, control) = pair();
        let (_audio_host, audio_server) = UnixStream::pair().expect("socket pair");
        let audio = SocketTransport::audio(PlatformStream::from(OwnedFd::from(audio_server)))
            .expect("the audio transport");
        assert!(audio.pulser().is_none(), "the audio lane has one writer");

        let mut control = control;
        let pulse = control
            .pulser()
            .expect("the control lane has a second writer");
        let frame = ControlFrame::Working {
            id: tutti_plugin_ipc::ReqId::new(3),
            stage: tutti_plugin_ipc::Working::Opening,
        };
        pulse(&frame).expect("the pulse is written");
        control.send(&frame).expect("the frame is written");
        let mut host = host;
        for _ in 0..2 {
            let got: ControlFrame = read_frame(&mut host).expect("a whole frame");
            assert!(matches!(
                got,
                ControlFrame::Working { id, .. } if id == tutti_plugin_ipc::ReqId::new(3)
            ));
        }
    }

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
