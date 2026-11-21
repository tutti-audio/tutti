//! `PluginServer` — the outer shell. Owns a [`Session`] and drives it
//! over a [`Transport`] across the connection dance the host expects: first a
//! handshake connection for plugin load / format negotiation / shared-memory
//! setup, then, for the rest of the session, two more: the audio connection,
//! which the render thread serves (`ProcessAudio` and `Reset`, nothing else),
//! and the control connection, which the main thread's event loop serves.

use crate::render::{NotHome, RenderSide, RenderThread};
use crate::session::{Reaction, Session};
use crate::transport::Pulser;
use crate::transport::{ControlTransport, Incoming, Transport, TransportListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use tutti_platform::{Executor, Task, join_within};
use tutti_plugin_ipc::{
    AudioReply, ControlFrame, ControlRequest, PROTOCOL_VERSION, ReqId, Request, Working,
};
use tutti_plugin_types::MainThreadMarker;
use tutti_plugin_types::{IpcError, IpcResult};

/// Ask the OS for the highest priority it grants this thread, now that it is
/// about to do nothing but per-block audio work (a plugin's thread-pool helpers;
/// the render thread gets its priority from `spawn_dedicated`), and log what it
/// got once.
/// That is not real time: on Linux it is nice -20 at best, and a refusal
/// (`EPERM`) is common (`tutti_platform::request_priority`).
///
/// The host side needs this less: CoreAudio, WASAPI and JACK create the
/// audio callback's thread at an elevated priority themselves (cpal's ALSA
/// backend does not: its callback thread is an ordinary one). A plugin subprocess
/// gets no such treatment — it is an ordinary `Command::spawn` at default
/// priority — yet it must answer within the same block period.
///
/// This is a *quality* knob, not a correctness one: a late reply yields silence,
/// never wrong audio, because the host reads a slot only when the slab's
/// sequence number says the server published it (see
/// `tutti-plugin-ipc`'s `transport::shm::header`).
///
/// Failure is logged and ignored — elevation needs privileges a sandbox or CI
/// runner may not grant, and degraded quality beats refusing to run.
#[cfg(feature = "clap")]
pub(crate) fn request_audio_priority(what: &str) {
    let outcome = tutti_platform::request_priority(tutti_platform::PriorityRequest::Highest);
    if outcome.is_granted() {
        tracing::debug!("plugin-server: {what}: {outcome}");
    } else {
        tracing::info!("plugin-server: {what}: {outcome}; expect more dropped blocks under load");
    }
}

/// How a served session ended, when it did not fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use]
pub enum Ended {
    /// The session ended and every plugin it loaded was destroyed.
    Clean,
    /// A plugin was leaked: its render thread returned the processor too late,
    /// and destroying the plugin would have freed what that thread still reads.
    /// A render thread that never returns ends the process outright instead
    /// (status 1), and a host that runs the server kills it after
    /// `Timeouts::shutdown`, so this status is for a supervisor that runs the
    /// server itself.
    PluginLeaked,
    /// Every plugin was destroyed, but tasks of the server (the render lane, the
    /// load's beat) did not stop within their patience and were left running
    /// until the process ends: how many. Only a supervisor that runs the server
    /// itself reads it, as [`EXIT_TASKS_DETACHED`].
    TasksDetached(u64),
    /// Every plugin was destroyed, but a render lane ended without handing its
    /// state back (it panicked, or was dropped under the executor's shutdown):
    /// how many. Read as [`EXIT_RENDER_LANE_LOST`].
    RenderLaneLost(u64),
}

/// The exit status of a server whose session ended with [`Ended::PluginLeaked`].
/// No host reads it: it is for a supervisor that runs the server itself, or a
/// person reading the status (see [`Ended::PluginLeaked`]).
pub const EXIT_PLUGIN_LEAKED: u8 = 3;

/// The exit status of a server whose session ended with [`Ended::TasksDetached`].
/// No host reads it, as for [`EXIT_PLUGIN_LEAKED`]: the host's own detached
/// tasks are its count, `PluginControls::detached_tasks`.
pub const EXIT_TASKS_DETACHED: u8 = 4;

/// The exit status of a server whose session ended with [`Ended::RenderLaneLost`].
/// No host reads it, as for [`EXIT_PLUGIN_LEAKED`].
pub const EXIT_RENDER_LANE_LOST: u8 = 6;

/// The exit status of a server that did not start because the thread that
/// watches its host could not start ([`IpcError::HostUnwatched`]). The host
/// reads it as how its server ended.
pub const EXIT_HOST_UNWATCHED: u8 = 5;

/// The exit status of a server whose session failed with `error`:
/// [`EXIT_HOST_UNWATCHED`] for [`IpcError::HostUnwatched`], and 1 for any other.
#[must_use]
pub fn failure_status(error: &IpcError) -> u8 {
    match error {
        IpcError::HostUnwatched(_) => EXIT_HOST_UNWATCHED,
        _ => 1,
    }
}

/// The subprocess-side server: one hosted plugin, one host connection.
///
/// Construct with [`PluginServer::new`] and hand control to
/// [`PluginServer::run`], which does not return until the host disconnects or
/// asks for shutdown. Dropping it closes any open editor window and unlinks the
/// socket.
pub struct PluginServer {
    /// The socket the host connects to, unlinked on drop.
    socket: PathBuf,
    session: Session,
    /// What renders the blocks, until the audio connection is there to hand it
    /// to a thread.
    render: Option<RenderSide>,
    /// The thread that built the server: the one the plugin's main-thread
    /// calls run on, claimed once at startup.
    main: MainThreadMarker,
    /// Sent to once the shutdown is under way: a host that ends then finds the
    /// teardown, itself bounded, and is let finish ([`watch_host`]).
    served: mpsc::Sender<()>,
    /// The watch on the host, held for the life of the server; a drop cancels it.
    _host_watch: Option<Task<()>>,
    /// Where this process's background work runs: built on the main thread and
    /// held until the server drops, so every task on it (the render lane, a
    /// beat, the host watch) is stopped before it goes. No task holds it: they
    /// hold its liveness or its timer. Its drop joins the host watch's thread,
    /// which ends within a watch slice (a quarter of a second).
    executor: Executor,
}

impl std::fmt::Debug for PluginServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginServer")
            .field("socket", &self.socket)
            .finish_non_exhaustive()
    }
}

impl PluginServer {
    /// Creates a server that listens at `socket`, and starts the watch on the spawning host:
    /// a host that ends is told to the main loop, which shuts down as it does
    /// when the control connection ends. A server that has not done so within
    /// two seconds (blocked in an accept, a plugin's modal dialog, a
    /// block) is ended outright, without running a plugin library's exit
    /// handlers, and its socket file is removed.
    ///
    /// The socket is not created until [`PluginServer::run`]. Call this early.
    ///
    /// # Errors
    ///
    /// The thread is not the process's main thread, the OS refused the
    /// session's bells, or the thread that watches the host could not start
    /// ([`IpcError::HostUnwatched`], [`EXIT_HOST_UNWATCHED`]).
    pub fn new(socket: PathBuf) -> IpcResult<Self> {
        let main = MainThreadMarker::new().ok_or(IpcError::ProtocolError(
            "the plugin server must be built on the process's main thread",
        ))?;
        let (session, render) = Session::new()?;
        let (served, shutdown_under_way) = mpsc::channel();
        let executor = Executor::new();
        let host_watch = watch_host(
            &executor,
            socket.clone(),
            Arc::clone(&session.ties().host_gone),
            session.ties().bell.clone(),
            shutdown_under_way,
        )
        .map_err(IpcError::HostUnwatched)?;
        Ok(Self {
            socket,
            session,
            render: Some(render),
            main,
            served,
            _host_watch: host_watch,
            executor,
        })
    }

    /// Serves one host, then returns how the session ended ([`Ended`]). Blocks
    /// for the lifetime of the session.
    ///
    /// # The connection dance
    ///
    /// The host connects three times, and the split is what lets the blocks run
    /// at the highest priority the OS grants (not real time) on a thread that
    /// nothing else shares:
    ///
    /// 1. **Handshake**: plugin load, format negotiation, shared-memory setup.
    ///    Returns early if the host asks for shutdown before any audio flows.
    /// 2. **Audio**: `ProcessAudio` and `Reset`, in order, answered one by one.
    ///    The **render thread** serves it (`render`): it owns the
    ///    plugin's `Processor`, runs at the highest priority the OS grants (the
    ///    executor's `spawn_dedicated` asks for it), and shares nothing with the
    ///    main thread but queues, so an editor's paint can never delay a block.
    /// 3. **Control**: every other request and the plugin's unsolicited news,
    ///    for the rest of the session. The **main thread** serves it in its
    ///    event loop (`main_loop`), which keeps the plugin's
    ///    `Controller`, its editor and the idle tick.
    ///
    /// Each phase opens with a [`ControlFrame::Ready`] carrying
    /// `PROTOCOL_VERSION`; the host refuses a version it does not know.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket cannot be bound, if a handshake frame
    /// does not decode (a host of another protocol), or if the control
    /// connection fails. The handshake connection closing is the host's way of
    /// ending that phase, not an error. However it ends, the plugin is
    /// released first.
    pub fn run(mut self) -> IpcResult<Ended> {
        let served = self.serve();
        // Again: the paths that never reached the control phase.
        let _ = self.served.send(());
        self.release();
        // The plugin and the session go with the server: what they leak, they
        // leak here.
        drop(self);
        served.map(|()| {
            match (
                crate::leak::happened(),
                crate::leak::lanes_lost(),
                crate::leak::detached(),
            ) {
                (true, ..) => Ended::PluginLeaked,
                (false, lost @ 1.., _) => Ended::RenderLaneLost(lost),
                (false, 0, 0) => Ended::Clean,
                (false, 0, detached) => Ended::TasksDetached(detached),
            }
        })
    }

    /// The phases of [`run`](Self::run), up to the plugin's release.
    fn serve(&mut self) -> IpcResult<()> {
        let listener = TransportListener::bind(&self.socket)?;

        // First connection: handshake.
        let mut handshake = listener.accept()?;
        handshake.send(&ControlFrame::Ready {
            protocol_version: PROTOCOL_VERSION,
        })?;
        if self.handshake_phase(&mut handshake)? {
            return Ok(());
        }
        drop(handshake);

        // Second connection: audio. Third: control.
        let mut audio = listener.accept_audio()?;
        audio.send(&AudioReply::Ready {
            protocol_version: PROTOCOL_VERSION,
        })?;
        let mut control = listener.accept()?;
        control.send(&ControlFrame::Ready {
            protocol_version: PROTOCOL_VERSION,
        })?;
        let side = self
            .render
            .take()
            .ok_or(IpcError::ProtocolError("the audio phase runs once"))?;
        let render =
            RenderThread::spawn(&self.executor, side, Some(Box::new(audio))).map_err(|failed| {
                // Whole again, for the release.
                self.render = Some(failed.side);
                failed.error
            })?;
        let served = self.control_phase(&mut control, &render);
        // The teardown below is bounded by the Processor's homecoming patience,
        // so the host watch has nothing left to guard: a host that ends now is
        // let finish it.
        let _ = self.served.send(());
        // The plugin goes home before the render thread stops: its Processor
        // is returned by that thread's poll, and the plugin is destroyed only
        // once it has it back.
        self.session.drop_plugin(self.main);
        self.take_home(render);
        self.session.report_final_losses();
        if served.is_ok() {
            tracing::info!(
                "plugin-server: the control phase ended cleanly and its teardown is done"
            );
        }
        served
    }

    /// Stops the render thread within [`RENDER_STOP_PATIENCE`]. A thread that
    /// panicked is reported (the main loop has seen it end the connection); one
    /// stuck inside the plugin ends the process outright, as an orphan's
    /// watchdog does: nothing can free a plugin that never returns from a call.
    fn take_home(&self, render: RenderThread) {
        match render.stop_within(RENDER_STOP_PATIENCE) {
            Ok(_side) => {}
            Err(lost @ (NotHome::Panicked(_) | NotHome::Dropped)) => note_lane_lost(&lost),
            Err(NotHome::Stuck) => {
                tracing::error!(
                    "plugin-server: the render lane is stuck in the plugin; ending the process"
                );
                crate::leak::note();
                let _ = std::fs::remove_file(&self.socket);
                // The group this server leads (the host spawned it as its leader)
                // goes with it: the helpers a plugin spawned must not outlive it.
                tutti_plugin_ipc_ffi::kill_group(std::process::id());
                tutti_plugin_ipc_ffi::terminate_process()
            }
        }
    }

    /// Releases a plugin whose audio phase never began: the Processor was lent
    /// to a seat nothing polls, so a render thread with no connection trades
    /// it home first, and the plugin is destroyed with it back.
    fn release(&mut self) {
        if !self.session.has_plugin() {
            return;
        }
        let Some(side) = self.render.take() else {
            return;
        };
        match RenderThread::spawn(&self.executor, side, None) {
            Ok(thread) => {
                self.session.drop_plugin(self.main);
                self.take_home(thread);
                self.session.report_final_losses();
            }
            Err(failed) => {
                tracing::error!(
                    "plugin-server: the plugin cannot be released: {}",
                    failed.error
                );
                self.render = Some(failed.side);
            }
        }
    }

    fn handshake_phase(&mut self, transport: &mut ControlTransport) -> IpcResult<bool> {
        loop {
            let (id, reaction) = match transport.recv() {
                Ok(Incoming::Request(Request { id, body })) => {
                    let reaction = if matches!(body, ControlRequest::LoadPlugin { .. }) {
                        self.load_working(transport, id, body)?
                    } else if matches!(body, ControlRequest::SaveState { .. }) {
                        working(&self.executor, transport, id, false, || {
                            self.session.handle_with(self.main, body, &mut |_| {})
                        })?
                    } else {
                        self.session.handle_with(self.main, body, &mut |_| {})?
                    };
                    (id, reaction)
                }
                Ok(Incoming::SetupSlab { id, layout, slab }) => {
                    (id, self.session.setup_slab(&slab, layout))
                }
                Ok(Incoming::LoadState {
                    id,
                    state_use,
                    state,
                }) => (
                    id,
                    working(&self.executor, transport, id, true, || {
                        self.session.load_state(self.main, &state, state_use)
                    }),
                ),
                Ok(Incoming::StageState { id, state }) => (id, self.session.stage_state(state)),
                Ok(Incoming::StateRefused { id, staging, error }) => {
                    (id, self.session.refuse_state(staging, error))
                }
                Err(IpcError::Io(_)) => return Ok(false),
                Err(e) => return Err(e),
            };
            match reaction {
                Reaction::Shutdown => return Ok(true),
                Reaction::Reply(reply) => {
                    if transport.send(&ControlFrame::Reply { id, reply }).is_err() {
                        return Ok(false);
                    }
                }
                Reaction::State(state) => {
                    if crate::transport::send_state(transport, id, &state).is_err() {
                        return Ok(false);
                    }
                }
                Reaction::None => {}
            }
        }
    }

    /// A load, with the host told the server is at work on it: once as it
    /// begins, when it reaches the restore of a staged state, and every
    /// [`WORKING_PERIOD`] in between by a thread of its own, since the load
    /// itself is one plugin call that tells nothing. The host's wait for the
    /// answer restarts on each. The thread has ended before the answer is sent.
    fn load_working(
        &mut self,
        transport: &mut ControlTransport,
        id: ReqId,
        body: ControlRequest,
    ) -> IpcResult<Reaction> {
        let restoring = Arc::new(AtomicBool::new(false));
        let pulse = transport.pulser();
        let _ = transport.send(&ControlFrame::Working {
            id,
            stage: Working::Opening,
        });
        beat_while(
            &self.executor,
            pulse,
            WORKING_PERIOD,
            id,
            &restoring,
            || {
                self.session.handle_with(self.main, body, &mut |stage| {
                    if stage == Working::Restoring {
                        restoring.store(true, Ordering::Release);
                    }
                    let _ = transport.send(&ControlFrame::Working { id, stage });
                })
            },
        )
    }

    fn control_phase(
        &mut self,
        transport: &mut ControlTransport,
        render: &RenderThread,
    ) -> IpcResult<()> {
        let main = self.main;
        crate::main_loop::serve(&mut self.session, transport, main, render, &self.executor)
    }
}

/// Runs `work` on request `id` with the host told the server is at work on it,
/// as [`Working::Restoring`] when `restoring` and as [`Working::Opening`]
/// otherwise, every [`WORKING_PERIOD`]: a plugin's state save or load is one
/// call that tells nothing, and the host's wait for the answer restarts on each
/// beat.
pub(crate) fn working<R>(
    executor: &Executor,
    transport: &ControlTransport,
    id: ReqId,
    restoring: bool,
    work: impl FnOnce() -> R,
) -> R {
    let restoring = Arc::new(AtomicBool::new(restoring));
    beat_while(
        executor,
        transport.pulser(),
        WORKING_PERIOD,
        id,
        &restoring,
        work,
    )
}

/// The beat of a request: a task on a thread of the executor's of its own that
/// sends the host `Working` every period. Dropping it ends the task and waits for
/// it, so nothing is sent after the drop returns.
///
/// The wait lasts as long as the pulse in flight, which is a blocking
/// `write_frame` on the control socket: a host that stops reading hangs the
/// pulse and the main reply alike, and the wait with them. It has no write
/// timeout.
struct Beat {
    /// Dropped to stop the task: its wait for the next period ends at once.
    stop: Option<mpsc::Sender<()>>,
    task: Option<Task<()>>,
}

impl Beat {
    /// Starts the beat of request `id` on `executor`, through `pulse`: as
    /// [`Working::Restoring`] once `restoring` is set, as [`Working::Opening`]
    /// before. `None` when the thread could not start: the host's wait then
    /// restarts on nothing, and is logged.
    fn start(
        executor: &Executor,
        pulse: Pulser<ControlFrame>,
        period: Duration,
        id: ReqId,
        restoring: &Arc<AtomicBool>,
    ) -> Option<Self> {
        let (stop, stopped) = mpsc::channel();
        let beating = beat(stopped, pulse, period, id, Arc::clone(restoring));
        match executor.spawn_dedicated("plugin-beat", None, beating) {
            Ok(task) => Some(Self {
                stop: Some(stop),
                task: Some(task),
            }),
            Err(error) => {
                tracing::warn!(
                    "plugin-server: the host cannot be told the load is at work: {error}"
                );
                None
            }
        }
    }
}

impl Drop for Beat {
    /// Stops the task at its wait for the next period, or after the frame it is
    /// sending: the wait is for that frame, which the host reads, and never for a
    /// period. Runs on the thread that serves a request (the main thread), never
    /// on one that drives an executor, which `join_within` would refuse.
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(task) = self.task.take()
            && let Err(task) = join_within(task, BEAT_STOP_PATIENCE)
        {
            tracing::warn!("plugin-server: the plugin-beat task did not stop; leaving it");
            crate::leak::note_detached();
            task.detach();
        }
    }
}

/// The beat's task: every `period`, tells the host `Working` for request `id`
/// until the host cannot be written to or `stopped` is dropped.
///
/// It waits on the stop with `recv_timeout(period)` and not on the executor's
/// `Timer`: a stop must end the wait at once, and a task parked on a timer has no
/// waker for the owner to ring. So an executor that shut down mid-period would be
/// noticed only after up to one [`WORKING_PERIOD`]; none can, since the [`Beat`]
/// borrows its executor and ends before it.
async fn beat(
    stopped: mpsc::Receiver<()>,
    pulse: Pulser<ControlFrame>,
    period: Duration,
    id: ReqId,
    restoring: Arc<AtomicBool>,
) {
    while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(period) {
        let stage = if restoring.load(Ordering::Acquire) {
            Working::Restoring
        } else {
            Working::Opening
        };
        if pulse(&ControlFrame::Working { id, stage }).is_err() {
            break;
        }
    }
}

/// Runs `work` while a task of its own sends the host `Working` for request
/// `id` every `period` (as [`Working::Restoring`] once `restoring` is set),
/// through `pulse`; without one, `work` runs alone.
///
/// The task has ended when this returns, and it ends on every exit of `work`, a
/// panic included: the beat is dropped on the way out, which waits for it, and a
/// beat that outlived a failed load would tell the host for ever that it is
/// alive.
fn beat_while<R>(
    executor: &Executor,
    pulse: Option<Pulser<ControlFrame>>,
    period: Duration,
    id: ReqId,
    restoring: &Arc<AtomicBool>,
    work: impl FnOnce() -> R,
) -> R {
    let _beat = pulse.and_then(|pulse| Beat::start(executor, pulse, period, id, restoring));
    work()
}

impl Drop for PluginServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// How long a stopped render thread is given to return: it ends at its next
/// step, so this is a thread stuck inside the plugin or in a read.
/// How often the server tells the host it is at work on a load.
pub(crate) const WORKING_PERIOD: Duration = Duration::from_secs(1);

/// How long a dropped [`Beat`] waits for its task: the pulse in flight is a write with
/// no timeout, so this bounds the wait, after which the task is left to end with it.
const BEAT_STOP_PATIENCE: Duration = Duration::from_secs(5);

pub(crate) const RENDER_STOP_PATIENCE: std::time::Duration = std::time::Duration::from_secs(3);

/// How long a server is given to shut down after its host ended, before it is
/// ended outright: a bound on a wait for the shutdown, never a delay to it.
const HOST_GONE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Logs and counts ([`leak::lanes_lost`](crate::leak::lanes_lost)) a render
/// lane that ended without its state: the count is what the exit status says
/// ([`EXIT_RENDER_LANE_LOST`]). A lane still running is not one: it ends the
/// process.
fn note_lane_lost(lost: &NotHome) {
    match lost {
        NotHome::Panicked(panicked) => tracing::error!("plugin-server: {panicked}"),
        NotHome::Dropped => {
            tracing::error!("plugin-server: the render lane was dropped with its state");
        }
        NotHome::Stuck => return,
    }
    crate::leak::note_lane_lost();
}

/// Starts the watch on the host. When the host ends, `host_gone` is set and
/// `bell` rung so the main loop shuts down; if `shutdown_under_way` is not sent to within
/// [`HOST_GONE_GRACE`] the socket file at `socket_path` is removed and the
/// process ended outright. A server whose host is unknown cannot watch it: that
/// is logged, and only the host's connections end it.
///
/// # Errors
///
/// The watch's thread could not be started.
fn watch_host(
    executor: &Executor,
    socket_path: std::path::PathBuf,
    host_gone: Arc<AtomicBool>,
    bell: crate::main_loop::Bell,
    shutdown_under_way: mpsc::Receiver<()>,
) -> std::io::Result<Option<Task<()>>> {
    let watch = tutti_plugin_ipc_ffi::watch_spawner(executor, move || {
        tracing::info!("the host process is gone; shutting the server down");
        host_gone.store(true, Ordering::Release);
        bell.ring();
        // A shutdown already under way is let finish; one that does not come is
        // ended: a plugin library's exit handlers must not run on an orphan
        // whose other threads are killed mid-call.
        if shutdown_under_way.recv_timeout(HOST_GONE_GRACE) == Err(RecvTimeoutError::Timeout) {
            let _ = std::fs::remove_file(&socket_path);
            tutti_plugin_ipc_ffi::terminate_process()
        }
    });
    if let Ok(None) = watch {
        tracing::warn!("the host's process id is unknown: this server cannot notice its end");
    }
    watch
}

#[cfg(test)]
mod beat_tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    const ID: ReqId = ReqId::new(9);
    /// A pace for the beat in these tests: a count of beats is what they
    /// wait on, never the time it took.
    const PACE: Duration = Duration::from_millis(1);
    /// Far past what a healthy run needs: a hang guard, never a speed bound.
    const HANG_GUARD: Duration = Duration::from_secs(60);

    fn stage_of(frame: &ControlFrame) -> Working {
        match frame {
            ControlFrame::Working { id, stage } => {
                assert_eq!(*id, ID, "a beat names its request");
                *stage
            }
            other => panic!("a beat sends only `Working`, sent {other:?}"),
        }
    }

    fn restoring(on: bool) -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(on))
    }

    /// **The beat repeats while the work runs, as `Opening` until the work says
    /// it is restoring and as `Restoring` after**: the work waits for three
    /// beats, flips the stage, and waits for three more, so a beat that sent
    /// once, or never changed its stage, leaves it waiting.
    #[test]
    fn the_beat_repeats_and_follows_the_stage() {
        let executor = Executor::new();
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let pulse: Pulser<ControlFrame> = Box::new(move |frame| {
            let _ = tx.lock().unwrap().send(stage_of(frame));
            Ok(())
        });
        let restoring = restoring(false);
        let seen = beat_while(&executor, Some(pulse), PACE, ID, &restoring, || {
            let mut seen = Vec::new();
            for _ in 0..3 {
                seen.push(rx.recv_timeout(HANG_GUARD).expect("a beat"));
            }
            restoring.store(true, Ordering::Release);
            // Beats already in flight when the stage flipped may still be Opening.
            while seen.iter().filter(|s| **s == Working::Restoring).count() < 3 {
                seen.push(rx.recv_timeout(HANG_GUARD).expect("a beat"));
            }
            seen
        });
        assert_eq!(seen[..3], [Working::Opening; 3]);
        assert!(seen.iter().filter(|s| **s == Working::Restoring).count() >= 3);
    }

    /// **The beat has ended before the work's result is returned, though a frame
    /// is in flight when the work ends**: the beat's send is held open until the
    /// work has said it is done, so the work ends with a `Working` still being
    /// written, and `beat_while` must wait for that send to finish. What the
    /// caller sends next is the answer, and a `Working` after it would describe a
    /// request that is over.
    #[test]
    fn the_beat_ends_before_the_result_is_returned() {
        let executor = Executor::new();
        let (sending, in_flight) = mpsc::channel::<()>();
        let (release, released) = mpsc::channel::<()>();
        let (work_done, done) = mpsc::channel::<()>();
        let sent = Arc::new(AtomicUsize::new(0));
        let pulse: Pulser<ControlFrame> = {
            let (sending, released, sent) =
                (Mutex::new(sending), Mutex::new(released), Arc::clone(&sent));
            Box::new(move |_| {
                let _ = sending.lock().unwrap().send(());
                let _ = released.lock().unwrap().recv();
                sent.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        };
        let finished = std::thread::spawn({
            let (sent, restoring) = (Arc::clone(&sent), restoring(false));
            move || {
                beat_while(&executor, Some(pulse), PACE, ID, &restoring, || {
                    in_flight.recv_timeout(HANG_GUARD).expect("a beat begins");
                    let _ = work_done.send(());
                });
                // The send that was in flight has finished, and no other began.
                sent.load(Ordering::SeqCst)
            }
        });
        done.recv_timeout(HANG_GUARD).expect("the work is done");
        drop(release);
        assert_eq!(
            finished.join().expect("beat_while returned"),
            1,
            "beat_while returned with a frame still being sent"
        );
    }

    /// **A panic in the work ends the beat, and the panic still propagates**: the
    /// beat is dropped on the way out, which waits for it, so a beat that only the
    /// work's own ending stopped would keep the host told for ever that the load
    /// lives. The test completes, within the hang guard, only if the beat was
    /// stopped.
    #[test]
    fn a_panic_in_the_work_ends_the_beat() {
        let (done_tx, done_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let executor = Executor::new();
            let beats = Arc::new(AtomicUsize::new(0));
            let pulse: Pulser<ControlFrame> = {
                let beats = Arc::clone(&beats);
                Box::new(move |_| {
                    beats.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            };
            let restoring = restoring(false);
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                beat_while(&executor, Some(pulse), PACE, ID, &restoring, || {
                    while beats.load(Ordering::SeqCst) < 2 {
                        std::thread::yield_now();
                    }
                    panic!("the load fails");
                })
            }));
            let _ = done_tx.send(caught.is_err());
        });
        assert_eq!(
            done_rx.recv_timeout(HANG_GUARD),
            Ok(true),
            "the beat never ended, or the panic was swallowed"
        );
    }

    /// **A beat stops when the host cannot be written to**: the pulse fails, the
    /// task ends by itself, and the work (which waits for that) goes on.
    #[test]
    fn a_failed_pulse_ends_the_beat() {
        let executor = Executor::new();
        let beats = Arc::new(AtomicUsize::new(0));
        let pulse: Pulser<ControlFrame> = {
            let beats = Arc::clone(&beats);
            Box::new(move |_| {
                beats.fetch_add(1, Ordering::SeqCst);
                Err(IpcError::ProcessCrashed)
            })
        };
        beat_while(&executor, Some(pulse), PACE, ID, &restoring(false), || {
            while beats.load(Ordering::SeqCst) < 1 {
                std::thread::yield_now();
            }
        });
        assert_eq!(beats.load(Ordering::SeqCst), 1, "a beat after the failure");
    }

    /// Without a pulse the work runs alone and its result is returned.
    #[test]
    fn without_a_pulse_the_work_runs_alone() {
        let executor = Executor::new();
        assert_eq!(
            beat_while(&executor, None, PACE, ID, &restoring(false), || 7),
            7
        );
    }
}

#[cfg(test)]
mod exit_tests {
    use super::*;

    /// **A server that cannot watch its host ends with its own named status**,
    /// distinct from every other, and any other failure ends with 1. The thread
    /// that watches cannot be made to fail from a test, so the mapping the
    /// binary uses is what is pinned.
    #[test]
    fn a_server_that_cannot_watch_its_host_has_its_own_exit_status() {
        let unwatched = IpcError::HostUnwatched(std::io::Error::other("no thread"));
        assert_eq!(failure_status(&unwatched), EXIT_HOST_UNWATCHED);
        assert_eq!(
            failure_status(&IpcError::ProtocolError("another failure")),
            1
        );
        let statuses = [
            EXIT_PLUGIN_LEAKED,
            EXIT_TASKS_DETACHED,
            EXIT_HOST_UNWATCHED,
            EXIT_RENDER_LANE_LOST,
        ];
        for (at, status) in statuses.iter().enumerate() {
            assert!(*status > 1, "{status} is not 0 or the generic failure");
            assert!(!statuses[..at].contains(status), "{status} is named twice");
        }
    }

    /// **A render lane that panicked or was dropped is counted; one still
    /// running is not**: it ends the process, and the count is what the exit
    /// status reads.
    #[test]
    fn a_render_lane_that_panicked_or_was_dropped_is_counted() {
        assert_eq!(crate::leak::lanes_lost(), 0);
        note_lane_lost(&NotHome::Dropped);
        note_lane_lost(&NotHome::Panicked(tutti_platform::Panicked {
            name: "plugin-render".to_owned(),
            message: "boom".to_owned(),
        }));
        assert_eq!(crate::leak::lanes_lost(), 2);
        note_lane_lost(&NotHome::Stuck);
        assert_eq!(
            crate::leak::lanes_lost(),
            2,
            "a stuck lane ends the process"
        );
    }
}
