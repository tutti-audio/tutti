//! The bridge's threads: one per lane. The audio lane's thread connects, then
//! starts the control lane's, and pumps [`AudioCommand`]s through
//! [`handle_audio`]; the control lane's thread pumps `ControlCommand`s through
//! [`handle_control`] and reads what the server says unprompted. Each owns its
//! connection, its queue and its wake-up, so a block never waits behind a
//! control request and all IPC calls on a lane are blocking.
//!
//! The audio-response ring has one producer, the audio lane's thread. The control
//! lane's thread never pushes to it: a failure it sees is latched (which wakes
//! both threads, [`Channels::waker`]) and the audio lane's thread, woken,
//! answers the queued blocks.

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
use crate::{AudioReply, ControlFrame, ControlRequest};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tutti_platform::ThreadHandle;
use tutti_plugin_types::{IpcError, IpcResult, PluginFailure};
use tutti_rt::Parker;

/// The bridge's audio lane thread of an `AudioBridge`, which owns the control
/// lane's: it speaks to the server for the queues. Dropping it shuts both down
/// and joins them.
pub struct IpcThread {
    channels: Channels,
    lifecycle: Lifecycle,
    handle: Option<ThreadHandle<()>>,
}

impl std::fmt::Debug for IpcThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcThread")
            .field("running", &self.handle.is_some())
            .finish_non_exhaustive()
    }
}

impl IpcThread {
    pub(super) fn spawn(
        channels: Channels,
        responses: ResponseWriter,
        waits: Waits,
        payloads: PayloadPool,
        lifecycle: Lifecycle,
        listener: ListenerSlot,
        socket_path: PathBuf,
    ) -> std::io::Result<Self> {
        let thread_channels = channels.clone();
        let thread_payloads = payloads.clone();
        let thread_lifecycle = lifecycle.clone();
        // Every block passes through this thread (callback, unpark, bridge,
        // server, bridge, next callback), and the OS elevates only the audio
        // callback's own thread, so a default-priority bridge delays the
        // reply. A refusal is an outcome on the handle, not an error.
        let handle = tutti_platform::spawn_named(
            "plugin-bridge",
            Some(tutti_platform::PriorityRequest::Highest),
            move |_| {
                run_thread(
                    thread_channels,
                    responses,
                    waits,
                    thread_payloads,
                    thread_lifecycle,
                    listener,
                    socket_path,
                )
            },
        )?;

        Ok(Self {
            channels,
            lifecycle,
            handle: Some(handle),
        })
    }

    /// Asks the threads to stop, without waiting for them: from here on a
    /// failure they read (the server's end, say) is not declared a death. The
    /// control thread says `Shutdown` to the server on its way out.
    pub fn request_stop(&self) {
        self.lifecycle.request_shutdown();
        self.channels.wake();
    }

    /// Asks the threads to stop and joins them. Idempotent.
    pub fn shutdown(&mut self) {
        self.lifecycle.request_shutdown();
        self.channels.wake();
        if let Some(handle) = self.handle.take() {
            // The thread ends on the stop flag and joins the control lane's
            // thread, so this waits for those and asks for nothing more. A
            // panic in it is not reported: the bridge is gone either way.
            let _ = handle.join();
        }
    }
}

impl Drop for IpcThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Whether a lane's thread goes on: the bridge was not asked to stop, and the
/// server is not declared gone (by this bridge, the process guard or the host).
fn live(lifecycle: &Lifecycle) -> bool {
    lifecycle.is_running() && !lifecycle.is_crashed()
}

fn run_thread(
    channels: Channels,
    responses: ResponseWriter,
    waits: Waits,
    payloads: PayloadPool,
    lifecycle: Lifecycle,
    listener: ListenerSlot,
    socket_path: PathBuf,
) {
    let Waits {
        audio: mut parker,
        control: control_wait,
    } = waits;
    // The response ring's writer is this thread's alone: it lives in the audio
    // lane's state.
    let mut state = AudioState::new(lifecycle.timeouts().drain, responses);
    // A bridge that never connects is as dead as one whose socket drops later,
    // and the audio thread tells them apart only through `is_crashed`. Every
    // exit below must mark the crash: returning silently would leave the flag
    // false forever, so the batcher's crash check could never fire, and the
    // output would be silent only because the slab sequence happened never to
    // match.
    let (audio, control) = match connect_lanes(&socket_path) {
        Ok(streams) => streams,
        Err(failure) => {
            crash(&lifecycle, &listener, failure);
            // A block queued before the crash was marked is never sent.
            drain_with_errors(&channels, &mut state);
            drop_controls(&channels);
            return;
        }
    };
    let control_thread = {
        let (channels, lifecycle, listener) =
            (channels.clone(), lifecycle.clone(), Arc::clone(&listener));
        let lane = Lane {
            stream: control,
            state: ControlState::new(lifecycle.timeouts().drain),
        };
        tutti_platform::spawn_named("plugin-control", None, move |_| {
            control_loop(&channels, &control_wait, &lifecycle, &listener, lane);
        })
    };
    let control_thread = match control_thread {
        Ok(handle) => handle,
        Err(e) => {
            crash(
                &lifecycle,
                &listener,
                PluginFailure::Connect {
                    source: Arc::new(IpcError::Io(e)),
                },
            );
            drain_with_errors(&channels, &mut state);
            drop_controls(&channels);
            return;
        }
    };
    audio_loop(
        &channels,
        &mut parker,
        &payloads,
        &lifecycle,
        &listener,
        Lane {
            stream: audio,
            state,
        },
    );
    // The control lane's thread ends on the same condition, rung awake by
    // whatever ended this one.
    channels.wake();
    let _ = control_thread.join();
}

/// The two connections, audio first and control second as the server accepts
/// them, each past its `Ready`.
///
/// Each `Ready` is consumed and its version checked rather than discarded.
/// `launch.rs` already gated the same server on connection 1, so a mismatch
/// here is not reachable today — but this is a wire boundary, the check is one
/// comparison off the audio path, and a silently-ignored version field is how
/// a skew becomes a mis-parse instead of an error.
fn connect_lanes(socket_path: &Path) -> Result<(ControlStream, ControlStream), PluginFailure> {
    let connect = || {
        let stream = ipc::connect(socket_path).map_err(|e| PluginFailure::Connect {
            source: Arc::new(e),
        })?;
        Ok(stream)
    };
    let mut audio = connect()?;
    expect_ready(ipc::recv::<AudioReply>(&mut audio).map(|r| match r {
        AudioReply::Ready { protocol_version } => Ok(protocol_version),
        other => Err(format!("{other:?}")),
    }))?;
    let mut control = connect()?;
    expect_ready(ipc::recv::<ControlFrame>(&mut control).map(|f| match f {
        ControlFrame::Ready { protocol_version } => Ok(protocol_version),
        other => Err(format!("{other:?}")),
    }))?;
    Ok((audio, control))
}

/// The audio lane's loop: park until a block-lane command is
/// queued, send it, and answer it into the response ring.
///
/// **Idle, the thread parks on its [`Parker`]**, unparked by a push, by
/// `shutdown` and by a crash anyone declares. It does not read the control
/// connection. There is no timeout in the park: a spurious return loops.
fn audio_loop(
    channels: &Channels,
    parker: &mut Parker,
    payloads: &PayloadPool,
    lifecycle: &Lifecycle,
    listener: &ListenerSlot,
    mut lane: Lane<AudioState>,
) {
    let mut failed = false;
    while live(lifecycle) {
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
            // no timeout.
            channels.wait_for_block(parker, || !live(lifecycle));
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
        let result = handle_audio(cmd, &mut lane, channels, payloads, lifecycle);
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
/// There is no timeout in the wait: a request, the server's end, a crash and
/// `shutdown` each wake it.
fn control_loop(
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
            channels.progress().wait_until(*deadline, || {
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
            match wait.wait(&lane.stream, None) {
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
    use crate::client::audio::messages::{ProcessPayload, UNSOLICITED_PARAMS};
    use std::sync::Arc;
    use tutti_fixtures::midi;
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

    /// **A bridge that never connects counts the MIDI queued before it
    /// failed.** `run_thread` runs here on the test thread, so the chunk is
    /// queued before the connect fails, deterministically.
    #[test]
    fn a_failed_connect_counts_the_queued_midi() {
        use tutti_plugin_types::WireMidi;

        let (channels, waits) =
            Channels::new(SampleRate::SR_48K, Arc::default()).expect("the channels");
        let lifecycle = Lifecycle::new(
            Arc::new(crate::ServerWatch::new()),
            tutti_plugin_types::Timeouts::default(),
        );
        let mut payload = Box::new(ProcessPayload::empty(SampleRate::SR_48K));
        for _ in 0..4 {
            payload.midi_events.push(WireMidi {
                offset: 0,
                ump: midi::note_on(MidiChannel::FIRST, 60, 1),
            });
        }
        assert!(channels.push_block(AudioCommand::Process(payload)));
        let socket =
            std::env::temp_dir().join(format!("tutti-no-such-bridge-{}.sock", std::process::id()));
        run_thread(
            channels.clone(),
            response_ring().0,
            waits,
            PayloadPool::new(
                SampleRate::SR_48K,
                channels.refusal_writer(),
                channels.leak_writer(),
            ),
            lifecycle.clone(),
            Arc::new(parking_lot::Mutex::new(None)),
            socket,
        );
        assert!(lifecycle.is_crashed());
        assert!(channels.pop_block().is_none());
        assert_eq!(channels.dropped_midi_cell().get(), 4);
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
