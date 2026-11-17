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
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tutti_clap_test_plugin::Site;
use tutti_fixtures::main_thread;
use tutti_plugin_ipc::{AudioSlab, BridgeMessage, HostMessage, ProcessAudioData};
use tutti_plugin_types::{BridgeError, BridgeResult, ContentScale, Load, SampleFormat};

use super::{Native, serve_every};

#[cfg(unix)]
mod fds;
#[cfg(all(unix, not(target_os = "macos")))]
mod x11;
use crate::render::{RenderSide, RenderThread, ToRender};
use crate::session::Session;
use crate::transport::Transport;

/// One thing the scripted host does.
#[allow(clippy::large_enum_variant)] // a test script, built once
enum Step {
    Send(HostMessage),
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
    holds: Box<dyn FnMut(&[BridgeMessage]) -> bool + Send>,
    /// When the wait gives up: [`HANG_GUARD`] after the first wait on it.
    give_up_at: Option<Instant>,
}

impl Until {
    fn step(holds: impl FnMut(&[BridgeMessage]) -> bool + Send + 'static) -> Step {
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
    heard: Vec<BridgeMessage>,
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
            heard: Vec::new(),
            waits: std::sync::Arc::default(),
            on_wait: None,
            message_due_by: None,
        }
    }
}

impl Transport for Script {
    fn wait_readable(&mut self, timeout: Duration) -> BridgeResult<bool> {
        if let Some(on_wait) = self.on_wait.as_mut() {
            on_wait();
        }
        self.waits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.wait_inner(timeout)
    }

    fn recv(&mut self) -> BridgeResult<HostMessage> {
        self.recv_inner()
    }

    fn send(&mut self, msg: &BridgeMessage) -> BridgeResult<()> {
        self.heard.push(msg.clone());
        Ok(())
    }
}

impl Script {
    fn wait_inner(&mut self, _timeout: Duration) -> BridgeResult<bool> {
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
                    // doorbell to wait on, so the script polls them.
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

    fn recv_inner(&mut self) -> BridgeResult<HostMessage> {
        self.message_due_by = None;
        match self.steps.pop_front() {
            Some(Step::Send(message)) => Ok(message),
            _ => Err(BridgeError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "the script is over",
            ))),
        }
    }
}

/// The server's end of the audio connection, in memory.
struct AudioEnd {
    requests: mpsc::Receiver<HostMessage>,
    replies: mpsc::Sender<BridgeMessage>,
    peeked: Option<HostMessage>,
}

impl Transport for AudioEnd {
    fn wait_readable(&mut self, timeout: Duration) -> BridgeResult<bool> {
        if self.peeked.is_none() {
            match self.requests.recv_timeout(timeout) {
                Ok(msg) => self.peeked = Some(msg),
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(false),
                // `recv` reports the host gone.
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(true),
            }
        }
        Ok(true)
    }

    fn recv(&mut self) -> BridgeResult<HostMessage> {
        match self.peeked.take() {
            Some(msg) => Ok(msg),
            None => self
                .requests
                .recv()
                .map_err(|_| BridgeError::Io(std::io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn send(&mut self, msg: &BridgeMessage) -> BridgeResult<()> {
        self.replies
            .send(msg.clone())
            .map_err(|_| BridgeError::Io(std::io::ErrorKind::BrokenPipe.into()))
    }
}

/// The host's end of the audio connection.
struct HostEnd {
    requests: mpsc::Sender<HostMessage>,
    replies: mpsc::Receiver<BridgeMessage>,
}

impl HostEnd {
    /// Sends block `seq` of `frames` and waits up to `within` for its answer:
    /// `None` when none came.
    fn block(&self, seq: u64, frames: usize, within: Duration) -> Option<BridgeMessage> {
        self.requests
            .send(HostMessage::ProcessAudio(ProcessAudioData {
                seq,
                num_samples: frames,
                ..tutti_plugin_ipc::ProcessAudioData::empty(tutti_plugin_types::SampleRate::SR_48K)
            }))
            .ok()?;
        self.replies.recv_timeout(within).ok()
    }
}

/// A session, its render side and the host's ends of both connections.
struct Fixture {
    session: Session,
    side: Option<RenderSide>,
    audio: Option<AudioEnd>,
    host: Option<HostEnd>,
}

impl Fixture {
    /// The host's end of the audio connection, taken once.
    fn host(&mut self) -> HostEnd {
        self.host.take().expect("the host's end is taken once")
    }
}

const TICK: Duration = Duration::from_millis(10);

fn load_message() -> HostMessage {
    HostMessage::LoadPlugin {
        path: std::path::PathBuf::from(crate::test_utils::clap_probe_path()),
        format: tutti_plugin_types::PluginFormat::Clap,
        load: Load::new(),
        sample_rate: tutti_plugin_types::SampleRate::SR_44K1,
        block_size: 512,
        preferred_format: SampleFormat::Float32,
        shm_name: String::new(),
        state: None,
    }
}

/// A session with a slab, as the handshake leaves it.
fn session(name: &str) -> (Fixture, tutti_plugin_ipc::SlabWriter) {
    let (mut session, side) = Session::new();
    let layout = tutti_plugin_ipc::SlabLayout {
        samples_per_channel: 512,
        format: SampleFormat::Float32,
        slots: tutti_plugin_ipc::RING_SLOTS as u32,
        inputs: vec![tutti_plugin_types::ChannelLayout::STEREO],
        outputs: vec![tutti_plugin_types::ChannelLayout::STEREO],
    };
    let shm = format!("tutti_evloop_{name}_{}", std::process::id());
    let creator = AudioSlab::create(shm.clone(), layout.clone()).expect("slab");
    session
        .render
        .send(ToRender::Slab(AudioSlab::open(shm, layout).expect("slab")));
    let (requests, host_requests) = mpsc::channel();
    let (replies, host_replies) = mpsc::channel();
    (
        Fixture {
            session,
            side: Some(side),
            audio: Some(AudioEnd {
                requests: host_requests,
                replies,
                peeked: None,
            }),
            host: Some(HostEnd {
                requests,
                replies: host_replies,
            }),
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
        Box::new(fixture.audio.take().expect("run once")),
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
    drop(render.stop());
    // A script that ends ends in `recv`'s refusal; any other end is a failure.
    assert!(
        matches!(result, Err(BridgeError::Io(ref e)) if e.to_string() == "the script is over")
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

/// Sets the period the plugin's next timer asks for; the default (0) again on
/// drop.
struct TimerPeriod;

impl TimerPeriod {
    fn of(millis: u32) -> Self {
        crate::test_utils::clap().set(tutti_clap_test_plugin::ClapSwitch::TimerPeriod, millis);
        Self
    }
}

impl Drop for TimerPeriod {
    fn drop(&mut self) {
        crate::test_utils::clap().set(tutti_clap_test_plugin::ClapSwitch::TimerPeriod, 0);
    }
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
    let _period = TimerPeriod::of(50);
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
        Step::Send(HostMessage::GetParameterList),
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
        Step::Send(HostMessage::GetParameterList),
    ]);
    script.waits = waits;

    run_ticking(&mut session, &mut script, Duration::from_secs(300));

    assert!(
        matches!(
            script.heard.last(),
            Some(BridgeMessage::ParameterList { parameters }) if !parameters.is_empty()
        ),
        "{:?}",
        script.heard
    );
}

/// **With nothing loaded, an editor request is answered, never dropped**: the
/// host blocks on the open and the resize.
#[test]
fn an_editor_request_with_no_plugin_is_refused_not_ignored() {
    let (mut session, _slab) = session("none");
    let mut script = Script::new([
        Step::Send(HostMessage::OpenEditor {
            parent: None,
            scale: ContentScale::IDENTITY,
        }),
        Step::Send(HostMessage::SetEditorSize {
            size: dpi::PhysicalSize::new(10, 10).into(),
        }),
        Step::Send(HostMessage::CloseEditor),
    ]);

    run(&mut session, &mut script);

    assert!(
        matches!(
            script.heard.as_slice(),
            [
                BridgeMessage::EditorOpened { result: Err(open) },
                BridgeMessage::EditorSized { result: Err(size) },
            ] if open.contains("no plugin") && size.contains("not open")
        ),
        "{:?}",
        script.heard
    );
}

/// Makes every plugin timer take this long, as a slow editor paint does; none
/// again on drop.
struct SlowPaint;

impl SlowPaint {
    fn of(millis: u32) -> Self {
        crate::test_utils::clap().set(tutti_clap_test_plugin::ClapSwitch::TimerStall, millis);
        Self
    }
}

impl Drop for SlowPaint {
    fn drop(&mut self) {
        crate::test_utils::clap().set(tutti_clap_test_plugin::ClapSwitch::TimerStall, 0);
    }
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
    let _paint = SlowPaint::of(PAINT_MS);
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
                        // ring has no doorbell, so the thread polls it.
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
