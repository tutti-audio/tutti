//! What happens when the plugin-server stops behaving?
//!
//! The existing mock-server tests all script a *well-formed* reply, and every
//! crash test uses `bridge_with_no_server()` — a peer that was **never there**.
//! Neither covers a peer that was there and then stopped being there, which is
//! the failure that actually happens in production: a plugin segfaults
//! mid-session, taking its server down with it, while the audio thread is
//! waiting on a reply.
//!
//! Two families here:
//!
//! - **Death mid-session.** The socket closes after a successful exchange. The
//!   host must mark the bridge crashed, release everything in flight, and keep
//!   the audio thread moving.
//! - **A peer that is hostile or corrupt.** `recv` reads a 4-byte big-endian
//!   length and then does `vec![0u8; len]` — an attacker-controlled allocation.
//!   Garbage postcard, a truncated body, and a valid-but-wrong message type are
//!   the same class: bytes the host must reject rather than trust.
//!
//! The bar is not that the host produces audio — a dead peer has none to give.
//! It is that the host **fails in bounded time and stays usable**: no hang, no
//! unbounded allocation, no panic escaping into the audio thread.

use super::PluginBridge;
use crate::client::audio::BridgeThread;
use crate::transport::shm::RING_SLOTS;
use crate::unique_socket_path;
use crate::{BridgeMessage, HostMessage, PROTOCOL_VERSION, SlabLayout};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tutti_buffers::FloatWidth;
use tutti_fixtures::main_thread;
use tutti_platform::Instant;
use tutti_plugin_types::ChannelLayout;
use tutti_plugin_types::{BridgeError, PluginFailure, StateError, Timeouts};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::SampleRate;

use interprocess::local_socket::{ListenerOptions, Stream, traits::Listener as _};

/// Upper bound on how long any single "the host must notice" wait may take.
///
/// A hang guard, never a speed bound: far past the bridge's own timeouts, so
/// a loaded machine cannot fail it, but finite so a genuine hang is a failure
/// rather than a hung test run. A test asserts completion, not how long it took.
const NOTICE_TIMEOUT: Duration = Duration::from_secs(60);

/// Timeouts under which a call that does not take the branch a test names
/// blocks past [`NOTICE_TIMEOUT`]: the test asserts the right branch by
/// completing, never by how fast it did.
fn unreachable_deadline() -> Timeouts {
    let mut t = Timeouts::default();
    t.param = Duration::from_secs(3_600);
    t
}

/// What the host adds to a bridge deadline before it gives up on an answer, in
/// these tests: shorter than production's, so a test of a timeout does not wait
/// out the real margin.
const REPLY_MARGIN: Duration = Duration::from_millis(200);

/// The shortened deadline the stall tests run with (the chunked-reassembly test
/// has its own, wider one).
///
/// Two orders of magnitude under production, so a stalled transfer is detected
/// in a fraction of a second, and comfortably over the 20 ms poll interval so a
/// scheduling hiccup does not read as a stall.
const STATE_PROGRESS_TIMEOUT: Duration = Duration::from_millis(400);

/// The default [`Timeouts`] with the wait for a rate change's answer set to
/// `ms` milliseconds.
fn rate_timeout(ms: u64) -> Timeouts {
    let mut t = Timeouts::default();
    t.rate = Duration::from_millis(ms);
    t
}

/// These peers never open an editor; one that tried is refused.
fn no_editor(
    _: tutti_plugin_types::MainThreadMarker,
    _: &tutti_plugin_types::MainWorkHandle,
    plugin: &tutti_plugin_types::PluginRef,
    _: tutti_plugin_types::PluginFormat,
) -> tutti_plugin_types::BridgeResult<Box<dyn tutti_plugin_types::Editor>> {
    Err(tutti_plugin_types::BridgeError::LoadFailed {
        path: Some(plugin.path().to_path_buf()),
        stage: tutti_plugin_types::LoadStage::Opening,
        reason: "no editor in these tests".into(),
    })
}

/// The answer to the request the bridge sends after a render mode: the server
/// answers its control lane in order, so this says the mode is applied.
fn applied_fence() -> BridgeMessage {
    BridgeMessage::CurrentPreset { id: None }
}

fn stereo_layout() -> SlabLayout {
    SlabLayout {
        slots: RING_SLOTS as u32,
        samples_per_channel: 512,
        format: FloatWidth::F32,
        inputs: vec![ChannelLayout::STEREO],
        outputs: vec![ChannelLayout::STEREO],
    }
}

fn recv_host_msg(stream: &Stream) -> std::io::Result<HostMessage> {
    let mut stream = stream;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    postcard::from_bytes(&buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn send_bridge_msg(stream: &Stream, msg: &BridgeMessage) -> std::io::Result<()> {
    let mut stream = stream;
    crate::protocol::frame::write_frame(&mut stream, msg).map_err(std::io::Error::other)
}

/// Write a raw frame the host must cope with but which no honest server would
/// send. `len` is the advertised length; `body` is what actually follows, so the
/// two can deliberately disagree.
fn send_raw_frame(stream: &Stream, len: u32, body: &[u8]) -> std::io::Result<()> {
    let mut stream = stream;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(body)
}

/// A mock server whose behaviour a test scripts per message, and which can drop
/// its connection part-way through.
///
/// The `respond` closure returns `Action`, so a test can answer normally for a
/// while and then die — which is the whole point of this file. Returning
/// [`Action::Die`] closes the socket the way a segfaulting server does: no
/// shutdown message, no goodbye, just EOF at the host's next read.
enum Action {
    /// Boxed: `BridgeMessage` inlines a ~5 KB MIDI vec, and an unboxed variant
    /// would make every `Action` that large.
    Reply(Box<BridgeMessage>),
    /// Answer nothing and keep the connection open (a hung server).
    Silent,
    /// Close the connection immediately (a crashed server).
    Die,
    /// Send a raw, deliberately malformed frame: `(advertised_len, body)`.
    Raw(u32, Vec<u8>),
    /// Answer with a sequence of frames, in order.
    ///
    /// Needed for plugin state, which is chunked: a single-`Reply` mock cannot
    /// produce a multi-frame answer, so without this the chunked path could
    /// only be tested one chunk long — the length at which it is
    /// indistinguishable from the single-frame shape it replaced.
    ReplyMany(Vec<BridgeMessage>),
    /// Answer with a sequence of frames, pausing `gap` before each.
    ///
    /// A slow-but-progressing peer: the thing a progress deadline must tolerate
    /// and a total budget cannot. `ReplyMany` sends back-to-back and so proves
    /// nothing about either.
    ReplyManySlowly {
        msgs: Vec<BridgeMessage>,
        gap: Duration,
    },
    /// Advertise `len`, then dribble one byte every `gap` — forever.
    ///
    /// The peer a per-syscall receive timeout cannot bound. `SO_RCVTIMEO`
    /// restarts on every `recv` and `read_exact` loops until the buffer is
    /// full, so any peer that makes *some* progress before each expiry keeps
    /// the reader inside one call indefinitely. Neither the socket timeout nor
    /// the caller's own can end it: the caller gives up and answers `None`,
    /// while the bridge thread stays parked in the read and never reaches the
    /// error that would mark the bridge crashed.
    Dribble { len: u32, gap: Duration },
    /// Send events forever, each frame's length prefix `split` before its
    /// body: a live peer that is part-way through a frame most of the time,
    /// though every frame completes within `split`.
    SplitEvents { split: Duration },
    /// Send `count` events `gap` apart, answer nothing, then look whether the
    /// host's next request is already waiting (within 300 ms): `probe` becomes
    /// [`PROBE_WAITING`] or [`PROBE_NOT_YET`]. The waiting request is handled
    /// as the next one.
    EventsThenProbe {
        count: usize,
        gap: Duration,
        probe: Arc<std::sync::atomic::AtomicU8>,
    },
}

/// [`Action::EventsThenProbe`] found the host's next request already sent.
const PROBE_WAITING: u8 = 1;
/// [`Action::EventsThenProbe`] found nothing sent yet.
const PROBE_NOT_YET: u8 = 2;

/// One connection of the mock server: answers each request as `respond` says.
fn serve_lane(
    stream: interprocess::local_socket::Stream,
    respond: &std::sync::Mutex<Box<dyn Fn(HostMessage) -> Action + Send>>,
) {
    let mut next = None;
    loop {
        let msg = match next.take() {
            Some(msg) => msg,
            None => match recv_host_msg(&stream) {
                Ok(msg) => msg,
                Err(_) => return, // host hung up
            },
        };
        match (respond.lock().unwrap())(msg) {
            Action::Reply(m) => {
                if send_bridge_msg(&stream, &m).is_err() {
                    return;
                }
            }
            Action::ReplyMany(ms) => {
                if ms.iter().any(|m| send_bridge_msg(&stream, m).is_err()) {
                    return;
                }
            }
            Action::ReplyManySlowly { msgs, gap } => {
                for m in &msgs {
                    std::thread::sleep(gap);
                    if send_bridge_msg(&stream, m).is_err() {
                        return;
                    }
                }
            }
            Action::Silent => {}
            Action::Raw(len, body) => {
                if send_raw_frame(&stream, len, &body).is_err() {
                    return;
                }
            }
            Action::Dribble { len, gap } => {
                let mut s = &stream;
                if s.write_all(&len.to_be_bytes()).is_err() {
                    return;
                }
                // Forever, or until the host hangs up. A bounded loop
                // would let the test pass by the peer running out of
                // bytes rather than by the host refusing the frame.
                loop {
                    std::thread::sleep(gap);
                    if s.write_all(&[0u8]).is_err() {
                        return;
                    }
                }
            }
            Action::EventsThenProbe { count, gap, probe } => {
                for n in 0..count {
                    std::thread::sleep(gap);
                    let event = BridgeMessage::LatencyChanged {
                        samples: tutti_plugin_types::Samples::new(n),
                    };
                    if send_bridge_msg(&stream, &event).is_err() {
                        return;
                    }
                }
                use interprocess::local_socket::traits::Stream as _;
                let _ = stream.set_recv_timeout(Some(Duration::from_millis(300)));
                let waiting = recv_host_msg(&stream).ok();
                let _ = stream.set_recv_timeout(None);
                probe.store(
                    if waiting.is_some() {
                        PROBE_WAITING
                    } else {
                        PROBE_NOT_YET
                    },
                    Ordering::SeqCst,
                );
                next = waiting;
            }
            Action::SplitEvents { split } => {
                let mut s = &stream;
                for n in 0.. {
                    let body = postcard::to_allocvec(&BridgeMessage::LatencyChanged {
                        samples: tutti_plugin_types::Samples::new(n),
                    })
                    .unwrap();
                    let prefix = (body.len() as u32).to_be_bytes();
                    if s.write_all(&prefix).is_err() {
                        return;
                    }
                    std::thread::sleep(split);
                    if s.write_all(&body).is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            // Dropping `stream` closes the socket, so the host's next
            // read sees EOF — exactly what a killed server produces.
            Action::Die => return,
        }
    }
}

/// How a mock lane answers: a script over the messages of its connection.
type Responder = Arc<std::sync::Mutex<Box<dyn Fn(HostMessage) -> Action + Send>>>;

struct MockServer {
    bridge: Arc<PluginBridge>,
    /// The ring the bridge's audio lane answers blocks into.
    replies: crate::client::audio::Replies,
    /// `Option` only so `Drop` can take it out and `mem::forget` it; see there.
    thread: Option<BridgeThread>,
    server: Option<std::thread::JoinHandle<()>>,
    socket: std::path::PathBuf,
}

impl MockServer {
    fn start(label: &str, respond: impl Fn(HostMessage) -> Action + Send + 'static) -> Self {
        // The production deadline, not the shortened one. Only the two tests
        // *about* the deadline shorten it: a 400 ms progress budget applied to
        // every mock made `a_state_larger_than_one_frame_round_trips` fail, and
        // failing correctly — 12 MiB moves in 4 MiB chunks, so the gap between
        // progress reports is a whole chunk's transfer, which under parallel
        // load exceeds 400 ms. That is a test-harness artefact and not a defect,
        // and it is exactly the "big state fails as a timeout" shape this change
        // exists to remove. Shortening the deadline by default would reintroduce
        // it in miniature.
        Self::start_with_timeouts(label, Timeouts::default(), respond)
    }

    /// As [`start`](Self::start), with an explicit state-transfer progress
    /// deadline.
    ///
    /// The stall tests pass a shortened one: production's is 10 s, and a test
    /// that proved the stall path by waiting it out would spend all of it. The
    /// chunked-reassembly test passes a window far past anything it spends.
    fn start_with_state_progress_timeout(
        label: &str,
        state_progress: Duration,
        respond: impl Fn(HostMessage) -> Action + Send + 'static,
    ) -> Self {
        let timeouts = {
            let mut t = Timeouts::default();
            t.state_progress = state_progress;
            t
        };
        Self::start_with_timeouts(label, timeouts, respond)
    }

    /// As [`start`](Self::start), under `timeouts` (with [`REPLY_MARGIN`] as
    /// the margin).
    fn start_with_timeouts(
        label: &str,
        timeouts: Timeouts,
        respond: impl Fn(HostMessage) -> Action + Send + 'static,
    ) -> Self {
        // One script for both lanes: a test's script sees the requests of both
        // as it saw the one's.
        let respond: Responder = Arc::new(std::sync::Mutex::new(Box::new(respond)));
        Self::start_on(label, timeouts, [Arc::clone(&respond), respond])
    }

    /// As [`start_with_timeouts`](Self::start_with_timeouts), each lane with a
    /// script of its own: a lane held in one answer does not hold the other.
    fn start_lanes(
        label: &str,
        audio: impl Fn(HostMessage) -> Action + Send + 'static,
        control: impl Fn(HostMessage) -> Action + Send + 'static,
    ) -> Self {
        // A request that is not answered stays outstanding past any test.
        Self::start_lanes_under(label, unreachable_deadline(), audio, control)
    }

    /// As [`start_lanes`](Self::start_lanes), under `timeouts`.
    fn start_lanes_under(
        label: &str,
        timeouts: Timeouts,
        audio: impl Fn(HostMessage) -> Action + Send + 'static,
        control: impl Fn(HostMessage) -> Action + Send + 'static,
    ) -> Self {
        fn lane(respond: impl Fn(HostMessage) -> Action + Send + 'static) -> Responder {
            Arc::new(std::sync::Mutex::new(Box::new(respond)))
        }
        Self::start_on(label, timeouts, [lane(audio), lane(control)])
    }

    fn start_on(label: &str, timeouts: Timeouts, lanes: [Responder; 2]) -> Self {
        let timeouts = {
            let mut t = timeouts;
            t.reply_margin = REPLY_MARGIN;
            t
        };
        let path = unique_socket_path().expect("the OS has a random source");
        let _ = std::fs::remove_file(&path);
        let name = crate::transport::control::socket_name(&path).expect("socket name");
        let listener = ListenerOptions::new()
            .name(name)
            .create_sync()
            .expect("listen");

        let (bridge, thread, replies) = PluginBridge::new(
            path.clone(),
            tutti_plugin_types::PluginRef::file("test.vst3"),
            tutti_plugin_types::PluginFormat::Vst3,
            Some(crate::EditorLoader::new(
                no_editor,
                tutti_fixtures::detached_work(),
            )),
            SampleRate::SR_48K,
            Arc::new(crate::ServerWatch::new()),
            timeouts,
        )
        .expect("bridge");

        // Two connections, as the server accepts them: audio, then control.
        // Each is served on its own thread by its lane's responder.
        let mut servers = Vec::new();
        for (lane, respond) in ["audio", "control"].into_iter().zip(lanes) {
            let stream = listener.accept().expect("accept");
            send_bridge_msg(
                &stream,
                &BridgeMessage::Ready {
                    protocol_version: PROTOCOL_VERSION,
                },
            )
            .expect("handshake");
            servers.push(
                std::thread::Builder::new()
                    .name(format!("mock-hostile-{label}-{lane}"))
                    .spawn(move || serve_lane(stream, &respond))
                    .expect("spawn mock"),
            );
        }
        let mut servers = servers.into_iter();
        let server = servers.next().expect("the audio lane");
        // The control lane's thread ends with the connection; nothing joins it.
        drop(servers);

        Self {
            bridge,
            replies,
            thread: Some(thread),
            server: Some(server),
            socket: path,
        }
    }

    /// Poll until the bridge reports itself crashed, or fail at `NOTICE_TIMEOUT`
    /// naming `what` was waited for.
    ///
    /// Polling rather than a single read because `mark_crashed` happens on the
    /// bridge thread and is published with `Release`/`Acquire` — a single read
    /// races it and would make this flaky rather than wrong.
    ///
    /// It settles the **flag and the latched cause**, and nothing else. In
    /// particular it is not a handshake for the crash *notification*: `crash`
    /// stores the flag before it takes the listener lock, so this can return
    /// while the callback has not run. A test about the event must wait on the
    /// event — see `a_listener_hears_the_crash_with_its_cause`.
    fn wait_for_crash(&self, what: &str) {
        tutti_fixtures::wait_until(what, NOTICE_TIMEOUT, || self.bridge.audio().is_crashed());
    }
}

impl Drop for MockServer {
    /// Tear down **without joining anything**.
    ///
    /// Both obvious joins can block indefinitely in exactly the scenarios this
    /// file constructs, and a hang in teardown is as bad as a hang in the test:
    ///
    /// - `BridgeThread::shutdown` joins the bridge thread, which may be parked
    ///   in `recv_within` waiting out a reply timeout.
    /// - joining the mock server thread waits on a socket read that only ends
    ///   when the host hangs up — which the line above was supposed to cause.
    ///
    /// Without it, raising the bridge's timeouts to an hour wedges the
    /// whole run, with three threads in `futex_wait` inside this `drop` and the
    /// mock threads in `unix_stream_data_wait`.
    ///
    /// So the socket file is removed and both threads are detached. Each exits
    /// on its own once its read fails or its timeout expires; the process is a
    /// test binary that is about to end anyway. `BridgeThread`'s own `Drop`
    /// joins, so it is `mem::forget`-ed rather than dropped — the cost is a
    /// detached thread for the life of the test binary, and the alternative is
    /// a run that never finishes.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        if let Some(h) = self.server.take() {
            drop(h); // detach: joining would wait on a socket read
        }
        if let Some(t) = self.thread.take() {
            std::mem::forget(t); // detach: its Drop joins
        }
    }
}

/// Run `f` on a worker thread and give up after `NOTICE_TIMEOUT`.
///
/// **A test asserting "this returns in bounded time" must not itself block
/// forever to find out.** Measuring `Instant::elapsed()` around a direct call
/// only works if the call returns: raising the bridge's timeouts to an hour made
/// two of these tests hang the whole `cargo test` run instead of failing, which
/// is strictly worse than a red test — CI reports a timeout with no attribution,
/// and a developer sees a wedged terminal.
///
/// So the call happens elsewhere and this waits on a channel. `Err` means it was
/// still running at the deadline, which is the failure the caller reports.
///
/// The worker is deliberately left running rather than joined: it is blocked in
/// the very call under test, so joining it would reintroduce the hang. It holds
/// only an `Arc<PluginBridge>` clone and exits when its timeout finally expires.
fn call_within<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> std::result::Result<T, Duration> {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let start = Instant::now();
    std::thread::Builder::new()
        .name("hostile-call".to_string())
        .spawn(move || {
            let value = f();
            let _ = tx.send(value);
        })
        .expect("spawn call thread");

    rx.recv_timeout(NOTICE_TIMEOUT).map_err(|_| start.elapsed())
}

// ── Death mid-session ────────────────────────────────────────────────────────

/// A server that dies **after** a successful exchange must mark the bridge
/// crashed.
///
/// Distinct from `bridge_with_no_server()`, which never connects: there the
/// failure happens in `run_thread` before a lane's loop is ever entered. Here the
/// bridge is inside its loop, mid-conversation, with the connection already
/// proven good — the path where a real plugin segfault lands, and the one no
/// existing test covers.
#[test]
fn a_server_that_dies_mid_session_marks_the_bridge_crashed() {
    use std::sync::atomic::AtomicUsize;
    static SEEN: AtomicUsize = AtomicUsize::new(0);
    SEEN.store(0, Ordering::SeqCst);

    let mock = MockServer::start("mid-death", |msg| match msg {
        HostMessage::GetParameter { .. } => {
            // Answer the first request, die on the second. Answering once is
            // what makes this "died mid-session" rather than "never worked".
            if SEEN.fetch_add(1, Ordering::SeqCst) == 0 {
                Action::Reply(Box::new(BridgeMessage::ParameterValue {
                    value: Some(Normalized::of(0.5)),
                }))
            } else {
                Action::Die
            }
        }
        _ => Action::Silent,
    });

    let first = mock.bridge.audio().parameter(ParamId::new(1));
    assert_eq!(
        first,
        Some(Normalized::of(0.5)),
        "the mock must answer the first request, or this test is just \
         `bridge_with_no_server` under another name"
    );
    assert!(
        !mock.bridge.audio().is_crashed(),
        "the bridge reported a crash while the server was still healthy"
    );

    // Second request: the server dies instead of replying.
    let second = mock.bridge.audio().parameter(ParamId::new(1));
    assert_eq!(
        second, None,
        "a request to a dead server returned a value: {second:?}"
    );
    mock.wait_for_crash("the bridge to mark itself crashed after the server closed the connection");
}

// ── The cause, and who hears about it ────────────────────────────────────────

/// Whether `cause` is what a peer that closes the connection leaves: the EOF
/// as `StreamClosed`, or a reset as `Io` over the I/O `BridgeError`.
fn is_closed_stream(cause: Option<&PluginFailure>) -> bool {
    use std::error::Error;
    match cause {
        Some(PluginFailure::StreamClosed) => true,
        Some(io @ PluginFailure::Io { .. }) => matches!(
            io.source().and_then(|s| s.downcast_ref::<BridgeError>()),
            Some(BridgeError::Io(_))
        ),
        _ => false,
    }
}

/// A mid-session death latches *why*, not just *that*.
///
/// The cause has to be captured where the failure is noticed: `BridgeError` is
/// not `Clone` and is dropped as soon as the failing call returns, so a host
/// that polls afterwards could otherwise only report a placeholder, identical
/// for every death alike.
///
/// Asserts the cause's kind, not its text: a closed stream reads as
/// `StreamClosed` (EOF) or, where the OS reports a reset instead, `Io` with
/// the `BridgeError` as its `source`. Pinning the text would make this a test
/// of the OS.
#[test]
fn a_crash_latches_a_cause_that_outlives_the_call() {
    let mock = MockServer::start("cause-latch", |_| Action::Die);

    assert!(
        mock.bridge.audio().crash_cause().is_none(),
        "a bridge reported a cause before anything went wrong"
    );

    let _ = mock.bridge.audio().parameter(ParamId::new(1));
    mock.wait_for_crash("the bridge to notice the peer died");

    let cause = mock.bridge.audio().crash_cause();
    assert!(
        is_closed_stream(cause.as_ref()),
        "a crashed bridge reported no cause ({cause:?}); the reason is gone \
         once the failing call returns, so latching it at the detection site \
         is the only chance to keep it"
    );
}

/// A listener installed before the death is told, and told why.
///
/// The notification is the half that makes a crash *prompt*: without it a host
/// has to poll every frame to discover something that already happened.
///
/// **Waits on the notification, not on `is_crashed`.** The two are published in
/// that order deliberately — `crash` latches the cause first, so that
/// `crash_cause` stays authoritative for a death nobody was subscribed to, and
/// only then takes the listener lock. So `wait_for_crash` returning true is
/// *not* a happens-before for the event: the bridge thread can be preempted
/// between the `Release` store and the `listener.lock()`, and a reader that
/// polls the flag and then reads the sink finds it empty. That window is real
/// rather than theoretical — a 60 ms sleep inserted at exactly that point fails
/// this assertion on every run with `got []`, which is the shape of the one
/// unreproducible failure this test produced under parallel load. Waiting on
/// the sink itself is the handshake; widening a timeout would only have made
/// the window rarer.
#[test]
fn a_listener_hears_the_crash_with_its_cause() {
    use parking_lot::Mutex as PlMutex;

    let mut mock = MockServer::start("crash-event", |_| Action::Die);

    let heard: Arc<PlMutex<Vec<PluginFailure>>> = Arc::new(PlMutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    mock.bridge.audio().set_listener(Some(Arc::new(move |ev| {
        if let crate::client::audio::BridgeEvent::Crashed { cause } = ev {
            sink.lock().push(cause);
        }
    })));

    let _ = mock.bridge.audio().parameter(ParamId::new(1));
    tutti_fixtures::wait_until(
        &format!(
            "a crash notification after the peer died (crashed={}, cause={:?}); a latched \
             cause with no event means the notification half of `crash` never fired",
            mock.bridge.audio().is_crashed(),
            mock.bridge.audio().crash_cause()
        ),
        NOTICE_TIMEOUT,
        || !heard.lock().is_empty(),
    );

    // Counted once both lane threads have ended (the shutdown joins them): a
    // second event, from the lane that saw the end after the first, would be in
    // the sink by then, and no sleep is needed to wait for one.
    mock.thread.as_mut().expect("the bridge thread").shutdown();
    let seen = heard.lock().clone();
    assert_eq!(
        seen.len(),
        1,
        "expected exactly one crash notification, got {seen:?} — firing per \
         failed in-flight request instead of once at the flag would flood a \
         host with duplicates of one death"
    );
    assert!(
        is_closed_stream(Some(&seen[0])),
        "the crash notification carried {:?}, not a closed stream",
        seen[0]
    );
}

/// **What the server says while no command waits is heard when it arrives.**
/// The mock answers a render mode with a latency change the host never asked
/// for, and the listener must hear it with no further command sent: the control
/// lane's thread waits on the control connection, so it reads the news when it
/// arrives.
#[test]
fn an_unprompted_event_reaches_the_listener_while_no_command_waits() {
    use parking_lot::Mutex as PlMutex;

    let mock = MockServer::start("unprompted", |msg| match msg {
        HostMessage::SetRenderMode { .. } => {
            Action::Reply(Box::new(BridgeMessage::LatencyChanged {
                samples: tutti_plugin_types::Samples::new(77),
            }))
        }
        HostMessage::GetCurrentPreset => Action::Reply(Box::new(applied_fence())),
        _ => Action::Silent,
    });
    let heard: Arc<PlMutex<Vec<usize>>> = Arc::new(PlMutex::new(Vec::new()));
    let sink = Arc::clone(&heard);
    mock.bridge.audio().set_listener(Some(Arc::new(move |ev| {
        if let crate::client::audio::BridgeEvent::LatencyChanged { samples } = ev {
            sink.lock().push(samples.get());
        }
    })));

    assert!(
        mock.bridge
            .audio()
            .set_render_mode(tutti_plugin_types::RenderMode::Realtime)
    );
    tutti_fixtures::wait_until(
        "the server's unprompted latency change to be heard",
        NOTICE_TIMEOUT,
        || !heard.lock().is_empty(),
    );
    assert_eq!(*heard.lock(), [77]);
}

/// **A layout no plugin has is dropped on receipt; the next real one is kept.**
/// The peer announces a hundred thousand zero-width buses, which every width
/// check passes, then a stereo plugin's. The host hears the second layout only,
/// once, and both resyncs.
#[test]
fn a_layout_with_absurdly_many_buses_is_dropped_on_receipt() {
    use crate::client::audio::BridgeEvent;
    use parking_lot::Mutex as PlMutex;
    use tutti_plugin_types::{Layout, PluginBus};

    let silly = Layout {
        inputs: (0..100_000)
            .map(|_| PluginBus::main(ChannelLayout::from_count(0)))
            .collect(),
        outputs: Vec::new(),
    };
    let real = Layout {
        inputs: vec![PluginBus::main(ChannelLayout::STEREO)],
        outputs: vec![PluginBus::main(ChannelLayout::STEREO)],
    };
    let sent = real.clone();
    let mock = MockServer::start("silly-layout", move |msg| match msg {
        HostMessage::SetRenderMode { .. } => Action::ReplyMany(vec![
            BridgeMessage::PluginIoChanged {
                layout: Some(silly.clone()),
            },
            BridgeMessage::PluginIoChanged {
                layout: Some(sent.clone()),
            },
        ]),
        HostMessage::GetCurrentPreset => Action::Reply(Box::new(applied_fence())),
        _ => Action::Silent,
    });
    let layouts: Arc<PlMutex<Vec<Layout>>> = Arc::new(PlMutex::new(Vec::new()));
    let resyncs = Arc::new(AtomicU64::new(0));
    let (sink, count) = (Arc::clone(&layouts), Arc::clone(&resyncs));
    mock.bridge
        .audio()
        .set_listener(Some(Arc::new(move |ev| match ev {
            BridgeEvent::IoLayout(layout) => sink.lock().push(layout),
            BridgeEvent::Resync(_) => {
                count.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        })));
    assert!(
        mock.bridge
            .audio()
            .set_render_mode(tutti_plugin_types::RenderMode::Realtime)
    );
    tutti_fixtures::wait_until("the real layout", NOTICE_TIMEOUT, || {
        !layouts.lock().is_empty()
    });
    assert_eq!(*layouts.lock(), [real], "only the plausible layout is kept");
    assert!(resyncs.load(Ordering::Relaxed) >= 1, "the resync is heard");
}

/// **A slab the server opens is taken; one it never answers, or dies over, fails
/// within the deadline and leaves the caller its old slab.** The bridge only
/// reports whether the server has the slab; what the caller keeps is its own.
#[test]
fn a_slab_setup_is_answered_refused_or_given_up_on_within_the_deadline() {
    let layout = stereo_layout();
    let ready = MockServer::start("slab-ready", |msg| match msg {
        HostMessage::SetupSharedMemory { .. } => {
            Action::Reply(Box::new(BridgeMessage::SharedMemoryReady))
        }
        _ => Action::Silent,
    });
    assert!(
        ready.bridge.audio().setup_shared_memory(
            tutti_fixtures::unique_name("hostile_slab-new"),
            layout.clone()
        ),
        "the server's ready is the answer"
    );

    let refusing = MockServer::start("slab-refused", |msg| match msg {
        HostMessage::SetupSharedMemory { .. } => Action::Reply(Box::new(BridgeMessage::Error {
            message: "the slab does not open".into(),
        })),
        _ => Action::Silent,
    });
    assert!(
        !refusing.bridge.audio().setup_shared_memory(
            tutti_fixtures::unique_name("hostile_slab-refused"),
            layout.clone()
        ),
        "the server's error is a refusal"
    );

    let mut quick = Timeouts::default();
    quick.param = Duration::from_millis(300);
    let silent = MockServer::start_with_timeouts("slab-silent", quick, |_| Action::Silent);
    let silent_bridge = Arc::clone(&silent.bridge);
    let silent_layout = layout.clone();
    let answered = call_within(move || {
        silent_bridge.audio().setup_shared_memory(
            tutti_fixtures::unique_name("hostile_slab-silent"),
            silent_layout,
        )
    })
    .unwrap_or_else(|w| panic!("a silent server held the caller for {w:?}"));
    assert!(!answered, "no answer is a refusal");

    let dying = MockServer::start("slab-dies", |msg| match msg {
        HostMessage::SetupSharedMemory { .. } => Action::Die,
        _ => Action::Silent,
    });
    let dying_bridge = Arc::clone(&dying.bridge);
    let answered = call_within(move || {
        dying_bridge
            .audio()
            .setup_shared_memory(tutti_fixtures::unique_name("hostile_slab-dies"), layout)
    })
    .unwrap_or_else(|w| panic!("a server that died mid-swap held the caller for {w:?}"));
    assert!(!answered, "a server that dies mid-swap is a refusal");
    dying.wait_for_crash("the bridge to say the server is gone");
}

/// A crash that happens **before** any listener exists is still reported.
///
/// This is the case a callback-only design loses. `PluginBridge::new` spawns
/// the bridge thread and `set_listener` runs afterwards, so a peer that never
/// connects — the common shape of a bad install — dies in the gap with nobody
/// subscribed. The latch is what makes the query authoritative rather than a
/// convenience beside the event.
#[test]
fn a_crash_before_any_listener_is_still_reported_by_the_query() {
    let mock = MockServer::start("pre-listener", |_| Action::Die);

    // No listener is ever installed. Provoke the death.
    let _ = mock.bridge.audio().parameter(ParamId::new(1));
    mock.wait_for_crash("the bridge to notice the peer died");

    assert!(
        mock.bridge.audio().crash_cause().is_some(),
        "a crash with no listener installed left no cause behind, so a host \
         that subscribes later can never learn why its plugin is dead"
    );
}

/// Once crashed, further calls must fail fast rather than block.
///
/// A host that keeps waiting the full timeout on every subsequent request turns
/// one dead plugin into a UI that hangs for `Timeouts::param` per interaction. The
/// crash flag exists to short-circuit that, so it has to actually be consulted.
#[test]
fn calls_after_a_crash_fail_fast_instead_of_waiting_out_the_timeout() {
    // An hour's deadline: a call that is not short-circuited by the crash flag
    // waits on it and outlasts the call guard.
    let mock =
        MockServer::start_with_timeouts("fail-fast", unreachable_deadline(), |_| Action::Die);

    // First call establishes the crash.
    let bridge = Arc::clone(&mock.bridge);
    let _ = call_within(move || bridge.audio().parameter(ParamId::new(1)))
        .unwrap_or_else(|w| panic!("the call that meets the death blocked for {w:?}"));
    mock.wait_for_crash("the bridge to notice the server was gone");

    let bridge = Arc::clone(&mock.bridge);
    let values = call_within(move || {
        (0..5)
            .map(|_| bridge.audio().parameter(ParamId::new(1)))
            .collect::<Vec<_>>()
    })
    .unwrap_or_else(|w| {
        panic!(
            "calls to a crashed bridge were still blocked after {w:?} — the crash \
             flag is not short-circuiting, so each call waits out its timeout"
        )
    });
    assert_eq!(values, vec![None; 5], "a crashed bridge returned a value");
}

/// A crash must not leave a caller blocked forever.
///
/// `SaveState` waits on a one-shot `Ask`. If the bridge thread dies without
/// sending into the paired `Reply`, the caller depends on either the channel
/// disconnecting or its own timeout. Both are acceptable; blocking forever is
/// not, and only a bounded test can tell them apart.
#[test]
fn a_crash_during_save_state_does_not_block_the_caller_forever() {
    let mock = MockServer::start("save-death", |msg| match msg {
        HostMessage::SaveState { .. } => Action::Die,
        _ => Action::Silent,
    });

    let bridge = Arc::clone(&mock.bridge);
    let saved = call_within(move || {
        bridge
            .audio()
            .save_state(tutti_plugin_types::StateUse::Project)
    })
    .unwrap_or_else(|waited| {
        panic!(
            "save_state against a server that died mid-request was still \
             blocked after {waited:?}; the caller is waiting on a reply that \
             will never arrive"
        )
    });
    assert!(
        saved.is_err(),
        "save_state returned {saved:?} from a server that never answered"
    );
}

// ── Hostile / corrupt peers ──────────────────────────────────────────────────

/// A wildly oversized length prefix must not be honoured as an allocation.
///
/// `recv` reads a 4-byte big-endian length. The length comes off the wire, so
/// `0xFFFFFFFF` asks the host to allocate 4 GiB before a single byte of body
/// has been validated. `MAX_FRAME_BYTES` is what stops it: the length is
/// checked *before* the `vec![0u8; len]`, so nothing is allocated at all.
///
/// The body deliberately does not follow: a server that advertises 4 GiB and
/// sends 4 bytes is exactly the corrupt-framing case. The host must fail the
/// read rather than sit on the allocation.
///
/// **The deadlines are what make this test meaningful.** An unbounded 4 GiB
/// `vec![0u8; len]` lowers to `calloc`, which `mmap`s lazily — measured at
/// 60 ns on Linux with `overcommit_memory=0` — so without the bound the host
/// would allocate nothing visible and sit waiting for the body. The bridge runs
/// under an hour-long deadline, so only the length comparison, made in the
/// four bytes it takes to read the prefix, lets the call return within the
/// guard.
#[test]
fn an_absurd_length_prefix_does_not_hang_or_exhaust_memory() {
    let mock =
        MockServer::start_with_timeouts("huge-len", unreachable_deadline(), |msg| match msg {
            HostMessage::GetParameter { .. } => Action::Raw(u32::MAX, vec![0u8; 4]),
            _ => Action::Silent,
        });

    let bridge = Arc::clone(&mock.bridge);
    let value =
        call_within(move || bridge.audio().parameter(ParamId::new(1))).unwrap_or_else(|waited| {
            panic!(
                "a 4 GiB length prefix left the host blocked after {waited:?} — it \
             is allocating on an unvalidated wire length and waiting for the body"
            )
        });
    assert_eq!(
        value, None,
        "the host returned a parameter value from a frame it could not have read"
    );
    mock.wait_for_crash("the bridge to mark an unreadable frame a crash");
}

/// A frame one byte over the cap is refused; one byte under it is not.
///
/// The pair is the point. `an_absurd_length_prefix…` uses `u32::MAX`, which any
/// plausible bound rejects — it cannot tell a real limit from a stray sanity
/// check, and would still pass if the constant were off by orders of magnitude.
/// Straddling `MAX_FRAME_BYTES` pins the constant the transport actually reads.
///
/// The under-cap half deliberately advertises a length it never fills, and the
/// two branches are told apart by the failure each leaves: a rejected length
/// closes the stream, an accepted one waits for its body and gives up as a
/// timeout.
#[test]
fn the_frame_cap_is_the_boundary_it_claims_to_be() {
    use crate::MAX_FRAME_BYTES;

    let over =
        MockServer::start_with_timeouts("cap-over", unreachable_deadline(), |msg| match msg {
            HostMessage::GetParameter { .. } => Action::Raw(MAX_FRAME_BYTES as u32 + 1, Vec::new()),
            _ => Action::Silent,
        });
    let bridge = Arc::clone(&over.bridge);
    call_within(move || bridge.audio().parameter(ParamId::new(1)))
        .unwrap_or_else(|w| panic!("an over-cap frame blocked the caller for {w:?}"));
    over.wait_for_crash("the bridge to refuse a frame over the cap");
    assert!(
        matches!(
            over.bridge.audio().crash_cause(),
            Some(PluginFailure::StreamClosed)
        ),
        "MAX_FRAME_BYTES + 1 was not refused by closing the stream: {:?}",
        over.bridge.audio().crash_cause()
    );

    let mut quick = Timeouts::default();
    quick.param = Duration::from_millis(300);
    let under = MockServer::start_with_timeouts("cap-under", quick, |msg| match msg {
        HostMessage::GetParameter { .. } => Action::Raw(MAX_FRAME_BYTES as u32, Vec::new()),
        _ => Action::Silent,
    });
    let bridge = Arc::clone(&under.bridge);
    call_within(move || bridge.audio().parameter(ParamId::new(1)))
        .unwrap_or_else(|w| panic!("an at-cap frame blocked the caller for {w:?}"));
    under.wait_for_crash("the bridge to give up on the body that never comes");
    assert!(
        matches!(
            under.bridge.audio().crash_cause(),
            Some(PluginFailure::TimedOut { .. })
        ),
        "a frame of exactly MAX_FRAME_BYTES was refused rather than accepted and \
         waited on — the bound is off by one in the direction that rejects \
         legitimate traffic: {:?}",
        under.bridge.audio().crash_cause()
    );
}

/// A peer that dribbles bytes must not hold the bridge thread forever.
///
/// **The hazard a receive timeout does not cover.** `recv_within` sets
/// `SO_RCVTIMEO` and calls `read_exact`, but the timeout bounds a single
/// `recv` syscall and `read_exact` loops until its buffer is full — and the
/// timer restarts on every syscall. So a peer that delivers one byte per
/// interval never lets the timeout fire, and the host stays inside one
/// `read_exact` for as long as the peer keeps dribbling. Measured directly on
/// a `UnixStream` pair: a 300 ms `SO_RCVTIMEO` survived 2.3 s of
/// one-byte-per-200 ms.
///
/// What makes it worse than a slow request is *who* is stuck. The caller has
/// its own `Timeouts::param` and returns `None` on schedule, so the symptom is
/// invisible from the audio thread — but the bridge thread never returns from
/// the handler, so the loop never runs `crash()`, `is_crashed()` stays false, and
/// every later request is queued behind a thread that is never coming back.
/// The bridge looks healthy and answers nothing, forever.
///
/// **Two mechanisms close this, and this test covers the first.** Here the
/// advertised length is over `MAX_FRAME_BYTES`, so it is rejected before any
/// read starts and there is no multi-syscall read for the dribble to extend.
/// An *under*-cap dribble reaches the read and is stopped by the total-elapsed
/// deadline instead — see
/// [`an_under_cap_dribble_is_stopped_by_the_total_deadline`], which is the case
/// the size bound alone leaves open.
///
/// The gap is deliberately shorter than every timeout in the bridge, so a host
/// that *did* enter the read unbounded would never escape and this test would
/// fail on its deadline rather than its assertion.
#[test]
fn a_dribbling_peer_cannot_hold_the_bridge_thread_open() {
    let mock = MockServer::start("dribble", |msg| match msg {
        HostMessage::GetParameter { .. } => Action::Dribble {
            len: u32::MAX,
            gap: Duration::from_millis(50),
        },
        _ => Action::Silent,
    });

    let bridge = Arc::clone(&mock.bridge);
    let value =
        call_within(move || bridge.audio().parameter(ParamId::new(1))).unwrap_or_else(|waited| {
            panic!(
                "a dribbling peer left the caller blocked after {waited:?} — the \
                     host entered a multi-syscall read on an unvalidated length"
            )
        });
    assert_eq!(value, None, "a dribbled frame produced a value");

    // The assertion that matters. The caller returning is not evidence: it has
    // its own timeout and would return `None` on schedule even with the bridge
    // thread wedged forever. Only the crash flag distinguishes "the host
    // rejected the frame" from "the host is still inside `read_exact` and the
    // caller gave up without it".
    mock.wait_for_crash("the bridge to report a crash");
}

/// Garbage where a postcard payload should be must be rejected, not decoded.
///
/// The length is honest here and the body is the right size; only the *contents*
/// are nonsense. That isolates deserialisation from framing — a host that
/// somehow produced a message from these bytes would be reading uninitialised
/// or attacker-chosen structure.
#[test]
fn a_garbage_payload_is_rejected_rather_than_decoded() {
    // Honest length, nonsense body: this isolates *deserialisation* from
    // framing, unlike the truncated and oversized cases.
    const GARBAGE_LEN: usize = 64;
    let mock = MockServer::start("garbage", |msg| match msg {
        HostMessage::GetParameter { .. } => {
            Action::Raw(GARBAGE_LEN as u32, vec![0xABu8; GARBAGE_LEN])
        }
        _ => Action::Silent,
    });

    let value = mock.bridge.audio().parameter(ParamId::new(1));
    assert_eq!(
        value, None,
        "the host decoded a parameter value out of 64 bytes of 0xAB"
    );
    mock.wait_for_crash("the bridge to mark an undecodable payload a crash");
}

/// A frame whose body is shorter than its own length prefix must fail.
///
/// This is what a server killed *mid-write* produces: the header made it out,
/// the body did not. The host must not block forever waiting for bytes that
/// will never arrive, nor treat the short read as a complete message.
#[test]
fn a_truncated_body_is_not_mistaken_for_a_complete_message() {
    let mock = MockServer::start("truncated", |msg| match msg {
        // Advertise 4 KiB, send 8 bytes, then the closure returns and the
        // server loops — the remaining bytes never come.
        HostMessage::GetParameter { .. } => Action::Raw(4096, vec![0u8; 8]),
        _ => Action::Silent,
    });

    let bridge = Arc::clone(&mock.bridge);
    let value =
        call_within(move || bridge.audio().parameter(ParamId::new(1))).unwrap_or_else(|waited| {
            panic!(
                "a truncated frame left the host blocked after {waited:?}, waiting \
             for a body the server never finished sending"
            )
        });
    assert_eq!(value, None, "a truncated frame produced a value");
}

/// A well-formed message of the *wrong type* must not satisfy a pending request.
///
/// Every byte here is valid — this is a legal `BridgeMessage`, just not the one
/// that was asked for. A host that matched on "some reply arrived" rather than
/// on the variant would accept it and return a value it never received. The
/// existing `trailing_unsolicited_events_dont_poison_next_reply` covers
/// *unsolicited* interleaving; this covers a substituted reply.
#[test]
fn a_valid_but_wrong_reply_type_does_not_satisfy_the_request() {
    let mock = MockServer::start("wrong-type", |msg| match msg {
        // Asked for a parameter value; answer with state data instead.
        HostMessage::GetParameter { .. } => Action::Reply(Box::new(BridgeMessage::StateChunk {
            seq: 0,
            last: true,
            bytes: vec![1, 2, 3, 4],
        })),
        _ => Action::Silent,
    });

    let value = mock.bridge.audio().parameter(ParamId::new(1));
    assert_eq!(
        value, None,
        "a StateData reply was accepted as the answer to GetParameter — the \
         host is matching on 'a reply arrived', not on which reply"
    );
}

/// Submits block `seq` through the bridge's audio entry, as the node's batcher does.
fn submit_block(bridge: &PluginBridge, seq: u64) {
    let submitted = bridge.audio().submit(
        seq,
        64,
        Default::default(),
        &mut tutti_plugin_types::ParameterChanges::new(),
        Default::default(),
        Default::default(),
        tutti_plugin_types::TransportInfo::new(SampleRate::SR_48K),
        Default::default(),
    );
    assert!(submitted.accepted(), "the block was accepted");
}

/// **A wrong-kind reply to a block is a protocol error, and not an answer.**
/// A server that answers every `ProcessAudio` with junk of another kind must
/// not read as a live plugin (the hang verdict counts answers): the host ends
/// the connection typed, and counts no reply.
#[test]
fn a_wrong_kind_reply_to_a_block_is_a_protocol_error_and_not_an_answer() {
    let mock = MockServer::start("wrong-kind-block", |msg| match msg {
        HostMessage::ProcessAudio(_) => Action::Reply(Box::new(BridgeMessage::StateChunk {
            seq: 0,
            last: true,
            bytes: vec![1],
        })),
        _ => Action::Silent,
    });

    submit_block(&mock.bridge, 1);
    mock.wait_for_crash("the junk reply to end the connection");
    assert_eq!(
        mock.bridge.audio().block_counts().replies,
        0,
        "a reply of the wrong kind was counted as the plugin answering"
    );
}

/// **A reply naming a block that was never sent is refused**, not recorded as
/// that block's fate: a peer-supplied `seq` far ahead would claim a word of the
/// fate ring and make every later fate that maps to it read as unanswered.
#[test]
fn a_reply_to_a_block_never_sent_is_a_protocol_error() {
    let mock = MockServer::start("future-seq", |msg| match msg {
        HostMessage::ProcessAudio(_) => Action::Reply(Box::new(BridgeMessage::AudioProcessed {
            latency_us: 0,
            seq: u64::MAX >> 1,
            midi_out: Default::default(),
            status: Default::default(),
            refused: false,
        })),
        _ => Action::Silent,
    });

    submit_block(&mock.bridge, 1);
    mock.wait_for_crash("the reply to a block never sent to end the connection");
    assert_eq!(mock.bridge.audio().block_counts().replies, 0);
}

/// A server that accepts requests but never answers must not hang the host.
///
/// Distinct from death: the connection stays open, so there is no EOF to notice.
/// Only the timeout can end this, which makes it the test that proves the
/// timeout exists at all.
#[test]
fn a_silent_server_is_bounded_by_the_timeout() {
    let mock = MockServer::start("silent", |_| Action::Silent);

    let bridge = Arc::clone(&mock.bridge);
    let value =
        call_within(move || bridge.audio().parameter(ParamId::new(1))).unwrap_or_else(|waited| {
            panic!(
                "a server that never replies left the caller blocked for {waited:?} \
             — the request timeout is not bounding this path"
            )
        });
    assert_eq!(value, None, "a silent server produced a value");
}

/// **A refusal is not a crash.** The server refuses the first rate and takes
/// the second: the bridge reports the refusal (rate and reason) while it
/// stands, stays healthy, and clears it when a later rate is taken.
#[test]
fn a_refused_rate_is_reported_without_crashing_the_bridge() {
    let calls = std::sync::atomic::AtomicU32::new(0);
    let mock = MockServer::start("refused_rate", move |msg| match msg {
        HostMessage::SetSampleRate { rate } => {
            let first = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            Action::Reply(Box::new(BridgeMessage::SampleRateSet {
                rate,
                error: first.then(|| "no".to_string()),
            }))
        }
        _ => Action::Silent,
    });
    assert!(mock.bridge.audio().set_sample_rate(SampleRate::SR_96K));
    tutti_fixtures::wait_until("a rate refusal to be recorded", NOTICE_TIMEOUT, || {
        mock.bridge.audio().rate_refusal().is_some()
    });
    assert_eq!(
        mock.bridge.audio().rate_refusal(),
        Some((SampleRate::SR_96K, "no".to_string()))
    );
    assert!(
        !mock.bridge.audio().is_crashed(),
        "a refusal is not a crash"
    );

    assert!(mock.bridge.audio().set_sample_rate(SampleRate::SR_48K));
    tutti_fixtures::wait_until("a rate refusal to be cleared", NOTICE_TIMEOUT, || {
        mock.bridge.audio().rate_refusal().is_none()
    });
    assert!(!mock.bridge.audio().is_crashed());
}

/// **A rate change that is not answered in time is a refusal, not a crash, and
/// its late answer settles it.** The wait is 300 ms; the server
/// answers (accepted) after 800 ms. In between the bridge reports the refusal
/// ("did not answer") and stays healthy; the late reply is taken off the stream
/// and clears it.
#[test]
fn an_unanswered_rate_change_is_a_refusal_and_its_late_answer_clears_it() {
    let mock = MockServer::start_with_timeouts("late_rate", rate_timeout(300), |msg| match msg {
        HostMessage::SetSampleRate { rate } => Action::ReplyManySlowly {
            msgs: vec![BridgeMessage::SampleRateSet { rate, error: None }],
            gap: Duration::from_millis(800),
        },
        HostMessage::GetParameter { .. } => {
            Action::Reply(Box::new(BridgeMessage::ParameterValue {
                value: Some(Normalized::of(0.5)),
            }))
        }
        _ => Action::Silent,
    });
    assert!(mock.bridge.audio().set_sample_rate(SampleRate::SR_96K));
    tutti_fixtures::wait_until("a rate refusal to be recorded", NOTICE_TIMEOUT, || {
        mock.bridge.audio().rate_refusal().is_some()
    });
    let (rate, reason) = mock.bridge.audio().rate_refusal().unwrap();
    assert_eq!(rate, SampleRate::SR_96K);
    assert!(reason.contains("did not answer"), "{reason}");
    assert!(
        !mock.bridge.audio().is_crashed(),
        "an unanswered rate is not a crash"
    );

    // The bridge reads the stream when it has something to do: the next
    // command finds the late answer ahead of its own reply and applies it.
    let value = mock.bridge.audio().parameter(ParamId::new(1));
    assert_eq!(
        value,
        Some(Normalized::of(0.5)),
        "the reply is its own, not the late one"
    );
    tutti_fixtures::wait_until(
        "the late answer to clear the refusal",
        NOTICE_TIMEOUT,
        || mock.bridge.audio().rate_refusal().is_none(),
    );
    assert!(!mock.bridge.audio().is_crashed());
}

/// **A late answer is applied in stream order.** Rate change A times out
/// (1 s), then B is sent. The server answers A (accepted) while B waits,
/// then B ("no") at once. B's answer is the later one in the stream and wins:
/// the refusal stands, naming B's rate. Applied after B (the late answer parked
/// until the command ended) A's success cleared B's refusal.
///
/// The order is made by handshakes, not by delays. The server holds A's answer
/// until the test has seen A refused, so A has timed out first. B's wait is
/// the same 1 s, which B's reply (a loopback write once released) cannot miss. The
/// barrier is a round trip behind B: the bridge handles commands one at a
/// time, so B's command, and anything it applies when it ends, is done before
/// the barrier returns.
#[test]
fn a_late_rate_answer_does_not_override_a_later_one() {
    let calls = std::sync::atomic::AtomicU32::new(0);
    let (release, released) = std::sync::mpsc::channel::<()>();
    let mock =
        MockServer::start_with_timeouts("late_then_refused", rate_timeout(1_000), move |msg| {
            match msg {
                HostMessage::SetSampleRate { rate } => {
                    if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                        // A: accepted, but only once the test has seen the bridge give up.
                        if released.recv().is_err() {
                            return Action::Silent;
                        }
                        Action::Reply(Box::new(BridgeMessage::SampleRateSet { rate, error: None }))
                    } else {
                        Action::Reply(Box::new(BridgeMessage::SampleRateSet {
                            rate,
                            error: Some("no".to_string()),
                        }))
                    }
                }
                HostMessage::GetParameter { .. } => {
                    Action::Reply(Box::new(BridgeMessage::ParameterValue {
                        value: Some(Normalized::of(0.5)),
                    }))
                }
                _ => Action::Silent,
            }
        });
    assert!(mock.bridge.audio().set_sample_rate(SampleRate::SR_96K));
    tutti_fixtures::wait_until("a rate refusal to be recorded", NOTICE_TIMEOUT, || {
        mock.bridge.audio().rate_refusal().is_some()
    });
    // B, queued behind A's wait; its answer follows A's on the stream.
    assert!(mock.bridge.audio().set_sample_rate(SampleRate::SR_88K2));
    release.send(()).expect("the mock is listening");
    tutti_fixtures::wait_until("B's refusal to be recorded", NOTICE_TIMEOUT, || {
        mock.bridge
            .audio()
            .rate_refusal()
            .is_some_and(|(rate, _)| rate == SampleRate::SR_88K2)
    });
    // Barrier: B's command has ended, with everything it applies.
    assert_eq!(
        mock.bridge.audio().parameter(ParamId::new(1)),
        Some(Normalized::of(0.5))
    );
    assert_eq!(
        mock.bridge.audio().rate_refusal(),
        Some((SampleRate::SR_88K2, "no".to_string())),
        "the later answer stands"
    );
    assert!(!mock.bridge.audio().is_crashed());
}

/// **Dropping the bridge ends a wait for a rate change that never answers.**
/// The bridge thread waits up to `Timeouts::rate` (10 s) for the reply, and the
/// drop joins it: the wait must notice the shutdown, not run out.
#[test]
fn dropping_the_bridge_ends_a_hung_rate_change() {
    let (asked, request_seen) = crossbeam_channel::bounded(1);
    let mut mock = MockServer::start("hung_rate", move |msg| {
        if matches!(msg, HostMessage::SetSampleRate { .. }) {
            let _ = asked.try_send(());
        }
        Action::Silent
    });
    assert!(mock.bridge.audio().set_sample_rate(SampleRate::SR_96K));
    // The request is on the wire, so the bridge thread is in the wait.
    request_seen
        .recv_timeout(NOTICE_TIMEOUT)
        .expect("the peer was never asked for the rate change");

    let thread = mock.thread.take().expect("the bridge thread");
    let (done, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        drop(thread);
        let _ = done.send(());
    });
    assert!(
        rx.recv_timeout(NOTICE_TIMEOUT).is_ok(),
        "the drop was still joining a bridge thread waiting out a rate change"
    );
}

/// **A bridge asked to stop tells the server to shut down**, on the control
/// connection: the server destroys its plugin before it exits, which a kill
/// never lets it do. The stop is a request the guard makes, and the bridge
/// declares nothing for the end the server then makes.
#[test]
fn a_bridge_asked_to_stop_tells_the_server_to_shut_down() {
    let (heard, shutdown_seen) = crossbeam_channel::bounded(1);
    let mock = MockServer::start("shutdown", move |msg| {
        if matches!(msg, HostMessage::Shutdown) {
            let _ = heard.try_send(());
        }
        Action::Silent
    });

    mock.thread
        .as_ref()
        .expect("the bridge thread")
        .request_stop();

    shutdown_seen
        .recv_timeout(NOTICE_TIMEOUT)
        .expect("the server was never told to shut down");
    assert!(
        !mock.bridge.audio().is_crashed(),
        "a requested stop is no death"
    );
}

/// The stand-in server of the guard's shutdown test is a shell.
#[cfg(unix)]
mod shell_server {
    use super::*;

    /// **Dropping the guard lets the server run its own shutdown before it ends
    /// the process**: the stand-in server does what a plugin's `destroy` does (it
    /// writes a file) when it is told `Shutdown`, and a guard that killed it first
    /// would never see the file. The mock tells the stand-in by a line on its stdin
    /// when the bridge's `Shutdown` arrives.
    #[test]
    fn dropping_the_guard_lets_the_server_finish_its_shutdown_before_it_is_killed() {
        use std::process::{Command, Stdio};

        let dir = tempfile::tempdir().expect("a scratch directory");
        let marker = dir.path().join("destroyed");
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("read _; echo > \"$0\"")
            .arg(&marker)
            .stdin(Stdio::piped())
            .spawn()
            .expect("the stand-in server spawns");
        let stdin = Arc::new(std::sync::Mutex::new(child.stdin.take().expect("piped")));
        let tell = Arc::clone(&stdin);
        let mut mock = MockServer::start("guard_shutdown", move |msg| {
            if matches!(msg, HostMessage::Shutdown) {
                let _ = writeln!(tell.lock().unwrap());
            }
            Action::Silent
        });
        let mut guard = crate::ProcessGuard::launched(
            tutti_plugin_ipc_ffi::Reaped::new(child),
            &crate::BridgeConfig::at(mock.socket.clone()),
        )
        .expect("the exit thread starts");
        guard.attach(mock.thread.take().expect("the bridge thread"));

        drop(guard);

        assert!(
            marker.exists(),
            "the server was killed before it finished its shutdown"
        );
    }
}

/// A dribble **under** the size cap must still end, on the clock.
///
/// This is the hazard `MAX_FRAME_BYTES` does not touch, and the reason the
/// deadline exists as a separate mechanism. The advertised length here is a
/// perfectly ordinary one — the size of a real reply — so the size check passes
/// it and the host enters `read_exact`. Every real preset frame is in this
/// range, so "the cap makes dribbling safe" was never true; it only moved the
/// ceiling from `u32::MAX` to 64 MiB, and both are unbounded in practice.
///
/// The assertion is on the **crash flag**, not on the caller's return: the
/// caller has its own `Timeouts::param` and answers `None` on schedule whether or
/// not the bridge thread is wedged, so only `is_crashed` distinguishes "the
/// read gave up" from "the read is still running and the caller left".
///
/// The gap is far shorter than `Timeouts::param`, so a per-syscall bound would
/// never fire and this test would fail on `wait_for_crash` — which is exactly
/// how it fails when the deadline is removed.
#[test]
fn an_under_cap_dribble_is_stopped_by_the_total_deadline() {
    let mock = MockServer::start("dribble-under", |msg| match msg {
        HostMessage::GetParameter { .. } => Action::Dribble {
            // Comfortably under MAX_FRAME_BYTES: the size check must let this
            // through, or the test proves nothing about the deadline.
            len: 4096,
            gap: Duration::from_millis(50),
        },
        _ => Action::Silent,
    });

    let bridge = Arc::clone(&mock.bridge);
    let value =
        call_within(move || bridge.audio().parameter(ParamId::new(1))).unwrap_or_else(|waited| {
            panic!("an under-cap dribbled frame left the caller blocked after {waited:?}")
        });
    assert_eq!(value, None, "a dribbled frame produced a value");
    mock.wait_for_crash("the bridge to report a crash");
}

/// A large state survives the round trip, in both directions.
///
/// A state over `MAX_FRAME_BYTES` must travel as chunks, not fail as a *crash*
/// (`save_state` answering "the bridge process crashed"), which would be
/// indistinguishable from a segfault and lose the user's preset under a wrong
/// explanation.
///
/// **Sized by `STATE_CHUNK_BYTES`, not by `MAX_FRAME_BYTES`.** What this test
/// has to prove is that a state spanning *several* chunks reassembles in order
/// and intact, and the chunk size is what decides that — a 68 MiB fixture
/// would prove the same property while moving 68 MiB through a socket, which
/// under parallel load overruns the harness's 10 s deadline and fails as a timeout
/// rather than a defect. Three chunks and a remainder exercises every boundary
/// the reassembler has: first, middle, last, and a partial final chunk.
#[test]
fn a_state_larger_than_one_frame_round_trips() {
    use crate::STATE_CHUNK_BYTES;

    // A byte pattern, not zeros: a chunking bug that drops or reorders a piece
    // leaves the length right and the contents wrong, which zeros would hide.
    let big: Vec<u8> = (0..STATE_CHUNK_BYTES * 3 + 4096)
        .map(|i| (i % 251) as u8)
        .collect();
    let expected = big.clone();

    let mock = MockServer::start("big-state", move |msg| match msg {
        HostMessage::SaveState { .. } => Action::ReplyMany(
            crate::transport::state_chunk::split(&big)
                .map(|(seq, last, bytes)| BridgeMessage::StateChunk {
                    seq,
                    last,
                    bytes: bytes.to_vec(),
                })
                .collect(),
        ),
        _ => Action::Silent,
    });

    let bridge = Arc::clone(&mock.bridge);
    let got = call_within(move || {
        bridge
            .audio()
            .save_state(tutti_plugin_types::StateUse::Project)
    })
    .unwrap_or_else(|w| panic!("save_state on a multi-chunk state blocked for {w:?}"));
    let got = got.unwrap_or_else(|e| {
        panic!(
            "a {}-byte state failed with {e} — a state larger than one frame \
             must be carried, not refused",
            expected.len()
        )
    });
    assert_eq!(
        got.len(),
        expected.len(),
        "reassembled state has the wrong length"
    );
    assert!(
        got == expected,
        "reassembled state does not match what was sent"
    );

    // Sanity: the fixture really did span several chunks. Without this the
    // test would still pass if `STATE_CHUNK_BYTES` were raised past the
    // fixture size, silently reverting to the single-frame shape it exists to
    // distinguish itself from.
    assert!(
        crate::transport::state_chunk::split(&expected).count() >= 4,
        "the fixture no longer spans multiple chunks, so this test would pass \
         without chunking existing"
    );
}

/// An over-limit `load_state` is a typed refusal, not a crash.
///
/// Separate properties, which is why the assertions are separate:
///
/// - **Typed.** Not `Rejected("the plugin did not answer within the state
///   timeout")`, which says the plugin refused or hung. It did neither — the
///   host declined to send. `TooLarge` names the real reason and carries both
///   numbers.
/// - **Answered.** The refusal is a length comparison and the reply is sent
///   with it; the peer here never answers anything, so a refusal that waited
///   on the peer would surface as `Stalled`, not `TooLarge`.
/// - **Non-fatal.** a lane's loop treats any handler error as connection-level, so
///   one oversized preset must not call `crash()` on a healthy session. Nothing
///   was written, so the socket is still synchronised and there is nothing to
///   crash about — which is what separates this from an over-cap *receive*.
#[test]
fn an_oversized_load_state_is_refused_without_killing_the_bridge() {
    use tutti_plugin_types::MAX_STATE_BYTES;

    let mock = MockServer::start("load-too-big", |_| Action::Silent);

    // Not actually allocated at full size: `load_state` refuses on the length,
    // so a `Vec` of the right length is what the check sees. Allocating a
    // gigabyte to prove a comparison would make this test unrunnable.
    let oversized = vec![0u8; MAX_STATE_BYTES + 1];
    let err = mock
        .bridge
        .load_state(
            main_thread(),
            &oversized,
            tutti_plugin_types::StateUse::Project,
        )
        .expect_err("an over-limit state was accepted");

    assert!(
        matches!(err, StateError::TooLarge { .. }),
        "an over-limit state reported {err:?}; the caller cannot tell a host \
         limit from a plugin that refused or a subprocess that died"
    );
    assert!(
        !mock.bridge.audio().is_crashed(),
        "an over-limit state killed the bridge; nothing was written, so the \
         socket is still synchronised and the session was healthy"
    );
    // And the session must still work afterwards, which is the property
    // `is_crashed` only implies.
    assert!(
        mock.bridge
            .load_state(
                main_thread(),
                b"small",
                tutti_plugin_types::StateUse::Project
            )
            .is_err(),
        "a later call should still reach the (silent) server and time out, \
         rather than short-circuit on a latched crash"
    );
}

// ── State transfer: progress, not a total ────────────────────────────────────

/// A *steadily progressing* transfer must complete, however long it takes in
/// total.
///
/// A fixed budget for the whole transfer would make
/// [`MAX_STATE_BYTES`](tutti_plugin_types::MAX_STATE_BYTES) unreachable. That a
/// transfer advancing inside its window is live however many windows it spans
/// is the `StallWatch` rule, tested over constructed instants in
/// `client::audio::progress`; this test carries the reassembly through the
/// bridge: 60 chunks, a byte each, arriving with a gap, come back in order.
/// The window and the guard are far past anything a loaded machine spends, and
/// the test asserts completion, never how long it took.
#[test]
fn a_chunked_state_transfer_reassembles_in_order() {
    const CHUNKS: usize = 60;
    const GAP: Duration = Duration::from_millis(2);

    // One byte per chunk; the byte pattern catches a chunk dropped or
    // reordered by a progress-accounting bug.
    let expected: Vec<u8> = (0..CHUNKS).map(|i| (i % 251) as u8).collect();
    let msgs: Vec<BridgeMessage> = (0..CHUNKS)
        .map(|i| BridgeMessage::StateChunk {
            seq: i as u32,
            last: i == CHUNKS - 1,
            bytes: vec![(i % 251) as u8],
        })
        .collect();

    let mock = MockServer::start_with_state_progress_timeout(
        "slow-state",
        Duration::from_secs(60),
        move |msg| match msg {
            HostMessage::SaveState { .. } => Action::ReplyManySlowly {
                msgs: msgs.clone(),
                gap: GAP,
            },
            _ => Action::Silent,
        },
    );

    let bridge = Arc::clone(&mock.bridge);
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send(
            bridge
                .audio()
                .save_state(tutti_plugin_types::StateUse::Project),
        );
    });
    let got = rx
        .recv_timeout(Duration::from_secs(60))
        .expect("a progressing save_state never returned");
    let got = got.unwrap_or_else(|e| panic!("a progressing transfer failed with {e}"));
    assert_eq!(
        got, expected,
        "the reassembled state does not match what the peer sent"
    );
}

/// A `save_state` that *stops* must fail as a typed stall, promptly, and leave
/// a session that still works.
///
/// The complement of the test above: tolerating a slow transfer is only correct
/// if a stalled one is still caught. Four properties:
///
/// - **Typed.** [`StateError::Stalled`] and not `Rejected`. The plugin refused
///   nothing — it went quiet mid-stream — and a string like
///   `"the plugin did not answer within the state timeout"` would blame it for
///   a judgement the host made. Also distinct from `TooLarge`: nothing here is
///   over any limit, and a caller that cannot tell the two apart cannot decide
///   whether retrying is worth anything.
/// - **Not crashed**, and held past the *production* deadline. The bridge
///   thread's own read expires first, and if that expiry were connection-level
///   the crash would land while the caller was still waiting.
/// - **Actually usable**, which `is_crashed()` only implies.
///
/// **The health probe is a live round trip, deliberately.** It read
/// `parameters().is_none()`, which proved nothing: `parameters` answers `None`
/// both when the bridge is crashed *and* when a healthy bridge times out
/// against a silent peer, so that assertion could not fail either way. Here the
/// peer answers `GetParameterList` with a real list, so `Some` is reachable only
/// by a command reaching a live bridge thread and a reply coming back —
/// impossible on a crashed bridge, which short-circuits on `is_crashed` before
/// it sends anything.
#[test]
fn a_stalled_save_state_fails_typed_and_leaves_the_bridge_healthy() {
    // Two chunks, then silence with `last` never sent: the transfer starts,
    // makes real progress, and then stops. Progress-then-stop is the case a
    // naive implementation gets wrong — it is neither "never answered" nor a
    // completed transfer.
    let mock = MockServer::start_with_state_progress_timeout(
        "stalled-save",
        STATE_PROGRESS_TIMEOUT,
        move |msg| match msg {
            HostMessage::SaveState { .. } => Action::ReplyManySlowly {
                msgs: (0..2)
                    .map(|i| BridgeMessage::StateChunk {
                        seq: i,
                        last: false,
                        bytes: vec![7u8; 32],
                    })
                    .collect(),
                gap: Duration::from_millis(10),
            },
            // Answered, so the health probe below is a real round trip rather
            // than another timeout.
            HostMessage::GetParameterList => {
                Action::Reply(Box::new(BridgeMessage::ParameterList {
                    parameters: Vec::new(),
                }))
            }
            _ => Action::Silent,
        },
    );

    let bridge = Arc::clone(&mock.bridge);
    let got = call_within(move || {
        bridge
            .audio()
            .save_state(tutti_plugin_types::StateUse::Project)
    })
    .unwrap_or_else(|w| panic!("a stalled save_state never returned; blocked for {w:?}"));
    let err = got.expect_err("a stalled transfer reported success");

    assert!(
        matches!(err, StateError::Stalled { .. }),
        "a stalled transfer reported {err:?}; the caller cannot tell a wedged \
         plugin from one that refused, from a state over the size limit, or \
         from a dead subprocess"
    );
    // It stalled *after* progress, and the error says so — which is what makes
    // the variant more useful than a bare timeout.
    if let StateError::Stalled { bytes, .. } = &err {
        assert_eq!(
            *bytes, 64,
            "the stall reported {bytes} bytes transferred; the count is what \
             separates a transfer that never started from one that died part-way"
        );
    }
    assert_healthy_after_stall(&mock, "save_state");
}

/// The same, for `load_state`.
///
/// A separate test rather than a parameter on the one above, because the two
/// directions stall through *different code*: `save_state` gives up waiting for
/// a chunk to arrive, `load_state` writes every chunk successfully and then
/// waits on the single `StateLoaded` acknowledgement that never comes. Only the
/// receive path was covered, so the load path's timeout — a distinct call
/// site, the `LoadState` arm's `answer` — was crashing the bridge with no test
/// to say so.
#[test]
fn a_stalled_load_state_fails_typed_and_leaves_the_bridge_healthy() {
    let mock = MockServer::start_with_state_progress_timeout(
        "stalled-load",
        STATE_PROGRESS_TIMEOUT,
        move |msg| match msg {
            // Chunks are accepted and never acknowledged: the transfer makes
            // full progress and then hangs on the answer.
            HostMessage::LoadStateChunk { .. } => Action::Silent,
            HostMessage::GetParameterList => {
                Action::Reply(Box::new(BridgeMessage::ParameterList {
                    parameters: Vec::new(),
                }))
            }
            _ => Action::Silent,
        },
    );

    let bridge = Arc::clone(&mock.bridge);
    let got = call_within(move || {
        bridge.load_state(
            main_thread(),
            &[3u8; 128],
            tutti_plugin_types::StateUse::Project,
        )
    })
    .unwrap_or_else(|w| panic!("a stalled load_state never returned; blocked for {w:?}"));
    let err = got.expect_err("an unacknowledged load reported success");

    assert!(
        matches!(err, StateError::Stalled { bytes: 128, .. }),
        "an unacknowledged load reported {err:?}; the bytes went out but the \
         plugin never confirmed it took them, which is a stall (after the 128 \
         bytes the bridge advanced the shared counter for) and not a \
         rejection, a size refusal or a crash"
    );
    assert_healthy_after_stall(&mock, "load_state");
}

/// Assert that a stall left the session genuinely usable, not merely unflagged.
///
/// Shared by both stall tests so they cannot drift into proving different
/// things. The probe is a live round trip, which a crashed bridge short-circuits
/// on `is_crashed` before it sends anything.
fn assert_healthy_after_stall(mock: &MockServer, direction: &str) {
    // The probe goes first and is the synchronisation: the bridge thread takes
    // commands in order, so a crash caused by its own read expiring lands
    // before it answers this one, and the probe cannot return `Some` after it.
    // A round trip returning `Some` is reachable only on a live bridge.
    assert!(
        mock.bridge.audio().parameters().is_some(),
        "after a stalled {direction} the bridge no longer completes a round \
         trip, so the session is dead in practice however `is_crashed` reads"
    );
    assert!(
        !mock.bridge.audio().is_crashed(),
        "a stalled {direction} crashed the bridge; the socket is intact and \
         synchronised, so a stall must not be treated as connection-level — \
         otherwise `Stalled` tells the caller to retry against a session that \
         has just been torn down"
    );
}

/// **A query that times out leaves the bridge healthy, and its late reply is
/// not the next query's answer**. The first `GetParameter` is
/// answered after the 5 s budget; the second at once. The bridge owes the
/// first reply, takes it off the stream when it lands, and gives the second
/// query its own value.
#[test]
fn a_late_query_reply_is_not_the_next_querys_answer() {
    let asked = Arc::new(AtomicU64::new(0));
    let mock = MockServer::start("late-query", {
        let asked = Arc::clone(&asked);
        move |msg| match msg {
            HostMessage::GetParameter { .. } => {
                let value = |v| BridgeMessage::ParameterValue {
                    value: Some(Normalized::of(v)),
                };
                if asked.fetch_add(1, Ordering::SeqCst) == 0 {
                    Action::ReplyManySlowly {
                        msgs: vec![value(0.25)],
                        gap: Duration::from_millis(5_300),
                    }
                } else {
                    Action::Reply(Box::new(value(0.75)))
                }
            }
            _ => Action::Silent,
        }
    });

    assert_eq!(
        mock.bridge.audio().parameter(ParamId::new(1)),
        None,
        "the first is late"
    );
    assert_eq!(
        mock.bridge.audio().parameter(ParamId::new(1)),
        Some(Normalized::of(0.75)),
        "the second query was answered with the first's late reply"
    );
    assert!(
        !mock.bridge.audio().is_crashed(),
        "a late reply crashed the bridge"
    );
}

/// **A plugin that keeps sending events does not extend a request's budget**. The first
/// `GetParameter` gets 6 s of `LatencyChanged` events at 100 ms and no answer. The bridge gives up
/// at its 5 s budget and sends the next request, so when the events stop it is already waiting at
/// the peer; with the budget restarting per event the bridge would still be waiting, until 5 s
/// after the last event.
///
/// The peer looks for the waiting request rather than the test timing the
/// answer: an oversleeping runner only lengthens the events, which widens the
/// margin. (A first version required the answer within the second caller's
/// 5 s wait, which macOS runners' 100 ms sleeps overran.)
#[test]
fn a_chatty_plugin_does_not_extend_the_budget() {
    let asked = Arc::new(AtomicU64::new(0));
    let probe = Arc::new(std::sync::atomic::AtomicU8::new(0));
    let mock = MockServer::start("chatty", {
        let asked = Arc::clone(&asked);
        let probe = Arc::clone(&probe);
        move |msg| match msg {
            HostMessage::GetParameter { .. } if asked.fetch_add(1, Ordering::SeqCst) == 0 => {
                Action::EventsThenProbe {
                    count: 60,
                    gap: Duration::from_millis(100),
                    probe: Arc::clone(&probe),
                }
            }
            HostMessage::GetParameterList => {
                Action::Reply(Box::new(BridgeMessage::ParameterList {
                    parameters: Vec::new(),
                }))
            }
            _ => Action::Silent,
        }
    });

    assert_eq!(mock.bridge.audio().parameter(ParamId::new(1)), None);
    // Sends the next request; its answer waits for the events to stop.
    let _ = mock.bridge.audio().parameters();
    tutti_fixtures::wait_until("the peer to finish its events", NOTICE_TIMEOUT, || {
        probe.load(Ordering::SeqCst) != 0
    });
    assert_eq!(
        probe.load(Ordering::SeqCst),
        PROBE_WAITING,
        "when the events stopped the bridge had not sent its next request: it \
         was still waiting on the chatty one"
    );
    assert!(
        !mock.bridge.audio().is_crashed(),
        "{:?}",
        mock.bridge.audio().crash_cause()
    );
}

/// **A live peer caught part-way through a frame at the budget is not declared
/// unable to frame.** The read finishes *that* frame within `Timeouts::drain`
/// and stops. Extending the whole read by `Timeouts::drain` instead let the next
/// frame start before the extended deadline and be caught part-way in turn,
/// and the bridge crashed on a peer whose every frame completes in 400 ms.
///
/// The peer's split is 200 ms against `Timeouts::drain`'s 500: a frame caught
/// part-way finishes with 300 ms to spare, which a loaded runner's late wakeups
/// do not use up. A 400 ms split left 100 ms and failed on a loaded macOS
/// runner.
#[test]
fn a_live_peer_mid_frame_at_the_budget_is_not_a_crash() {
    let mut shorter = Timeouts::default();
    shorter.param = Duration::from_secs(1);
    let mock = MockServer::start_with_timeouts("split-events", shorter, |msg| match msg {
        HostMessage::GetParameter { .. } => Action::SplitEvents {
            split: Duration::from_millis(200),
        },
        _ => Action::Silent,
    });

    assert_eq!(mock.bridge.audio().parameter(ParamId::new(1)), None);
    // The barrier: the bridge thread takes commands in order, so this one is
    // handled only after the first has ended, drain included. A crash the
    // first caused is latched by then.
    let _ = mock.bridge.audio().parameters();
    assert!(
        !mock.bridge.audio().is_crashed(),
        "a live peer was declared unable to frame: {:?}",
        mock.bridge.audio().crash_cause()
    );
}

/// **The rest of a stalled state is taken off the stream, not read as the next
/// answer**. `SaveState` stalls before its first chunk; the two
/// chunks land later, ahead of the answer to `GetParameterList`.
#[test]
fn the_rest_of_a_stalled_state_is_not_the_next_answer() {
    let mock = MockServer::start_with_state_progress_timeout(
        "stalled-rest",
        STATE_PROGRESS_TIMEOUT,
        |msg| match msg {
            HostMessage::SaveState { .. } => Action::ReplyManySlowly {
                msgs: (0..2)
                    .map(|seq| BridgeMessage::StateChunk {
                        seq,
                        last: seq == 1,
                        bytes: vec![7u8; 32],
                    })
                    .collect(),
                gap: STATE_PROGRESS_TIMEOUT * 3 / 2,
            },
            HostMessage::GetParameterList => {
                Action::Reply(Box::new(BridgeMessage::ParameterList {
                    parameters: Vec::new(),
                }))
            }
            _ => Action::Silent,
        },
    );

    let err = mock
        .bridge
        .audio()
        .save_state(tutti_plugin_types::StateUse::Project)
        .expect_err("the transfer stalls");
    assert!(
        matches!(err, StateError::Stalled { bytes: 0, .. }),
        "{err:?}"
    );
    assert!(
        mock.bridge.audio().parameters().is_some(),
        "a late state chunk was read as the parameter list's answer"
    );
    assert!(!mock.bridge.audio().is_crashed());
}

/// **A control call made while a control reply is owed waits for it** and is
/// answered once the slot is free, in place of failing: a project save must not
/// fail during an editor open. The first request (a parameter list) is never
/// answered; the bridge gives up on it at its own constructed deadline (under
/// one receive slice, so it is not rounded up), which frees the slot, and the second call (a parameter value, which has the slot
/// only by waiting) is then sent and answered. A slot that refused the second
/// call at once returns `None` for it, whatever the timing.
#[test]
fn a_second_control_call_while_a_reply_is_owed_waits_for_it() {
    let log = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let (seen, seen_rx) = crossbeam_channel::bounded(1);
    let timeouts = {
        let mut t = Timeouts::default();
        t.param = Duration::from_millis(200);
        t
    };
    let mock = {
        let log = Arc::clone(&log);
        MockServer::start_with_timeouts("queued", timeouts, move |msg| match msg {
            HostMessage::GetParameterList => {
                log.lock().unwrap().push("list");
                let _ = seen.try_send(());
                Action::Silent
            }
            HostMessage::GetParameter { .. } => {
                log.lock().unwrap().push("value");
                Action::Reply(Box::new(BridgeMessage::ParameterValue {
                    value: Some(Normalized::of(0.25)),
                }))
            }
            _ => Action::Silent,
        })
    };
    let bridge = Arc::clone(&mock.bridge);
    let first = std::thread::spawn(move || bridge.audio().parameters());
    seen_rx
        .recv_timeout(NOTICE_TIMEOUT)
        .expect("the first request reached the server");

    // The first request owes its reply until the bridge's deadline passes.
    assert_eq!(
        mock.bridge.audio().parameter(ParamId::new(2)),
        Some(Normalized::of(0.25)),
        "the second call failed instead of waiting for the slot"
    );
    assert!(first.join().expect("the first caller").is_none());
    assert_eq!(*log.lock().unwrap(), ["list", "value"]);
}

// ── The two lanes ────────────────────────────────────────────────────────────

/// The audio lane's script for these tests: every block is answered.
fn answer_blocks(msg: HostMessage) -> Action {
    match msg {
        HostMessage::ProcessAudio(data) => Action::Reply(Box::new(BridgeMessage::AudioProcessed {
            latency_us: 0,
            seq: data.seq,
            midi_out: Default::default(),
            status: Default::default(),
            refused: false,
        })),
        _ => Action::Silent,
    }
}

/// Whether block `seq` has been answered on the ring.
fn answered(mock: &mut MockServer, seq: u64) -> bool {
    let (mut midi, mut status) = (Default::default(), Default::default());
    mock.replies.take(seq, &mut midi, &mut status)
}

/// A control lane that holds the first `GetParameter` until `release` says go,
/// telling `seen` it arrived, and answers every later one at once.
fn hold_first_query(
    seen: crossbeam_channel::Sender<()>,
    release: crossbeam_channel::Receiver<()>,
) -> impl Fn(HostMessage) -> Action + Send + 'static {
    let first = std::sync::atomic::AtomicBool::new(true);
    move |msg| match msg {
        HostMessage::GetParameter { .. } => {
            if first.swap(false, Ordering::SeqCst) {
                seen.send(()).expect("the test is listening");
                release.recv().expect("the test releases the request");
            }
            Action::Reply(Box::new(BridgeMessage::ParameterValue {
                value: Some(Normalized::of(0.5)),
            }))
        }
        _ => Action::Silent,
    }
}

/// **A block is answered while a control request is still outstanding.** The
/// control lane holds a query unanswered; a block sent meanwhile completes on
/// the audio lane. One thread serving both lanes parks in the query and never
/// sends the block.
#[test]
fn a_block_completes_while_a_control_request_is_outstanding() {
    let (seen, seen_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release) = crossbeam_channel::bounded(1);
    let mut mock = MockServer::start_lanes("block-beside-control", answer_blocks, {
        hold_first_query(seen, release)
    });
    let bridge = Arc::clone(&mock.bridge);
    let query = std::thread::spawn(move || bridge.audio().parameter(ParamId::new(1)));
    seen_rx
        .recv_timeout(NOTICE_TIMEOUT)
        .expect("the query reached the control lane");

    submit_block(&mock.bridge, 1);
    tutti_fixtures::wait_until(
        "the block to be answered while the query is outstanding",
        NOTICE_TIMEOUT,
        || answered(&mut mock, 1),
    );

    release_tx.send(()).expect("the control lane is waiting");
    assert_eq!(
        query.join().expect("the query's thread"),
        Some(Normalized::of(0.5))
    );
}

/// **A settle is ordered behind the settings queued before it.** The rate
/// change goes through the audio lane's queue; the settle's fence must reach the
/// server after it, whatever the two threads' timing.
#[test]
fn a_settle_reaches_the_server_after_the_rate_change_queued_before_it() {
    let log = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let control = {
        let log = Arc::clone(&log);
        move |msg: HostMessage| match msg {
            HostMessage::SetSampleRate { rate } => {
                log.lock().unwrap().push("rate");
                Action::Reply(Box::new(BridgeMessage::SampleRateSet { rate, error: None }))
            }
            HostMessage::GetCurrentPreset => {
                log.lock().unwrap().push("fence");
                Action::Reply(Box::new(applied_fence()))
            }
            _ => Action::Silent,
        }
    };
    let mock = MockServer::start_lanes("settle-order", answer_blocks, control);
    assert!(mock.bridge.audio().set_sample_rate(SampleRate::SR_96K));
    let bridge = Arc::clone(&mock.bridge);
    call_within(move || bridge.audio().settle()).expect("the settle returns");
    assert_eq!(*log.lock().unwrap(), ["rate", "fence", "fence"]);
}

/// **A crash the control lane sees is answered on the reply ring by the audio
/// lane's thread**, the ring's one producer: the control lane only wakes it.
/// Nothing is in flight on the audio lane, so the `Error` for every block
/// (`seq: None`) can only be the woken thread's.
#[test]
fn a_crash_the_control_lane_sees_is_answered_by_the_audio_lane() {
    let mut mock = MockServer::start_lanes(
        "control-death",
        |_| Action::Silent,
        |msg| match msg {
            HostMessage::GetParameter { .. } => Action::Die,
            _ => Action::Silent,
        },
    );
    assert_eq!(mock.bridge.audio().parameter(ParamId::new(1)), None);
    mock.wait_for_crash("the control lane's end to be seen");
    tutti_fixtures::wait_until(
        "the audio lane's thread to answer the crash",
        NOTICE_TIMEOUT,
        || answered(&mut mock, 99),
    );
}

/// **A request that finds the control queue full is `Busy`, and counted.** The
/// control lane is held in a render mode's fence, which owes no request slot, and
/// its queue is filled behind it.
#[test]
fn a_control_call_on_a_full_queue_is_busy() {
    let (seen, seen_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release) = crossbeam_channel::bounded::<()>(1);
    let mock = MockServer::start_lanes("full", answer_blocks, move |msg| match msg {
        HostMessage::GetCurrentPreset => {
            let _ = seen.try_send(());
            let _ = release.recv();
            Action::Reply(Box::new(applied_fence()))
        }
        _ => Action::Silent,
    });
    assert!(
        mock.bridge
            .audio()
            .set_render_mode(tutti_plugin_types::RenderMode::Offline)
    );
    seen_rx
        .recv_timeout(NOTICE_TIMEOUT)
        .expect("the control lane is held in the fence");
    while mock
        .bridge
        .audio()
        .set_automation_state_rt(tutti_plugin_types::AutomationMode::Off)
        == tutti_plugin_types::Delivered::Yes
    {}

    let refused = mock.bridge.audio().commands_refused_cell();
    let before = refused.get();
    assert_eq!(
        mock.bridge
            .audio()
            .save_state(tutti_plugin_types::StateUse::Project),
        Err(StateError::Busy)
    );
    assert!(refused.get() > before, "the refusal is counted");
    release_tx.send(()).expect("the control lane is waiting");
}

/// **A shutdown ends while a control request is stuck in the server.** The
/// audio lane's thread joins the control lane's, which is in a query nobody
/// answers; it leaves when that query's own deadline (constructed here, short)
/// passes. What is asserted is that the shutdown completes, not how fast.
#[test]
fn a_shutdown_ends_when_a_control_request_is_stuck_in_the_server() {
    let (seen, seen_rx) = crossbeam_channel::bounded(1);
    let timeouts = {
        let mut t = Timeouts::default();
        t.param = Duration::from_millis(200);
        t
    };
    let mut mock =
        MockServer::start_lanes_under("stuck-shutdown", timeouts, answer_blocks, move |msg| {
            if matches!(msg, HostMessage::GetParameter { .. }) {
                let _ = seen.try_send(());
            }
            Action::Silent
        });
    let bridge = Arc::clone(&mock.bridge);
    let query = std::thread::spawn(move || bridge.audio().parameter(ParamId::new(1)));
    seen_rx
        .recv_timeout(NOTICE_TIMEOUT)
        .expect("the query is stuck in the server");

    let mut thread = mock.thread.take().expect("the bridge thread");
    call_within(move || thread.shutdown()).expect("the shutdown completes");
    assert_eq!(query.join().expect("the query's thread"), None);
}

/// **A setting the control lane cannot take in time is counted, not lost
/// silently.** The control lane is held in an editor open the server never
/// answers; a render mode queued meanwhile is held for by the audio lane until
/// its constructed deadline passes, then counted unapplied.
#[test]
fn a_setting_the_control_lane_cannot_take_in_time_is_counted() {
    let (seen, seen_rx) = crossbeam_channel::bounded(1);
    let timeouts = {
        let mut t = Timeouts::default();
        t.param = Duration::from_millis(100);
        t.rate = Duration::from_millis(10);
        t.editor = Duration::from_secs(3_600);
        t
    };
    let mock = MockServer::start_lanes_under("unapplied", timeouts, answer_blocks, move |msg| {
        if matches!(msg, HostMessage::OpenEditor { .. }) {
            let _ = seen.try_send(());
        }
        Action::Silent
    });
    let bridge = Arc::clone(&mock.bridge);
    std::thread::spawn(move || {
        let _ = bridge
            .audio()
            .open_editor_in_server(None, tutti_plugin_types::ContentScale::IDENTITY);
    });
    seen_rx
        .recv_timeout(NOTICE_TIMEOUT)
        .expect("the control lane is held in the editor open");

    assert_eq!(mock.bridge.audio().unapplied_settings(), 0);
    assert!(
        mock.bridge
            .audio()
            .set_render_mode(tutti_plugin_types::RenderMode::Offline)
    );
    tutti_fixtures::wait_until("the render mode to be given up on", NOTICE_TIMEOUT, || {
        mock.bridge.audio().unapplied_settings() == 1
    });
}

/// A control script that holds a setting while a block may overtake it: the
/// setting's request waits up to a window for the block to arrive at the audio
/// lane's script (it ends at once if one does, and is only waited out by a host
/// that holds the block back), then logs `name`.
fn log_after_window(
    log: &Arc<std::sync::Mutex<Vec<&'static str>>>,
    block_rx: &crossbeam_channel::Receiver<()>,
    name: &'static str,
) {
    let _ = block_rx.recv_timeout(Duration::from_millis(300));
    log.lock().unwrap().push(name);
}

/// The audio lane's script for the ordering tests: logs the block, then answers.
fn log_blocks(
    log: Arc<std::sync::Mutex<Vec<&'static str>>>,
    block_seen: crossbeam_channel::Sender<()>,
) -> impl Fn(HostMessage) -> Action + Send + 'static {
    move |msg| {
        if matches!(msg, HostMessage::ProcessAudio(_)) {
            log.lock().unwrap().push("block");
            let _ = block_seen.try_send(());
        }
        answer_blocks(msg)
    }
}

/// **A render mode is applied before the next block is sent.** The control lane
/// gives the mode a window to be overtaken by a block; a block held for the
/// mode's fence is not, and the server's two lanes see the mode, its fence and
/// then the block. A bridge that stops holding the blocks logs the block first.
#[test]
fn a_render_mode_is_applied_before_the_next_block() {
    let log = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let (block_seen, block_rx) = crossbeam_channel::bounded(1);
    let control = {
        let log = Arc::clone(&log);
        move |msg: HostMessage| match msg {
            HostMessage::SetRenderMode { .. } => {
                log_after_window(&log, &block_rx, "mode");
                Action::Silent
            }
            HostMessage::GetCurrentPreset => {
                log.lock().unwrap().push("fence");
                Action::Reply(Box::new(applied_fence()))
            }
            _ => Action::Silent,
        }
    };
    let mut mock = MockServer::start_lanes(
        "render-mode-order",
        log_blocks(Arc::clone(&log), block_seen),
        control,
    );

    assert!(
        mock.bridge
            .audio()
            .set_render_mode(tutti_plugin_types::RenderMode::Offline)
    );
    submit_block(&mock.bridge, 1);
    tutti_fixtures::wait_until("the block to be answered", NOTICE_TIMEOUT, || {
        answered(&mut mock, 1)
    });
    assert_eq!(*log.lock().unwrap(), ["mode", "fence", "block"]);
}

/// **A rate change is applied before the next block is sent**, as a render mode
/// is: the server sees the rate change, and only then the block.
#[test]
fn a_rate_change_is_applied_before_the_next_block() {
    let log = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let (block_seen, block_rx) = crossbeam_channel::bounded(1);
    let control = {
        let log = Arc::clone(&log);
        move |msg: HostMessage| match msg {
            HostMessage::SetSampleRate { rate } => {
                log_after_window(&log, &block_rx, "rate");
                Action::Reply(Box::new(BridgeMessage::SampleRateSet { rate, error: None }))
            }
            _ => Action::Silent,
        }
    };
    let mut mock = MockServer::start_lanes(
        "rate-order",
        log_blocks(Arc::clone(&log), block_seen),
        control,
    );

    assert!(mock.bridge.audio().set_sample_rate(SampleRate::SR_96K));
    submit_block(&mock.bridge, 1);
    tutti_fixtures::wait_until("the block to be answered", NOTICE_TIMEOUT, || {
        answered(&mut mock, 1)
    });
    assert_eq!(*log.lock().unwrap(), ["rate", "block"]);
}

/// **A control command pushed after a block reaches the server after it.** With
/// a thread per lane nothing else orders them: the audio lane is held for a
/// render mode's fence (the control mock holds it), a block and then an
/// automation mode are pushed, and once the fence is answered the control lane
/// has the command to send while the audio lane is still waking to send the
/// block. The server's log must read mode, fence, block, command; a bridge
/// that does not make the command wait for the block logs it before the block.
#[test]
fn a_control_command_pushed_after_a_block_reaches_the_server_after_it() {
    let log = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
    let (fence_seen, fence_rx) = crossbeam_channel::bounded(1);
    let (release_tx, release) = crossbeam_channel::bounded::<()>(1);
    let audio = {
        let log = Arc::clone(&log);
        move |msg: HostMessage| {
            if matches!(msg, HostMessage::ProcessAudio(_)) {
                log.lock().unwrap().push("block");
            }
            answer_blocks(msg)
        }
    };
    let control = {
        let log = Arc::clone(&log);
        move |msg: HostMessage| match msg {
            HostMessage::SetRenderMode { .. } => {
                log.lock().unwrap().push("mode");
                Action::Silent
            }
            HostMessage::GetCurrentPreset => {
                log.lock().unwrap().push("fence");
                fence_seen.send(()).expect("the test listens");
                release.recv().expect("the test releases the fence");
                Action::Reply(Box::new(applied_fence()))
            }
            HostMessage::SetAutomationState { .. } => {
                log.lock().unwrap().push("command");
                Action::Silent
            }
            _ => Action::Silent,
        }
    };
    let mut mock = MockServer::start_lanes("control-after-block", audio, control);

    assert!(
        mock.bridge
            .audio()
            .set_render_mode(tutti_plugin_types::RenderMode::Offline)
    );
    submit_block(&mock.bridge, 1);
    fence_rx
        .recv_timeout(NOTICE_TIMEOUT)
        .expect("the control lane is held in the fence");
    assert_eq!(
        mock.bridge
            .audio()
            .set_automation_state_rt(tutti_plugin_types::AutomationMode::Reading),
        tutti_plugin_types::Delivered::Yes
    );
    release_tx.send(()).expect("the control lane is waiting");
    tutti_fixtures::wait_until("the block to be answered", NOTICE_TIMEOUT, || {
        answered(&mut mock, 1)
    });
    tutti_fixtures::wait_until("the command to reach the server", NOTICE_TIMEOUT, || {
        log.lock().unwrap().contains(&"command")
    });
    assert_eq!(*log.lock().unwrap(), ["mode", "fence", "block", "command"]);
}
