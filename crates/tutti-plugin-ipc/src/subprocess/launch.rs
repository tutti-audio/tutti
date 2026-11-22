//! Full launch sequence for a plugin-server subprocess:
//! spawn → handshake → load plugin → setup shared memory → open the audio lane →
//! return a [`LaunchedServer`] holding the lane and the broker connection, ready
//! for the RT bridge thread.

use crate::ProcessGuard;
use crate::config::IpcConfig;
use crate::conversation::Conversation;
use crate::error::unexpected_message;
use crate::transport::PendingLane;
use crate::transport::control::{self as ipc, ControlStream};
use crate::transport::shm::{AudioSlab, SlabLayout, SlabWriter};
use crate::{
    ControlFrame, ControlReply, ControlRequest, Lane, ReqId, Request, StagedState, Working,
};
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use tutti_buffers::FloatWidth;
use tutti_platform::{Executor, Instant};
use tutti_plugin_ipc_ffi::{Blob, Reaped};
use tutti_plugin_types::MAX_CHUNK;
use tutti_plugin_types::StateUse;
use tutti_plugin_types::Timeouts;
use tutti_plugin_types::{Helper, Locator, PluginFormat, PluginId, PluginRef};
use tutti_plugin_types::{IpcError, IpcResult, ServerError, StateError};
use tutti_plugin_types::{PluginDescriptor, PluginWiring};
use tutti_units::SampleRate;

/// How long to wait between connect attempts.
///
/// Short enough that a fast spawn is not rounded up to something a user
/// notices, long enough that a slow one does not spin the CPU. At this interval
/// the common case returns in single-digit milliseconds.
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(2);

/// A plugin-server that has loaded its plugin and opened its audio lane, and
/// waits for the bridge thread to speak on its connections ([`launch`]).
pub struct LaunchedServer {
    /// The server process, killed and reaped when this drops, and the watch
    /// on whether it is alive.
    pub server: ProcessGuard,
    /// The plugin the server opened: the one the request chose, or the file's
    /// first.
    pub id: PluginId,
    /// Catalog identity (name, vendor, class, editor).
    pub descriptor: PluginDescriptor,
    /// Engine-wiring data from instantiation (bus widths, latency, f64).
    pub loaded: PluginWiring,
    /// The sample format the blocks travel in.
    pub format: FloatWidth,
    /// The shared-memory slab the server mapped, as the writer of its inputs:
    /// the one in this process (`SlabWriter`). Readers share
    /// [`SlabWriter::slab`].
    pub slab: SlabWriter,
    /// The audio lane the server opened over the broker: blocks out, block
    /// replies in.
    pub audio: ControlStream,
    /// The broker connection, which serves as the control lane from here on.
    pub control: ControlStream,
    /// What became of the state the launch was given: `None` when it had none,
    /// else the plugin's refusal as text (`Ok` when it took it).
    pub state_restore: Option<std::result::Result<(), StateError>>,
}

impl std::fmt::Debug for LaunchedServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchedServer")
            .field("server", &self.server)
            .field("id", &self.id)
            .field("descriptor", &self.descriptor)
            .field("loaded", &self.loaded)
            .field("format", &self.format)
            .field("state_restore", &self.state_restore)
            .finish_non_exhaustive()
    }
}

/// What a launch loads: the plugin, the format that opens it, the rate it
/// starts at and the state it starts from.
#[derive(Debug, Clone, Copy)]
pub struct LoadRequest<'a> {
    /// The file and the plugin chosen in it.
    pub origin: &'a PluginRef,
    /// The format that opens the file.
    pub format: PluginFormat,
    /// The sample rate the plugin is activated at.
    pub rate: SampleRate,
    /// The state to restore before the plugin is activated, if any.
    pub restore: Option<Restore<'a>>,
}

/// State a launch restores into the plugin before it is activated, the order
/// every format allows and a plugin that takes its mode from the state at
/// activation needs.
#[derive(Debug, Clone, Copy)]
pub struct Restore<'a> {
    /// The state's bytes.
    pub state: &'a [u8],
    /// What the state was saved for.
    pub state_use: StateUse,
}

impl Restore<'_> {
    /// The declaration the load carries once the server has the state staged.
    fn staged(&self) -> StagedState {
        StagedState {
            len: self.state.len() as u64,
            state_use: self.state_use,
        }
    }
}

/// Spawn a plugin-server and bring it to the point where the RT bridge thread
/// can speak to it.
///
/// **Every failure after the spawn must reap the child.** The child is held
/// by a [`ProcessGuard`] from the moment it is spawned, which kills and reaps it
/// when it drops, so no `?` between the spawn and the end of the launch leaves
/// a `plugin-server` alive with nothing holding it (it would survive the host,
/// keep its socket bound, and hold the plugin's audio device claim). The
/// guard's watch is also what a failed connect asks whether the server died.
///
/// # Errors
///
/// The server cannot be found or spawned, it does not connect or answer the
/// handshake in time, or it fails to load the plugin.
pub fn launch(
    config: &IpcConfig,
    request: &LoadRequest<'_>,
    executor: &Executor,
) -> IpcResult<LaunchedServer> {
    launch_with_state(&Locator::Search, config, request, executor)
}

/// [`launch`], with the server binary named explicitly rather than searched
/// for.
///
/// Exists so a test can substitute a stand-in server **without touching
/// process-global state**. The alternative — `std::env::set_var` around the
/// call — makes the choice of binary visible to every other thread in the test
/// binary, so under a parallel runner one test's stand-in is another's
/// production lookup. A save/restore guard does not close that window; it *is*
/// the window.
#[cfg(all(test, unix))]
pub(super) fn launch_with(
    locator: &Locator,
    config: &IpcConfig,
    plugin_path: &Path,
    format: PluginFormat,
    sample_rate: SampleRate,
    executor: &Executor,
) -> IpcResult<LaunchedServer> {
    let origin = PluginRef::file(plugin_path);
    launch_with_state(
        locator,
        config,
        &LoadRequest {
            origin: &origin,
            format,
            rate: sample_rate,
            restore: None,
        },
        executor,
    )
}

/// `launch_with`, restoring the request's state into the plugin before it is
/// activated: staged ahead of the load (`ControlRequest::StageState`),
/// applied by the server between opening the plugin and activating it, so the
/// activation, and the reply's buses and latency, are the state's.
pub(super) fn launch_with_state(
    locator: &Locator,
    config: &IpcConfig,
    request: &LoadRequest<'_>,
    executor: &Executor,
) -> IpcResult<LaunchedServer> {
    config.timeouts.validate()?;
    let server = ProcessGuard::launched(spawn_process(locator, config)?, config, executor)
        .map_err(IpcError::ExitUnwatched)?;

    // The guard is borrowed, not moved: the connect retry asks its watch so a
    // server that died on startup is reported at once, and the teardown below
    // still needs to drop it.
    let result = (|server: &ProcessGuard| {
        let mut launch = Launch::new(handshake(config, server)?, &config.timeouts);
        // A state the server would not take (over the limit, a broken sequence)
        // is a refusal like any other: the plugin loads at its defaults. A stage
        // that got no answer is not: the stream may have a late reply in flight,
        // so the launch ends.
        let staged = match &request.restore {
            Some(restore) => Some(stage_state(&mut launch, config, restore.state)?),
            None => None,
        };
        let declared = match (&staged, &request.restore) {
            (Some(Ok(())), Some(restore)) => Some(restore.staged()),
            _ => None,
        };
        let (id, descriptor, loaded, format, loaded_error) =
            load_plugin(&mut launch, config, request, declared)?;
        let state_restore = match staged {
            Some(Err(e)) => Some(Err(e)),
            Some(Ok(())) => Some(loaded_error.map_or(Ok(()), Err)),
            None => None,
        };
        let slab = setup_shm(&mut launch, config, &loaded, format)?;
        let audio = open_audio_lane(&mut launch, config)?;
        Ok((
            id,
            descriptor,
            loaded,
            format,
            slab,
            audio,
            launch.stream,
            state_restore,
        ))
    })(&server);

    match result {
        Ok((id, descriptor, loaded, format, slab, audio, control, state_restore)) => {
            Ok(LaunchedServer {
                server,
                id,
                descriptor: *descriptor,
                loaded,
                format,
                slab,
                audio,
                control,
                state_restore,
            })
        }
        Err(e) => {
            // `server` drops here, killed and reaped, and its socket removed. Best-effort and
            // deliberately un-propagated: the launch error is the one worth
            // reporting, and a failed clean-up must not mask it.
            drop(server);
            Err(e)
        }
    }
}

/// Starts the server as the leader of a process group of its own, in the
/// host's kill-on-close job on Windows, so its helpers and the server itself
/// end with it.
fn spawn_process(locator: &Locator, config: &IpcConfig) -> IpcResult<Reaped> {
    let server_path = locator
        .resolve(Helper::Server)
        .ok_or(IpcError::ServerNotFound)?;
    tracing::debug!("spawning plugin-server: {}", server_path.display());
    // Idempotent: every launch that asks for the protection has it or is refused.
    if config.protect_host_memory {
        tutti_plugin_ipc_ffi::protect_host_memory().map_err(IpcError::HostMemoryUnprotected)?;
    }
    let mut command = Command::new(server_path);
    command
        .arg(&config.socket_path)
        .env(
            tutti_plugin_ipc_ffi::HOST_PID_ENV,
            std::process::id().to_string(),
        )
        .stderr(std::process::Stdio::inherit());
    tutti_plugin_ipc_ffi::own_group(&mut command);
    let child = command.spawn().map_err(IpcError::Io)?;
    // A server that cannot be bound to the host's end could outlive a host
    // that dies: it is ended and the launch refused. A child spawned before the
    // assignment stays outside the job. (Unix binds at the spawn, in
    // `own_group`, and has nothing to do here.)
    let bound = tutti_plugin_ipc_ffi::bind_to_host(&child);
    // On the refusal below `reaped` drops: its group is killed and reaped.
    let reaped = Reaped::leading_group(child);
    bound.map_err(IpcError::ServerNotBound)?;
    Ok(reaped)
}

/// Connect as soon as the server is listening, rather than after a fixed wait.
///
/// `connect` fails immediately while nothing is bound, so retrying is how the
/// host learns the socket is up. The path is unique per launch
/// (`unique_socket_path`), so a successful connect can only be *this* server —
/// there is no stale socket from a previous run to attach to by mistake.
///
/// The server's watch is asked between attempts, and the pause between them
/// is a wait on it ([`ServerWatch::wait_dead`](crate::ServerWatch::wait_dead)):
/// a server that died on startup — a missing dynamic library, an immediate
/// panic — never binds, and the wait ends at its death and not at the end of
/// a retry period, so the host reports a failure whose cause is known at once
/// and not after the full timeout. No event says "the socket is bound" (the
/// server would have to announce it over a channel that exists before the
/// socket does), so the attempts themselves are the poll, one per
/// [`CONNECT_RETRY_INTERVAL`].
fn connect_when_listening(
    socket: &Path,
    server: &ProcessGuard,
    patience: Duration,
) -> IpcResult<ControlStream> {
    let deadline = Instant::now() + patience;
    loop {
        match ipc::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                if let Some(cause) = server.watch().cause() {
                    return Err(IpcError::ServerExited { cause });
                }
                if Instant::now() >= deadline {
                    return Err(e);
                }
                server.watch().wait_dead(CONNECT_RETRY_INTERVAL);
            }
        }
    }
}

/// How many times [`Timeouts::load`] a load may be kept waiting by the server's
/// signs of life: a server that is still working is waited for, but a plugin
/// that works for ever is not.
const LOAD_CEILING_FACTOR: u32 = 10;

fn handshake(config: &IpcConfig, server: &ProcessGuard) -> IpcResult<ControlStream> {
    let mut stream = connect_when_listening(&config.socket_path, server, config.timeouts.connect)?;
    let timeout = config.timeouts.call;
    // A server from before version 27 frames its `Hello` in bincode, which does
    // not decode as one message of ours: say which binary is stale rather than
    // report a bare codec error.
    let hello = ipc::recv_within::<ControlFrame>(&mut stream, timeout).map_err(|e| match e {
        undecodable @ (IpcError::Serialization(_) | IpcError::TrailingBytes { .. }) => {
            IpcError::StaleServer {
                cause: Box::new(undecodable),
            }
        }
        other => other,
    })?;
    match hello {
        ControlFrame::Hello { protocol_version } => {
            crate::protocol::check_protocol_version(protocol_version)?;
            Ok(stream)
        }
        ref other => Err(unexpected_message("Hello", other)),
    }
}

/// Which wait a `LoadPlugin` reply is under: the open (library, instance,
/// init) has the load window ([`Timeouts::load`]); once the server says it has begun
/// restoring staged state the restore has a window of its own and answers for
/// a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadPhase {
    Opening,
    Restoring,
}

impl LoadPhase {
    /// `error` as this phase answers for it: a restore's failure is the
    /// state's ([`IpcError::StateRestore`]); an open's is its own.
    fn blame(self, error: IpcError) -> IpcError {
        match self {
            Self::Opening => error,
            Self::Restoring => IpcError::StateRestore {
                cause: Box::new(error),
            },
        }
    }
}

/// A launch's requests and what has been heard of them: the part of a launch
/// that does no I/O. Its requests are numbered and the answers matched to them
/// by a [`Conversation`], whose deadlines the server's signs of life move, and
/// the load's phase only ever goes forward.
struct Exchange<'t> {
    conv: Conversation<ReqId, ()>,
    last_id: u64,
    timeouts: &'t Timeouts,
    phase: LoadPhase,
    /// The window the request in flight is under, for the error that names it.
    window: Duration,
}

/// What a frame heard in a launch came to.
#[derive(Debug)]
enum Heard {
    /// The answer to the request waited for.
    Answer(ControlReply),
    /// A sign of life, which moved the wait.
    Working,
}

impl<'t> Exchange<'t> {
    fn new(timeouts: &'t Timeouts) -> Self {
        Self {
            conv: Conversation::new(),
            last_id: 0,
            timeouts,
            phase: LoadPhase::Opening,
            window: timeouts.call,
        }
    }

    /// Numbers the next request, sent at `now`, which the server has `window`
    /// to answer (restarting at each sign of life, up to `ceiling` from the
    /// send).
    fn open(&mut self, now: Instant, window: Duration, ceiling: Duration) -> ReqId {
        self.last_id += 1;
        let id = ReqId::new(self.last_id);
        self.conv.open(id, (), now, window, ceiling);
        self.phase = LoadPhase::Opening;
        self.window = window;
        id
    }

    /// What `frame`, heard at `now`, does to the wait for request `id`.
    ///
    /// At work opening the plugin restarts the load window, up to its ceiling;
    /// at work restoring the state the request is under the restore's own
    /// window and ceiling from that instant, and from then on a failure is the
    /// state's to answer for. The phase never goes back: a sign of life that
    /// says `Opening` after the restore began (the beat and the notice come from
    /// two threads) is a sign of life and no more.
    ///
    /// # Errors
    ///
    /// A frame that is not an answer to `id` or a sign of life: nothing the
    /// plugin says unprompted comes before it is loaded, so news is no part of
    /// this connection.
    fn hear(&mut self, frame: ControlFrame, id: ReqId, now: Instant) -> IpcResult<Heard> {
        match frame {
            ControlFrame::Working { id: of, stage } if of == id => {
                match stage {
                    Working::Opening => {
                        self.conv.extend(id, now);
                    }
                    // The restore's window and ceiling are set once, as it
                    // begins; the beat that follows only extends the wait
                    // within that ceiling, or a restore that never ends would
                    // be waited for for ever.
                    Working::Restoring if self.phase == LoadPhase::Opening => {
                        self.phase = LoadPhase::Restoring;
                        self.window = self.timeouts.state_restore;
                        let restore = self.timeouts.state_restore;
                        self.conv.restart(id, now, restore, restore);
                    }
                    Working::Restoring => {
                        self.conv.extend(id, now);
                    }
                }
                Ok(Heard::Working)
            }
            ControlFrame::Working { .. } => Ok(Heard::Working),
            ControlFrame::Reply { id: of, reply } => match self.conv.hear(of) {
                Ok(crate::conversation::Heard::Answer) if of == id => Ok(Heard::Answer(reply)),
                _ => Err(unexpected_message("the answer to the request", &reply)),
            },
            ref other => Err(unexpected_message("a reply", other)),
        }
    }

    /// How long request `id` may still be waited for at `now`.
    fn left(&self, id: ReqId, now: Instant) -> Duration {
        self.conv.left(id, now).unwrap_or(Duration::ZERO)
    }

    /// `error` as the wait names it: a timeout carries the window the request
    /// was under, and a failure in the restore is the state's.
    fn blame(&self, error: IpcError) -> IpcError {
        self.phase.blame(match error {
            IpcError::Timeout {
                operation, partial, ..
            } => IpcError::Timeout {
                operation,
                after: self.window,
                partial,
            },
            other => other,
        })
    }
}

/// A launch's connection and its [`Exchange`].
struct Launch<'t> {
    stream: ControlStream,
    exchange: Exchange<'t>,
}

impl<'t> Launch<'t> {
    fn new(stream: ControlStream, timeouts: &'t Timeouts) -> Self {
        Self {
            stream,
            exchange: Exchange::new(timeouts),
        }
    }

    /// Numbers the next request (see [`Exchange::open`]).
    fn open(&mut self, window: Duration, ceiling: Duration) -> ReqId {
        self.exchange.open(Instant::now(), window, ceiling)
    }

    /// Sends a frame of the request `id`.
    fn send(&mut self, id: ReqId, body: ControlRequest) -> IpcResult<()> {
        ipc::send(&mut self.stream, &Request { id, body })
    }

    /// The answer to request `id`, the signs of life on the way moving the
    /// wait.
    fn answer(&mut self, id: ReqId) -> IpcResult<ControlReply> {
        loop {
            let left = self.exchange.left(id, Instant::now());
            let frame = ipc::recv_within::<ControlFrame>(&mut self.stream, left)
                .map_err(|e| self.exchange.blame(e))?;
            if let Heard::Answer(reply) = self.exchange.hear(frame, id, Instant::now())? {
                return Ok(reply);
            }
        }
    }
}

fn load_plugin(
    launch: &mut Launch<'_>,
    config: &IpcConfig,
    request: &LoadRequest<'_>,
    state: Option<StagedState>,
) -> IpcResult<(
    PluginId,
    Box<PluginDescriptor>,
    PluginWiring,
    FloatWidth,
    Option<StateError>,
)> {
    // The open (library, instance, init) runs under the bridge's load window,
    // and the server's signs of life restart it. When the server says it has
    // begun restoring the state, the wait changes: a restore is one opaque
    // plugin call with no signal inside it to measure progress by, and a large
    // state is legitimately slow to restore, so it gets a window of its own. A
    // failure from then on is the state's to answer for
    // ([`IpcError::StateRestore`]); one before it is a load failure, and
    // blames nothing.
    let load = config.timeouts.load;
    let id = launch.open(load, load.saturating_mul(LOAD_CEILING_FACTOR));
    launch.send(
        id,
        ControlRequest::LoadPlugin {
            path: request.origin.path().to_path_buf(),
            format: request.format,
            plugin: request.origin.id().cloned(),
            sample_rate: request.rate,
            block_size: config.max_buffer_size,
            preferred_format: config.preferred_format,
            state,
        },
    )?;
    match launch.answer(id)? {
        ControlReply::PluginLoaded {
            id,
            descriptor,
            loaded,
            negotiated_format,
            state_error,
        } => Ok((id, descriptor, loaded, negotiated_format, state_error)),
        ControlReply::Error {
            error: ServerError::Load { stage, reason },
        } => Err(IpcError::LoadFailed {
            path: Some(request.origin.path().to_path_buf()),
            stage,
            reason,
        }),
        ControlReply::Error { error } => Err(IpcError::Server(error)),
        ref other => Err(unexpected_message("PluginLoaded", other)),
    }
}

/// Hands the server `state` for the plugin its next load opens. The inner `Err`
/// is the server's refusal (too large, a broken sequence): the plugin then loads
/// without it. The outer `Err` is a stage that got no answer, and ends the launch.
fn stage_state(
    launch: &mut Launch<'_>,
    config: &IpcConfig,
    state: &[u8],
) -> IpcResult<std::result::Result<(), StateError>> {
    if state.len() > tutti_plugin_types::MAX_STATE_BYTES {
        return Ok(Err(StateError::TooLarge {
            bytes: state.len(),
            limit: tutti_plugin_types::MAX_STATE_BYTES,
        }));
    }
    let blob = match Blob::create(state) {
        Ok(blob) => blob,
        Err(e) => {
            return Ok(Err(StateError::NotShared {
                reason: e.to_string(),
            }));
        }
    };
    let call = config.timeouts.call;
    let id = launch.open(call, call);
    let body = ControlRequest::StageState {
        len: state.len() as u64,
        blob: blob.wire(),
    };
    // Held until the answer: where the object is read by name, it stays until then.
    let _lease = ipc::send_blob(&mut launch.stream, id, body, blob)?;
    match launch.answer(id)? {
        ControlReply::StateLoaded { error: None } => Ok(Ok(())),
        ControlReply::StateLoaded { error: Some(e) } => Ok(Err(e)),
        other => Err(unexpected_message("StateLoaded", &other)),
    }
}

/// The slab shape for a freshly loaded plugin: how many flat channels, how they
/// partition into buses, and how many samples per channel cross the boundary.
///
/// Split out of [`setup_shm`] because it is the whole of the decision and none
/// of the I/O — every input is a plain value, so it is unit-testable, whereas
/// `setup_shm` needs a live subprocess on the other end of `stream`. That
/// matters more than it looks: with the decision fused to the I/O, the aliasing
/// choice below can only be checked by launching a real plugin.
fn slab_layout_for(
    loaded: &PluginWiring,
    format: FloatWidth,
    max_buffer_size: usize,
) -> SlabLayout {
    // Sized to the largest chunk the node ships (`MAX_CHUNK`: one device
    // callback, see the batcher), NOT to `config.max_buffer_size`. The two
    // are different quantities: `max_buffer_size` (8192 by default) is
    // what the *plugin* is told to size its own buffers for on
    // `LoadPlugin`, whereas this is what crosses the shared region per
    // chunk. `min` rather than a bare `MAX_CHUNK` so a host that
    // deliberately configures a *smaller* buffer still gets a slab it
    // cannot overrun (the node's chunk is capped at the same figure).
    // The per-bus widths are carried so both processes agree how each
    // direction's channels partition into buses.
    SlabLayout::new(
        max_buffer_size.min(MAX_CHUNK),
        format,
        loaded.input_widths(),
        loaded.output_widths(),
    )
}

fn setup_shm(
    launch: &mut Launch<'_>,
    config: &IpcConfig,
    loaded: &PluginWiring,
    format: FloatWidth,
) -> IpcResult<SlabWriter> {
    let layout = slab_layout_for(loaded, format, config.max_buffer_size);
    let (slab, handle) = AudioSlab::create(layout.clone())?;

    let call = config.timeouts.call;
    let id = launch.open(call, call);
    ipc::send_setup(&mut launch.stream, id, layout, &handle)?;
    match launch.answer(id)? {
        ControlReply::SharedMemoryReady => Ok(slab),
        ref other => Err(unexpected_message("SharedMemoryReady", other)),
    }
}

/// Opens the audio lane over the broker: a fresh stream whose server end the
/// server serves, and whose host end is returned once the server says it holds
/// its own.
fn open_audio_lane(launch: &mut Launch<'_>, config: &IpcConfig) -> IpcResult<ControlStream> {
    let lane = PendingLane::new()?;
    let call = config.timeouts.call;
    let id = launch.open(call, call);
    ipc::send_open_lane(&mut launch.stream, id, Lane::Audio, &lane)?;
    match launch.answer(id)? {
        ControlReply::LaneOpened => lane.stream(),
        ref other => Err(unexpected_message("LaneOpened", other)),
    }
}

/// The slab a running server renders through when the plugin's buses are
/// `loaded`'s: `None` when `held` is already laid out for them (and the server
/// is known to render through it, `unsure` false), else a new one the server has
/// opened and will render through from its next block. After a failed setup the
/// server may be on a slab the host never got the answer for, so `unsure` asks
/// for a new one whatever `held` is.
///
/// Control thread, bounded by the bridge's query deadline. The server swaps at
/// a block head and hands the slab it leaves back off its render thread; the
/// caller drops the slab it held once this returns, which unmaps the host's own
/// view and cannot touch the server's.
///
/// # Errors
///
/// The slab cannot be created, or the server refused it, did not answer in
/// time or is gone. The server may have opened the slab before the answer was
/// lost; `held` stays valid and the next activation asks again.
pub(crate) fn swap_slab(
    bridge: &crate::PluginBridge,
    loaded: &PluginWiring,
    format: FloatWidth,
    ceiling: usize,
    held: &SlabLayout,
    unsure: bool,
) -> IpcResult<Option<SlabWriter>> {
    let layout = slab_layout_for(loaded, format, ceiling);
    if layout == *held && !unsure {
        return Ok(None);
    }
    let (slab, handle) = AudioSlab::create(layout.clone())?;
    if bridge.audio().setup_shared_memory(handle, layout) {
        Ok(Some(slab))
    } else {
        Err(IpcError::SlabRefused)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::shm::RING_SLOTS;
    use std::path::PathBuf;
    use std::process::Child;
    use tutti_plugin_types::ChannelLayout;
    use tutti_plugin_types::Timeouts;

    /// A failed launch must not leave the subprocess running.
    ///
    /// `process` is a bare [`Child`], whose `Drop` explicitly does *not* kill or
    /// reap — so any `?` between the spawn and `ProcessGuard`'s construction
    /// (which happens much later, in `PluginClient::new`) would strand a
    /// `plugin-server` with nothing holding it.
    /// It outlives the host, keeps its socket bound, and holds the plugin's
    /// device claim; a user retrying a failing plugin accumulates them.
    ///
    /// The stand-in has to satisfy three requirements at once: never speak the
    /// protocol, so `handshake` fails the way a hung server does; **stay alive
    /// while doing so**, or there is no leak left to detect; and leave the
    /// socket path alone, so the failure under test is the ordinary
    /// connect/handshake one.
    ///
    /// The middle requirement is easy to get wrong, because `spawn_process`
    /// hands the socket path to the stand-in as argv[1]. `sleep` reads it as a
    /// duration, rejects it, and exits. `cat` reads it as a filename and exits
    /// too, since nothing has created it. Both looked right and both made this
    /// test vacuous — caught only by restoring the leak and watching it still
    /// pass. Creating the path as a FIFO keeps `cat` alive but breaks the third
    /// requirement: `connect` then fails with ENOTSOCK long before the
    /// handshake, testing a different path than the one that matters.
    ///
    /// So the stand-in is a tiny shell script that ignores its arguments and
    /// sleeps. It survives, it never speaks, and it never touches the socket.
    ///
    /// # Keeping it deterministic under parallel load
    ///
    /// **The stand-in is chosen with a [`Locator`]** passed to
    /// `launch_with`, not through `TUTTI_PLUGIN_SERVER`: an env var is
    /// process-global, so every other thread in this binary would see it.
    ///
    /// **Liveness is not checked with a process-table scan** (`pgrep`): the
    /// stand-in's command line is identical in every test process, so a sibling
    /// test binary's healthy stand-in would count as this test's leak.
    ///
    /// Instead the stand-in writes **its own pid** to a path unique to this
    /// test, so the process observed is provably the one `launch` spawned and
    /// no scan is involved. `kill(pid, 0)` then asks
    /// about that process without signalling it, which distinguishes the two
    /// outcomes that matter: a still-running orphan answers `Ok`, while a
    /// killed-and-reaped child answers `ESRCH`. A zombie also answers `Ok`, so
    /// this catches a missing `wait` as well as a missing `kill`.
    ///
    /// **No wall-clock budget.** `launch` kills and waits *synchronously*
    /// before returning, so by the time it has returned the child is already
    /// reaped — there is nothing to wait for, and a retry loop would hide that
    /// certainty behind a deadline a loaded machine could exhaust. The pid is
    /// read once, and asserted once.
    #[test]
    #[cfg(unix)]
    fn a_failed_launch_does_not_strand_the_subprocess() {
        let dir = tempfile::tempdir().expect("a private temp dir");
        let pid_file = dir.path().join("standin.pid");
        let stand_in = dir.path().join("standin.sh");

        // `$$` is the script's own pid, and `exec` replaces that same process
        // image with `sleep` — so the recorded pid is the process that is still
        // alive afterwards, not a parent that has already exited.
        std::fs::write(
            &stand_in,
            format!(
                "#!/bin/sh\necho $$ > {}\nexec sleep 120\n",
                pid_file.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stand_in, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let config = IpcConfig {
            timeouts: {
                let mut t = Timeouts::default();
                t.call = Duration::from_millis(200);
                t
            },
            ..IpcConfig::at(crate::unique_socket_path().expect("the OS has a random source"))
        };

        let err = match launch_with(
            &Locator::at(&stand_in),
            &config,
            Path::new("/nonexistent.vst3"),
            PluginFormat::Vst3,
            SampleRate::SR_48K,
            tutti_fixtures::executor(),
        ) {
            Err(e) => e,
            // Cannot happen (`sleep` never sends `Hello`), but reap rather than
            // leak if the premise ever changes.
            Ok(server) => {
                server.server.terminate();
                panic!("a stand-in server that never speaks the protocol completed a launch");
            }
        };

        // The stand-in records its pid before `exec`, and `launch` cannot return
        // until it has connected-or-timed-out against a server it spawned — so
        // an absent file means the stand-in was never run, which would make
        // every assertion below vacuous.
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap_or_else(|e| {
                panic!(
                    "the stand-in server never recorded its pid ({e}) — it was not \
                     spawned, so this test would prove nothing about cleanup. \
                     launch returned: {err}"
                )
            })
            .trim()
            .parse()
            .expect("the stand-in writes a bare pid");

        // No retry loop: `launch` kills AND waits before returning, so the child
        // is reaped by the time control is back here. A budget would only hide a
        // missing `wait` behind a deadline.
        assert!(
            !is_alive(pid),
            "a failed launch ({err}) left stand-in server pid {pid} running — a \
             bare `?` after the spawn strands the subprocess, because \
             `Child::drop` neither kills nor reaps"
        );
    }

    /// A launch that restores a staged state, against the reference plugin, with
    /// its switches set in this process's environment (the server inherits it;
    /// nextest runs each test in a process of its own).
    #[cfg(unix)]
    fn stateful_launch(
        env: &[(&str, &str)],
        call: Duration,
        state_restore: Duration,
    ) -> IpcResult<LaunchedServer> {
        // Held for the launch: the server inherits the switches when it spawns.
        let mut switches = tutti_fixtures::ScopedEnv::new();
        for (k, v) in env {
            switches.set(k, v);
        }
        let config = IpcConfig {
            timeouts: {
                let mut t = Timeouts::default();
                t.call = call;
                t.state_restore = state_restore;
                t
            },
            ..IpcConfig::at(crate::unique_socket_path().expect("the OS has a random source"))
        };
        let origin = PluginRef::file(tutti_fixtures::clap_probe());
        launch_with_state(
            &Locator::at(tutti_fixtures::plugin_server()),
            &config,
            &LoadRequest {
                origin: &origin,
                format: PluginFormat::Clap,
                rate: SampleRate::SR_48K,
                restore: Some(Restore {
                    state: &[0u8; 16],
                    state_use: StateUse::Project,
                }),
            },
            tutti_fixtures::executor(),
        )
    }

    #[cfg(unix)]
    fn reap(server: IpcResult<LaunchedServer>) {
        // The guard kills and reaps the server as it drops.
        drop(server);
    }

    /// Launches against the reference plugin with its restore held on a fifo the
    /// test owns: the plugin is in its restore when it opens the fifo for
    /// reading, and the restore returns when the last writer closes it.
    /// `releaser` is given the fifo's path and runs on its own thread.
    #[cfg(unix)]
    mod held_restore {
        use super::*;

        fn launch_held(
            call: Duration,
            state_restore: Duration,
            releaser: Option<fn(&Path)>,
        ) -> IpcResult<LaunchedServer> {
            let dir = tempfile::tempdir().expect("a private temp dir");
            let fifo = dir.path().join("restore-hold");
            let made = Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
            assert!(made.success(), "mkfifo {}", fifo.display());
            let release = releaser.map(|release| {
                let fifo = fifo.clone();
                std::thread::spawn(move || release(&fifo))
            });
            let launched = stateful_launch(
                &[(
                    "TUTTI_CLAP_PROBE_STATE_LOAD_HOLD_FIFO",
                    fifo.to_str().expect("a utf-8 path"),
                )],
                call,
                state_restore,
            );
            if let Some(release) = release {
                // A launch that failed before the plugin opened the fifo leaves
                // the releaser blocked opening it for writing. Opening it for
                // reading and writing never blocks and pairs with that open, so
                // the releaser ends whenever the launch did.
                drop(
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&fifo)
                        .expect("the fifo opens"),
                );
                release.join().expect("the releaser ends");
            }
            launched
        }

        /// **A restore that returns when the plugin is done is a load**, however
        /// long that is: the plugin blocks in its state restore until the test
        /// releases it, after the server has said it began restoring, and the
        /// launch waits for it and completes. Both windows are hang guards, so no
        /// clock decides the outcome.
        ///
        /// This does not pin "call window short, restore long, succeeds": that
        /// needs a call window the opening finishes inside and a restore that
        /// outlasts it, and nothing here can say the call window has passed
        /// without a clock the launch does not take (on the Mac the opening alone
        /// outlasts a 1 s call window). The window limit is pinned by
        /// `a_restore_is_waited_on_for_its_own_window` (the window each phase
        /// waits under) and `a_restore_that_outlasts_its_window_is_blamed_on_the_state`
        /// (the restore window ends a held restore while the call window is far
        /// longer).
        ///
        /// Needs `cargo build -p tutti-plugin-server` first.
        #[test]
        fn a_restore_that_returns_when_released_completes_the_launch_as_a_load() {
            let launched = launch_held(
                Duration::from_secs(60),
                Duration::from_secs(60),
                Some(|fifo| {
                    // Opening for writing returns when the plugin opens for
                    // reading, and closing it ends the restore.
                    drop(std::fs::OpenOptions::new().write(true).open(fifo));
                }),
            );
            let server = launched.expect("a restore that returns is not a failure");
            assert!(server.state_restore.is_some(), "a restore was attempted");
            reap(Ok(server));
        }

        /// **A restore that never returns is the state's to answer for**: past
        /// its window the launch fails as `StateRestore`, and the call
        /// window, a hang guard here, is not what ended the wait. No one writes
        /// to the fifo, so the restore outlasts its 200 ms window in every run.
        #[test]
        fn a_restore_that_outlasts_its_window_is_blamed_on_the_state() {
            let launched = launch_held(Duration::from_secs(60), Duration::from_millis(200), None);
            assert!(
                matches!(&launched, Err(IpcError::StateRestore { .. })),
                "{:?}",
                launched.as_ref().err()
            );
        }
    }

    /// **A plugin that dies in its state restore is blamed on the state.** The
    /// death is read off the connection, so the windows are hang guards.
    #[test]
    #[cfg(unix)]
    fn a_crash_in_the_restore_is_blamed_on_the_state() {
        let launched = stateful_launch(
            &[("TUTTI_CLAP_PROBE_CRASH_ON_STATE_LOAD", "1")],
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        assert!(
            matches!(&launched, Err(IpcError::StateRestore { .. })),
            "{:?}",
            launched.as_ref().err()
        );
    }

    /// **Only a restore's failure is the state's**: an open that times out
    /// blames nothing, and the same timeout once the server said it was
    /// restoring is the state's, with the timeout as its cause. (The windows
    /// are the conversation's, tested over constructed instants.)
    #[test]
    fn only_a_restores_failure_is_the_states() {
        let timed_out = || IpcError::Timeout {
            operation: "a reply",
            after: Duration::from_millis(1),
            partial: false,
        };
        assert!(matches!(
            LoadPhase::Opening.blame(timed_out()),
            IpcError::Timeout { .. }
        ));
        assert!(matches!(
            LoadPhase::Restoring.blame(timed_out()),
            IpcError::StateRestore { cause } if matches!(*cause, IpcError::Timeout { .. })
        ));
    }

    fn exchange_timeouts() -> Timeouts {
        let mut t = Timeouts::default();
        t.load = Duration::from_secs(5);
        t.state_restore = Duration::from_secs(120);
        t
    }

    fn working(id: ReqId, stage: Working) -> ControlFrame {
        ControlFrame::Working { id, stage }
    }

    /// **The server's signs of life restart the load's wait, and only for its
    /// window**: each `Working` at work opening moves the deadline a load
    /// window past the sign, and the wait is still due that window after the
    /// last one.
    #[test]
    fn a_sign_of_life_restarts_the_loads_wait() {
        let t = exchange_timeouts();
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        let mut exchange = Exchange::new(&t);
        let id = exchange.open(start, t.load, t.load.saturating_mul(LOAD_CEILING_FACTOR));

        let heard = exchange
            .hear(working(id, Working::Opening), id, at(4))
            .unwrap();
        assert!(matches!(heard, Heard::Working));
        assert_eq!(
            exchange.left(id, at(4)),
            Duration::from_secs(5),
            "restarted at the sign"
        );
        assert_eq!(
            exchange.left(id, at(9)),
            Duration::ZERO,
            "a window after the last"
        );
        assert_eq!(exchange.phase, LoadPhase::Opening);
    }

    /// **A restore has its own window and its own ceiling, from the instant the
    /// server says it began**: with a 5 s load window (a 50 s ceiling) and a
    /// 120 s restore, the restore is still waited for 100 s in, long past the
    /// load's ceiling, and a failure from then on is the state's.
    #[test]
    fn a_restore_has_its_own_window_and_ceiling() {
        let t = exchange_timeouts();
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        let mut exchange = Exchange::new(&t);
        let id = exchange.open(start, t.load, t.load.saturating_mul(LOAD_CEILING_FACTOR));

        exchange
            .hear(working(id, Working::Restoring), id, at(3))
            .unwrap();
        assert_eq!(exchange.phase, LoadPhase::Restoring);
        assert_eq!(exchange.left(id, at(3)), Duration::from_secs(120));
        assert!(
            exchange.left(id, at(100)) > Duration::ZERO,
            "past the load's 50 s ceiling and still within the restore's"
        );
        assert_eq!(
            exchange.left(id, at(123)),
            Duration::ZERO,
            "120 s after it began"
        );
        assert!(matches!(
            exchange.blame(IpcError::Timeout {
                operation: "a reply",
                after: Duration::ZERO,
                partial: false,
            }),
            IpcError::StateRestore { cause }
                if matches!(*cause, IpcError::Timeout { after, .. } if after == Duration::from_secs(120))
        ));
    }

    /// **A restore that beats for ever is not waited for for ever**: the server
    /// says `Restoring` every second of a restore that hangs, and the wait still
    /// ends at the ceiling set when the restore began, whatever the beat does.
    #[test]
    fn a_restoring_beat_does_not_move_the_ceiling() {
        let t = exchange_timeouts();
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        let mut exchange = Exchange::new(&t);
        let id = exchange.open(start, t.load, t.load.saturating_mul(LOAD_CEILING_FACTOR));

        for second in 3..=400 {
            exchange
                .hear(working(id, Working::Restoring), id, at(second))
                .unwrap();
        }
        // The restore began at 3 s with a 120 s ceiling: due at 123 s, however
        // many beats came after.
        assert_eq!(exchange.left(id, at(122)), Duration::from_secs(1));
        assert_eq!(exchange.left(id, at(123)), Duration::ZERO);
    }

    /// **The phase never goes back**: a beat's `Opening` that arrives after the
    /// restore began (the beat and the notice are two threads) leaves the load
    /// in its restore, so a failure after it is still the state's.
    #[test]
    fn the_phase_only_goes_forward() {
        let t = exchange_timeouts();
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);
        let mut exchange = Exchange::new(&t);
        let id = exchange.open(start, t.load, t.load.saturating_mul(LOAD_CEILING_FACTOR));
        exchange
            .hear(working(id, Working::Restoring), id, at(1))
            .unwrap();
        exchange
            .hear(working(id, Working::Opening), id, at(2))
            .unwrap();
        assert_eq!(exchange.phase, LoadPhase::Restoring);
        assert!(matches!(
            exchange.blame(IpcError::ProcessCrashed),
            IpcError::StateRestore { .. }
        ));
    }

    /// **The answer is the one of the request waited for**, and a frame that is
    /// neither an answer nor a sign of life is refused.
    #[test]
    fn only_the_awaited_answer_ends_the_wait() {
        let t = exchange_timeouts();
        let now = Instant::now();
        let mut exchange = Exchange::new(&t);
        let id = exchange.open(now, t.call, t.call);
        let reply = |of| ControlFrame::Reply {
            id: of,
            reply: ControlReply::SharedMemoryReady,
        };
        assert!(exchange.hear(reply(ReqId::new(99)), id, now).is_err());
        assert!(matches!(
            exchange.hear(reply(id), id, now),
            Ok(Heard::Answer(ControlReply::SharedMemoryReady))
        ));
        let other = ControlFrame::News(crate::News::EditorClosed);
        assert!(exchange.hear(other, id, now).is_err());
    }

    /// **A launch refuses a second `Hello` and a `LaneOpened` it did not ask
    /// for**: neither is the answer to the request waited for, nor a sign of life.
    #[test]
    fn a_second_hello_and_an_unrequested_lane_answer_end_the_wait() {
        let t = exchange_timeouts();
        let now = Instant::now();
        let mut exchange = Exchange::new(&t);
        let id = exchange.open(now, t.call, t.call);
        let hello = ControlFrame::Hello {
            protocol_version: crate::PROTOCOL_VERSION,
        };
        let unrequested = ControlFrame::Reply {
            id: ReqId::new(99),
            reply: ControlReply::LaneOpened,
        };
        assert!(exchange.hear(hello, id, now).is_err());
        assert!(exchange.hear(unrequested, id, now).is_err());
    }

    /// **The first frame of a server must be its `Hello`**: a reply sent before
    /// it is refused as the unexpected message it is, and no version is read.
    #[test]
    fn a_frame_before_hello_is_refused() {
        let err = handshake_against(
            postcard::to_allocvec(&ControlFrame::Reply {
                id: ReqId::new(1),
                reply: ControlReply::LaneOpened,
            })
            .unwrap(),
        );
        assert!(
            matches!(
                err,
                IpcError::UnexpectedMessage {
                    expected: "Hello",
                    ..
                }
            ),
            "{err}"
        );
    }

    /// **A plugin that dies before the restore is not blamed on the state**: it
    /// dies initializing, which comes first, and the launch fails as any load does.
    #[test]
    #[cfg(unix)]
    fn a_crash_before_the_restore_is_not_blamed_on_the_state() {
        let launched = stateful_launch(
            &[("TUTTI_CLAP_PROBE_CRASH_ON_INIT", "1")],
            Duration::from_secs(60),
            Duration::from_secs(60),
        );
        assert!(
            launched.is_err() && !matches!(&launched, Err(IpcError::StateRestore { .. })),
            "{:?}",
            launched.as_ref().err()
        );
    }

    /// A library that is not a plugin fails the launch with the loader's own
    /// reason. The server answers the load; it does not exit, which the host
    /// would report as `ProcessCrashed`.
    ///
    /// The reference CLAP plugin's cdylib, loaded as VST3, is a real library the
    /// VST3 host refuses. The failure this test excludes is the server's refusal
    /// of a format it does not serve (`unsupported plugin format`), which
    /// answers before any format's loader runs.
    ///
    /// Needs `cargo build -p tutti-plugin-server` first (`CLAUDE.md`).
    #[test]
    #[cfg(unix)]
    fn a_library_that_is_not_a_plugin_fails_the_launch_with_the_loaders_reason() {
        let dir = tempfile::tempdir().expect("a private temp dir");
        let config =
            IpcConfig::at(crate::unique_socket_path().expect("the OS has a random source"));
        // A copy in this test's own directory, not `publish_with_extension`:
        // that names the link `real.with_extension(..)`, which on Linux is the
        // cdylib's own path.
        let not_a_plugin = dir.path().join("not_a_plugin.vst3");
        std::fs::copy(
            tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin"),
            &not_a_plugin,
        )
        .expect("copy the cdylib under a VST3 name");

        let err = match launch_with(
            &Locator::at(tutti_fixtures::plugin_server()),
            &config,
            &not_a_plugin,
            PluginFormat::Vst3,
            SampleRate::SR_48K,
            tutti_fixtures::executor(),
        ) {
            Err(e) => e,
            Ok(server) => {
                server.server.terminate();
                panic!("a library with no plugin entry point launched");
            }
        };
        match err {
            IpcError::LoadFailed { reason, .. } => {
                assert!(
                    !reason.contains("unsupported plugin format"),
                    "reason: {reason}"
                );
                assert!(!reason.is_empty(), "the reply names a reason");
            }
            other => panic!("expected LoadFailed, got {other:?}"),
        }
    }

    /// Whether process `pid` still exists, asked without signalling it.
    ///
    /// `kill -0` is the POSIX existence probe: it performs permission checks and
    /// reports the result, but delivers no signal. Exit status 0 means the
    /// process is there (a zombie included, which is what makes a *missing*
    /// `wait` detectable), non-zero means ESRCH.
    ///
    /// Shelling out rather than calling `libc::kill` only because this crate has
    /// no `libc` dependency and one test does not earn it. The property that
    /// matters is unchanged either way: this asks about **one known pid**, so
    /// unlike the `pgrep` scan it replaced it cannot see any other test's
    /// processes.
    #[cfg(unix)]
    fn is_alive(pid: i32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn loaded(inputs: &[ChannelLayout], outputs: &[ChannelLayout]) -> PluginWiring {
        let buses = |widths: &[ChannelLayout]| {
            widths
                .iter()
                .map(|&w| tutti_plugin_types::PluginBus::main(w))
                .collect()
        };
        PluginWiring {
            inputs: buses(inputs),
            outputs: buses(outputs),
            ..Default::default()
        }
    }

    /// The default `IpcConfig::max_buffer_size`. Named here so the sizing
    /// tests below read as "the shipped default", not as a magic number.
    const DEFAULT_MAX_BUFFER: usize = 8192;

    /// A plain stereo-in/stereo-out plugin with one bus per direction is the
    /// *common* case, and the one most easily collapsed onto a single shared
    /// region: at `output_base == 0` the host's own input write lands where it
    /// later reads the plugin's output. The two directions must be separately
    /// sized and separately addressed.
    #[test]
    fn stereo_one_bus_each_direction_gets_disjoint_regions() {
        let layout = slab_layout_for(
            &loaded(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO]),
            FloatWidth::F32,
            DEFAULT_MAX_BUFFER,
        );

        assert_eq!(layout.input_channels(), 2);
        assert_eq!(layout.output_channels(), 2);
        assert_eq!(
            layout.input_ring_bytes(),
            layout.ring_bytes(layout.output_channels())
        );
        // Both directions are counted. Half this total would mean the two
        // directions share one region, which is the aliasing.
        assert_eq!(
            layout.checked_byte_size_with_header(0),
            Some(layout.input_ring_bytes() * 2)
        );
    }

    /// A sidechain widens only the input direction.
    #[test]
    fn sidechain_input_widens_only_its_own_ring() {
        let layout = slab_layout_for(
            &loaded(
                &[ChannelLayout::STEREO, ChannelLayout::MONO],
                &[ChannelLayout::STEREO],
            ),
            FloatWidth::F32,
            DEFAULT_MAX_BUFFER,
        );

        assert_eq!(layout.input_channels(), 3);
        assert_eq!(layout.output_channels(), 2);
        assert!(layout.input_ring_bytes() > layout.ring_bytes(layout.output_channels()));
    }

    /// A direction with no buses (an instrument's inputs) is one bus of no
    /// channels at the slab, which refuses an empty list: it occupies no
    /// bytes, and the output ring is sized as before.
    #[test]
    fn a_direction_with_no_buses_is_one_bus_of_no_channels() {
        let layout = slab_layout_for(
            &loaded(&[], &[ChannelLayout::STEREO]),
            FloatWidth::F32,
            DEFAULT_MAX_BUFFER,
        );
        assert_eq!(layout.inputs, vec![ChannelLayout::EMPTY]);
        assert_eq!(layout.input_ring_bytes(), 0);
        assert_eq!(layout.output_channels(), 2);
    }

    /// Every layout this function produces must be one the slab will accept.
    /// Cheaper to assert here than to discover at plugin-load time on a user's
    /// machine.
    #[test]
    fn every_produced_layout_is_addressable() {
        let cases = [
            loaded(&[], &[]),
            loaded(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO]),
            loaded(&[ChannelLayout::MONO], &[ChannelLayout::STEREO]),
            loaded(
                &[ChannelLayout::STEREO, ChannelLayout::MONO],
                &[ChannelLayout::STEREO],
            ),
        ];
        for (i, l) in cases.iter().enumerate() {
            let layout = slab_layout_for(l, FloatWidth::F32, DEFAULT_MAX_BUFFER);
            assert!(!layout.inputs.is_empty(), "case {i}: empty input buses");
            assert!(!layout.outputs.is_empty(), "case {i}: empty output buses");
            assert_eq!(layout.slots as usize, RING_SLOTS, "case {i}");
        }
    }

    /// The slab is sized to the largest chunk the node ships — one device
    /// callback, at most `MAX_CHUNK` — not to `max_buffer_size`, which is the
    /// plugin's own buffer bound. A live plugin gets a whole device callback
    /// per chunk, and the slab holds the largest one.
    #[test]
    fn slab_is_sized_to_the_largest_chunk_not_the_configured_maximum() {
        let l = loaded(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO]);
        let layout = slab_layout_for(&l, FloatWidth::F32, DEFAULT_MAX_BUFFER);
        assert_eq!(layout.samples_per_channel, MAX_CHUNK);
        const { assert!(MAX_CHUNK < DEFAULT_MAX_BUFFER) };
    }

    /// A host configured *below* the chunk ceiling gets a slab it cannot overrun —
    /// the reason this is a `min` and not a bare `MAX_CHUNK`.
    #[test]
    fn a_smaller_configured_buffer_wins() {
        let l = loaded(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO]);
        let layout = slab_layout_for(&l, FloatWidth::F32, 32);
        assert_eq!(layout.samples_per_channel, 32);
    }

    /// f64 negotiation doubles the region; the channel/bus decisions are
    /// unaffected by sample format.
    #[test]
    fn f64_doubles_the_byte_size_only() {
        let l = loaded(&[ChannelLayout::STEREO], &[ChannelLayout::STEREO]);
        let f32_layout = slab_layout_for(&l, FloatWidth::F32, DEFAULT_MAX_BUFFER);
        let f64_layout = slab_layout_for(&l, FloatWidth::F64, DEFAULT_MAX_BUFFER);

        assert_eq!(f64_layout.inputs, f32_layout.inputs);
        assert_eq!(
            f64_layout.samples_per_channel,
            f32_layout.samples_per_channel
        );
        assert_eq!(
            f64_layout.checked_byte_size_with_header(0),
            f32_layout.checked_byte_size_with_header(0).map(|n| n * 2)
        );
    }

    /// Drives the real [`handshake`] against a stand-in server whose first frame
    /// is `body`, and returns the error it must end in.
    fn handshake_against(body: Vec<u8>) -> IpcError {
        use interprocess::local_socket::{ListenerOptions, traits::Listener as _};

        // Unique per run. A fixed name collides with a leftover socket from an
        // earlier run, and `connect` then fails with EINVAL *before* the version
        // is ever compared — an IO error that would masquerade as a working gate.
        let path = crate::unique_socket_path().expect("the OS has a random source");
        let _ = std::fs::remove_file(&path);
        let name = crate::transport::control::socket_name(&path).unwrap();
        let listener = ListenerOptions::new().name(name).create_sync().unwrap();

        let server = std::thread::spawn(move || {
            use std::io::Write;
            let Ok(stream) = listener.accept() else {
                return;
            };
            let mut stream = &stream;
            let _ = stream.write_all(&(body.len() as u32).to_be_bytes());
            let _ = stream.write_all(&body);
        });

        let config = IpcConfig {
            timeouts: {
                let mut t = Timeouts::default();
                t.call = Duration::from_secs(2);
                t
            },
            ..IpcConfig::at(path.clone())
        };
        // The listener above stands in for the server, so there is no child to
        // watch — but `handshake` takes a guard to notice a spawn that died. A
        // lingering stand-in stays alive for the whole test; the guard kills it.
        let stand_in = ProcessGuard::launched(
            Reaped::new(spawn_stand_in(StandIn::Lingers)),
            &config,
            tutti_fixtures::executor(),
        )
        .expect("the exit thread starts");
        let err = handshake(&config, &stand_in)
            .expect_err("a stale server must not complete the handshake");
        drop(stand_in);
        let _ = server.join();
        let _ = std::fs::remove_file(&path);
        err
    }

    /// A leftover v3 `plugin-server` on `PATH` must be refused at the handshake
    /// with a named error, not crash and not proceed.
    ///
    /// This matters because the wire is not self-describing. A v3 server that got
    /// as far as `SetupSharedMemory` would read our `slots` field out of the
    /// bytes where its own build expects `channels`, get a plausible small
    /// integer, and map a wrong-sized region — in silence. The version gate is
    /// what stops that, so it has to fire before any payload is sent, and the
    /// failure has to name both versions or the operator has no idea which
    /// binary is stale.
    ///
    /// Drives the real [`handshake`] against a socket that speaks v3, rather
    /// than unit-testing `check_protocol_version` in isolation — the risk being
    /// covered is a handshake path that forgets to call the gate at all.
    #[test]
    fn a_stale_v3_server_is_refused_with_a_named_version_error() {
        use crate::PROTOCOL_VERSION;

        const STALE_VERSION: u32 = 3;
        assert_ne!(
            STALE_VERSION, PROTOCOL_VERSION,
            "this test is meaningless once the current version reaches 3"
        );

        let err = handshake_against(
            postcard::to_allocvec(&ControlFrame::Hello {
                protocol_version: STALE_VERSION,
            })
            .unwrap(),
        );
        match err {
            IpcError::ProtocolMismatch { expected, got } => {
                assert_eq!(expected, PROTOCOL_VERSION);
                assert_eq!(got, STALE_VERSION);
            }
            ref other => panic!("expected a ProtocolMismatch naming both versions, got: {other}"),
        }

        // The operator reads this line, so it must carry both numbers.
        let text = format!("{err}");
        assert!(
            text.contains(&STALE_VERSION.to_string())
                && text.contains(&PROTOCOL_VERSION.to_string()),
            "the message must name both versions, got: {text}"
        );
    }

    /// **A version-31 server, whose `AudioProcessed` reply has no `status`, is
    /// refused at the handshake** (version 32 added it): the reply would
    /// otherwise decode a byte short and misread the block. Pins that the
    /// block-status wire change bumped the version.
    #[test]
    fn a_server_from_before_the_block_status_is_refused() {
        use crate::PROTOCOL_VERSION;

        const BEFORE_STATUS: u32 = 31;
        let err = handshake_against(
            postcard::to_allocvec(&ControlFrame::Hello {
                protocol_version: BEFORE_STATUS,
            })
            .unwrap(),
        );
        assert!(
            matches!(
                err,
                IpcError::ProtocolMismatch { expected, got }
                    if expected == PROTOCOL_VERSION && got == BEFORE_STATUS
            ),
            "expected a ProtocolMismatch naming 32 and 31, got: {err}"
        );
    }

    /// **A server from the bincode wire (protocol 26 and older) is refused at
    /// the handshake, and the error says to rebuild it.** Its `Hello` is a
    /// four-byte little-endian variant tag then the version as a four-byte
    /// little-endian `u32`, written out here from bincode 1's layout; under
    /// postcard those bytes do not decode as one message, so the version is
    /// never read and the codec error is what the operator would otherwise see.
    #[test]
    fn a_bincode_era_server_is_refused_with_a_rebuild_hint() {
        let hello = postcard::to_allocvec(&ControlFrame::Hello {
            protocol_version: 0,
        })
        .unwrap()[0];
        let mut bincode_ready = vec![hello, 0, 0, 0];
        bincode_ready.extend_from_slice(&26u32.to_le_bytes());

        match handshake_against(bincode_ready) {
            stale @ IpcError::StaleServer { .. } => {
                assert!(stale.to_string().contains("rebuild the plugin-server"));
            }
            other => panic!("expected StaleServer, got: {other}"),
        }
    }

    /// **A late bind still connects.** The stand-in server binds its socket only
    /// when the test writes a byte to its stdin, which the test does once the
    /// connect is running on its own thread, so the connect is retrying when the
    /// socket appears. A connect that gave up at its first failure fails here
    /// whenever its first attempt came before the bind.
    #[test]
    fn a_late_bind_still_connects() {
        use std::io::Write as _;

        let path = std::env::temp_dir().join(format!("tutti_late_bind_{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut child = spawn_stand_in(StandIn::BindsOnByte(path.clone()));
        let mut release = child.stdin.take().expect("the stand-in's stdin is piped");
        let stand_in = ProcessGuard::launched(
            Reaped::new(child),
            &IpcConfig::at(path.clone()),
            tutti_fixtures::executor(),
        )
        .expect("the exit thread starts");

        let (running, is_running) = std::sync::mpsc::channel();
        let result = std::thread::scope(|scope| {
            let connect = scope.spawn(|| {
                running.send(()).expect("the test waits");
                connect_when_listening(&path, &stand_in, Timeouts::default().connect)
            });
            is_running.recv().expect("the connect thread starts");
            release
                .write_all(&[1])
                .expect("the stand-in reads its stdin");
            connect.join().expect("the connect thread ends")
        });
        drop(stand_in);

        assert!(
            result.is_ok(),
            "the connect must retry until the server binds, got: {:?}",
            result.err()
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A server that dies before binding is reported as dead, not as an
    /// unreachable socket after the timeout.
    ///
    /// Without the watch on the process this case is indistinguishable from a
    /// slow spawn, so the host would retry for the whole patience and report the
    /// connect failure. The patience here is a hang guard far past anything the
    /// retry needs: only the watch can produce "exited before binding".
    #[test]
    fn a_server_that_never_binds_is_reported_dead_not_unreachable() {
        let path = std::env::temp_dir().join(format!("tutti_never_bound_{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // Exits immediately and binds nothing — the shape of a plugin-server
        // that dies on a missing dynamic library.
        let dead = ProcessGuard::launched(
            Reaped::new(spawn_stand_in(StandIn::Exits)),
            &IpcConfig::at(path.clone()),
            tutti_fixtures::executor(),
        )
        .expect("the exit thread starts");

        let err = connect_when_listening(&path, &dead, Duration::from_secs(60))
            .expect_err("a server that never binds cannot connect");

        assert!(
            matches!(err, IpcError::ServerExited { .. }),
            "the error must say the server died rather than blaming the socket, got: {err}"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Which throwaway child a connect test needs in place of a plugin-server.
    enum StandIn {
        /// Exits at once, binding nothing.
        Exits,
        /// Stays alive, binding nothing, until the test kills it.
        Lingers,
        /// Binds the socket at this path once its stdin has a byte, accepts a
        /// connection, and stays until its stdin closes.
        BindsOnByte(PathBuf),
    }

    /// Set only in a [`StandIn::Lingers`] child, so that [`stand_in_lingers`]
    /// waits there and is an instant no-op in an ordinary test run.
    const STAND_IN_ENV: &str = "TUTTI_LAUNCH_TEST_STAND_IN";

    /// Set only in a [`StandIn::BindsOnByte`] child: the path it binds.
    const STAND_IN_BIND_ENV: &str = "TUTTI_LAUNCH_TEST_STAND_IN_BIND";

    /// Spawns a stand-in by re-running **this test binary**, never a program
    /// looked up on `PATH`.
    ///
    /// These were `sleep 30` and `true`. Neither is a Windows program: there
    /// they resolve to whatever `PATH` offers (Git's MSYS `usr/bin` on CI) or to
    /// nothing. An MSYS `true` can take seconds to start and exit under load,
    /// and the never-binds test measures exactly that interval — on Windows CI
    /// it took 5.7 s against its 2.5 s budget and failed, having tested the
    /// runner's `PATH` rather than `connect_when_listening`. The test binary
    /// exists on every target by construction, and its harness starts and
    /// exits in milliseconds.
    ///
    /// - `Exits` asks the harness for an `--exact` name no test has: libtest
    ///   filters everything out and exits 0 without running anything.
    /// - `Lingers` runs [`stand_in_lingers`] alone, with [`STAND_IN_ENV`] set so
    ///   it waits. If that name ever stops matching, the child exits at once
    ///   instead and `the_connect_waits_for_a_socket_that_is_not_bound_yet`
    ///   fails with `ServerExited` — the drift cannot pass silently.
    fn spawn_stand_in(kind: StandIn) -> Child {
        let exe = std::env::current_exe().expect("the test binary's own path");
        let mut cmd = Command::new(exe);
        // libtest names a unit test by its path *within* the crate, so drop the
        // leading `tutti_plugin_ipc::` from `module_path!()`.
        let module = module_path!()
            .split_once("::")
            .map_or(module_path!(), |(_, rest)| rest);
        match kind {
            StandIn::BindsOnByte(path) => {
                cmd.args(["--exact", &format!("{module}::stand_in_binds_late")])
                    .env(STAND_IN_BIND_ENV, path);
            }
            StandIn::Exits => {
                cmd.args(["--exact", "no test in this binary has this name"]);
            }
            StandIn::Lingers => {
                cmd.args(["--exact", &format!("{module}::stand_in_lingers")])
                    .env(STAND_IN_ENV, "1");
            }
        }
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("failed to re-run the test binary as a stand-in child")
    }

    /// The body of a [`StandIn::Lingers`] child; a no-op anywhere else.
    ///
    /// It waits for its stdin to close, which its parent's end of the pipe does
    /// when the test kills it or ends, so a leaked one goes away by itself.
    #[test]
    fn stand_in_lingers() {
        if std::env::var_os(STAND_IN_ENV).is_some() {
            let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        }
    }

    /// The body of a [`StandIn::BindsOnByte`] child; a no-op anywhere else.
    #[test]
    fn stand_in_binds_late() {
        use interprocess::local_socket::{ListenerOptions, traits::Listener as _};
        use std::io::Read as _;

        let Some(path) = std::env::var_os(STAND_IN_BIND_ENV) else {
            return;
        };
        let mut stdin = std::io::stdin();
        stdin.read_exact(&mut [0u8]).expect("the parent's byte");
        let name = crate::transport::control::socket_name(Path::new(&path)).expect("socket name");
        let listener = ListenerOptions::new()
            .name(name)
            .create_sync()
            .expect("bind the late listener");
        let _connection = listener.accept();
        let _ = std::io::copy(&mut stdin, &mut std::io::sink());
    }

    /// **A real plugin-server that is ended is declared dead by its process's
    /// end**, with a failing status, though nothing reads its socket (no bridge
    /// thread is attached, so the stream's EOF is not a signal here): the watch
    /// does not wait on a timeout, and nothing would end this wait but the
    /// process's end.
    ///
    /// No bridge is attached, so by construction only the process's end can set
    /// the watch; the 5 s bound is a ceiling for a loaded machine, not a pace.
    ///
    /// Needs `cargo build -p tutti-plugin-server` first.
    #[test]
    fn an_ended_real_server_is_declared_dead_through_its_exit() {
        use tutti_plugin_types::PluginFailure;

        let config =
            IpcConfig::at(crate::unique_socket_path().expect("the OS has a random source"));
        let origin = PluginRef::file(tutti_fixtures::clap_probe());
        let launched = launch_with_state(
            &Locator::at(tutti_fixtures::plugin_server()),
            &config,
            &LoadRequest {
                origin: &origin,
                format: PluginFormat::Clap,
                rate: SampleRate::SR_48K,
                restore: None,
            },
            tutti_fixtures::executor(),
        )
        .expect("the reference plugin launches");
        let guard = &launched.server;
        assert!(!guard.watch().is_dead(), "a launched server is alive");
        guard.terminate();

        assert!(
            guard.watch().wait_dead(Duration::from_secs(60)),
            "the end of the process was never declared"
        );
        match guard.watch().cause() {
            Some(PluginFailure::ServerExited { status }) => {
                assert!(!status.success(), "{status:?}");
            }
            other => panic!("expected the process's end, got {other:?}"),
        }
    }
}

/// The audio lane's open, against a mock server over a socket pair.
#[cfg(all(test, unix))]
mod lane_tests;
