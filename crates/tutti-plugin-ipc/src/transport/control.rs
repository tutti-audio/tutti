//! Host-side IPC to the plugin-server: length-prefixed postcard over a
//! local socket (Unix domain socket on Unix, named pipe on Windows).
//!
//! Wire: `[u32 big-endian length][postcard payload]` both directions, with the body capped at
//! the message type's own cap ([`Framed::CAP`]) — the length arrives from the peer, so it
//! is checked before it is believed, in both `send` and `recv`. EOF at either end surfaces as
//! [`IpcError::ProcessCrashed`], and so does a length prefix over the cap: a peer that cannot
//! frame has desynchronised the stream, and there is no resynchronisation point to recover to.
//!
//! **Two independent bounds, because they answer different questions.**
//! the cap bounds how *much* a peer can make the host
//! allocate; the deadline in [`read_exact_by`] bounds how *long* a peer can make it wait. A size
//! cap alone does not bound time — an under-cap frame delivered one byte per receive timeout is
//! still unbounded, because `SO_RCVTIMEO` restarts on every syscall while `read_exact` loops. Both
//! are needed and neither subsumes the other.

use crate::protocol::frame::{decode_body, decode_prefix, write_frame};
use crate::transport::PendingLane;
use crate::{
    ControlRequest, Framed, Lane, LoadState, Message, OpenLane, ReqId, Request, SetupSharedMemory,
    SlabHandle, SlabLayout, StageState,
};
use tutti_plugin_ipc_ffi::{Blob, Connection, Lease, Received};
use tutti_plugin_types::StateUse;
use tutti_plugin_types::{IpcError, IpcResult};

use super::sys::Wakeup;
pub(crate) use super::sys::{ControlUnparker, ControlWait, Watch, control_unparker};
use interprocess::local_socket::Stream;
use std::io::Read;
use std::thread;
use std::time::Duration;
use tutti_platform::Instant;

/// Duplex byte stream to the plugin-server.
pub type ControlStream = Stream;

/// How long one [`send`] may block on a peer that reads nothing.
///
/// A write blocks only when the socket buffer is full, which on this
/// connection means the server has stopped reading: it handles one request
/// at a time and reads the next as soon as it answers. A server that reads
/// nothing for this long is hung, and a send that gives up part-way has
/// unframed the stream, so the bridge crashes on it. Generous,
/// because a slow plugin call delays the server's next read by its length.
pub(crate) const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Write one frame.
///
/// # Errors
///
/// A frame the peer must reject (over the cap, or not encodable) is refused
/// with nothing written, so the stream stays synchronised: a send the host
/// itself declines is not a crash. A write the peer does not take within the
/// send deadline is [`IpcError::Timeout`] with `partial` set: some of the
/// frame may be on the wire, and the connection cannot go on.
pub(crate) fn send<T: Framed>(stream: &mut ControlStream, msg: &T) -> IpcResult<()> {
    write_frame(stream, msg).map_err(|e| match e {
        IpcError::Io(io) => send_failed(io),
        other => other,
    })
}

/// A write error as the error of a send: a timeout is [`IpcError::Timeout`] with
/// `partial` set, and anything else the OS's.
fn send_failed(io: std::io::Error) -> IpcError {
    if io.kind() == std::io::ErrorKind::TimedOut || io.kind() == std::io::ErrorKind::WouldBlock {
        IpcError::Timeout {
            operation: "send",
            after: SEND_TIMEOUT,
            partial: true,
        }
    } else {
        IpcError::Io(io)
    }
}

/// An object that follows the frame announcing it: the second half of
/// [`send_attached`].
pub(crate) trait Attached {
    /// What the sender holds until the server has answered: a [`Lease`] for a blob
    /// read by name, the object itself where the server maps it by name, or `()`.
    type Held;

    /// Writes the object to the connection, right after its frame.
    fn send(self, conn: Connection<'_>) -> std::io::Result<Self::Held>;
}

/// No object: the frame stands alone.
impl Attached for () {
    type Held = ();

    fn send(self, _: Connection<'_>) -> std::io::Result<()> {
        Ok(())
    }
}

impl Attached for Blob {
    type Held = Lease;

    fn send(self, conn: Connection<'_>) -> std::io::Result<Lease> {
        Blob::send(self, conn)
    }
}

impl Attached for SlabHandle {
    type Held = Self;

    fn send(self, conn: Connection<'_>) -> std::io::Result<Self> {
        SlabHandle::send(&self, conn)?;
        Ok(self)
    }
}

impl Attached for PendingLane {
    type Held = Self;

    fn send(self, conn: Connection<'_>) -> std::io::Result<Self> {
        self.send_end(conn)?;
        Ok(self)
    }
}

/// A request that is followed by an object, and how it is made from that object.
pub(crate) trait Announces: Message {
    /// The object that follows the request.
    type Object: Attached;
    /// What the request carries beside the object's wire.
    type Rest;

    /// The request that announces `object`, with `rest` of its fields: what the
    /// frame carries of the object (its wire, and its length where it has one)
    /// is the object's own.
    fn announce(object: &Self::Object, rest: Self::Rest) -> Self;
}

impl Announces for SetupSharedMemory {
    type Object = SlabHandle;
    /// The slab's layout.
    type Rest = SlabLayout;

    fn announce(slab: &SlabHandle, layout: SlabLayout) -> Self {
        Self {
            slab: slab.wire(),
            layout,
        }
    }
}

impl Announces for OpenLane {
    type Object = PendingLane;
    type Rest = Lane;

    fn announce(object: &PendingLane, lane: Lane) -> Self {
        Self {
            lane,
            wire: object.wire(),
        }
    }
}

impl Announces for LoadState {
    type Object = Blob;
    /// What the state is restored for.
    type Rest = StateUse;

    fn announce(blob: &Blob, state_use: StateUse) -> Self {
        Self {
            state_use,
            len: blob.byte_len(),
            blob: blob.wire(),
        }
    }
}

impl Announces for StageState {
    type Object = Blob;
    /// Nothing: the state's length is the blob's.
    type Rest = ();

    fn announce(blob: &Blob, (): ()) -> Self {
        Self {
            len: blob.byte_len(),
            blob: blob.wire(),
        }
    }
}

/// A request that announces an object, with that object. The request is made by
/// [`Announces::announce`] from the object itself, so the frame names this
/// object and no other: a caller supplies the object and the request's other
/// fields, and never a wire.
pub(crate) struct Announced<M: Announces> {
    msg: M,
    object: M::Object,
}

impl<M: Announces> Announced<M> {
    /// `object`, with the request that announces it and carries `rest`.
    pub(crate) fn new(object: M::Object, rest: M::Rest) -> Self {
        Self {
            msg: M::announce(&object, rest),
            object,
        }
    }

    /// The request and the object it announces, the object's kind erased for a
    /// queue that holds either.
    pub(crate) fn detach(self) -> (M, Detached)
    where
        M::Object: Into<Detached>,
    {
        (self.msg, self.object.into())
    }
}

/// The object of a queued [`Announced`], its kind erased so one queue holds
/// either: sent only by [`send_detached`].
pub(crate) enum Detached {
    /// A state's blob.
    Blob(Blob),
    /// A slab.
    Slab(SlabHandle),
}

impl From<Blob> for Detached {
    fn from(blob: Blob) -> Self {
        Self::Blob(blob)
    }
}

impl From<SlabHandle> for Detached {
    fn from(slab: SlabHandle) -> Self {
        Self::Slab(slab)
    }
}

/// Writes the request of `announced` and then its object: the server's
/// transport reads both before the request reaches its session, so a frame
/// never goes without its object. Returns what the sender holds until the server
/// has answered.
///
/// # Errors
///
/// As [`send`], and a failure to send the object itself, which like any failure
/// after the frame's first byte leaves the stream mid-message: the connection
/// cannot go on.
pub(crate) fn send_attached<M: Announces>(
    stream: &mut ControlStream,
    id: ReqId,
    announced: Announced<M>,
) -> IpcResult<<M::Object as Attached>::Held> {
    let Announced { msg, object } = announced;
    send_object(stream, id, msg, object)
}

/// Writes the request `msg` and then the object `detached`, which `msg`
/// announces: for a request that travels apart from its object (a queued
/// command). What the sender holds is a [`Lease`] for a blob.
///
/// # Errors
///
/// As [`send_attached`].
pub(crate) fn send_detached(
    stream: &mut ControlStream,
    id: ReqId,
    msg: impl Into<ControlRequest>,
    detached: Option<Detached>,
) -> IpcResult<Option<Lease>> {
    match detached {
        Some(Detached::Blob(blob)) => send_object(stream, id, msg, blob).map(Some),
        Some(Detached::Slab(slab)) => send_object(stream, id, msg, slab).map(|_| None),
        None => send_object(stream, id, msg, ()).map(|()| None),
    }
}

/// Writes the request `msg` and then `object`, which `msg` announces.
fn send_object<A: Attached>(
    stream: &mut ControlStream,
    id: ReqId,
    msg: impl Into<ControlRequest>,
    object: A,
) -> IpcResult<A::Held> {
    send(
        stream,
        &Request {
            id,
            body: msg.into(),
        },
    )?;
    object
        .send(super::sys::connection(stream))
        .map_err(send_failed)
}

/// Writes `state` as the answer to request `id`: its `StateReady` frame and the
/// object that follows it, for the mock servers of the tests that serve over
/// this transport.
///
/// # Errors
///
/// The object cannot be made, or a write to the stream fails.
#[cfg(feature = "testing")]
pub fn send_state_reply(stream: &ControlStream, id: ReqId, state: &[u8]) -> IpcResult<()> {
    let object = Blob::create(state)?;
    let reply = crate::ControlReply::StateReady {
        len: state.len() as u64,
        blob: object.wire(),
        pending: Vec::new(),
    };
    write_frame(&mut &*stream, &crate::ControlFrame::Reply { id, reply })?;
    let _lease = object
        .send(super::sys::connection(stream))
        .map_err(send_failed)?;
    Ok(())
}

/// Wait longer each time a peeked stream comes back empty, never past `remaining`.
///
/// **The first polls yield instead of sleeping, and that is not an optimisation.**
/// This stream carries per-block process replies, which arrive within one block
/// period — a few milliseconds at most. Any fixed sleep coarse enough to be
/// cheap is coarse enough to blow the deadline it exists to enforce, so the
/// early polls have to be nearly free. Once a stream has been quiet long enough
/// that no reply is plausibly in flight, asking often buys nothing, and the wait
/// grows to [`POLL_INTERVAL`] — the same cadence the `SO_RCVTIMEO` arm uses, for
/// the same reason.
///
/// Only a named pipe reaches this nap, and no event exists to replace it: a
/// named pipe read through `interprocess` has no `poll`, no receive timeout and
/// no wake that a blocked read honours (an overlapped read would be one), so
/// the peek is the only way to ask.
///
/// Capping each nap at `remaining` is what makes the deadline *exact* on this
/// path rather than exact-plus-one-sleep: the loop cannot oversleep past the
/// instant it is meant to give up.
pub(super) fn back_off(empty_polls: &mut u32, remaining: Duration) {
    /// Polls served by `yield_now` before any sleeping starts.
    const SPIN_POLLS: u32 = 64;
    /// First sleep once spinning stops, doubled per poll up to [`POLL_INTERVAL`].
    const FIRST_NAP: Duration = Duration::from_micros(100);
    /// 100µs << 11 is 204.8ms, the last step below `POLL_INTERVAL`.
    const MAX_SHIFT: u32 = 11;

    let nap = empty_polls
        .checked_sub(SPIN_POLLS)
        .map(|over| (FIRST_NAP * (1u32 << over.min(MAX_SHIFT))).min(POLL_INTERVAL));
    *empty_polls = empty_polls.saturating_add(1);
    match nap {
        Some(d) => thread::sleep(d.min(remaining)),
        None => thread::yield_now(),
    }
}

/// Longest a single blocked `read` may sit before the loop re-checks its
/// deadline. Not a bound on anything by itself — purely how often
/// [`read_exact_by`] gets to look at the clock.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Read exactly `buf.len()` bytes, giving up at `deadline` however the peer
/// paces them.
///
/// **`Read::read_exact` cannot express this, which is why it is not used.** Its
/// loop retries until the buffer is full, and the only bound available to it is
/// the socket's `SO_RCVTIMEO` — which the kernel restarts on *every* `recv`
/// syscall. So a peer that delivers a single byte before each expiry resets the
/// clock forever and holds the caller inside one `read_exact` indefinitely.
/// Measured on a `UnixStream` pair: a 300 ms receive timeout survived 2.3 s of
/// one-byte-per-200 ms dribble, and the ceiling scales with the frame size, so
/// a legitimate under-cap frame is as exploitable as an over-cap one. That is
/// why the frame cap does not close this on its own.
///
/// The deadline is **total**, not per-syscall, and progress does not extend it.
/// That is the whole point: a peer controls the pacing but not the wall clock.
///
/// `WouldBlock`/`TimedOut` is not an error here — it is how a per-syscall
/// timeout reports "nothing yet" — so it re-loops and lets the deadline decide.
/// `Interrupted` likewise retries, since a signal is not the peer's doing.
/// `filled` is **in/out**: the caller passes how many bytes of `buf` a previous
/// attempt already read, and this updates it as bytes land. That is what makes a
/// timeout resumable — see [`PartialFrame`]. Without it a retry would re-read
/// from offset 0, and the bytes already taken off the socket would be lost.
fn read_exact_by(
    stream: &mut ControlStream,
    buf: &mut [u8],
    deadline: Instant,
    filled: &mut usize,
    wakeup: &Wakeup,
) -> std::io::Result<()> {
    // Consecutive polls that found the stream empty, for `back_off`. Reset the
    // moment bytes land, so a peer that is merely slow between frames never
    // drags a responsive stream into the coarse end of the backoff.
    let mut empty_polls: u32 = 0;

    while *filled < buf.len() {
        let now = Instant::now();
        if now >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "frame read exceeded its total deadline with {filled} of {} bytes read",
                    buf.len()
                ),
            ));
        }
        // Where the wakeup is a peek, ask before reading. A blocking `read` on
        // an empty pipe parks until the peer speaks, and the check above only
        // runs between syscalls — so without this the deadline is unenforceable
        // on Windows however short it is.
        if wakeup.would_park(stream) {
            back_off(&mut empty_polls, deadline - now);
            continue;
        }
        match stream.read(&mut buf[*filled..]) {
            // Zero bytes on a blocking stream means the peer closed.
            // `read_exact`'s own contract calls this `UnexpectedEof`, and
            // `crashed_on_eof` maps it to `ProcessCrashed`.
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "peer closed mid-frame",
                ));
            }
            Ok(n) => {
                *filled += n;
                empty_polls = 0;
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                // Nothing to sleep on under either wakeup, and that is a
                // property rather than an oversight. `Timeout` means the kernel
                // already blocked for the poll interval. The peek (Windows) means the read
                // only ran because bytes were waiting, so a would-block here is
                // the peek and the read disagreeing — a race, not a state — and
                // the next pass re-peeks and backs off there. Sleeping in
                // either case stacks a delay on one already taken.
            }
            // Anything else is a real failure, and *where* it happened is most
            // of the diagnosis: a bare `EINVAL` from a socket read says nothing
            // about whether the stream was mid-frame, how much had landed, or
            // which of the two reads in `recv_by` raised it. The kind is
            // preserved so `crashed_on_eof` and the `WouldBlock` arm above still
            // classify correctly; only the message grows.
            Err(e) => {
                return Err(std::io::Error::new(
                    e.kind(),
                    format!(
                        "read failed with {filled} of {} bytes in hand: {e}",
                        buf.len()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Where a frame read stopped, so the next attempt continues instead of
/// restarting.
///
/// **A stateless retry is not a resume.** A frame is a 4-byte big-endian length
/// prefix followed by that many bytes. [`recv_by`] begins every call by reading a
/// fresh prefix, so a deadline that expires part-way through a frame leaves
/// those bytes consumed and unrecoverable: the retry reads the *body's* first
/// four bytes as a length, and every frame after it is garbage. The symptom is a
/// decode error ("unexpected end of file") arriving long after the timeout that
/// caused it, on a peer that did nothing wrong.
///
/// So the state has to outlive the call. A caller that means to resume keeps one
/// of these across attempts and hands it back; [`recv_resumable`] fills in
/// whichever stage is outstanding and clears it once a whole frame is decoded.
///
/// Held by the bridge thread only, one per connection — the same scope as the
/// stream whose position it describes. Sharing one between streams would resume
/// one socket's frame from another's cursor.
#[derive(Default)]
pub(crate) struct PartialFrame {
    /// The 4-byte length prefix, and how much of it is in.
    prefix: [u8; 4],
    prefix_filled: usize,
    /// The body, allocated once the prefix completes, and how much of it is in.
    body: Vec<u8>,
    body_filled: usize,
    /// Whether the prefix is complete, so `body` is the live stage.
    have_len: bool,
    /// The blob the last frame read announced, taken from the stream right after
    /// it; replaced (and so closed) by the next frame read.
    blob: Option<Received>,
}

impl PartialFrame {
    /// True when a frame is half-read, so the stream is **not** on a frame
    /// boundary.
    ///
    /// A caller must not abandon the stream in this state: the unread remainder
    /// would be parsed as the next frame's length. Either resume this frame or
    /// treat the connection as lost.
    pub(crate) fn in_progress(&self) -> bool {
        self.prefix_filled > 0 || self.have_len
    }

    /// The blob the last frame read announced, if it announced one and it has
    /// not been taken.
    pub(crate) fn take_blob(&mut self) -> Option<Received> {
        self.blob.take()
    }

    /// Back to a frame boundary, once a frame is off the wire (whether or not
    /// it decoded). The body keeps its capacity for the next frame, trimmed
    /// after a large one. (There is no way to resynchronise a half-read
    /// frame, so this is never a recovery from one.)
    fn clear(&mut self) {
        self.prefix_filled = 0;
        self.body_filled = 0;
        self.have_len = false;
        crate::protocol::frame::keep(&mut self.body);
    }
}

/// Read one message, bounded in both size and total time, resuming `partial`.
///
/// `deadline` covers the whole frame — prefix and body together — so a peer
/// cannot buy extra time by splitting one across many reads.
///
/// Every stage reads into `partial` rather than into locals, so a deadline that
/// expires part-way through leaves the progress recorded and the next call picks
/// it up exactly where this one stopped. On success `partial` is cleared and the
/// stream is back on a frame boundary.
fn recv_by<T: Framed>(
    stream: &mut ControlStream,
    deadline: Instant,
    partial: &mut PartialFrame,
    wakeup: &Wakeup,
) -> IpcResult<T> {
    if !partial.have_len {
        let mut prefix = partial.prefix;
        let r = read_exact_by(
            stream,
            &mut prefix,
            deadline,
            &mut partial.prefix_filled,
            wakeup,
        );
        // Record what landed before propagating: the bytes are off the socket
        // either way, and losing them is exactly the desync this exists to stop.
        partial.prefix = prefix;
        r.map_err(|e| annotate(e, "length prefix"))
            .map_err(crashed_on_eof)?;

        // Reject before allocating: the length is the peer's word for how much
        // memory to reserve. The verdict is `ProcessCrashed` rather than a
        // decode error: a peer that cannot frame is one the host cannot keep
        // talking to, and the stream is desynchronised. `pump` turns it into
        // `crash()`. A send the host itself declines is **not** a crash; see
        // [`send`].
        let len = decode_prefix(partial.prefix, T::CAP).map_err(|e| {
            tracing::warn!("plugin server sent a {e}; treating it as a crash");
            IpcError::ProcessCrashed
        })?;
        // The buffer is the connection's: its capacity is kept between frames
        // (and trimmed after a large one, `clear`), so a block's reply reads
        // without allocating.
        partial.body.clear();
        partial.body.resize(len, 0);
        partial.body_filled = 0;
        partial.have_len = true;
    }

    // `body` is moved out and back so the borrow checker sees one mutable
    // borrow of `partial` at a time; the swap is two pointer writes.
    let mut body = core::mem::take(&mut partial.body);
    let r = read_exact_by(
        stream,
        &mut body,
        deadline,
        &mut partial.body_filled,
        wakeup,
    );
    partial.body = body;
    r.map_err(|e| annotate(e, "frame body"))
        .map_err(crashed_on_eof)?;

    let msg = decode_body(&partial.body);
    // The frame is off the wire and accounted for, so the stream is back on a
    // boundary whether or not its *contents* decoded. Clearing only on success
    // would leave a decode failure looking like a half-read frame forever.
    partial.clear();
    partial.blob = None;
    let msg: T = msg?;
    // The blob a frame announces is the next message on the stream, read before
    // any other read can reach it. A frame that announces none drops the last's.
    partial.blob = match msg.blob() {
        Some(wire) => Some(Blob::receive(super::sys::connection(stream), wire.clone())?),
        None => None,
    };
    Ok(msg)
}

/// Recv with a total-time bound, **not** resumable.
///
/// `timeout` is the budget for the **whole frame**, not for one syscall.
/// `SO_RCVTIMEO` is still set — it is what wakes a blocked `read` so the
/// deadline can be re-checked — but it no longer bounds the call, because a
/// peer resets it on every byte it sends.
///
/// A timeout here abandons whatever was read, so the stream may be left
/// mid-frame and the only safe response is to stop using it. Callers that need
/// to survive a timeout and keep the connection must use [`recv_resumable`]
/// instead; this one is for the paths where a timeout is already fatal
/// (handshake, probe, launch).
pub(crate) fn recv_within<T: Framed>(
    stream: &mut ControlStream,
    timeout: Duration,
) -> IpcResult<T> {
    let mut partial = PartialFrame::default();
    recv_resumable(stream, timeout, &mut partial)
}

/// Recv with a total-time bound, resuming a frame a previous call left half-read.
///
/// This is the one a caller may retry after a timeout. `partial` carries the
/// frame cursor between attempts: on `Timeout` it holds however much of the
/// frame arrived, and the next call with the *same* `partial` continues from
/// there rather than re-reading a length prefix that is no longer at the front
/// of the stream.
///
/// Check [`PartialFrame::in_progress`] on a timeout to tell "nothing arrived, the
/// stream is on a boundary" from "a frame is half-read and must be finished".
pub(crate) fn recv_resumable<T: Framed>(
    stream: &mut ControlStream,
    timeout: Duration,
    partial: &mut PartialFrame,
) -> IpcResult<T> {
    let deadline = Instant::now() + timeout;
    with_poll_timeout(stream, timeout, |s, wakeup| {
        recv_by(s, deadline, partial, wakeup)
    })
    .map_err(|e| match e {
        IpcError::Io(io)
            if io.kind() == std::io::ErrorKind::TimedOut
                || io.kind() == std::io::ErrorKind::WouldBlock =>
        {
            IpcError::Timeout {
                operation: "recv",
                after: timeout,
                partial: partial.in_progress(),
            }
        }
        other => other,
    })
}

/// Run `f` with the stream's receive timeout set short enough to re-check a
/// deadline, restoring blocking mode afterwards.
///
/// The poll interval is deliberately *not* the caller's budget: a blocked
/// `read` must wake often enough for the deadline check to be meaningful, and
/// with a 5 s budget a 5 s syscall timeout would let the whole budget elapse
/// inside one uninterruptible wait.
///
/// # The wakeup is per-platform, and arming it is best-effort
///
/// Treating a failed `set_recv_timeout` as fatal was a bug on two platforms at
/// once, and on one of them it made the transport unusable:
///
/// - **Windows.** `interprocess` refuses a receive timeout on a named pipe
///   unconditionally — there is no such option. Every receive failed before
///   reading a byte, which was most of what the Windows suite had to say the
///   first time it was ever run.
/// - **macOS.** `setsockopt(SO_RCVTIMEO)` starts refusing with `EINVAL` once the
///   peer closes its end of the Unix socket, so a receive that should have
///   drained the last buffered frame and reported EOF failed with "Invalid
///   argument" instead.
///
/// But *tolerating* the failure is not enough either, because the timeout is
/// what lets a blocked read return so the deadline can be re-checked. Without
/// some wakeup, a peer that goes silent mid-frame holds the thread until it
/// speaks again — measured, not assumed: with the fallback removed, two of the
/// nineteen hostile-peer tests fail, both the stalled-peer bounds.
///
/// So each platform gets the mechanism it actually has. Unix arms `SO_RCVTIMEO`
/// and lets the kernel end the wait; Windows never starts one, asking
/// `bytes_available` whether a read would block and doing the waiting on this
/// side. `set_nonblocking` looks like the Windows answer and is not — see
/// [`Wakeup::arm`] for what happened when it was tried.
///
/// The two are not equally precise, and the difference favours the peek: a
/// receive timeout can overshoot a deadline by up to one poll interval, while
/// `back_off` caps each nap at the time actually remaining.
///
/// The dribble attack in the module header is unaffected by any of this: a peer
/// that keeps *sending* makes the read return on its own, so the deadline is
/// checked between syscalls whatever is armed.
///
fn with_poll_timeout<T>(
    stream: &mut ControlStream,
    budget: Duration,
    f: impl FnOnce(&mut ControlStream, &Wakeup) -> IpcResult<T>,
) -> IpcResult<T> {
    let poll = budget.min(POLL_INTERVAL).max(Duration::from_millis(1));
    let wakeup = Wakeup::arm(stream, poll);
    let result = f(stream, &wakeup);
    wakeup.disarm(stream);
    result
}

/// Name which of `recv_by`'s two reads an error came from.
///
/// Kept separate from the error itself because the two are indistinguishable
/// otherwise — both are `read_exact_by` on the same stream — and the difference
/// decides whether the stream is on a frame boundary or stranded mid-frame.
fn annotate(e: std::io::Error, stage: &str) -> std::io::Error {
    std::io::Error::new(e.kind(), format!("reading the {stage}: {e}"))
}

fn crashed_on_eof(e: std::io::Error) -> IpcError {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        IpcError::ProcessCrashed
    } else {
        e.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioReply, AudioRequest, ControlFrame, News};

    /// A connected pair of local sockets.
    ///
    /// A real socket rather than an in-memory fake: the property under test is
    /// how the read loop behaves against a peer that stops mid-frame, and that
    /// only exists on something with a receive timeout and a kernel buffer.
    struct Pair {
        host: ControlStream,
        /// `Option` so a test can move the writing end onto its own thread; the
        /// stream has no `try_clone` in scope here and the tests need the two
        /// ends on different threads to interleave a pause with a read.
        peer: Option<ControlStream>,
    }

    impl Pair {
        /// Take the peer end, for a test that writes from another thread.
        fn take_peer(&mut self) -> ControlStream {
            self.peer.take().expect("peer already taken")
        }
    }

    fn pair() -> Pair {
        let (host, peer) = crate::transport::broker_pair();
        Pair {
            host,
            peer: Some(peer),
        }
    }

    /// **A setup is a frame and then its slab, and the slab opens the creator's
    /// memory.** The server's end reads the `SetupSharedMemory` frame, takes the
    /// slab right after it, and maps what the host wrote into.
    #[test]
    fn an_attached_request_is_a_frame_then_the_slab_it_announced() {
        use crate::protocol::frame::read_frame;
        use crate::{AudioSlab, RING_SLOTS, SlabLayout};
        let layout = SlabLayout {
            samples_per_channel: 8,
            format: tutti_buffers::FloatWidth::F32,
            slots: RING_SLOTS as u32,
            inputs: vec![tutti_plugin_types::ChannelLayout::MONO],
            outputs: vec![tutti_plugin_types::ChannelLayout::MONO],
        };
        let (mut host_slab, handle) = AudioSlab::create(layout.clone()).unwrap();
        host_slab.write(1, 0, &[0.25f32; 8]).unwrap();
        host_slab.publish(1);

        let mut p = pair();
        let mut server = p.take_peer();
        let announced = Announced::<crate::SetupSharedMemory>::new(handle, layout.clone());
        send_attached(&mut p.host, ReqId::new(7), announced).unwrap();

        let Request { id, body } = read_frame(&mut server).unwrap();
        assert_eq!(id, ReqId::new(7));
        let ControlRequest::SetupSharedMemory { slab, layout: got } = body else {
            panic!("the frame is not a setup");
        };
        assert_eq!(got, layout);
        let received = SlabHandle::receive(super::super::sys::connection(&server), slab).unwrap();
        let opened = AudioSlab::open(&received, layout).unwrap();
        let mut back = [0.0f32; 8];
        assert!(opened.has_input(1), "the host's published block is visible");
        opened.read_input_into(1, 0, &mut back).unwrap();
        assert_eq!(back, [0.25f32; 8]);
    }

    /// **A block's round trip allocates nothing once the buffers are warm**: the host sends
    /// `ProcessAudio`, the server end reads it and answers `AudioProcessed`, and the host reads the
    /// answer, over a real socket pair, inside `assert_no_alloc`. A block with no parameter changes
    /// and no MIDI, the common one; a change list that is not empty is a `Vec` the decode builds.
    #[test]
    fn a_block_round_trip_allocates_nothing_once_warm() {
        use crate::ProcessAudioData;
        use crate::protocol::frame::read_frame;

        let mut p = pair();
        let mut server = p.take_peer();
        let mut partial = PartialFrame::default();
        let block = AudioRequest::ProcessAudio(ProcessAudioData {
            seq: 1,
            num_samples: 64,
            ..ProcessAudioData::empty(tutti_plugin_types::SampleRate::SR_48K)
        });
        let reply = AudioReply::AudioProcessed {
            latency_us: 0,
            seq: 1,
            midi_out: Default::default(),
            status: Default::default(),
            refused: false,
            writes: crate::BlockWrites::Handed,
        };
        let mut round = || {
            send(&mut p.host, &block).expect("send");
            let got: AudioRequest = read_frame(&mut server).expect("server read");
            assert!(matches!(got, AudioRequest::ProcessAudio(ref d) if d.seq == 1));
            write_frame(&mut server, &reply).expect("server write");
            let back =
                recv_resumable::<AudioReply>(&mut p.host, Duration::from_secs(5), &mut partial)
                    .expect("host read");
            assert!(matches!(back, AudioReply::AudioProcessed { seq: 1, .. }));
        };
        round(); // warms this thread's buffers and the cursor's
        assert_no_alloc::assert_no_alloc(&mut round);
    }

    /// Send one framed `ControlFrame`, holding the body back between the
    /// length prefix and the body so a reader's deadline lands in the middle of
    /// the frame: `prefix_sent` is signalled once the prefix is on the wire, and
    /// the body goes out when `release_body` fires.
    ///
    /// The body is withheld until the test releases it, which is what puts the
    /// reader in the state a restart cannot recover from, however the scheduler
    /// runs the two threads.
    fn send_split(
        peer: &ControlStream,
        msg: &ControlFrame,
        prefix_sent: &std::sync::mpsc::Sender<()>,
        release_body: &std::sync::mpsc::Receiver<()>,
    ) {
        use std::io::Write;
        let mut peer = peer;
        let data = postcard::to_allocvec(msg).unwrap();
        peer.write_all(&(data.len() as u32).to_be_bytes()).unwrap();
        peer.flush().unwrap();
        prefix_sent.send(()).unwrap();
        release_body
            .recv_timeout(Duration::from_secs(60))
            .expect("the test releases the body");
        peer.write_all(&data).unwrap();
        peer.flush().unwrap();
    }

    fn latency_msg(samples: i64) -> ControlFrame {
        ControlFrame::News(News::LatencyChanged {
            samples: tutti_plugin_types::Samples::new(samples as usize),
        })
    }

    /// **A timeout part-way through a frame is resumed, not restarted.**
    ///
    /// This is the property the whole `PartialFrame` type exists for, and the
    /// one a stateless retry silently violates. `recv_by` reads a fresh 4-byte
    /// length prefix at the top of every call, so a second attempt without the
    /// cursor reads the *body's* first four bytes as a length — a number in the
    /// hundreds of millions for typical message bytes — and then either trips
    /// the frame cap or blocks forever waiting for a body that will never come.
    /// Either way the frame is lost and every frame behind it is garbage.
    ///
    /// The failure is silent where it happens: nothing reports a desync, and the
    /// symptom is a decode error much later, on a peer that did nothing wrong.
    /// That is why this asserts on the *delivered message* rather than on an
    /// error code — a restart cannot produce the right one by luck.
    ///
    /// The body is sent only after the first read has timed out, and the first
    /// read starts only after the prefix is on the wire, so the first read
    /// lands mid-frame however the threads are scheduled.
    #[test]
    fn a_frame_interrupted_by_a_deadline_is_resumed_from_where_it_stopped() {
        let mut p = pair();
        let msg = latency_msg(4242);

        let sent = msg.clone();
        let peer = p.take_peer();
        let (prefix_sent, prefix_on_wire) = std::sync::mpsc::channel();
        let (release_body, body_released) = std::sync::mpsc::channel();
        let writer =
            std::thread::spawn(move || send_split(&peer, &sent, &prefix_sent, &body_released));
        let host = &mut p.host;

        let mut partial = PartialFrame::default();

        // First attempt: the prefix has landed, the body has not.
        prefix_on_wire
            .recv_timeout(Duration::from_secs(60))
            .expect("the writer sent the prefix");
        let err = recv_resumable::<ControlFrame>(host, Duration::from_millis(20), &mut partial)
            .unwrap_err();
        match err {
            IpcError::Timeout { partial: p, .. } => assert!(
                p,
                "the prefix was consumed, so the stream is mid-frame and the \
                 timeout must say so — a caller told otherwise would restart \
                 and misparse the body"
            ),
            other => panic!("expected a mid-frame timeout, got {other:?}"),
        }
        assert!(
            partial.in_progress(),
            "the cursor must hold the half-read frame; an empty cursor is a \
             restart by another name"
        );

        // Second attempt with the SAME cursor: continues from the body.
        release_body.send(()).expect("the writer waits");
        let got = recv_resumable::<ControlFrame>(host, Duration::from_secs(5), &mut partial)
            .expect("the resumed read must deliver the frame the peer sent");
        assert_eq!(
            format!("{got:?}"),
            format!("{msg:?}"),
            "the resumed read delivered a different message than was sent, so \
             it re-parsed the stream from the wrong offset rather than \
             continuing the interrupted frame"
        );
        assert!(
            !partial.in_progress(),
            "a completed frame must leave the cursor clear, or the next read \
             resumes a frame that is already finished"
        );

        writer.join().unwrap();
    }

    /// After a resumed frame, the **next** frame parses correctly.
    ///
    /// The half of the property the single-frame test cannot show. A resume that
    /// consumed one byte too few or too many can still return a plausible first
    /// message, and only the frame behind it reveals the stream is off by that
    /// much. Sending a
    /// second, distinguishable message is what makes the offset observable.
    #[test]
    fn the_frame_after_a_resumed_one_still_parses() {
        let mut p = pair();
        let first = latency_msg(1111);
        let second = latency_msg(2222);

        let (a, b) = (first.clone(), second.clone());
        let peer = p.take_peer();
        let (prefix_sent, prefix_on_wire) = std::sync::mpsc::channel();
        let (release_body, body_released) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            // Interrupt the first frame, then send the second normally.
            send_split(&peer, &a, &prefix_sent, &body_released);
            write_frame(&mut &peer, &b).unwrap();
        });

        let host = &mut p.host;
        let mut partial = PartialFrame::default();
        prefix_on_wire
            .recv_timeout(Duration::from_secs(60))
            .expect("the writer sent the prefix");
        let _ = recv_resumable::<ControlFrame>(host, Duration::from_millis(20), &mut partial)
            .unwrap_err();
        release_body.send(()).expect("the writer waits");

        let got_first =
            recv_resumable::<ControlFrame>(host, Duration::from_secs(5), &mut partial).unwrap();
        assert_eq!(format!("{got_first:?}"), format!("{first:?}"));

        let got_second = recv_resumable::<ControlFrame>(host, Duration::from_secs(5), &mut partial)
            .expect("the frame after a resumed one must parse");
        assert_eq!(
            format!("{got_second:?}"),
            format!("{second:?}"),
            "the second frame decoded wrongly, so the resumed read left the \
             stream off by some bytes — the desync a restart causes, just one \
             frame later"
        );

        writer.join().unwrap();
    }

    /// A deadline that expires with **nothing read** reports `partial: false`.
    ///
    /// This is the case a caller may resume from: the stream is still on a frame
    /// boundary, so the next read starts a whole message. Without the
    /// distinction a caller has to treat every timeout as fatal, which is what
    /// made one missed block budget kill a live plugin session.
    #[test]
    fn a_timeout_with_nothing_read_reports_a_framed_stream() {
        let mut p = pair();
        // The peer sends nothing at all.
        let err = recv_within::<ControlFrame>(&mut p.host, Duration::from_millis(20)).unwrap_err();
        match err {
            IpcError::Timeout { partial, .. } => assert!(
                !partial,
                "no byte was ever sent, so the stream is still on a frame \
                 boundary — reporting `partial` here would make every idle \
                 timeout look like an unrecoverable desync"
            ),
            other => panic!("expected a timeout, got {other:?}"),
        }
    }

    /// A deadline that expires **part-way through a frame** reports
    /// `partial: true`.
    ///
    /// This is the case a caller may *not* resume from: the unread remainder
    /// will be parsed as the next frame's length prefix, and every frame after
    /// it is garbage. The desync is silent where it happens and surfaces much
    /// later as a decode error, so the flag is the only thing that can attribute
    /// it — and the only thing that lets a caller finish the frame instead.
    #[test]
    fn a_timeout_part_way_through_a_frame_reports_a_broken_stream() {
        let mut p = pair();
        // Announce a 4 KiB body, then send 8 bytes of it and stop.
        {
            use std::io::Write;
            let peer = p.peer.as_ref().unwrap();
            let mut peer = peer;
            peer.write_all(&4096u32.to_be_bytes()).unwrap();
            peer.write_all(&[0u8; 8]).unwrap();
            peer.flush().unwrap();
        }
        let err = recv_within::<ControlFrame>(&mut p.host, Duration::from_millis(50)).unwrap_err();
        match err {
            IpcError::Timeout { partial, .. } => assert!(
                partial,
                "the prefix and 8 body bytes were consumed, so the stream is \
                 mid-frame. Reporting it as clean would let a caller resume and \
                 read the frame's tail as a length prefix"
            ),
            other => panic!("expected a timeout, got {other:?}"),
        }
    }

    /// Tests that exist only where the socket has a kernel send timeout.
    #[cfg(unix)]
    mod unix_send_bound {
        use super::super::*;
        use crate::ControlRequest;
        use interprocess::local_socket::traits::Stream as _;

        /// **A send to a peer that reads nothing gives up at the send deadline**, as a `partial`
        /// timeout: the frame may be half on the wire, so the connection cannot go on. Before, the
        /// write blocked until the peer read or died.
        ///
        /// Unix only: Windows has no send timeout on a named pipe (a hung write there ends
        /// when the server is ended and the pipe closes).
        #[test]
        fn a_send_to_a_peer_that_reads_nothing_is_bounded() {
            let (mut host, _peer) = crate::transport::broker_pair();
            host.set_send_timeout(Some(Duration::from_millis(200)))
                .expect("a send timeout");
            let chunk = crate::Request {
                id: crate::ReqId::new(1),
                body: ControlRequest::GetParameterValueFromText {
                    param_id: tutti_units::ParamId::new(1),
                    text: "x".repeat(1 << 20),
                },
            };
            // Far more than any socket buffer holds: the peer reads nothing.
            let result = (0..256).try_for_each(|_| send(&mut host, &chunk));
            assert!(
                matches!(result, Err(IpcError::Timeout { partial: true, .. })),
                "{result:?}"
            );
        }
    }
}
