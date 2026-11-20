//! The event loop over a scripted host and the reference CLAP plugin: what it
//! serves while the host is quiet, and the editor on the served instance.
//!
//! The host is a script of messages and silences on the control connection, so
//! the loop runs on the test's own thread (it holds the `MainThreadMarker` token)
//! against real time: a silence is slept out in `wait_readable`, as the
//! socket's would be. The audio connection is a real one in the same sense: the
//! render thread serves it, over an in-memory pair of channels that the host's
//! end ([`HostEnd`]) sends blocks into from wherever a test wants.

use std::collections::VecDeque;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;
use tutti_platform::Instant;

use tutti_buffers::FloatWidth;
use tutti_clap_test_plugin::{ClapSwitch, Site};
use tutti_fixtures::main_thread;
use tutti_plugin_ipc::{
    AudioReply, AudioRequest, AudioSlab, ControlFrame, ControlReply, ControlRequest,
    ProcessAudioData, ReqId, Request, read_frame, write_frame,
};
use tutti_plugin_types::{ContentScale, IpcError, IpcResult};

use super::{Native, Stream, serve_every};

mod fds;
#[cfg(not(target_os = "macos"))]
mod x11;
use crate::main_loop::Bell;
use crate::render::{RenderSide, RenderThread};
use crate::session::Session;
use crate::transport::{Alone, SocketTransport, Transport};

/// One thing the scripted host does.
#[allow(clippy::large_enum_variant)] // a test script, built once
enum Step {
    Send(ControlRequest),
    /// Does something (on the loop's thread, when the script reaches it).
    Call(Box<dyn FnOnce() + Send>),
    /// Says nothing until the condition holds over what the host has heard so
    /// far, tried on each wait: the event a test is waiting for, not a length of
    /// time. [`HANG_GUARD`] fails a script
    /// whose event never comes.
    Until(Until),
}

/// A condition the script's silence ends on, run on the loop's thread.
struct Until {
    holds: Box<dyn FnMut(&[ControlFrame]) -> bool + Send>,
    /// When the wait gives up: [`HANG_GUARD`] after the first wait on it.
    give_up_at: Option<Instant>,
}

impl Until {
    fn step(holds: impl FnMut(&[ControlFrame]) -> bool + Send + 'static) -> Step {
        Step::Until(Self {
            holds: Box::new(holds),
            give_up_at: None,
        })
    }
}

/// How long a script waits for the event it is silent until: a guard against a
/// hang, a hundred times what a loaded runner needs.
const HANG_GUARD: Duration = Duration::from_secs(60);

/// A host that follows a script and records what it is sent.
struct Script {
    steps: VecDeque<Step>,
    /// How many requests the script has sent: the id of the last.
    sent: u64,
    heard: Vec<ControlFrame>,
    /// How many times the loop has gone to wait, shared with whoever watches it.
    waits: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Run each time the loop goes to wait, on its thread, before `waits` counts it.
    on_wait: Option<Box<dyn FnMut() + Send>>,
    /// When the loop must have read the message it was told was readable.
    message_due_by: Option<Instant>,
}

impl Script {
    fn new(steps: impl IntoIterator<Item = Step>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
            sent: 0,
            heard: Vec::new(),
            waits: std::sync::Arc::default(),
            on_wait: None,
            message_due_by: None,
        }
    }
}

impl Transport<Request, ControlFrame> for Script {
    fn wait_readable(&mut self, timeout: Option<Duration>, _: &Bell) -> IpcResult<bool> {
        let timeout = timeout.expect("the loop waits for a bounded time");
        if let Some(on_wait) = self.on_wait.as_mut() {
            on_wait();
        }
        self.waits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.wait_inner(timeout)
    }

    fn recv(&mut self) -> IpcResult<Request> {
        self.recv_inner()
    }

    fn send(&mut self, msg: &ControlFrame) -> IpcResult<()> {
        self.heard.push(msg.clone());
        Ok(())
    }
}

impl Script {
    fn wait_inner(&mut self, _timeout: Duration) -> IpcResult<bool> {
        match self.steps.front() {
            Some(Step::Call(_)) => {
                let Some(Step::Call(call)) = self.steps.pop_front() else {
                    unreachable!("matched a call")
                };
                call();
                Ok(false)
            }
            Some(Step::Until(_)) => {
                let Some(Step::Until(until)) = self.steps.front_mut() else {
                    unreachable!("matched an until")
                };
                let give_up_at = *until
                    .give_up_at
                    .get_or_insert_with(|| Instant::now() + HANG_GUARD);
                assert!(Instant::now() < give_up_at, "the script's event never came");
                if (until.holds)(&self.heard) {
                    self.steps.pop_front();
                } else {
                    // The predicates read other threads' state, which has no
                    // unparker to park on, so the script polls them.
                    std::thread::yield_now();
                }
                Ok(false)
            }
            // A message is there to read, or the script is over and `recv` says so.
            front => {
                if matches!(front, Some(Step::Send(_))) {
                    let give_up_at = *self
                        .message_due_by
                        .get_or_insert_with(|| Instant::now() + HANG_GUARD);
                    assert!(
                        Instant::now() < give_up_at,
                        "the loop never read the message"
                    );
                }
                Ok(true)
            }
        }
    }

    fn recv_inner(&mut self) -> IpcResult<Request> {
        self.message_due_by = None;
        match self.steps.pop_front() {
            Some(Step::Send(body)) => {
                self.sent += 1;
                Ok(Request {
                    id: ReqId::new(self.sent),
                    body,
                })
            }
            _ => Err(IpcError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the script is over",
            ))),
        }
    }
}

/// The host's end of the audio connection: a socket the server's end reads
/// frames from, as the real host's does.
struct HostEnd {
    stream: UnixStream,
}

impl HostEnd {
    /// Sends block `seq` of `frames` and waits up to `within` for its answer:
    /// `None` when none came.
    fn block(&self, seq: u64, frames: usize, within: Duration) -> Option<AudioReply> {
        write_frame(
            &mut &self.stream,
            &AudioRequest::ProcessAudio(ProcessAudioData {
                seq,
                num_samples: frames,
                ..ProcessAudioData::empty(tutti_plugin_types::SampleRate::SR_48K)
            }),
        )
        .ok()?;
        self.stream.set_read_timeout(Some(within)).ok()?;
        read_frame(&mut &self.stream).ok()
    }
}

/// A session, its render side and the host's ends of both connections.
struct Fixture {
    session: Session,
    side: Option<RenderSide>,
    audio: Option<SocketTransport<AudioRequest, AudioReply, Alone>>,
    host: Option<HostEnd>,
}

impl Fixture {
    /// The host's end of the audio connection, taken once.
    fn host(&mut self) -> HostEnd {
        self.host.take().expect("the host's end is taken once")
    }
}

const TICK: Duration = Duration::from_millis(10);

fn load_message() -> ControlRequest {
    ControlRequest::LoadPlugin {
        path: std::path::PathBuf::from(crate::test_utils::clap_probe_path()),
        format: tutti_plugin_types::PluginFormat::Clap,
        plugin: None,
        sample_rate: tutti_plugin_types::SampleRate::SR_44K1,
        block_size: 512,
        preferred_format: FloatWidth::F32,
        shm_name: String::new(),
        state: None,
    }
}

/// A session with a slab, as the handshake leaves it.
fn session(name: &str) -> (Fixture, tutti_plugin_ipc::SlabWriter) {
    let (mut session, side) = Session::new().expect("bells");
    let layout = tutti_plugin_ipc::SlabLayout {
        samples_per_channel: 512,
        format: FloatWidth::F32,
        slots: tutti_plugin_ipc::RING_SLOTS as u32,
        inputs: vec![tutti_plugin_types::ChannelLayout::STEREO],
        outputs: vec![tutti_plugin_types::ChannelLayout::STEREO],
    };
    let shm = format!("tutti_evloop_{name}_{}", std::process::id());
    let creator = AudioSlab::create(shm.clone(), layout.clone()).expect("slab");
    session
        .render
        .attach_slab(AudioSlab::open(shm, layout).expect("slab"));
    let (host_end, server_end) = UnixStream::pair().expect("socket pair");
    (
        Fixture {
            session,
            side: Some(side),
            audio: Some(
                SocketTransport::audio(Stream::from(OwnedFd::from(server_end)))
                    .expect("the audio transport"),
            ),
            host: Some(HostEnd { stream: host_end }),
        },
        creator,
    )
}

fn run(fixture: &mut Fixture, script: &mut Script) {
    run_ticking(fixture, script, TICK);
}

/// [`run`], with the loop's idle tick every `tick`: a long one shows what the
/// loop serves without it.
fn run_ticking(fixture: &mut Fixture, script: &mut Script, tick: Duration) {
    let main = main_thread();
    let render = RenderThread::spawn(
        fixture.side.take().expect("run once"),
        Some(Box::new(fixture.audio.take().expect("run once"))),
    )
    .expect("the render thread");
    let result = serve_every(
        &mut fixture.session,
        script,
        &mut Native::default(),
        main,
        &render,
        tick,
    );
    // As the server ends: the plugin goes home, then the render thread stops.
    fixture.session.drop_plugin(main);
    drop(render);
    // A script that ends ends in `recv`'s refusal; any other end is a failure.
    assert!(
        matches!(result, Err(IpcError::Io(ref e)) if e.to_string() == "the script is over")
            || result.is_ok(),
        "{result:?}"
    );
}

/// **A quiet host does not stop the plugin's main-thread work.** The loop used
/// to block in `recv`, so a timer the plugin registered was served only at the
/// next message: after one block, a quiet host saw it tick at most once. The
/// plugin registers a period-0 timer (due at every service pass) in the
/// callback its first block asks for, and the loop's idle ticks must keep
/// serving it while the host says nothing at all.
///
/// The script is silent until the timer has been served three times: a loop
/// that waits for the host serves it once and the script's guard fails.
#[test]
fn a_quiet_host_does_not_stop_the_plugins_timers() {
    let _lock = crate::test_utils::plugin_load_lock();
    let probe = crate::test_utils::clap();
    probe.thread_reset();
    let _cmd =
        crate::test_utils::clap_probe_thread_command(tutti_clap_test_plugin::CMD_REGISTER_TIMER);
    let (mut session, _slab) = session("timers");
    let host = session.host();
    let mut script = Script::new([
        Step::Send(load_message()),
        Step::Call(Box::new(move || {
            assert!(
                host.block(1, 64, HANG_GUARD).is_some(),
                "the render thread answers the block"
            );
        })),
        Until::step(move |_| probe_visits(probe) >= 3),
    ]);

    run(&mut session, &mut script);
}

/// How many times the plugin's timer has been served on this thread.
fn probe_visits(probe: &tutti_clap_test_plugin::ClapProbe) -> u64 {
    u64::from(probe.thread_capture().sites[Site::OnTimer as usize].visits)
}

/// Sets the period the plugin's next timer asks for until the guard drops; the
/// default (0) again then.
fn timer_period(millis: u32) -> crate::test_utils::Restore {
    crate::test_utils::clap_switch_until_drop(ClapSwitch::TimerPeriod, millis, 0)
}

/// **A timer is served at its deadline through the loop, not at its tick.** The
/// plugin registers a 50 ms timer, and the loop's tick is five minutes: what
/// serves the timer is the loop's wait ending at the timer's deadline
/// (`wakeups`). The script is silent until the timer has fired three times; a
/// loop that served it only at the tick would not within the script's guard.
#[test]
fn a_timer_is_served_at_its_deadline_not_the_tick() {
    let _lock = crate::test_utils::plugin_load_lock();
    let probe = crate::test_utils::clap();
    probe.thread_reset();
    let _period = timer_period(50);
    let _cmd =
        crate::test_utils::clap_probe_thread_command(tutti_clap_test_plugin::CMD_REGISTER_TIMER);
    let (mut session, _slab) = session("period");
    let host = session.host();
    let mut script = Script::new([
        Step::Send(load_message()),
        Step::Call(Box::new(move || {
            assert!(host.block(1, 64, HANG_GUARD).is_some());
        })),
        // A message is served as it comes, and with it the callback the block
        // asked for, which registers the timer.
        Step::Send(ControlRequest::GetParameterList),
        Until::step(move |_| probe_visits(probe) >= 3),
    ]);

    run_ticking(&mut session, &mut script, Duration::from_secs(300));

    assert!(
        probe.thread_capture().timer_registered,
        "the plugin registered its timer"
    );
}

/// **A request is answered as it arrives, not at the next tick.** The loop's
/// tick is five minutes: only the wait ending readable can serve the message,
/// and a loop that read messages only at its tick never would within the
/// script's guard.
#[test]
fn a_message_after_silence_is_answered() {
    let _lock = crate::test_utils::plugin_load_lock();
    let (mut session, _slab) = session("answers");
    let waits = std::sync::Arc::<std::sync::atomic::AtomicU64>::default();
    let mut script = Script::new([
        Step::Send(load_message()),
        // The loop has gone back to waiting a few times before the host speaks.
        Until::step({
            let waits = waits.clone();
            move |_| waits.load(std::sync::atomic::Ordering::SeqCst) >= 3
        }),
        Step::Send(ControlRequest::GetParameterList),
    ]);
    script.waits = waits;

    run_ticking(&mut session, &mut script, Duration::from_secs(300));

    assert!(
        matches!(
            script.heard.last(),
            Some(ControlFrame::Reply {
                reply: ControlReply::ParameterList { parameters },
                ..
            }) if !parameters.is_empty()
        ),
        "{:?}",
        script.heard
    );
}

/// **A lost plugin ends the server with an error, and the host hears it as a
/// crash.** The plugin is left as a panic in a call into it leaves it; with no
/// message to read and nothing else to wake the loop, the loop returns the
/// plugin's loss rather than wait for the host (the clean shutdown a quiet
/// host gets).
#[test]
fn a_lost_plugin_ends_the_server_abnormally() {
    let _lock = crate::test_utils::plugin_load_lock();
    let (mut fixture, _slab) = session("lost");
    let main = main_thread();
    assert!(matches!(
        fixture.session.handle(main, load_message()).expect("load"),
        crate::session::Reaction::Reply(ControlReply::PluginLoaded { .. })
    ));
    fixture.session.lose_plugin();
    let render = RenderThread::spawn(
        fixture.side.take().expect("run once"),
        Some(Box::new(fixture.audio.take().expect("run once"))),
    )
    .expect("the render thread");
    let mut script = Script::new([]);
    let result = serve_every(
        &mut fixture.session,
        &mut script,
        &mut Native::default(),
        main,
        &render,
        TICK,
    );
    fixture.session.drop_plugin(main);
    drop(render);
    assert!(
        matches!(
            result,
            Err(IpcError::State(
                tutti_plugin_types::StateError::PluginCrashed
            ))
        ),
        "{result:?}"
    );
}

/// **An audio connection that breaks the protocol ends the session, typed.** A
/// frame that does not decode kills the render thread's connection while the
/// control connection goes on; the loop reports it as `AudioConnection`.
#[test]
fn an_undecodable_audio_frame_ends_the_session() {
    use std::io::Write as _;
    let (mut fixture, _slab) = session("garbage");
    let host = fixture.host();
    let main = main_thread();
    let render = RenderThread::spawn(
        fixture.side.take().expect("run once"),
        Some(Box::new(fixture.audio.take().expect("run once"))),
    )
    .expect("the render thread");
    // Silent until the loop ends itself: the garbage is sent once the loop
    // first waits, and the script then waits for the loop to act on it.
    let mut script = Script::new([
        Step::Call(Box::new(move || {
            let mut stream = &host.stream;
            let body = [0xff_u8; 8];
            stream
                .write_all(&(body.len() as u32).to_be_bytes())
                .and_then(|()| stream.write_all(&body))
                .expect("the host writes");
        })),
        Until::step(|_| false),
    ]);
    let result = serve_every(
        &mut fixture.session,
        &mut script,
        &mut Native::default(),
        main,
        &render,
        TICK,
    );
    fixture.session.drop_plugin(main);
    drop(render);
    assert!(
        matches!(
            result,
            Err(IpcError::AudioConnection { ref cause })
                if matches!(**cause, IpcError::Serialization(_))
        ),
        "{result:?}"
    );
}

/// **With nothing loaded, an editor request is answered, never dropped**: the
/// host blocks on the open and the resize.
#[test]
fn an_editor_request_with_no_plugin_is_refused_not_ignored() {
    let (mut session, _slab) = session("none");
    let mut script = Script::new([
        Step::Send(ControlRequest::OpenEditor {
            parent: None,
            scale: ContentScale::IDENTITY,
        }),
        Step::Send(ControlRequest::SetEditorSize {
            size: dpi::PhysicalSize::new(10, 10).into(),
        }),
        Step::Send(ControlRequest::CloseEditor),
    ]);

    run(&mut session, &mut script);

    assert!(
        matches!(
            script.heard.as_slice(),
            [
                ControlFrame::Reply { reply: ControlReply::EditorOpened { result: Err(open) }, .. },
                ControlFrame::Reply { reply: ControlReply::EditorSized { result: Err(size) }, .. },
            ] if open.contains("no plugin") && size.contains("not open")
        ),
        "{:?}",
        script.heard
    );
}

/// Makes every plugin timer take `millis`, as a slow editor paint does, until
/// the guard drops; none again then.
fn slow_paint(millis: u32) -> crate::test_utils::Restore {
    crate::test_utils::clap_switch_until_drop(ClapSwitch::TimerStall, millis, 0)
}

/// **A slow editor paint on the main loop delays no block**. The plugin's
/// timer, served by the loop, takes 250 ms each time, which is what a slow
/// `editor_idle` does to the loop. The host sends blocks through all of it.
/// The loop's waits are counted, and a stretch between two waits that held a
/// paint is one the loop was away in: with the render on the main loop, a block
/// sent in it is answered only after the loop is back at a wait, so none is
/// sent and answered inside one. The script is silent until five blocks have
/// been sent and answered inside a single such stretch.
#[test]
fn a_slow_paint_on_the_main_loop_delays_no_block() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    const PAINT_MS: u32 = 250;
    const BLOCKS_IN_ONE_PAINT: usize = 5;

    let _lock = crate::test_utils::plugin_load_lock();
    let probe = crate::test_utils::clap();
    probe.thread_reset();
    let _cmd =
        crate::test_utils::clap_probe_thread_command(tutti_clap_test_plugin::CMD_REGISTER_TIMER);
    let _paint = slow_paint(PAINT_MS);
    let (mut session, _slab) = session("paint");
    let host = session.host();
    // The wait count each block was sent and answered within, when it was both.
    let within: Arc<Mutex<Vec<u64>>> = Arc::default();
    // The wait counts whose stretch held a paint.
    let painted: Arc<Mutex<Vec<u64>>> = Arc::default();
    let (stop, done) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let waits = Arc::<AtomicU64>::default();
    /// Stops the sender when the test ends any way, a panic in the loop included.
    struct StopOnDrop(Arc<AtomicBool>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let _stop_on_exit = StopOnDrop(stop.clone());
    let sender = Arc::new(Mutex::new(None));
    let mut script = Script::new([
        Step::Send(load_message()),
        Step::Call(Box::new({
            let (within, sender, stop, done, waits) = (
                within.clone(),
                sender.clone(),
                stop.clone(),
                done.clone(),
                waits.clone(),
            );
            move || {
                // The first block asks the plugin for its callback, which
                // registers the timer the paint runs in.
                assert!(host.block(1, 64, HANG_GUARD).is_some());
                *sender.lock().unwrap() = Some(std::thread::spawn(move || {
                    for seq in 2.. {
                        if stop.load(Ordering::SeqCst) {
                            break;
                        }
                        let sent_at = waits.load(Ordering::SeqCst);
                        assert!(host.block(seq, 64, HANG_GUARD).is_some());
                        if waits.load(Ordering::SeqCst) == sent_at {
                            within.lock().unwrap().push(sent_at);
                        }
                        // Each block is a round trip to the render thread, whose
                        // ring has no unparker, so the thread polls it.
                        std::thread::yield_now();
                    }
                    done.store(true, Ordering::SeqCst);
                }));
            }
        })),
        Until::step({
            let (within, painted, stop, done) =
                (within.clone(), painted.clone(), stop.clone(), done.clone());
            move |_| {
                if done.load(Ordering::SeqCst) {
                    return true;
                }
                let painted = painted.lock().unwrap();
                let within = within.lock().unwrap();
                if painted
                    .iter()
                    .any(|p| within.iter().filter(|w| *w == p).count() >= BLOCKS_IN_ONE_PAINT)
                {
                    stop.store(true, Ordering::SeqCst);
                }
                false
            }
        }),
    ]);
    script.waits = waits.clone();
    script.on_wait = Some(Box::new({
        let (painted, waits) = (painted.clone(), waits.clone());
        let mut seen = 0;
        move || {
            let served = probe_visits(probe);
            if served > seen {
                seen = served;
                painted.lock().unwrap().push(waits.load(Ordering::SeqCst));
            }
        }
    }));

    run(&mut session, &mut script);
    sender
        .lock()
        .unwrap()
        .take()
        .expect("the host ran")
        .join()
        .unwrap();
}
