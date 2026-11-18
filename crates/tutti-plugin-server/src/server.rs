//! `PluginServer` — the outer shell. Owns a [`Session`] and drives it
//! over a [`Transport`] across the connection dance the host expects: first a
//! handshake connection for plugin load / format negotiation / shared-memory
//! setup, then, for the rest of the session, two more: the audio connection,
//! which the render thread serves (`ProcessAudio` and `Reset`, nothing else),
//! and the control connection, which the main thread's event loop serves.

use crate::render::{RenderSide, RenderThread};
use crate::session::{Reaction, Session};
use crate::transport::{Transport, TransportListener};
use tutti_plugin_ipc::{BridgeConfig, BridgeMessage, PROTOCOL_VERSION};
use tutti_plugin_types::MainThreadMarker;
use tutti_plugin_types::{BridgeError, BridgeResult};

/// Ask the OS for the highest priority it grants this thread, now that it is
/// about to do nothing but per-block audio work (the render thread, and a
/// plugin's thread-pool helpers), and log what it got once.
/// That is not real time: on Linux it is nice -20 at best, and a refusal
/// (`EPERM`) is common (`tutti_rt::request_priority`).
///
/// The host side needs this less: CoreAudio, WASAPI and JACK create the
/// audio callback's thread at an elevated priority themselves (cpal's ALSA
/// backend does not: its callback thread is an ordinary one). A plugin subprocess
/// gets no such treatment — it is an ordinary `Command::spawn` at default
/// priority — yet it must answer within the same block period. Measured with
/// eight real plugins at 64/48k, that asymmetry swung the number of on-time
/// replies between 29 and 492 out of 500 across identical runs.
///
/// This is a *quality* knob, not a correctness one: a late reply yields silence,
/// never wrong audio, because the host reads a slot only when the slab's
/// sequence number says the server published it (see
/// `tutti-plugin-ipc`'s `transport::shm::header`).
///
/// Failure is logged and ignored — elevation needs privileges a sandbox or CI
/// runner may not grant, and degraded quality beats refusing to run.
pub(crate) fn request_audio_priority(what: &str) {
    let outcome = tutti_rt::request_priority(tutti_rt::PriorityRequest::Highest);
    if outcome.is_granted() {
        tracing::debug!("plugin-server: {what}: {outcome}");
    } else {
        tracing::info!("plugin-server: {what}: {outcome}; expect more dropped blocks under load");
    }
}

/// The subprocess-side server: one hosted plugin, one host connection.
///
/// Construct with [`PluginServer::new`] and hand control to
/// [`PluginServer::run`], which does not return until the host disconnects or
/// asks for shutdown. Dropping it closes any open editor window and unlinks the
/// socket.
pub struct PluginServer {
    config: BridgeConfig,
    session: Session,
    /// What renders the blocks, until the audio connection is there to hand it
    /// to a thread.
    render: Option<RenderSide>,
    /// The thread that built the server: the one the plugin's main-thread
    /// calls run on, claimed once at startup.
    main: MainThreadMarker,
}

impl std::fmt::Debug for PluginServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginServer")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl PluginServer {
    /// Creates a server for `config`, and starts the watch on the spawning host:
    /// a host that ends, wherever this process is then blocked (an accept, a
    /// plugin's modal dialog, a block), ends the server too, without running a
    /// plugin library's exit handlers, and removes the socket file.
    ///
    /// The socket is not created until [`PluginServer::run`]. Call this early.
    ///
    /// # Errors
    ///
    /// The thread is not the process's main thread, or the OS refused the
    /// session's bells.
    pub fn new(config: BridgeConfig) -> BridgeResult<Self> {
        let main = MainThreadMarker::new().ok_or(BridgeError::ProtocolError(
            "the plugin server must be built on the process's main thread",
        ))?;
        watch_host(config.socket_path.clone());
        let (session, render) = Session::new()?;
        Ok(Self {
            config,
            session,
            render: Some(render),
            main,
        })
    }

    /// Serves one host, then returns. Blocks for the lifetime of the session.
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
    /// Each phase opens with a [`BridgeMessage::Ready`] carrying
    /// `PROTOCOL_VERSION`; the host refuses a version it does not know.
    ///
    /// # Errors
    ///
    /// Returns an error if the socket cannot be bound, if a handshake frame
    /// does not decode (a host of another protocol), or if the control
    /// connection fails. The handshake connection closing is the host's way of
    /// ending that phase, not an error. However it ends, the plugin is
    /// released first.
    pub fn run(mut self) -> BridgeResult<()> {
        let served = self.serve();
        self.release();
        served
    }

    /// The phases of [`run`](Self::run), up to the plugin's release.
    fn serve(&mut self) -> BridgeResult<()> {
        let listener = TransportListener::bind(&self.config.socket_path)?;

        // First connection: handshake.
        let mut handshake = listener.accept()?;
        handshake.send(&BridgeMessage::Ready {
            protocol_version: PROTOCOL_VERSION,
        })?;
        if self.handshake_phase(&mut handshake)? {
            return Ok(());
        }
        drop(handshake);

        // Second connection: audio. Third: control.
        let mut audio = listener.accept_audio()?;
        audio.send(&BridgeMessage::Ready {
            protocol_version: PROTOCOL_VERSION,
        })?;
        let mut control = listener.accept()?;
        control.send(&BridgeMessage::Ready {
            protocol_version: PROTOCOL_VERSION,
        })?;
        let side = self
            .render
            .take()
            .ok_or(BridgeError::ProtocolError("the audio phase runs once"))?;
        let render = RenderThread::spawn(side, Some(Box::new(audio)))?;
        let served = self.control_phase(&mut control, &render);
        // The plugin goes home before the render thread stops: its Processor
        // is returned by that thread's poll, and the plugin is destroyed only
        // once it has it back.
        self.session.drop_plugin(self.main);
        drop(render.stop());
        self.session.report_final_losses();
        served
    }

    /// Releases a plugin whose audio phase never began: the Processor was lent
    /// to a seat nothing polls, so a render thread with no connection trades
    /// it home first, and the plugin is destroyed with it back.
    fn release(&mut self) {
        let Some(side) = self.render.take() else {
            return;
        };
        match RenderThread::spawn(side, None) {
            Ok(thread) => {
                self.session.drop_plugin(self.main);
                drop(thread.stop());
                self.session.report_final_losses();
            }
            Err(e) => tracing::error!("plugin-server: the plugin cannot be released: {e}"),
        }
    }

    fn handshake_phase(&mut self, transport: &mut dyn Transport) -> BridgeResult<bool> {
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
                let _ = transport.send(&m);
            })?;
            match reaction {
                Reaction::Shutdown => return Ok(true),
                Reaction::Reply(m) => {
                    if transport.send(&m).is_err() {
                        return Ok(false);
                    }
                }
                Reaction::ReplyMany(ms) => {
                    for m in &ms {
                        if transport.send(m).is_err() {
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
        transport: &mut dyn Transport,
        render: &RenderThread,
    ) -> BridgeResult<()> {
        let main = self.main;
        crate::main_loop::serve(&mut self.session, transport, main, render)
    }
}

impl Drop for PluginServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.config.socket_path);
    }
}

/// Environment variable through which the host states its own PID at spawn,
/// before this process exists: neither platform can recover it afterwards (a
/// host that dies during its child's startup is reparented first). Duplicated
/// in `tutti-plugin-ipc`'s launch rather than shared: it is the wire between
/// the two, as the socket-path argument is.
const HOST_PID_ENV: &str = "TUTTI_PLUGIN_HOST_PID";

/// The host's process id: its own statement in [`HOST_PID_ENV`], else the
/// parent in the process table (a server started by hand). `None` when neither
/// names one.
fn host_pid() -> Option<u32> {
    std::env::var(HOST_PID_ENV)
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&pid| pid > 1)
        .or_else(tutti_plugin_ipc_ffi::parent_process_id)
}

/// Starts the watch on the host that ends this process, and removes the socket
/// file at `socket_path`, when the host ends. A server whose host is unknown
/// cannot watch it: that is logged, and only the host's connections end it.
fn watch_host(socket_path: std::path::PathBuf) {
    let Some(host) = host_pid() else {
        tracing::warn!("the host's process id is unknown: this server cannot notice its end");
        return;
    };
    let watch = tutti_plugin_ipc_ffi::watch_host(host, move || {
        tracing::info!("the host process is gone; ending the server");
        let _ = std::fs::remove_file(&socket_path);
        // Ended outright: a plugin library's exit handlers must not run on an
        // orphan whose other threads are killed mid-call.
        tutti_plugin_ipc_ffi::terminate_process()
    });
    if let Err(e) = watch {
        tracing::warn!("the host watch cannot start: {e}");
    }
}
