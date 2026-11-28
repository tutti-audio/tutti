//! The server's main-thread event loop.
//!
//! The main thread serves **one loop** with three sources, and **renders
//! nothing**: the blocks are the
//! render thread's ([`crate::render`]), on a connection of their own, so
//! whatever this loop spends (an editor's paint, a plugin's timer) delays no
//! block.
//!
//! 1. **The control connection**, waited on ([`Native::wait`]) until the next
//!    tick is due. A readable socket ends the wait the instant a
//!    byte arrives: every request but the blocks, and the plugin's
//!    unsolicited news in the other direction, rides here.
//! 2. **An idle tick**, [`TICK_PERIOD`] (about 30 Hz): the open editor's idle
//!    ([`EditorWindow::editor_idle`](tutti_plugin_types::EditorWindow): VST2's
//!    `effEditIdle`), the plugin's own callbacks (CLAP's, served by
//!    [`Session::tick`]'s service pass), and a plugin's resize request. A tick
//!    is due once and not owed: a loop that was busy with blocks for 100 ms
//!    runs one tick after them, never three.
//!
//!    **The plugin's own event sources are not the tick's.** What a plugin asks
//!    to have waited on (CLAP's `posix-fd-support` and `timer-support`, VST3's
//!    `Linux::IRunLoop`) is [`EditorWindow::wakeups`](tutti_plugin_types::EditorWindow):
//!    its descriptors and the earliest of its timers. The loop's wait covers
//!    them with the socket (one `poll(2)` on unix, [`Native::wait`]) and ends at
//!    the earliest deadline, so a registered descriptor that becomes readable is
//!    served the instant it does and a timer at its period, not on the next
//!    tick; what fired is handed back with
//!    [`EditorWindow::wake`](tutti_plugin_types::EditorWindow) ([`Session::wake`]).
//!    The other backends wait for timers alone: no plugin registers a
//!    descriptor where its format has none.
//! 3. **The bell** ([`Bell`]): a plugin that calls the host from a thread of its
//!    own (its `request_process`, say) ends the wait, and what it raised leaves
//!    at once, not on the next tick.
//!
//! The loop owns no state: what a message or a tick does is [`Session`]'s.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tutti_platform::{Executor, Instant};

use tutti_plugin_ipc::{ControlFrame, Reaction};
use tutti_plugin_ipc_ffi::Ready;
use tutti_plugin_types::{IpcError, IpcResult, MainThreadMarker, StateError, Wakeups};

use crate::render::RenderThread;
use crate::session::Session;
use crate::transport::ControlTransport;

/// How often the idle tick runs while nothing else wakes the loop: about 30 Hz,
/// the rate a plugin's editor idle is conventionally driven at.
pub(crate) const TICK_PERIOD: Duration = Duration::from_millis(33);

/// When the next idle tick is due, as a value of the clock it is given.
///
/// Pure: it reads no clock, so its arithmetic is tested with made-up instants.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ticker {
    period: Duration,
    next: Instant,
}

impl Ticker {
    /// A ticker whose first tick is due `period` after `now`.
    pub(crate) fn new(now: Instant, period: Duration) -> Self {
        Self {
            period,
            next: now + period,
        }
    }

    /// How long until the tick is due: zero when it is.
    pub(crate) fn wait(&self, now: Instant) -> Duration {
        self.next.saturating_duration_since(now)
    }

    /// Whether a tick is due at `now`. If it is, the next one is a period from
    /// `now`: a late tick is not made up for.
    pub(crate) fn take_due(&mut self, now: Instant) -> bool {
        if now < self.next {
            return false;
        }
        self.next = now + self.period;
        true
    }
}

// One backend per OS, chosen here and nowhere else in the crate: what the loop
// waits with, the bell it is woken by, the platform's `Stream` and how the host's
// connection becomes one. These attributes carry the only platform `cfg` of the server's
// non-test code. A native loop (`MsgWaitForMultipleObjectsEx` with overlapped
// pipe I/O on Windows, `CFRunLoop` with the socket as a source on macOS)
// replaces one file and changes nothing in the loop below.
#[cfg_attr(unix, path = "unix.rs")]
#[cfg_attr(not(unix), path = "windows.rs")]
mod sys;

pub(crate) use sys::{
    Bell, LaneEnd, Native, Stream, connection, meet, receive_lane, wait_readable,
};
pub use sys::{HostEnd, adopt_host};

impl Native {
    /// Waits until `transport` has a frame to read (`Ok(true)`), the `bell`
    /// rings or `timeout` passes (`Ok(false)`, `ready` empty), or, where the
    /// platform can wait on them, a descriptor the plugin asked to have watched
    /// (`wakeups`) is ready (into `ready`, `Ok(false)`).
    ///
    /// A transport that is not a socket (a test's script) has no stream to
    /// wait on: its own wait stands, and no plugin descriptor is watched.
    ///
    /// # Errors
    ///
    /// The wait fails.
    pub(crate) fn wait(
        &mut self,
        transport: &mut ControlTransport,
        timeout: Duration,
        wakeups: &Wakeups,
        bell: &Bell,
        ready: &mut Vec<Ready>,
    ) -> IpcResult<bool> {
        ready.clear();
        match transport.stream() {
            Some(stream) => self.wait_on(stream, wakeups, timeout, bell, ready),
            None => transport.wait_readable(Some(timeout), bell),
        }
    }
}

/// Serves `transport`, the host's control connection, until the host
/// disconnects or asks for shutdown. `render` is the thread serving its audio
/// connection: the loop ends if that peer breaks the protocol. `executor` runs
/// the beat that tells the host a state call is at work.
///
/// # Errors
///
/// The transport fails (the host went away mid-frame, or sent one that does
/// not decode), the audio connection's peer sends what it does not carry, or
/// `session` fails in a way it cannot answer as a reply.
pub(crate) fn serve(
    session: &mut Session,
    transport: &mut ControlTransport,
    main: MainThreadMarker,
    render: &RenderThread,
    executor: &Executor,
) -> IpcResult<()> {
    serve_every(
        session,
        transport,
        &mut Native::default(),
        main,
        render,
        executor,
        TICK_PERIOD,
    )
}

/// A lost plugin ends the server abnormally, not through the clean shutdown:
/// the host reads the exit as a crash and recovers the plugin. What the last
/// pass queued has been sent by the time this is called.
///
/// # Errors
///
/// [`StateError::PluginCrashed`], when the plugin is lost.
fn ensure_not_lost(session: &Session) -> IpcResult<()> {
    if session.plugin_lost() {
        tracing::error!("plugin-server: the plugin is lost; the server exits");
        return Err(IpcError::State(StateError::PluginCrashed));
    }
    Ok(())
}

/// Whether a connection's end is the peer leaving: a clean end of stream, or
/// the socket reset or broken under it. Every other error is the peer
/// breaking the protocol.
fn host_left(error: &IpcError) -> bool {
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset, UnexpectedEof};
    /// Windows' `ERROR_PIPE_NOT_CONNECTED`, a named pipe read after the peer
    /// closed: std maps it to no kind (`ERROR_BROKEN_PIPE`, 109, is
    /// `BrokenPipe`), and no unix errno is 233.
    const PIPE_NOT_CONNECTED: i32 = 233;
    match error {
        IpcError::ProcessCrashed => true,
        IpcError::Io(e) => {
            matches!(
                e.kind(),
                UnexpectedEof | BrokenPipe | ConnectionReset | ConnectionAborted
            ) || e.raw_os_error() == Some(PIPE_NOT_CONNECTED)
        }
        _ => false,
    }
}

/// [`serve`] with a tick every `period`.
pub(crate) fn serve_every(
    session: &mut Session,
    transport: &mut ControlTransport,
    backend: &mut Native,
    main: MainThreadMarker,
    render: &RenderThread,
    executor: &Executor,
    period: Duration,
) -> IpcResult<()> {
    let bell = session.ties().bell.clone();
    let host_gone = Arc::clone(&session.ties().host_gone);
    let mut ticker = Ticker::new(Instant::now(), period);
    // What the plugin wants waited on, and what the wait found ready: reused,
    // so a steady watch set allocates nothing.
    let mut wakeups = Wakeups::new();
    let mut ready: Vec<Ready> = Vec::new();
    loop {
        ensure_not_lost(session)?;
        // An audio connection that ends cleanly is the host leaving, which the
        // control connection says too. Any other end is a peer that broke the
        // protocol (a frame that does not decode, an oversize or version-skewed
        // one) while the control connection goes on: logged here, off the
        // render thread, and the session ends with it.
        if let Some(error) = render.ended()
            && !host_left(error)
        {
            tracing::error!("plugin-server: the audio connection failed: {error}; ending");
            return Err(IpcError::AudioConnection {
                cause: Arc::clone(error),
            });
        }
        if ticker.take_due(Instant::now()) {
            for event in session.tick(main) {
                transport.send(&event.into())?;
            }
        }
        // A tick that lost the plugin ends the server before it waits.
        ensure_not_lost(session)?;
        // The plugin's own event sources are waited on here, with the socket
        // and the tick: its descriptors, and its earliest timer bounds the
        // wait. Read after the tick and the last message, which change them.
        wakeups.clear();
        session.wakeups(main, &mut wakeups);
        let now = Instant::now();
        let timeout = wakeups
            .until_due(now)
            .map_or(ticker.wait(now), |due| due.min(ticker.wait(now)));
        let readable = backend.wait(transport, timeout, &wakeups, &bell, &mut ready)?;
        // The host process ended (the watch rang the bell to end the wait): the
        // session is over, as when the control connection ends.
        if host_gone.load(Ordering::Acquire) {
            return Ok(());
        }
        // A plugin called the host from a thread of its own (its `request_process`,
        // say) and rang the bell: what it raised leaves now, not on the tick.
        if bell.rung() {
            for event in session.drain_async_events(main) {
                transport.send(&event.into())?;
            }
        }
        let now = Instant::now();
        if !ready.is_empty() || wakeups.is_due(now) {
            for event in session.wake(main, &ready, now) {
                transport.send(&event.into())?;
            }
        }
        if !readable {
            continue;
        }
        let incoming = transport.recv()?;
        let (id, reaction) = crate::server::react(session, main, executor, transport, incoming);
        match reaction {
            Reaction::Shutdown => return Ok(()),
            Reaction::Reply(reply) => transport.send(&ControlFrame::Reply { id, reply })?,
            Reaction::State(state) => crate::transport::send_state(transport, id, state)?,
            Reaction::None => {}
        }
        for event in session.drain_async_events(main) {
            transport.send(&event.into())?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Only the peer leaving is "host left"**: a clean end of stream, a
    /// broken, reset or aborted connection, or a named pipe read after its
    /// peer closed (`ERROR_PIPE_NOT_CONNECTED`, which std gives no kind). A
    /// frame that does not decode, an oversize or skewed one, and any other
    /// I/O error is the peer breaking the protocol.
    #[test]
    fn only_the_peer_leaving_is_host_left() {
        use std::io::{Error, ErrorKind};
        let io = |e: Error| IpcError::Io(e);
        for kind in [
            ErrorKind::UnexpectedEof,
            ErrorKind::BrokenPipe,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
        ] {
            assert!(host_left(&io(Error::from(kind))), "{kind:?}");
        }
        assert!(host_left(&IpcError::ProcessCrashed));
        assert!(host_left(&io(Error::from_raw_os_error(233))));
        for kind in [
            ErrorKind::InvalidData,
            ErrorKind::PermissionDenied,
            ErrorKind::TimedOut,
        ] {
            assert!(!host_left(&io(Error::from(kind))), "{kind:?}");
        }
        assert!(!host_left(&IpcError::ProtocolMismatch {
            expected: 1,
            got: 2
        }));
        assert!(!host_left(&IpcError::ProtocolError("junk")));
    }

    const PERIOD: Duration = Duration::from_millis(33);

    /// **A tick is due a period after the last, and not before.** The wait
    /// shrinks as the instant nears and is zero once it is due.
    #[test]
    fn a_tick_is_due_a_period_after_the_last() {
        let t0 = Instant::now();
        let mut ticker = Ticker::new(t0, PERIOD);
        assert_eq!(ticker.wait(t0), PERIOD);
        assert_eq!(ticker.wait(t0 + PERIOD / 3), PERIOD - PERIOD / 3);
        assert!(!ticker.take_due(t0 + PERIOD - Duration::from_millis(1)));
        assert!(ticker.take_due(t0 + PERIOD));
        assert_eq!(ticker.wait(t0 + PERIOD), PERIOD, "the next is a period on");
        assert_eq!(ticker.wait(t0 + PERIOD * 5), Duration::ZERO);
    }

    /// **A loop that was busy owes one tick, not one per period it missed.**
    #[test]
    fn a_late_tick_is_not_made_up_for() {
        let t0 = Instant::now();
        let mut ticker = Ticker::new(t0, PERIOD);
        let late = t0 + PERIOD * 10;
        assert!(ticker.take_due(late));
        assert!(
            !ticker.take_due(late),
            "one tick for all the periods missed"
        );
        assert!(!ticker.take_due(late + PERIOD - Duration::from_millis(1)));
        assert!(ticker.take_due(late + PERIOD));
    }
}

#[cfg(all(test, unix, feature = "clap"))]
mod loop_tests;
