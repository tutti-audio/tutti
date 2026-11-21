//! The bridge's lanes: one task per lane, each on a thread of its own that the
//! executor starts. The audio lane's task connects, hands the control lane's
//! stream to the control lane's task, and pumps [`AudioCommand`]s through
//! [`handle_audio`]; the control lane's task pumps `ControlCommand`s through
//! [`handle_control`] and reads what the server says unprompted. Each owns its
//! connection, its queue and its wake-up, so a block never waits behind a
//! control request and all IPC calls on a lane are blocking.
//!
//! The audio-response ring has one producer, the audio lane's task. The control
//! lane's task never pushes to it: a failure it sees is latched (which wakes
//! both lanes, [`Channels::waker`]) and the audio lane's task, woken,
//! answers the queued blocks.
//!
//! A lane's task blocks in slices of work and returns to the executor between
//! them ([`yield_now`]), so an executor that shuts down drops it at the next
//! slice and not when the server next speaks. The bridge keeps each lane's
//! [`Task`], which every way out of the lane resolves, and waits on it.

use super::ListenerSlot;
#[cfg(test)]
use super::channels::response_ring;
use super::channels::{Channels, ResponseWriter, Sequenced, Waits};
use super::dispatch::{
    AudioState, ControlState, Lane, apply_setting, handle_audio, handle_control, handle_setting,
    read_news, tell,
};
use super::lifecycle::Lifecycle;
use super::messages::{AudioCommand, AudioResponse, ControlCommand, IpcEvent, Unsolicited};
use super::payload_pool::PayloadPool;
use crate::transport::control::{self as ipc, ControlStream, ControlWait};
use crate::yield_now::yield_now;
use crate::{AudioReply, ControlFrame, ControlRequest};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use tutti_platform::{Executor, PriorityRequest, Task, WeakExecutor, join};
use tutti_plugin_types::{IpcError, IpcResult, PluginFailure};
use tutti_rt::Parker;

/// How long the control lane waits on its connection, its bell and the stream it
/// is handed before it returns to the executor: only what bounds how long an
/// executor that shuts down waits for an idle lane, since every event the lane
/// serves ends its wait at once.
const LANE_SLICE: Duration = Duration::from_millis(100);

/// How long a server that has accepted a connection has to say `Ready` on it:
/// generous, because a server still loading a large plugin binary on a slow
/// machine legitimately takes seconds, and finite because nothing else would end
/// the wait.
const READY_WITHIN: Duration = Duration::from_secs(30);

/// The two lanes of an `AudioBridge`'s connection, which speak to the server for
/// the queues. Dropping it shuts both down and waits for both.
pub struct IpcThread {
    channels: Channels,
    lifecycle: Lifecycle,
    lanes: Option<Lanes>,
}

/// The tasks of the two lanes.
struct Lanes {
    audio: Task<()>,
    control: Task<()>,
}

impl std::fmt::Debug for IpcThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcThread")
            .field("running", &self.lanes.is_some())
            .finish_non_exhaustive()
    }
}

impl IpcThread {
    /// Starts both lanes on `executor`: the control lane waits for the stream the
    /// audio lane hands it once it has connected, so neither lane's future holds
    /// the executor, and neither starts the other.
    ///
    /// # Errors
    ///
    /// The OS refused a lane's thread; the lane already started is cancelled.
    pub(super) fn spawn(
        executor: &Executor,
        channels: Channels,
        responses: ResponseWriter,
        waits: Waits,
        payloads: PayloadPool,
        lifecycle: Lifecycle,
        listener: ListenerSlot,
        socket_path: PathBuf,
    ) -> std::io::Result<Self> {
        let Waits {
            audio: parker,
            control: control_wait,
        } = waits;
        // The response ring's writer is the audio lane's alone: it lives in the
        // audio lane's state, which its guard owns.
        let orphans = (
            Orphaned::new(&channels, &lifecycle, &listener),
            Orphaned::of_audio(
                &channels,
                &lifecycle,
                &listener,
                AudioState::new(lifecycle.timeouts().drain, responses),
            ),
        );
        let (stream_tx, stream_rx) = mpsc::channel();
        let control = executor.spawn_dedicated(
            "plugin-control",
            None,
            control_lane(
                ControlLaneParts {
                    channels: channels.clone(),
                    wait: control_wait,
                    lifecycle: lifecycle.clone(),
                    listener: Arc::clone(&listener),
                    stream: stream_rx,
                },
                orphans.0,
            ),
        )?;
        // Every block passes through this lane (callback, unpark, bridge,
        // server, bridge, next callback), and the OS elevates only the audio
        // callback's own thread, so a default-priority bridge delays the
        // reply. A refusal is an outcome the executor logs, not an error.
        // A refused start drops `control`, which cancels it.
        let audio = executor.spawn_dedicated(
            "plugin-bridge",
            Some(PriorityRequest::Highest),
            audio_lane(
                AudioLaneParts {
                    channels: channels.clone(),
                    parker,
                    payloads,
                    lifecycle: lifecycle.clone(),
                    listener,
                    socket_path,
                    control_stream: stream_tx,
                    executor: executor.downgrade(),
                    ready_within: READY_WITHIN,
                },
                orphans.1,
            ),
        )?;
        Ok(Self {
            channels,
            lifecycle,
            lanes: Some(Lanes { audio, control }),
        })
    }

    /// Asks the lanes to stop, without waiting for them: from here on a failure
    /// they read (the server's end, say) is not declared a death. The control
    /// lane says `Shutdown` to the server on its way out.
    pub fn request_stop(&self) {
        self.lifecycle.request_shutdown();
        self.channels.wake();
    }

    /// Asks the lanes to stop and waits for both to end. Idempotent.
    ///
    /// Each lane ends on the stop flag at its next look: after the call it is
    /// in (a block's reply, a request's answer; each bound by its own deadline
    /// in the bridge's timeouts) and, when idle, at once. A lane the executor
    /// dropped has ended already.
    ///
    /// # Panics
    ///
    /// On a thread that drives an executor ([`join`]): a bridge is dropped
    /// from the control side, never from a task.
    pub fn shutdown(&mut self) {
        self.lifecycle.request_shutdown();
        self.channels.wake();
        if let Some(lanes) = self.lanes.take() {
            join(lanes.audio);
            join(lanes.control);
        }
    }
}

impl Drop for IpcThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Whether a lane goes on: the bridge was not asked to stop, and the server is
/// not declared gone (by this bridge, the process guard or the host).
fn live(lifecycle: &Lifecycle) -> bool {
    lifecycle.is_running() && !lifecycle.is_crashed()
}

/// What the audio lane's task owns.
struct AudioLaneParts {
    channels: Channels,
    parker: Parker,
    payloads: PayloadPool,
    lifecycle: Lifecycle,
    listener: ListenerSlot,
    socket_path: PathBuf,
    /// Where the control lane's connection goes once it is made.
    control_stream: mpsc::Sender<ControlStream>,
    /// Reaches the executor's liveness, and nothing else of it: the lane ends
    /// when the last clone of the executor drops, busy or idle.
    executor: WeakExecutor,
    /// How long each connection has to say `Ready`.
    ready_within: Duration,
}

/// What the control lane's task owns.
struct ControlLaneParts {
    channels: Channels,
    wait: ControlWait,
    lifecycle: Lifecycle,
    listener: ListenerSlot,
    /// The control connection, from the audio lane.
    stream: mpsc::Receiver<ControlStream>,
}

/// Declares the bridge dead when a lane's task is dropped before it ended: the
/// executor shut down under it, or never ran it. A lane that ended itself
/// disarms it. The task's future holds one from its creation.
///
/// The audio lane's guard owns the lane's state (and, once connected, its
/// connection), so a dropped audio lane answers the queued blocks and the block
/// in flight with errors exactly as a crash does.
struct Orphaned {
    channels: Channels,
    lifecycle: Lifecycle,
    listener: ListenerSlot,
    ended: bool,
    audio: Held,
}

/// What the audio lane's guard holds.
enum Held {
    /// The control lane's guard holds nothing.
    Nothing,
    /// The audio lane before it has connected.
    State(AudioState),
    /// The audio lane connected.
    Lane(Lane<AudioState>),
}

impl Orphaned {
    fn new(channels: &Channels, lifecycle: &Lifecycle, listener: &ListenerSlot) -> Self {
        Self {
            channels: channels.clone(),
            lifecycle: lifecycle.clone(),
            listener: Arc::clone(listener),
            ended: false,
            audio: Held::Nothing,
        }
    }

    /// The audio lane's guard, holding its `state`.
    fn of_audio(
        channels: &Channels,
        lifecycle: &Lifecycle,
        listener: &ListenerSlot,
        state: AudioState,
    ) -> Self {
        let mut guard = Self::new(channels, lifecycle, listener);
        guard.audio = Held::State(state);
        guard
    }

    /// The audio lane's state, before and after it has connected.
    fn audio_state(&mut self) -> Option<&mut AudioState> {
        match &mut self.audio {
            Held::Nothing => None,
            Held::State(state) => Some(state),
            Held::Lane(lane) => Some(&mut lane.state),
        }
    }

    /// The audio lane has connected on `stream`.
    fn connected(&mut self, stream: ControlStream) {
        if let Held::State(state) = std::mem::replace(&mut self.audio, Held::Nothing) {
            self.audio = Held::Lane(Lane { stream, state });
        }
    }

    /// The connected audio lane.
    fn lane(&mut self) -> Option<&mut Lane<AudioState>> {
        match &mut self.audio {
            Held::Lane(lane) => Some(lane),
            _ => None,
        }
    }
}

impl Drop for Orphaned {
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        crash(&self.lifecycle, &self.listener, PluginFailure::StreamClosed);
        if self.lifecycle.is_crashed() {
            let channels = self.channels.clone();
            if let Some(state) = self.audio_state() {
                state.respond(&channels, AudioResponse::Error { seq: None });
                drain_with_errors(&channels, state);
            }
        }
        drop_controls(&self.channels);
    }
}

async fn audio_lane(parts: AudioLaneParts, mut orphaned: Orphaned) {
    let AudioLaneParts {
        channels,
        mut parker,
        payloads,
        lifecycle,
        listener,
        socket_path,
        control_stream,
        executor,
        ready_within,
    } = parts;
    // A bridge that never connects is as dead as one whose socket drops later,
    // and the audio thread tells them apart only through `is_crashed`. Every
    // exit below must mark the crash: returning silently would leave the flag
    // false forever, so the batcher's crash check could never fire, and the
    // output would be silent only because the slab sequence happened never to
    // match.
    let (audio, control) = match connect_lanes(&socket_path, ready_within) {
        Ok(streams) => streams,
        Err(failure) => {
            crash(&lifecycle, &listener, failure);
            // A block queued before the crash was marked is never sent.
            if let Some(state) = orphaned.audio_state() {
                drain_with_errors(&channels, state);
            }
            drop_controls(&channels);
            orphaned.ended = true;
            return;
        }
    };
    if control_stream.send(control).is_err() {
        // The control lane's task is gone: the executor shut down under it.
        crash(
            &lifecycle,
            &listener,
            PluginFailure::Connect {
                source: Arc::new(IpcError::Io(std::io::Error::other(
                    "the control lane ended before it was handed its connection",
                ))),
            },
        );
        if let Some(state) = orphaned.audio_state() {
            drain_with_errors(&channels, state);
        }
        drop_controls(&channels);
        orphaned.ended = true;
        return;
    }
    orphaned.connected(audio);
    let mut stopped_by_executor = false;
    if let Some(lane) = orphaned.lane() {
        stopped_by_executor = !audio_loop(
            &channels,
            &mut parker,
            &payloads,
            &lifecycle,
            &listener,
            &executor,
            lane,
        )
        .await;
    }
    // The control lane ends on the same condition, rung awake by whatever ended
    // this one.
    channels.wake();
    // A lane the executor's end stopped has not ended itself: the guard declares
    // the bridge dead and answers what is queued, as it does for a dropped lane.
    orphaned.ended = !stopped_by_executor;
}

async fn control_lane(parts: ControlLaneParts, mut orphaned: Orphaned) {
    let ControlLaneParts {
        channels,
        wait,
        lifecycle,
        listener,
        stream,
    } = parts;
    let stream = loop {
        match stream.recv_timeout(LANE_SLICE) {
            Ok(stream) => break Some(stream),
            // The audio lane ended without connecting, and told the bridge.
            Err(RecvTimeoutError::Disconnected) => break None,
            Err(RecvTimeoutError::Timeout) if !lifecycle.is_running() => break None,
            Err(RecvTimeoutError::Timeout) => yield_now().await,
        }
    };
    match stream {
        Some(stream) => {
            let lane = Lane {
                stream,
                state: ControlState::new(lifecycle.timeouts().drain),
            };
            control_loop(&channels, &wait, &lifecycle, &listener, lane).await;
        }
        None => drop_controls(&channels),
    }
    orphaned.ended = true;
}

/// The two connections, audio first and control second as the server accepts
/// them, each past its `Ready`.
///
/// Each `Ready` is consumed and its version checked rather than discarded.
/// `launch.rs` already gated the same server on connection 1, so a mismatch
/// here is not reachable today — but this is a wire boundary, the check is one
/// comparison off the audio path, and a silently-ignored version field is how
/// a skew becomes a mis-parse instead of an error.
fn connect_lanes(
    socket_path: &Path,
    ready_within: Duration,
) -> Result<(ControlStream, ControlStream), PluginFailure> {
    let connect = || {
        let stream = ipc::connect(socket_path).map_err(|e| PluginFailure::Connect {
            source: Arc::new(e),
        })?;
        Ok(stream)
    };
    let mut audio = connect()?;
    expect_ready(
        ipc::recv_within::<AudioReply>(&mut audio, ready_within).map(|r| match r {
            AudioReply::Ready { protocol_version } => Ok(protocol_version),
            other => Err(format!("{other:?}")),
        }),
    )?;
    let mut control = connect()?;
    expect_ready(
        ipc::recv_within::<ControlFrame>(&mut control, ready_within).map(|f| match f {
            ControlFrame::Ready { protocol_version } => Ok(protocol_version),
            other => Err(format!("{other:?}")),
        }),
    )?;
    Ok((audio, control))
}

/// The audio lane's loop: park until a block-lane command is
/// queued, send it, and answer it into the response ring.
///
/// **Idle, the thread parks on its [`Parker`]**, unparked by a push, by
/// `shutdown` and by a crash anyone declares. It does not read the control
/// connection. There is no timeout in the park.
///
/// **A block's turn makes no call into the executor**: it pops, sends, waits for
/// the reply and answers into the ring, and loops. It reads the executor's
/// liveness once per turn (one atomic load of the weak handle's count: no lock,
/// no allocation, no `Waker`) and returns `false` when the last clone of the
/// executor has dropped, so a lane that is fed without a break stops with it. The
/// park's stop condition reads the same load, and the count falls before the
/// executor unparks its threads, so an unpark the lane's other parks took cannot
/// hide the stop: every park is followed by a look at the count. The lane yields
/// to the executor nowhere, as a call into it (a wake, a queue push, its timers'
/// lock) would put the Highest-priority lane behind the executor's other threads.
/// Returns `true` when the lane ended on its own account.
async fn audio_loop(
    channels: &Channels,
    parker: &mut Parker,
    payloads: &PayloadPool,
    lifecycle: &Lifecycle,
    listener: &ListenerSlot,
    executor: &WeakExecutor,
    lane: &mut Lane<AudioState>,
) -> bool {
    let mut failed = false;
    while live(lifecycle) {
        // The executor's end, one atomic load a turn: a lane that is fed without
        // a break never idles, so it cannot wait to be stopped.
        if !executor.is_alive() {
            return false;
        }
        // Read before the pop: a block pushed after this read is not counted,
        // and one counted was queued, so an empty queue means it was dealt with.
        let pushed = channels.pushed();
        let Some(cmd) = channels.pop_block() else {
            // Idle: a setting asked for meanwhile is applied now.
            if let Some(setting) = channels.take_request(None) {
                apply_setting(channels, lifecycle, setting);
                continue;
            }
            // Nothing queued: every block pushed before the look above has been
            // dealt with, which a control command waiting for the blocks before
            // it may rely on.
            channels.note_handled(pushed);
            channels.progress().notify();
            // Park: `push_block` unparks the thread the instant a block
            // arrives, so the socket round-trip starts immediately. The
            // unpark cannot be lost and `shutdown` unparks too, so there is
            // no timeout; the executor's end is in the stop condition.
            channels.wait_for_block(parker, || !live(lifecycle) || !executor.is_alive());
            continue;
        };
        // The settings asked for before a block go before it. A `Reset` takes
        // none: it clears the plugin's signal history, which no setting changes
        // the need for, and the next block applies them.
        if let AudioCommand::Process(payload) = &cmd {
            while let Some(setting) = channels.take_request(Some(payload.seq)) {
                apply_setting(channels, lifecycle, setting);
            }
        }
        let result = handle_audio(cmd, lane, channels, payloads, lifecycle);
        channels.turn_done();

        if let Err(e) = result {
            // The error is classified here, at the only place it exists.
            // `IpcError` is not `Clone`, and the crash notification outlives
            // this frame, so the failure holds it behind an `Arc`.
            crash(lifecycle, listener, PluginFailure::from_stream(e));
            failed = true;
            break;
        }
    }
    if failed || lifecycle.is_crashed() {
        // Connection-level: the server is gone, so this ends every in-flight
        // and queued block, not just one. `None` matches whichever request the
        // audio thread is waiting on. The control lane saw the end when it was
        // not this lane that did: it rang this thread, and this thread answers.
        lane.state
            .respond(channels, AudioResponse::Error { seq: None });
        drain_with_errors(channels, &mut lane.state);
    }
    notify_external_crash(lifecycle, listener);
    true
}

/// The control lane's loop: sleep until a request is queued or the server
/// speaks, and run what woke it.
///
/// **Idle, the thread waits on the control connection and its bell together**
/// ([`ControlWait`]), so what the server says unprompted (a request to be
/// processed again, a latency or parameter change, its own death) is read as it
/// arrives, and a request is sent as it is queued. While a request waits for
/// its reply, `handle_control` reads the same lane and the unsolicited events
/// ride in on that read; either way they go to the listener after the request.
/// The wait ends at once for a request, the server's end, a crash and `shutdown`,
/// and after [`LANE_SLICE`] for the executor to look at the task.
async fn control_loop(
    channels: &Channels,
    wait: &ControlWait,
    lifecycle: &Lifecycle,
    listener: &ListenerSlot,
    mut lane: Lane<ControlState>,
) {
    // Unsolicited events read while a request waited for its reply. One
    // buffer for the connection, handed to the listener after each turn.
    let mut inbox = Unsolicited::with_lifecycle(lifecycle.clone());
    match ipc::Watch::of(&lane.stream) {
        Ok(watch) => channels.set_probe(Arc::new(watch)),
        Err(e) => tracing::warn!("the bridge cannot watch the control connection: {e}"),
    }
    // The control command taken off its queue and not yet sent, with the end of
    // its wait for the blocks before it.
    let mut held: Option<(Sequenced<ControlCommand>, tutti_platform::Instant)> = None;
    while live(lifecycle) {
        yield_now().await;
        if held.is_none() {
            held = channels.pop_control().map(|cmd| {
                let wait = crate::Wake::deadline_in(lifecycle.timeouts().settle_wait());
                (cmd, wait)
            });
        }
        // A setting first: the audio lane's blocks wait for it.
        let result = if let Some((setting, applied)) = channels.take_setting() {
            // A command's turn reads the news that comes during it too.
            channels.set_news();
            handle_setting(setting, applied, &mut lane, &mut inbox, lifecycle)
        } else if let Some((stamped, deadline)) =
            held.take_if(|(c, _)| channels.handled() >= c.after)
        {
            // **A command is sent after the blocks submitted before it**, as
            // the one thread used to do: the audio lane has sent or skipped the
            // block it follows.
            let _ = deadline;
            channels.set_news();
            handle_control(stamped.cmd, &mut lane, &mut inbox, lifecycle)
        } else if let Some((stamped, deadline)) = &held {
            // Not yet: wait for the audio lane (or a setting it is holding the
            // block for, the end of the bridge, the command's own bound). A
            // command the blocks never let through is failed, never sent out of
            // order: its caller is told, by the reply's drop, that none comes.
            let after = stamped.after;
            // At most a slice, so the executor can look at the task.
            let slice = crate::Wake::deadline_in(LANE_SLICE).min(*deadline);
            channels.progress().wait_until(slice, || {
                channels.handled() >= after || channels.setting_waiting() || !live(lifecycle)
            });
            if crate::Wake::deadline_in(Duration::ZERO) >= *deadline && channels.handled() < after {
                tracing::warn!(
                    "a control command was not sent: the blocks before it were not dealt with in time"
                );
                // A command with no reply to tell its caller is counted.
                if let Some((stamped, _)) = held.take()
                    && stamped.cmd.is_fire_and_forget()
                {
                    channels.note_command_refused();
                }
            }
            continue;
        } else {
            match wait.wait(&lane.stream, Some(LANE_SLICE)) {
                Ok(woken) if woken.stream => {
                    channels.set_news();
                    read_news(&mut lane, &mut inbox)
                }
                // A ring: look at the queue and the stop condition again.
                Ok(_) => continue,
                Err(e) => Err(IpcError::Io(e)),
            }
        };
        drain_unsolicited(&mut inbox, channels, listener);
        channels.clear_news();
        channels.turn_done();

        if let Err(e) = result {
            // Latching it rings the audio lane's thread, which answers the
            // blocks: this thread pushes no response.
            crash(lifecycle, listener, PluginFailure::from_stream(e));
            break;
        }
    }
    notify_external_crash(lifecycle, listener);
    say_shutdown(lifecycle, &mut lane);
    channels.clear_probe();
    drop_controls(channels);
}

/// Tells the server to shut down when the bridge was asked to stop and the
/// server is still there to hear it: its plugin is then destroyed by the server
/// itself, and the host's wait for the process's end (the guard's) is the
/// grace. Best effort: a server that cannot be written to is gone or hung, and
/// the guard kills it at the end of the grace.
fn say_shutdown(lifecycle: &Lifecycle, lane: &mut Lane<ControlState>) {
    if !lifecycle.is_running() && lifecycle.crash_cause().is_none() {
        let _ = tell(&mut lane.stream, &mut lane.state, ControlRequest::Shutdown);
    }
}

/// Reads a connection's `Ready` and checks its version.
///
/// The reason is spelled out per case rather than as one string: a version skew
/// and a peer that never said `Ready` are different problems for whoever reads
/// the message, and the first is actionable.
fn expect_ready(read: IpcResult<Result<u32, String>>) -> Result<(), PluginFailure> {
    let source = match read {
        Ok(Ok(version)) => match crate::protocol::check_protocol_version(version) {
            Ok(()) => return Ok(()),
            Err(skew) => skew,
        },
        Ok(Err(got)) => IpcError::UnexpectedMessage {
            expected: "Ready",
            got: got.into(),
        },
        Err(e) => e,
    };
    Err(PluginFailure::Handshake {
        source: Arc::new(source),
    })
}

/// Declare the bridge dead: latch the cause, then tell whoever is listening.
///
/// One helper rather than two calls at each site, because the latch and the
/// notification must not drift apart — a crash that fired an event without
/// latching would be invisible to anything that asked later, and one that
/// latched without firing is the polling design this replaces. The two lanes'
/// threads can each see the end: the first to get here tells the listener, once.
///
/// The listener may legitimately be absent. `PluginBridge::new` spawns this
/// thread and `set_listener` runs afterwards, so the connect- and
/// handshake-failure sites usually have no subscriber yet. That is exactly why
/// the latch comes first and is authoritative: `Lifecycle::crash_cause` answers
/// for a crash nobody heard.
///
/// **A bridge that was asked to stop declares nothing.** Its owner ends the
/// server on the way out (the guard kills it), and the EOF that follows is the
/// teardown's, not a death: the shared watch must not read it as one, because
/// an offline fork's wait holds that watch beyond the teardown. The stop is
/// requested before the kill, so every failure the kill causes is read after
/// the request.
fn crash(lifecycle: &Lifecycle, listener: &ListenerSlot, cause: PluginFailure) {
    // Pairs with the fence in `Lifecycle::request_shutdown`: the failure was
    // read after the kill, the kill after that fence, so the load sees the stop.
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    if !lifecycle.is_running() {
        return;
    }
    lifecycle.mark_crashed(cause.clone());
    notify_crash(lifecycle, listener, cause);
}

/// Tells the listener of the crash, once for the bridge: the first of its two
/// threads to get here does.
fn notify_crash(lifecycle: &Lifecycle, listener: &ListenerSlot, cause: PluginFailure) {
    if !lifecycle.announce_crash() {
        return;
    }
    let cb = listener.lock().clone();
    if let Some(f) = cb.as_ref() {
        f(IpcEvent::Crashed { cause });
    }
}

/// A lane's thread ends: when the server was declared gone by another route
/// (the process guard's exit watch, the host), no failure of this bridge's
/// streams told the listener, so it hears it now, with the latched cause.
fn notify_external_crash(lifecycle: &Lifecycle, listener: &ListenerSlot) {
    if let Some(cause) = lifecycle.crash_cause() {
        notify_crash(lifecycle, listener, cause);
    }
}

fn drain_unsolicited(inbox: &mut Unsolicited, channels: &Channels, listener: &ListenerSlot) {
    channels.note_dropped_param_events(inbox.take_dropped());
    let cb = listener.lock().clone();
    for ev in inbox.drain() {
        if let Some(f) = cb.as_ref() {
            f(ev);
        }
    }
}

/// Answer every queued block-lane command with an error. A queued chunk is
/// never sent, so its MIDI is counted dropped; nothing can splice past it, as
/// the server is gone.
fn drain_with_errors(channels: &Channels, state: &mut AudioState) {
    while let Some(cmd) = channels.pop_block() {
        if let AudioCommand::Process(payload) = &cmd {
            channels.note_dropped_midi(payload.midi_events.len() as u64);
        }
        state.respond(channels, AudioResponse::Error { seq: None });
    }
}

/// Drop every queued control-lane command: a request's caller, holding the
/// other end of its reply, is told at once that none is coming.
fn drop_controls(channels: &Channels) {
    while let Some(Sequenced { cmd, .. }) = channels.pop_control() {
        drop(cmd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioRequest;
    use crate::client::audio::messages::{ProcessPayload, UNSOLICITED_PARAMS};
    use interprocess::local_socket::{ListenerOptions, Stream, traits::Listener as _};
    use std::sync::Arc;
    use tutti_fixtures::midi;
    use tutti_platform::join_within;
    use tutti_plugin_types::ResyncKind;
    use tutti_ump::MidiChannel;
    use tutti_units::Normalized;
    use tutti_units::ParamId;
    use tutti_units::SampleRate;

    /// **A crash's drain counts the queued chunks' MIDI.**
    #[test]
    fn a_crash_drain_counts_the_queued_midi() {
        use tutti_plugin_types::WireMidi;

        let channels = Channels::new(SampleRate::SR_48K, Arc::default())
            .expect("the channels")
            .0;
        let mut state = AudioState::new(
            tutti_plugin_types::Timeouts::default().drain,
            response_ring().0,
        );
        for n in [3, 0, 2] {
            let mut payload = Box::new(ProcessPayload::empty(SampleRate::SR_48K));
            for _ in 0..n {
                payload.midi_events.push(WireMidi {
                    offset: 0,
                    ump: midi::note_on(MidiChannel::FIRST, 60, 1),
                });
            }
            assert!(channels.push_block(AudioCommand::Process(payload)));
        }
        drain_with_errors(&channels, &mut state);
        assert!(channels.pop_block().is_none());
        assert_eq!(channels.dropped_midi_cell().get(), 5);
    }

    /// A hang guard for a wait on an event: far past anything the OS takes.
    const HANG_GUARD: Duration = Duration::from_secs(60);

    /// A bridge's lanes started on an executor, against a peer that has said
    /// `Ready` on both connections and answers nothing.
    struct Rig {
        thread: IpcThread,
        channels: Channels,
        lifecycle: Lifecycle,
        /// Cloned into each lane: its count says whether the lanes' futures are alive.
        listener: ListenerSlot,
        /// Held so the connections stay open: a peer that drops them is a crash.
        audio_peer: Stream,
        control_peer: Stream,
        replies: crate::Replies,
    }

    fn timeouts() -> tutti_plugin_types::Timeouts {
        tutti_plugin_types::Timeouts::default()
    }

    fn payloads_of(channels: &Channels) -> PayloadPool {
        PayloadPool::new(
            SampleRate::SR_48K,
            channels.refusal_writer(),
            channels.leak_writer(),
        )
    }

    fn silent_listener() -> ListenerSlot {
        Arc::new(parking_lot::Mutex::new(None))
    }

    fn rig(executor: &Executor) -> Rig {
        let (channels, waits) =
            Channels::new(SampleRate::SR_48K, Arc::default()).expect("the channels");
        let lifecycle = Lifecycle::new(Arc::new(crate::ServerWatch::new()), timeouts());
        let socket = crate::unique_socket_path().expect("the OS has a random source");
        let _ = std::fs::remove_file(&socket);
        let listener = ListenerOptions::new()
            .name(ipc::socket_name(&socket).expect("socket name"))
            .create_sync()
            .expect("listen");
        let (responses, replies, _lost) = response_ring();
        let lanes_listener = silent_listener();
        let thread = IpcThread::spawn(
            executor,
            channels.clone(),
            responses,
            waits,
            payloads_of(&channels),
            lifecycle.clone(),
            Arc::clone(&lanes_listener),
            socket,
        )
        .expect("the lanes start");
        let audio_peer = listener.accept().expect("the audio connection");
        crate::write_frame(
            &mut &audio_peer,
            &AudioReply::Ready {
                protocol_version: crate::PROTOCOL_VERSION,
            },
        )
        .expect("ready");
        let control_peer = listener.accept().expect("the control connection");
        crate::write_frame(
            &mut &control_peer,
            &ControlFrame::Ready {
                protocol_version: crate::PROTOCOL_VERSION,
            },
        )
        .expect("ready");
        Rig {
            thread,
            channels,
            lifecycle,
            listener: lanes_listener,
            audio_peer,
            control_peer,
            replies,
        }
    }

    /// Whether `lane` ends within the hang guard: the wait is on its task.
    fn ends(lane: Task<()>) -> bool {
        join_within(lane, HANG_GUARD).is_ok()
    }

    fn payload_with_midi(n: usize) -> AudioCommand {
        use tutti_plugin_types::WireMidi;

        let mut payload = Box::new(ProcessPayload::empty(SampleRate::SR_48K));
        for _ in 0..n {
            payload.midi_events.push(WireMidi {
                offset: 0,
                ump: midi::note_on(MidiChannel::FIRST, 60, 1),
            });
        }
        AudioCommand::Process(payload)
    }

    /// **A bridge that never connects counts the MIDI queued before it
    /// failed.** The chunk is queued before the lanes start, so it is there when
    /// the connect fails, deterministically.
    #[test]
    fn a_failed_connect_counts_the_queued_midi() {
        let (channels, waits) =
            Channels::new(SampleRate::SR_48K, Arc::default()).expect("the channels");
        let lifecycle = Lifecycle::new(Arc::new(crate::ServerWatch::new()), timeouts());
        assert!(channels.push_block(payload_with_midi(4)));
        let socket =
            std::env::temp_dir().join(format!("tutti-no-such-bridge-{}.sock", std::process::id()));
        let mut thread = IpcThread::spawn(
            tutti_fixtures::executor(),
            channels.clone(),
            response_ring().0,
            waits,
            payloads_of(&channels),
            lifecycle.clone(),
            silent_listener(),
            socket,
        )
        .expect("the lanes start");
        let lanes = thread.lanes.take().expect("the lanes run");
        assert!(ends(lanes.audio), "the audio lane ended");
        assert!(lifecycle.is_crashed());
        assert!(channels.pop_block().is_none());
        assert_eq!(channels.dropped_midi_cell().get(), 4);
    }

    /// **A crash drains the queued blocks**: the server's end, read while a block
    /// is in flight, answers the blocks queued behind it with errors and counts
    /// their MIDI dropped, and nothing stays in the queue.
    #[test]
    fn a_crash_drains_the_queued_blocks() {
        let mut rig = rig(tutti_fixtures::executor());
        assert!(rig.channels.push_block(payload_with_midi(0)));
        // The lane has sent the first block, so it is waiting on it: the next two
        // queue behind it.
        let _sent: AudioRequest = crate::read_frame(&mut &rig.audio_peer).expect("the block");
        assert!(rig.channels.push_block(payload_with_midi(3)));
        assert!(rig.channels.push_block(payload_with_midi(2)));
        let lanes = rig.thread.lanes.take().expect("the lanes run");

        // The server's end: both connections close.
        let channels = rig.channels.clone();
        drop(rig.audio_peer);
        drop(rig.control_peer);

        assert!(ends(lanes.audio), "the audio lane ended");
        assert!(rig.lifecycle.is_crashed());
        assert!(channels.pop_block().is_none(), "the queue is drained");
        assert_eq!(channels.dropped_midi_cell().get(), 5);
        let mut answered = 0;
        while rig.replies.pop().is_some() {
            answered += 1;
        }
        assert!(answered >= 3, "each of the three blocks was answered");
        assert!(ends(lanes.control), "the control lane ended");
    }

    /// **Shutdown joins both lanes**: when it returns neither lane's future is
    /// alive (each held a clone of the listener), so neither is still running with
    /// the bridge's queues.
    #[test]
    fn shutdown_joins_both_lanes() {
        let mut rig = rig(tutti_fixtures::executor());
        assert!(Arc::strong_count(&rig.listener) > 1, "the lanes run");

        rig.thread.shutdown();

        assert_eq!(
            Arc::strong_count(&rig.listener),
            1,
            "a lane is still running after the join"
        );
    }

    /// **A lane never polled resolves its task and declares the bridge dead**:
    /// the executor that took the tasks shut down before it ran them.
    #[test]
    fn a_lane_dropped_unpolled_resolves_its_task_and_declares_the_bridge_dead() {
        let (channels, waits) =
            Channels::new(SampleRate::SR_48K, Arc::default()).expect("the channels");
        let lifecycle = Lifecycle::new(Arc::new(crate::ServerWatch::new()), timeouts());
        let listener = silent_listener();
        let executor = Executor::new();
        let (stream_tx, stream_rx) = mpsc::channel();
        let audio = executor.spawn(audio_lane(
            AudioLaneParts {
                channels: channels.clone(),
                parker: waits.audio,
                payloads: payloads_of(&channels),
                lifecycle: lifecycle.clone(),
                listener: Arc::clone(&listener),
                socket_path: PathBuf::new(),
                control_stream: stream_tx,
                executor: executor.downgrade(),
                ready_within: HANG_GUARD,
            },
            Orphaned::of_audio(
                &channels,
                &lifecycle,
                &listener,
                AudioState::new(timeouts().drain, response_ring().0),
            ),
        ));
        let control = executor.spawn(control_lane(
            ControlLaneParts {
                channels: channels.clone(),
                wait: waits.control,
                lifecycle: lifecycle.clone(),
                listener: Arc::clone(&listener),
                stream: stream_rx,
            },
            Orphaned::new(&channels, &lifecycle, &listener),
        ));
        assert!(!audio.is_finished() && !control.is_finished());
        assert!(!lifecycle.is_crashed());

        drop(executor);

        assert!(
            matches!(join_within(audio, HANG_GUARD), Ok(None)),
            "the audio lane's task"
        );
        assert!(
            lifecycle.is_crashed(),
            "a bridge whose lane is gone is dead"
        );
        assert!(
            matches!(join_within(control, HANG_GUARD), Ok(None)),
            "the control lane's task"
        );
    }

    /// **A live bridge does not keep the executor alive**: with every strong
    /// handle dropped while the lanes run, the executor has shut down, and
    /// dropping it ended both lanes (it drops an idle lane at its next slice).
    #[test]
    fn a_live_bridge_does_not_keep_the_executor_alive() {
        let executor = Executor::new();
        let weak = executor.downgrade();
        let mut rig = rig(&executor);

        drop(executor);

        assert!(weak.upgrade().is_none(), "the bridge kept the executor");
        let lanes = rig.thread.lanes.as_ref().expect("the lanes run");
        assert!(lanes.audio.is_finished(), "the audio lane outlived it");
        assert!(lanes.control.is_finished(), "the control lane outlived it");
        assert!(rig.lifecycle.is_crashed(), "the bridge's lanes are gone");
        // Joining lanes that have ended is immediate.
        rig.thread.shutdown();
    }

    /// **A dropped audio lane answers the queued blocks as a crash does**: the
    /// executor that took the task shut down with a block queued, and the block
    /// gets its error and leaves the queue.
    #[test]
    fn a_dropped_audio_lane_answers_the_queued_blocks() {
        let (channels, waits) =
            Channels::new(SampleRate::SR_48K, Arc::default()).expect("the channels");
        let lifecycle = Lifecycle::new(Arc::new(crate::ServerWatch::new()), timeouts());
        let listener = silent_listener();
        let (writer, mut replies, _lost) = response_ring();
        let (stream_tx, _stream_rx) = mpsc::channel();
        let lane = audio_lane(
            AudioLaneParts {
                channels: channels.clone(),
                parker: waits.audio,
                payloads: payloads_of(&channels),
                lifecycle: lifecycle.clone(),
                listener: Arc::clone(&listener),
                socket_path: PathBuf::new(),
                control_stream: stream_tx,
                executor: tutti_fixtures::executor().downgrade(),
                ready_within: HANG_GUARD,
            },
            Orphaned::of_audio(
                &channels,
                &lifecycle,
                &listener,
                AudioState::new(timeouts().drain, writer),
            ),
        );
        assert!(channels.push_block(payload_with_midi(2)));

        drop(lane);

        assert!(channels.pop_block().is_none(), "the queue is drained");
        assert_eq!(channels.dropped_midi_cell().get(), 2);
        let mut errors = 0;
        while let Some(response) = replies.pop() {
            assert!(matches!(response, AudioResponse::Error { .. }));
            errors += 1;
        }
        assert!(
            errors >= 2,
            "the in flight slot and the queued block: {errors}"
        );
    }

    /// **A server that accepts and never says `Ready` is a crash at the
    /// handshake deadline**, and the lane ends: neither `shutdown` nor an
    /// executor's drop waits on a silent peer.
    #[test]
    fn a_server_that_never_says_ready_crashes_the_bridge_at_the_deadline() {
        let (channels, waits) =
            Channels::new(SampleRate::SR_48K, Arc::default()).expect("the channels");
        let lifecycle = Lifecycle::new(Arc::new(crate::ServerWatch::new()), timeouts());
        let listener = silent_listener();
        let socket = crate::unique_socket_path().expect("the OS has a random source");
        let _ = std::fs::remove_file(&socket);
        let served = ListenerOptions::new()
            .name(ipc::socket_name(&socket).expect("socket name"))
            .create_sync()
            .expect("listen");
        let (stream_tx, _stream_rx) = mpsc::channel();
        let lane = tutti_fixtures::executor()
            .spawn_dedicated(
                "test-lane",
                None,
                audio_lane(
                    AudioLaneParts {
                        channels: channels.clone(),
                        parker: waits.audio,
                        payloads: payloads_of(&channels),
                        lifecycle: lifecycle.clone(),
                        listener: Arc::clone(&listener),
                        socket_path: socket,
                        control_stream: stream_tx,
                        executor: tutti_fixtures::executor().downgrade(),
                        ready_within: Duration::from_millis(100),
                    },
                    Orphaned::of_audio(
                        &channels,
                        &lifecycle,
                        &listener,
                        AudioState::new(timeouts().drain, response_ring().0),
                    ),
                ),
            )
            .expect("the lane starts");
        let _silent = served.accept().expect("the audio connection");
        assert!(
            ends(lane),
            "the audio lane waits on a peer that never speaks"
        );
        assert!(lifecycle.is_crashed());
        assert!(matches!(
            lifecycle.crash_cause(),
            Some(PluginFailure::Handshake { .. })
        ));
    }

    /// **A late reply to a block the lane gave up on is skipped by its `seq`**:
    /// the next block's wait reads it, forwards it as the abandoned block's, and
    /// goes on to the next block's own answer.
    #[test]
    fn a_late_reply_is_skipped_by_seq_on_the_next_block() {
        let mut rig = rig(tutti_fixtures::executor());
        let processed = |seq| AudioReply::AudioProcessed {
            latency_us: 0,
            seq,
            midi_out: Default::default(),
            status: Default::default(),
            refused: false,
        };
        let block = |seq| {
            let AudioCommand::Process(mut payload) = payload_with_midi(0) else {
                unreachable!()
            };
            payload.seq = seq;
            AudioCommand::Process(payload)
        };
        let mut heard = Vec::new();
        fn hear(rig: &mut Rig, heard: &mut Vec<AudioResponse>, count: usize) {
            tutti_fixtures::wait_until("the responses", HANG_GUARD, || {
                while let Some(response) = rig.replies.pop() {
                    heard.push(response);
                }
                heard.len() >= count
            });
        }

        // Block 1 is sent and unanswered: its budget (a few block periods) runs
        // out, and the lane answers the block with an error and goes on.
        assert!(rig.channels.push_block(block(1)));
        let _first: AudioRequest = crate::read_frame(&mut &rig.audio_peer).expect("block 1");
        hear(&mut rig, &mut heard, 1);
        assert!(matches!(heard[0], AudioResponse::Error { seq: Some(1) }));

        // The reply arrives late, and block 2 is sent. Its wait has no deadline
        // here, so only the skip by `seq` decides what it takes as its answer.
        rig.channels.set_reply_wait(Some(HANG_GUARD));
        crate::write_frame(&mut &rig.audio_peer, &processed(1)).expect("the late reply");
        assert!(rig.channels.push_block(block(2)));
        let _second: AudioRequest = crate::read_frame(&mut &rig.audio_peer).expect("block 2");
        crate::write_frame(&mut &rig.audio_peer, &processed(2)).expect("block 2's reply");
        hear(&mut rig, &mut heard, 3);

        assert!(matches!(
            heard[1],
            AudioResponse::AudioProcessed { seq: 1, .. }
        ));
        assert!(matches!(
            heard[2],
            AudioResponse::AudioProcessed { seq: 2, .. }
        ));
        assert!(!rig.lifecycle.is_crashed());
        rig.thread.shutdown();
    }

    /// The audio lane alone on `executor` (the control lane is not started, so
    /// nothing else's end declares the bridge dead and stops the lane through
    /// that), connected to a peer that has said `Ready`.
    struct Lone {
        channels: Channels,
        replies: crate::Replies,
        audio_peer: Arc<Stream>,
        /// Held so the connections stay open.
        _control_peer: Stream,
        _stream_rx: mpsc::Receiver<ControlStream>,
        lane: Task<()>,
    }

    fn lone_audio_lane(executor: &Executor) -> Lone {
        let (channels, waits) =
            Channels::new(SampleRate::SR_48K, Arc::default()).expect("the channels");
        let lifecycle = Lifecycle::new(Arc::new(crate::ServerWatch::new()), timeouts());
        let listener = silent_listener();
        let socket = crate::unique_socket_path().expect("the OS has a random source");
        let _ = std::fs::remove_file(&socket);
        let served = ListenerOptions::new()
            .name(ipc::socket_name(&socket).expect("socket name"))
            .create_sync()
            .expect("listen");
        let (stream_tx, stream_rx) = mpsc::channel();
        let (responses, replies, _lost) = response_ring();
        let lane = executor
            .spawn_dedicated(
                "test-lane",
                None,
                audio_lane(
                    AudioLaneParts {
                        channels: channels.clone(),
                        parker: waits.audio,
                        payloads: payloads_of(&channels),
                        lifecycle: lifecycle.clone(),
                        listener: Arc::clone(&listener),
                        socket_path: socket,
                        control_stream: stream_tx,
                        executor: executor.downgrade(),
                        ready_within: HANG_GUARD,
                    },
                    Orphaned::of_audio(
                        &channels,
                        &lifecycle,
                        &listener,
                        AudioState::new(timeouts().drain, responses),
                    ),
                ),
            )
            .expect("the lane starts");
        let audio_peer = Arc::new(served.accept().expect("the audio connection"));
        crate::write_frame(
            &mut &*audio_peer,
            &AudioReply::Ready {
                protocol_version: crate::PROTOCOL_VERSION,
            },
        )
        .expect("ready");
        let control_peer = served.accept().expect("the control connection");
        crate::write_frame(
            &mut &control_peer,
            &ControlFrame::Ready {
                protocol_version: crate::PROTOCOL_VERSION,
            },
        )
        .expect("ready");
        Lone {
            channels,
            replies,
            audio_peer,
            _control_peer: control_peer,
            _stream_rx: stream_rx,
            lane,
        }
    }

    /// Drops `executor` on another thread and waits for the drop behind the hang
    /// guard: `true` when it completed.
    fn drops_within_guard(executor: Executor) -> bool {
        let (dropped_tx, dropped_rx) = mpsc::channel();
        std::thread::spawn(move || {
            drop(executor);
            let _ = dropped_tx.send(());
        });
        dropped_rx.recv_timeout(HANG_GUARD).is_ok()
    }

    /// **An idle lane stops with the executor**: nothing is fed, the lane is
    /// parked (a std park on the thread the executor's drive registered), and the
    /// executor's drop wakes it to a look at the executor's liveness.
    #[test]
    fn an_idle_lane_stops_when_the_executor_drops() {
        let executor = Executor::new();
        let lone = lone_audio_lane(&executor);
        assert!(!lone.lane.is_finished());

        assert!(
            drops_within_guard(executor),
            "the executor's drop waits on an idle lane"
        );
        assert!(lone.lane.is_finished(), "the lane outlived the executor");
        drop(lone.lane);
    }

    /// **A lane fed without a break stops with the executor**: the audio lane
    /// is answered block after block, so it rarely idles, and dropping the last
    /// executor clone still ends it (the drop joins its thread).
    #[test]
    fn a_busy_lane_stops_when_the_executor_drops() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let executor = Executor::new();
        let mut lone = lone_audio_lane(&executor);

        // The server answers every block as it comes.
        let peer = Arc::clone(&lone.audio_peer);
        let server = std::thread::spawn(move || {
            while let Ok(AudioRequest::ProcessAudio(block)) =
                crate::read_frame::<_, AudioRequest>(&mut &*peer)
            {
                let reply = AudioReply::AudioProcessed {
                    latency_us: 0,
                    seq: block.seq,
                    midi_out: Default::default(),
                    status: Default::default(),
                    refused: false,
                };
                if crate::write_frame(&mut &*peer, &reply).is_err() {
                    break;
                }
            }
        });
        let feeding = Arc::new(AtomicBool::new(true));
        let feeder = {
            let (channels, feeding) = (lone.channels.clone(), Arc::clone(&feeding));
            std::thread::spawn(move || {
                let mut seq = 0;
                while feeding.load(Ordering::Acquire) {
                    seq += 1;
                    let AudioCommand::Process(mut payload) = payload_with_midi(0) else {
                        unreachable!()
                    };
                    payload.seq = seq;
                    channels.note_submitted(seq);
                    if !channels.push_block(AudioCommand::Process(payload)) {
                        std::thread::yield_now();
                    }
                }
            })
        };
        tutti_fixtures::wait_until("the lane to answer blocks", HANG_GUARD, || {
            lone.replies.pop().is_some()
        });

        let dropped = drops_within_guard(executor);
        feeding.store(false, Ordering::Release);
        assert!(dropped, "the executor's drop waits on a busy lane");
        assert!(lone.lane.is_finished(), "the lane outlived the executor");
        feeder.join().expect("the feeder ends");
        drop(lone.audio_peer);
        let _ = server.join();
        drop(lone.lane);
    }

    /// Records the threads that wake it.
    struct Recorder(std::sync::Mutex<Vec<std::thread::ThreadId>>);

    impl std::task::Wake for Recorder {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.lock().unwrap().push(std::thread::current().id());
        }
    }

    /// **The audio side never touches a `Waker`**: the audio lane's future is
    /// polled on one thread with a waker that records who wakes it, while the
    /// other thread queues a block and rings the lane (what the audio callback
    /// does). The lane runs the block's whole turn, and nothing wakes the waker.
    #[test]
    fn the_audio_side_never_touches_a_waker() {
        use std::task::{Context, Poll, Waker};

        let (channels, waits) =
            Channels::new(SampleRate::SR_48K, Arc::default()).expect("the channels");
        let lifecycle = Lifecycle::new(Arc::new(crate::ServerWatch::new()), timeouts());
        let listener = silent_listener();
        let socket = crate::unique_socket_path().expect("the OS has a random source");
        let _ = std::fs::remove_file(&socket);
        let served = ListenerOptions::new()
            .name(ipc::socket_name(&socket).expect("socket name"))
            .create_sync()
            .expect("listen");
        let (stream_tx, _stream_rx) = mpsc::channel();
        let lane = audio_lane(
            AudioLaneParts {
                channels: channels.clone(),
                parker: waits.audio,
                payloads: payloads_of(&channels),
                lifecycle: lifecycle.clone(),
                listener: Arc::clone(&listener),
                socket_path: socket,
                control_stream: stream_tx,
                executor: tutti_fixtures::executor().downgrade(),
                ready_within: HANG_GUARD,
            },
            Orphaned::of_audio(
                &channels,
                &lifecycle,
                &listener,
                AudioState::new(timeouts().drain, response_ring().0),
            ),
        );
        let recorder = Arc::new(Recorder(std::sync::Mutex::new(Vec::new())));
        let (polling_tx, polling_rx) = mpsc::channel();
        let poller = {
            let recorder = Arc::clone(&recorder);
            std::thread::spawn(move || {
                polling_tx.send(std::thread::current().id()).expect("sent");
                let waker = Waker::from(recorder);
                let mut cx = Context::from_waker(&waker);
                let mut lane = std::pin::pin!(lane);
                while lane.as_mut().poll(&mut cx) == Poll::Pending {}
            })
        };
        let polling = polling_rx.recv().expect("the poller's id");

        // The lane connects; the audio callback's side then queues a block and
        // rings the lane, and the peer ends the connection once the block is read.
        let audio_peer = served.accept().expect("the audio connection");
        crate::write_frame(
            &mut &audio_peer,
            &AudioReply::Ready {
                protocol_version: crate::PROTOCOL_VERSION,
            },
        )
        .expect("ready");
        let _control_peer = served.accept().expect("the control connection");
        crate::write_frame(
            &mut &_control_peer,
            &ControlFrame::Ready {
                protocol_version: crate::PROTOCOL_VERSION,
            },
        )
        .expect("ready");
        assert!(channels.push_block(payload_with_midi(1)));
        channels.wake();
        let _sent: AudioRequest = crate::read_frame(&mut &audio_peer).expect("the block");
        drop(audio_peer);
        poller.join().expect("the lane ended");

        let woken_by = recorder.0.lock().unwrap().clone();
        assert!(
            woken_by.is_empty(),
            "the lane or the thread that fed it woke a Waker: {} wakes, the lane's thread {polling:?}",
            woken_by.len()
        );
    }

    /// **A drain hands every held event to the listener and publishes the
    /// drop count to the bridge's counter, cumulatively.** The inbox's own
    /// test pins what it keeps; this pins that a lane loop's drain reports it.
    #[test]
    fn a_drain_reports_the_drops() {
        let channels = Channels::new(SampleRate::SR_48K, Arc::default())
            .expect("the channels")
            .0;
        let heard = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let listener: ListenerSlot = {
            let heard = Arc::clone(&heard);
            Arc::new(parking_lot::Mutex::new(Some(Arc::new(move |ev| {
                heard.lock().push(format!("{ev:?}"));
            }))))
        };
        let mut inbox = Unsolicited::new();
        let over = 72;
        for round in 1..=2u64 {
            for i in 0..(UNSOLICITED_PARAMS + over) as i32 {
                inbox.param(tutti_plugin_types::ParamEdit::change(
                    ParamId::new(i as u32),
                    Normalized::default(),
                ));
            }
            drain_unsolicited(&mut inbox, &channels, &listener);
            assert_eq!(channels.dropped_param_events(), round * over as u64);
        }
        let heard = heard.lock();
        assert_eq!(
            heard.len(),
            2 * (UNSOLICITED_PARAMS + 1),
            "each drain: the kept params and one resync"
        );
        assert_eq!(
            heard.last().map(String::as_str),
            Some(format!("{:?}", IpcEvent::Resync(ResyncKind::ParamValues)).as_str())
        );
    }
}
