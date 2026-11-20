//! `PluginServer` — the outer shell. Owns a [`Session`] and drives it
//! over a [`Transport`] across the connection dance the host expects: first a
//! handshake connection for plugin load / format negotiation / shared-memory
//! setup, then, for the rest of the session, two more: the audio connection,
//! which the render thread serves (`ProcessAudio` and `Reset`, nothing else),
//! and the control connection, which the main thread's event loop serves.

use crate::render::{NotHome, RenderSide, RenderThread};
use crate::session::{Reaction, Session};
use crate::transport::{ControlTransport, Transport, TransportListener};
use std::path::PathBuf;
use tutti_platform::StopToken;
use tutti_plugin_ipc::{AudioReply, ControlFrame, ControlReply, PROTOCOL_VERSION};
use tutti_plugin_types::MainThreadMarker;
use tutti_plugin_types::{BridgeError, BridgeResult};

/// Ask the OS for the highest priority it grants this thread, now that it is
/// about to do nothing but per-block audio work (the render thread, and a
/// plugin's thread-pool helpers), and log what it got once.
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
}

/// The exit status of a server whose session ended with [`Ended::PluginLeaked`]
/// (see there for who can read it).
pub const EXIT_PLUGIN_LEAKED: u8 = 3;

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
    /// Stopped once the shutdown is under way: a host that ends then finds the
    /// teardown, itself bounded, and is let finish ([`watch_host`]).
    served: StopToken,
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
    /// The thread is not the process's main thread, or the OS refused the
    /// session's bells.
    pub fn new(socket: PathBuf) -> BridgeResult<Self> {
        let main = MainThreadMarker::new().ok_or(BridgeError::ProtocolError(
            "the plugin server must be built on the process's main thread",
        ))?;
        let (session, render) = Session::new()?;
        let served = StopToken::new();
        watch_host(
            socket.clone(),
            session.ties().host_gone.clone(),
            session.ties().bell.clone(),
            served.clone(),
        );
        Ok(Self {
            socket,
            session,
            render: Some(render),
            main,
            served,
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
    ///    plugin's `Processor`, asks for the highest priority on entry
    ///    (`request_audio_priority`), and shares nothing with the main thread
    ///    but queues, so an editor's paint can never delay a block.
    /// 3. **Control**: every other request and the plugin's unsolicited news,
    ///    for the rest of the session. The **main thread** serves it in its
    ///    event loop (`main_loop`), which keeps the plugin's
    ///    `Controller`, its editor and the idle tick.
    ///
    /// Each phase opens with a [`ControlReply::Ready`] carrying
    /// `PROTOCOL_VERSION`; the host refuses a version it does not know.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket cannot be bound, if a handshake frame
    /// does not decode (a host of another protocol), or if the control
    /// connection fails. The handshake connection closing is the host's way of
    /// ending that phase, not an error. However it ends, the plugin is
    /// released first.
    pub fn run(mut self) -> BridgeResult<Ended> {
        let served = self.serve();
        // Idempotent: the paths that never reached the control phase.
        self.served.stop();
        self.release();
        // The plugin and the session go with the server: what they leak, they
        // leak here.
        drop(self);
        served.map(|()| {
            if crate::leak::happened() {
                Ended::PluginLeaked
            } else {
                Ended::Clean
            }
        })
    }

    /// The phases of [`run`](Self::run), up to the plugin's release.
    fn serve(&mut self) -> BridgeResult<()> {
        let listener = TransportListener::bind(&self.socket)?;

        // First connection: handshake.
        let mut handshake = listener.accept()?;
        handshake.send(&ControlFrame::Reply(ControlReply::Ready {
            protocol_version: PROTOCOL_VERSION,
        }))?;
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
        control.send(&ControlFrame::Reply(ControlReply::Ready {
            protocol_version: PROTOCOL_VERSION,
        }))?;
        let side = self
            .render
            .take()
            .ok_or(BridgeError::ProtocolError("the audio phase runs once"))?;
        let render = RenderThread::spawn(side, Some(Box::new(audio))).map_err(|failed| {
            // Whole again, for the release.
            self.render = Some(failed.side);
            failed.error
        })?;
        let served = self.control_phase(&mut control, &render);
        // The teardown below is bounded by the Processor's homecoming patience,
        // so the host watch has nothing left to guard: a host that ends now is
        // let finish it.
        self.served.stop();
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
            Err(NotHome::Panicked(panicked)) => {
                tracing::error!("plugin-server: {panicked}");
            }
            Err(NotHome::Stuck) => {
                tracing::error!(
                    "plugin-server: the render thread is stuck in the plugin; ending the process"
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
        match RenderThread::spawn(side, None) {
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

    fn handshake_phase(&mut self, transport: &mut ControlTransport) -> BridgeResult<bool> {
        loop {
            let msg = match transport.recv() {
                Ok(m) => m,
                // The host closed the connection, or went away: the phase is over.
                Err(BridgeError::Io(_)) => return Ok(false),
                // A frame that does not decode (a host of another protocol), or
                // one past the cap: the cause is the error, not a host that left.
                Err(e) => return Err(e),
            };
            // A load says it is restoring a state before it does (the host's wait
            // changes with it); the transport is the interim sink.
            let reaction = self.session.handle_with(self.main, msg, &mut |m| {
                let _ = transport.send(&m.into());
            })?;
            match reaction {
                Reaction::Shutdown => return Ok(true),
                Reaction::Reply(m) => {
                    if transport.send(&m.into()).is_err() {
                        return Ok(false);
                    }
                }
                Reaction::ReplyMany(ms) => {
                    for m in ms {
                        if transport.send(&m.into()).is_err() {
                            return Ok(false);
                        }
                    }
                }
                Reaction::None => {}
            }
        }
    }

    /// The control phase: the main thread's [event loop](crate::main_loop),
    /// serving the control connection and the editor's idle tick until the
    /// host leaves.
    fn control_phase(
        &mut self,
        transport: &mut ControlTransport,
        render: &RenderThread,
    ) -> BridgeResult<()> {
        let main = self.main;
        crate::main_loop::serve(&mut self.session, transport, main, render)
    }
}

impl Drop for PluginServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// How long a stopped render thread is given to return: it ends at its next
/// step, so this is a thread stuck inside the plugin or in a read.
const RENDER_STOP_PATIENCE: std::time::Duration = std::time::Duration::from_secs(3);

/// How long a server is given to shut down after its host ended, before it is
/// ended outright: a bound on a wait for the shutdown, never a delay to it.
const HOST_GONE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Starts the watch on the host. When the host ends, `host_gone` is stopped and
/// `bell` rung so the main loop shuts down; if `served` is not stopped within
/// [`HOST_GONE_GRACE`] the socket file at `socket_path` is removed and the
/// process ended outright. A server whose host is unknown cannot watch it: that
/// is logged, and only the host's connections end it.
fn watch_host(
    socket_path: std::path::PathBuf,
    host_gone: StopToken,
    bell: crate::main_loop::Bell,
    served: StopToken,
) {
    let watch = tutti_plugin_ipc_ffi::watch_spawner(move || {
        tracing::info!("the host process is gone; shutting the server down");
        host_gone.stop();
        bell.ring();
        // A shutdown already under way is let finish; one that does not come is
        // ended: a plugin library's exit handlers must not run on an orphan
        // whose other threads are killed mid-call.
        if !served.wait(HOST_GONE_GRACE) {
            let _ = std::fs::remove_file(&socket_path);
            tutti_plugin_ipc_ffi::terminate_process()
        }
    });
    match watch {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!("the host's process id is unknown: this server cannot notice its end");
        }
        Err(e) => tracing::warn!("the host watch cannot start: {e}"),
    }
}
