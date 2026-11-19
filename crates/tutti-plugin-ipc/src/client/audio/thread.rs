//! The bridge thread — connects to the plugin-server's RT socket and
//! pumps its two lanes' commands through `handle_audio` and `handle_control`. Runs entirely on one OS
//! thread; all IPC calls are blocking.

use super::ListenerSlot;
#[cfg(test)]
use super::channels::response_ring;
use super::channels::{BridgeSide, Channels, ResponseWriter};
use super::dispatch::{Lane, Lanes, Owed, handle_audio, handle_control, read_news};
use super::lifecycle::Lifecycle;
use super::messages::{AudioCommand, AudioResponse, BridgeEvent, Unsolicited};
use super::payload_pool::PayloadPool;
use crate::HostMessage;
use crate::transport::control::{self as ipc, ControlStream, ControlWait};
use std::path::PathBuf;
use std::sync::Arc;
use tutti_plugin_types::{BridgeError, PluginFailure};
use tutti_rt::ThreadHandle;

/// The bridge thread of an `AudioBridge`: it speaks to
/// the server for the queues. Dropping it shuts it down and joins it.
pub struct BridgeThread {
    channels: Channels,
    lifecycle: Lifecycle,
    handle: Option<ThreadHandle<()>>,
}

impl std::fmt::Debug for BridgeThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeThread")
            .field("running", &self.handle.is_some())
            .finish_non_exhaustive()
    }
}

impl BridgeThread {
    pub(super) fn spawn(
        channels: Channels,
        responses: ResponseWriter,
        wait: ControlWait,
        payloads: PayloadPool,
        lifecycle: Lifecycle,
        listener: ListenerSlot,
        socket_path: PathBuf,
    ) -> std::io::Result<Self> {
        let thread_channels = channels.clone();
        let thread_payloads = payloads.clone();
        let thread_lifecycle = lifecycle.clone();
        // Every block passes through this thread (callback, doorbell, bridge,
        // server, bridge, next callback), and the OS elevates only the audio
        // callback's own thread, so a default-priority bridge delays the
        // reply. A refusal is an outcome on the handle, not an error.
        let handle = tutti_rt::spawn_named(
            "plugin-bridge",
            Some(tutti_rt::PriorityRequest::Highest),
            move |_| {
                run_thread(
                    thread_channels,
                    responses,
                    wait,
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

    /// Asks the thread to stop, without waiting for it: from here on a failure
    /// it reads (the server's end, say) is not declared a death. The thread
    /// says `Shutdown` to the server on its way out.
    pub fn request_stop(&self) {
        self.lifecycle.request_shutdown();
        self.channels.wake();
    }

    /// Asks the thread to stop and joins it. Idempotent.
    pub fn shutdown(&mut self) {
        self.lifecycle.request_shutdown();
        self.channels.wake();
        if let Some(handle) = self.handle.take() {
            // The thread ends on the stop flag, so this waits for that and asks
            // for nothing more. A panic in it is not reported: the bridge is
            // gone either way.
            let _ = handle.join();
        }
    }
}

impl Drop for BridgeThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_thread(
    channels: Channels,
    responses: ResponseWriter,
    wait: ControlWait,
    payloads: PayloadPool,
    lifecycle: Lifecycle,
    listener: ListenerSlot,
    socket_path: PathBuf,
) {
    // The response ring's writer is this thread's alone.
    let mut channels = BridgeSide::new(channels, responses);
    // A bridge that never connects is as dead as one whose socket drops later,
    // and the audio thread tells them apart only through `is_crashed`. Both
    // exits below must mark the crash: returning silently would leave the flag
    // false forever, so the batcher's crash check could never fire, and the
    // output would be silent only because the slab sequence happened never to
    // match.
    let stream = ipc::connect(&socket_path);
    let mut stream = match stream {
        Ok(s) => s,
        Err(e) => {
            crash(
                &lifecycle,
                &listener,
                PluginFailure::Connect {
                    source: Arc::new(e),
                },
            );
            // A block queued before the crash was marked is never sent.
            drain_with_errors(&mut channels);
            return;
        }
    };
    // Consume the server's Ready handshake for connection 2, and check its
    // version rather than discarding it. `launch.rs` already gated the same
    // server on connection 1, so a mismatch here is not reachable today — but
    // this is a wire boundary, the check is one comparison off the audio path,
    // and a silently-ignored version field is how a skew becomes a mis-parse
    // instead of an error.
    if let Err(failure) = expect_ready(&mut stream) {
        crash(&lifecycle, &listener, failure);
        drain_with_errors(&mut channels);
        return;
    }
    // The control connection (3): the server accepts it after the audio one.
    let control = match ipc::connect(&socket_path) {
        Ok(s) => s,
        Err(e) => {
            crash(
                &lifecycle,
                &listener,
                PluginFailure::Connect {
                    source: Arc::new(e),
                },
            );
            drain_with_errors(&mut channels);
            return;
        }
    };
    let mut control = control;
    if let Err(failure) = expect_ready(&mut control) {
        crash(&lifecycle, &listener, failure);
        drain_with_errors(&mut channels);
        return;
    }
    let mut lanes = Lanes {
        audio: Lane {
            stream,
            owed: Owed::finishing(lifecycle.timeouts().drain),
        },
        control: Lane {
            stream: control,
            owed: Owed::finishing(lifecycle.timeouts().drain),
        },
    };
    // The control connection is also read when the server speaks unprompted
    // (a request to be processed again, a latency change), whether or not a
    // command is waiting: the thread's wait is on it and the bell together. The
    // probe lets `news_settled` ask whether bytes are left unread.
    match ipc::Watch::of(&lanes.control.stream) {
        Ok(watch) => channels.set_probe(Arc::new(watch)),
        Err(e) => tracing::warn!("the bridge cannot watch the control connection: {e}"),
    }
    pump(
        &mut channels,
        &wait,
        &payloads,
        &lifecycle,
        &listener,
        &mut lanes,
    );
    channels.clear_probe();
    say_shutdown(&lifecycle, &mut lanes);
}

/// Tells the server to shut down when the bridge was asked to stop and the
/// server is still there to hear it: its plugin is then destroyed by the server
/// itself, and the host's wait for the process's end (the guard's) is the
/// grace. Best effort: a server that cannot be written to is gone or hung, and
/// the guard kills it at the end of the grace.
fn say_shutdown(lifecycle: &Lifecycle, lanes: &mut Lanes) {
    if !lifecycle.is_running() && lifecycle.crash_cause().is_none() {
        let _ = ipc::send(&mut lanes.control.stream, &HostMessage::Shutdown);
    }
}

/// Reads a connection's `Ready` and checks its version.
///
/// The reason is spelled out per case rather than as one string: a version skew
/// and a peer that never said `Ready` are different problems for whoever reads
/// the message, and the first is actionable.
fn expect_ready(stream: &mut ControlStream) -> Result<(), PluginFailure> {
    match ipc::recv(stream) {
        Ok(crate::BridgeMessage::Ready { protocol_version })
            if crate::protocol::check_protocol_version(protocol_version).is_ok() =>
        {
            Ok(())
        }
        other => {
            let source = match other {
                Ok(crate::BridgeMessage::Ready { protocol_version }) => {
                    BridgeError::ProtocolMismatch {
                        expected: crate::PROTOCOL_VERSION,
                        got: protocol_version,
                    }
                }
                Ok(got) => crate::error::unexpected_message("Ready", &got),
                Err(e) => e,
            };
            Err(PluginFailure::Handshake {
                source: Arc::new(source),
            })
        }
    }
}

/// The bridge's command loop.
///
/// **Idle, the bridge waits on the control connection and its bell together**
/// ([`ControlWait`]), so what the server says unprompted (a request to be
/// processed again, a latency or parameter change, its own death) is read as it
/// arrives, not with the next command. While a command waits for its reply,
/// `handle` reads the same lane and the unsolicited events ride in on that
/// read; either way they go to the listener after the command. There is no
/// timeout in the wait: a command and `shutdown` each ring the bell, and a
/// spurious return loops.
fn pump(
    channels: &mut BridgeSide,
    wait: &ControlWait,
    payloads: &PayloadPool,
    lifecycle: &Lifecycle,
    listener: &ListenerSlot,
    lanes: &mut Lanes,
) {
    // Unsolicited events read while a command waited for its reply. One
    // buffer for the connection, handed to the listener after each command.
    let mut inbox = Unsolicited::with_lifecycle(lifecycle.clone());
    while live(lifecycle) {
        let result = if let Some(cmd) = channels.pop_block() {
            handle_audio(cmd, &mut lanes.audio, channels, payloads, &mut inbox).and_then(|()| {
                // The plugin's news arrives on the control lane, which a block
                // does not read: take what is there between blocks.
                if ipc::has_pending(&lanes.control.stream) {
                    read_news(&mut lanes.control, channels, &mut inbox)
                } else {
                    Ok(())
                }
            })
        } else if let Some(cmd) = channels.pop_control() {
            handle_control(cmd, &mut lanes.control, channels, &mut inbox, lifecycle)
        } else {
            match wait.wait(&lanes.control.stream, None) {
                // The server spoke: read the lane.
                Ok(woken) if woken.stream => {
                    channels.set_news();
                    read_news(&mut lanes.control, channels, &mut inbox)
                }
                // A ring: look at the queues and the stop condition again.
                Ok(_) => continue,
                Err(e) => {
                    crash(
                        lifecycle,
                        listener,
                        PluginFailure::from_stream(BridgeError::Io(e)),
                    );
                    channels.push_audio_response(AudioResponse::Error { seq: None });
                    drain_with_errors(channels);
                    return;
                }
            }
        };
        drain_unsolicited(&mut inbox, channels, listener);
        channels.clear_news();
        channels.turn_done();

        if let Err(e) = result {
            // The error is classified here, at the only place it exists.
            // `BridgeError` is not `Clone`, and the crash notification outlives
            // this frame, so the failure holds it behind an `Arc`.
            crash(lifecycle, listener, PluginFailure::from_stream(e));
            // Connection-level: the stream is gone, so this ends every
            // in-flight and queued block, not just one. `None` matches
            // whichever request the audio thread is waiting on.
            channels.push_audio_response(AudioResponse::Error { seq: None });
            drain_with_errors(channels);
            return;
        }
    }
    // Out of the loop with the bridge still asked to run: the server was
    // declared gone by another route (the process guard's exit watch, the host).
    // The queued blocks are answered as a failed stream's are.
    if lifecycle.is_crashed() {
        channels.push_audio_response(AudioResponse::Error { seq: None });
        drain_with_errors(channels);
    }
    notify_external_crash(lifecycle, listener);
}

/// Whether the thread goes on: the bridge was not asked to stop, and the server
/// is not declared gone (by this bridge, the process guard or the host).
fn live(lifecycle: &Lifecycle) -> bool {
    lifecycle.is_running() && !lifecycle.is_crashed()
}

/// Declare the bridge dead: latch the cause, then tell whoever is listening.
///
/// One helper rather than two calls at each site, because the latch and the
/// notification must not drift apart — a crash that fired an event without
/// latching would be invisible to anything that asked later, and one that
/// latched without firing is the polling design this replaces.
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

/// Tells the listener of the crash, once for the bridge.
fn notify_crash(lifecycle: &Lifecycle, listener: &ListenerSlot, cause: PluginFailure) {
    if !lifecycle.announce_crash() {
        return;
    }
    let cb = listener.lock().clone();
    if let Some(f) = cb.as_ref() {
        f(BridgeEvent::Crashed { cause });
    }
}

/// The thread ends: when the server was declared gone by another route (the
/// process guard's exit watch, the host), no failure of this bridge's stream
/// told the listener, so it hears it now, with the latched cause.
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

/// Answer every queued command with an error. A queued chunk is never sent,
/// so its MIDI is counted dropped; nothing can splice past it, as the server
/// is gone.
fn drain_with_errors(channels: &mut BridgeSide) {
    while let Some(cmd) = channels.pop_block() {
        if let AudioCommand::Process(payload) = &cmd {
            channels.note_dropped_midi(payload.midi_events.len() as u64);
        }
        channels.push_audio_response(AudioResponse::Error { seq: None });
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
        let mut channels = BridgeSide::new(channels, response_ring().0);
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
        drain_with_errors(&mut channels);
        assert!(channels.pop_block().is_none());
        assert_eq!(channels.dropped_midi_cell().get(), 5);
    }

    /// **A bridge that never connects counts the MIDI queued before it
    /// failed.** `run_thread` runs here on the test thread, so the chunk is
    /// queued before the connect fails, deterministically.
    #[test]
    fn a_failed_connect_counts_the_queued_midi() {
        use tutti_plugin_types::WireMidi;

        let (channels, wait) =
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
            wait,
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
    /// test pins what it keeps; this pins that `pump`'s drain reports it.
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
            Some(format!("{:?}", BridgeEvent::Resync(ResyncKind::ParamValues)).as_str())
        );
    }
}
