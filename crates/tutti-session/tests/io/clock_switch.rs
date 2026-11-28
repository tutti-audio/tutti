//! A session changes clock: the system clock to a sound card and back, device-free
//! (the card is a `ManualStreamDriver`, the timer is real). What a switch
//! keeps, what it refuses before anything stops, and what it hands back.

use tutti_buffers::InterleavedMut;
use tutti_platform::Instant;

use tutti_buffers::{ChannelLayout, ChannelTopology, MaxBlock};
use tutti_cpal::{DeviceId, DeviceSelector};
use tutti_engine::{ManualStreamDriver, OutputSpec};
use tutti_fixtures::{HANG_GUARD, wait_until};
use tutti_graph::ForkByClone;
use tutti_nodes::testing::{Const, Through};
use tutti_session::{
    Error, InputPort, LiveSession, MainOutput, OutputPort, Session, SessionEvent, SystemSession,
};
use tutti_units::{SampleRate, Samples};

const BLOCK: usize = 480;

fn spec(rate: SampleRate, channels: ChannelLayout) -> OutputSpec {
    OutputSpec::new(rate, channels, tutti_buffers::SampleFormat::F32)
}

fn stereo(rate: SampleRate) -> OutputSpec {
    spec(rate, ChannelLayout::STEREO)
}

fn system_builder() -> tutti_session::SessionBuilder<tutti_session::SystemClock> {
    Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .max_block(MaxBlock::of(Samples::new(BLOCK)))
        .system_clock()
}

fn system() -> SystemSession {
    system_builder().build().expect("a timer thread starts")
}

/// Wires a constant to both outputs and commits it.
fn wire_constant<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(
    s: &mut Session<K, C>,
    level: f32,
) {
    let node = s.graph().add(ForkByClone(Const::mono(level)));
    for port in 0..2 {
        s.graph()
            .set_sources(MainOutput::channel(port), [OutputPort::new(node, 0)])
            .unwrap()
            .unfaded();
    }
    let _ = s.update();
}

/// **A timer asked for a block past the callback ceiling is refused, never
/// opened at a shorter one.** One frame past `MAX_FRAMES` names the quantum.
#[test]
fn a_system_clock_block_past_the_callback_ceiling_is_refused() {
    let past = tutti_engine::MAX_FRAMES + 1;
    let refused = system_builder()
        .max_block(MaxBlock::of(Samples::new(past)))
        .build();
    assert!(
        matches!(&refused, Err(Error::Quantum(q)) if q.0 == past),
        "{:?}",
        refused.err()
    );
}

fn play<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(s: &Session<K, C>) {
    s.play().expect("room");
}

fn frame<K: tutti_session::AppKey, C: tutti_session::Clock<Float = f32>>(s: &Session<K, C>) -> u64 {
    s.playhead().frame().get()
}

/// Waits for the timer to have rendered past `than`, updating as a host does.
fn wait_past(s: &mut SystemSession, than: u64) -> u64 {
    wait_until("the timer to render another block", HANG_GUARD, || {
        let _ = s.update();
        frame(s) > than
    });
    frame(s)
}

/// **A system session moves onto a sound card and its graph plays there**: the
/// device is running, and the constant wired before the switch is what the
/// card's first blocks render.
#[test]
fn a_system_session_switches_to_a_device_and_its_graph_plays_there() {
    let mut s = system();
    wire_constant(&mut s, 0.25);
    play(&s);
    let (driver, stream) = ManualStreamDriver::new();
    let mut live: LiveSession = s
        .into_device_on(stereo(SampleRate::SR_48K), driver)
        .unwrap_or_else(|e| panic!("switches: {}", e.error()));
    assert!(live.device().is_running(), "the card's stream runs");
    let _ = live.update();
    stream.render_block(Samples::new(64)).expect("open");
    let block = stream.render_block(Samples::new(64)).expect("open");
    assert!(
        block.iter().all(|&x| x == 0.25),
        "the wired constant plays on the card: {:?}",
        &block[..4]
    );
}

/// **A device session moves onto the timer and its graph keeps rendering
/// there**: the card's stream is stopped, the heard tap carries the constant
/// the timer's blocks render, and no fault is recorded.
#[test]
fn a_device_session_switches_to_the_timer_and_its_graph_keeps_rendering() {
    let (driver, stream) = ManualStreamDriver::new();
    let mut live = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(stereo(SampleRate::SR_48K), driver)
        .expect("builds with no device");
    wire_constant(&mut live, 0.25);
    stream.render_block(Samples::new(64)).expect("open");
    let mut s = live
        .into_system_clock()
        .unwrap_or_else(|e| panic!("switches: {}", e.error()));
    assert!(
        stream.render_block(Samples::new(64)).is_none(),
        "the card's stream stopped"
    );
    play(&s);
    let mut tap = s.open_heard_tap(Samples::new(4 * BLOCK));
    let mut heard = vec![0.0f32; 2 * BLOCK];
    wait_until("the constant to reach the tap", HANG_GUARD, || {
        let _ = s.update();
        let frames = tap
            .pop_into(InterleavedMut::new(&mut heard, tap.layout()).unwrap())
            .unwrap()
            .get();
        frames > 0 && heard[..2 * frames].iter().all(|&x| x == 0.25)
    });
    assert_eq!(s.faults().count(), 0);
}

/// **A device that will not open fails the switch with the session still
/// running on the timer**: the error is the device's, nothing was stopped, and
/// the session's playhead goes on moving.
#[test]
fn a_device_that_will_not_open_returns_the_session_running_on_its_old_clock() {
    let mut s = system();
    play(&s);
    let before = wait_past(&mut s, 0);
    let err = s
        .into_device(DeviceSelector::Id(DeviceId::new("no such device")))
        .expect_err("there is no such device");
    assert!(
        matches!(err.error().error(), Error::Device(_)),
        "{}",
        err.error()
    );
    let mut s = err.into_session();
    wait_past(&mut s, before);
}

/// **A main output the new device cannot place is refused before the timer stops**:
/// six unassigned buses on a stereo card. The error is named, the card's stream
/// never opened, and the timer renders on.
#[test]
fn a_main_output_the_device_cannot_place_is_refused_before_the_timer_stops() {
    let mut s = system_builder()
        .outputs(ChannelLayout::from(6u16))
        .main_output_topology(ChannelTopology::unassigned(ChannelLayout::from(6u16)).unwrap())
        .build()
        .expect("builds");
    play(&s);
    let before = wait_past(&mut s, 0);
    let (driver, stream) = ManualStreamDriver::new();
    let err = s
        .into_device_on(stereo(SampleRate::SR_48K), driver)
        .expect_err("a six-wide unassigned main output has no place on two channels");
    assert!(
        matches!(err.error().error(), Error::Unplaceable { .. }),
        "{}",
        err.error()
    );
    assert!(!stream.is_open(), "the card's stream never opened");
    let mut s = err.into_session();
    assert_eq!(s.graph_ref().outputs(), 6, "the graph is as it was");
    wait_past(&mut s, before);
}

/// **A graph that does not compile fails the switch, and the timer starts
/// again**: the failure follows the stop, the card never started, and the
/// session is handed back running on the timer.
#[test]
fn a_graph_that_no_longer_compiles_fails_the_switch_and_the_timer_runs_again() {
    let mut s = system();
    play(&s);
    let osc = s.graph().add(ForkByClone(Const::mono(0.1)));
    let lim = s.graph().add(ForkByClone(Through::mono()));
    s.graph()
        .set_sources(InputPort::new(lim, 0), [OutputPort::new(osc, 0)])
        .unwrap();
    s.graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(lim, 0)])
        .unwrap();
    let _ = s.update();
    let before = wait_past(&mut s, 0);
    // An edit not yet committed feeds the node from a node it feeds.
    let loop_back = s.graph().add(ForkByClone(Through::mono()));
    s.graph()
        .set_sources(InputPort::new(loop_back, 0), [OutputPort::new(lim, 0)])
        .unwrap();
    s.graph()
        .set_sources(InputPort::new(lim, 0), [OutputPort::new(loop_back, 0)])
        .unwrap();
    let (driver, stream) = ManualStreamDriver::new();
    let err = s
        .into_device_on(stereo(SampleRate::SR_48K), driver)
        .expect_err("the graph no longer compiles");
    assert!(
        matches!(err.error().error(), Error::Reprepare(_) | Error::Output(_)),
        "{}",
        err.error()
    );
    assert!(!stream.is_open(), "the card never started");
    let mut s = err.into_session();
    assert_eq!(s.stream_spec().sample_rate, SampleRate::SR_48K);
    assert_eq!(
        s.stream_spec().quantum,
        Some(tutti_engine::OutputQuantum::of(BLOCK))
    );
    wait_past(&mut s, before);
}

/// A node that cannot run above 90 kHz: its `prepare` panics there, which is
/// how a re-prepare poisons an editor.
struct Grenade;

impl tutti_graph::Node for Grenade {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(ChannelLayout::MONO)
    }

    fn prepare(&mut self, prepare: &tutti_graph::Prepare) {
        assert!(prepare.sample_rate().get() < 90_000.0, "no such rate");
    }

    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        io.output(0).fill(0.0);
        tutti_graph::Status::Modified
    }

    fn reset(&mut self) {}
}

/// **A re-prepare that poisons the graph is named and keeps its host**: the
/// switch onto a 96 kHz card panics a node's `prepare`, the way back to the
/// timer cannot re-prepare either, and the session comes back with
/// `SwitchNotUndone`, the card never opened, and its host in its slot at the
/// graph's rate.
#[test]
fn a_poisoned_graph_names_the_double_failure_and_keeps_its_host() {
    let mut s = system();
    play(&s);
    s.graph().add(tutti_graph::Unforkable(Grenade));
    let _ = s.update();
    wait_past(&mut s, 0);
    let (driver, stream) = ManualStreamDriver::new();
    let err = s
        .into_device_on(stereo(SampleRate::of(96_000.0)), driver)
        .expect_err("the graph is poisoned by the re-prepare");
    assert!(
        matches!(err.error().error(), Error::SwitchNotUndone { .. }),
        "the double failure is named: {}",
        err.error()
    );
    assert!(!stream.is_open(), "the card never started");
    let s = err.into_session();
    assert_eq!(
        s.stream_spec().sample_rate,
        s.sample_rate(),
        "the host says what the graph is prepared at"
    );
    assert_eq!(s.faults().count(), 0, "and the session still answers");
}

/// **A rolling transport resumes on a whole block**: after the card's blocks,
/// the timer's first block starts where the card's last ended, so the frames
/// the timer has rendered since the switch are a whole number of its blocks.
#[test]
fn the_transport_resumes_where_the_last_block_ended_going_to_the_timer() {
    let (driver, stream) = ManualStreamDriver::new();
    let mut live = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(stereo(SampleRate::SR_48K), driver)
        .expect("builds with no device");
    let _ = live.update();
    play(&live);
    for _ in 0..5 {
        stream.render_block(Samples::new(BLOCK)).expect("open");
    }
    let timer_block = tutti_session::LIVE_MAX_BLOCK.samples().get() as u64;
    // The playhead reads the frame the last block started at.
    let ended = frame(&live) + BLOCK as u64;
    assert_eq!(ended, 5 * BLOCK as u64, "five card blocks of play");
    let mut s = live
        .into_system_clock()
        .unwrap_or_else(|e| panic!("switches: {}", e.error()));
    let after = wait_past(&mut s, ended);
    assert_eq!((after - ended) % timer_block, 0, "{ended} -> {after}");
}

/// **A rolling transport resumes where the timer's last block ended going to
/// the card**: the timer rendered whole blocks from frame zero, so the first
/// block the card renders starts on a whole number of timer blocks (the
/// playhead reads the frame a block started at).
#[test]
fn the_transport_resumes_where_the_last_block_ended_going_to_the_device() {
    let mut s = system();
    play(&s);
    let timer_ran = wait_past(&mut s, 3 * BLOCK as u64);
    let (driver, stream) = ManualStreamDriver::new();
    let live = s
        .into_device_on(stereo(SampleRate::SR_48K), driver)
        .unwrap_or_else(|e| panic!("switches: {}", e.error()));
    stream.render_block(Samples::new(64)).expect("open");
    let started = frame(&live);
    assert_eq!(
        started % BLOCK as u64,
        0,
        "the card's first block starts where a timer block ended: {started}"
    );
    assert!(
        started >= timer_ran + BLOCK as u64,
        "and after the ones read: {started}"
    );
}

/// **A switch to a card at another rate reports the rate change** and moves the
/// transport's rate with the graph's.
#[test]
fn a_switch_to_a_device_at_another_rate_reports_the_change() {
    let s = system();
    let (driver, _stream) = ManualStreamDriver::new();
    let mut live = s
        .into_device_on(stereo(SampleRate::SR_44K1), driver)
        .unwrap_or_else(|e| panic!("switches: {}", e.error()));
    let events = live.update();
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::RateChanged { from, to }
                if *from == SampleRate::SR_48K && *to == SampleRate::SR_44K1
        )),
        "{events:?}"
    );
    assert_eq!(live.sample_rate(), SampleRate::SR_44K1);
    assert_eq!(live.device().graph_rate(), SampleRate::SR_44K1);
}

/// **The output policy carries over a clock switch, and a device the map does
/// not fit is refused before the timer stops**: as a device restart refuses
/// it, with the map named and kept (never reset to the identity). Once the
/// host sets a map the card has places for, the switch goes through and the
/// policy is in force on the card.
#[test]
fn the_output_policy_carries_over_and_a_card_the_map_does_not_fit_is_refused() {
    use tutti_buffers::Bed;
    use tutti_engine::OutputMap;
    use tutti_session::OutputPolicy;
    let mut s = system_builder()
        .outputs(ChannelLayout::from(6u16))
        .build()
        .expect("builds");
    let policy = OutputPolicy {
        monitor: Some(Bed::Stereo.topology()),
        map: OutputMap::channels([4, 5]).unwrap(),
        ..OutputPolicy::default()
    };
    s.set(s.policy::<OutputPolicy>().to(policy.clone()))
        .unwrap();
    play(&s);
    let before = wait_past(&mut s, 0);

    let four = spec(SampleRate::SR_48K, ChannelLayout::from(4u16));
    let (driver, card) = ManualStreamDriver::new();
    let err = s
        .into_device_on(four.clone(), driver)
        .expect_err("the map has no output 4 on a four-channel card");
    match err.error().error() {
        Error::MapUnplaceable { map, device, .. } => {
            assert_eq!(*map, policy.map);
            assert_eq!(device.count(), 4);
        }
        other => panic!("expected the map named, got {other:?}"),
    }
    assert!(!card.is_open(), "the card never opened");
    let mut s = err.into_session();
    assert_eq!(
        s.get(s.policy::<OutputPolicy>()).unwrap().map,
        policy.map,
        "never reset to the identity"
    );
    wait_past(&mut s, before);

    let mut fits = policy;
    fits.map = OutputMap::channels([2, 3]).unwrap();
    s.set(s.policy::<OutputPolicy>().to(fits.clone())).unwrap();
    let (driver, _card) = ManualStreamDriver::new();
    let live = s
        .into_device_on(four, driver)
        .unwrap_or_else(|e| panic!("outputs 3 and 4 are there: {}", e.error()));
    assert_eq!(
        live.get(live.policy::<OutputPolicy>()).unwrap().map,
        fits.map
    );
    assert_eq!(live.output_device().unwrap().channels.width().count(), 4);
}

// --- A start that fails after the old stream stopped ---

/// The handle of a stream that never opens.
struct Never;

impl tutti_engine::RunningStream for Never {
    fn stop(self: Box<Self>) {}
}

/// A card whose stream will not open, after the device was found.
struct UnopenableCard;

impl tutti_engine::StreamDriver for UnopenableCard {
    type Running = Never;
    type Error = tutti_cpal::DeviceError;

    fn open(
        self,
        _: &OutputSpec,
        _: tutti_engine::OutputBlock,
        _: std::sync::Arc<tutti_engine::StreamFaults>,
    ) -> Result<tutti_engine::OpenedOutput<Never>, Self::Error> {
        Err(tutti_cpal::DeviceError::InvalidDevice(
            "a card that will not open".into(),
        ))
    }
}

/// A timer whose thread the OS refuses.
struct UnstartableTimer;

impl tutti_engine::StreamDriver for UnstartableTimer {
    type Running = Never;
    type Error = tutti_engine::TimerError;

    fn open(
        self,
        _: &OutputSpec,
        _: tutti_engine::OutputBlock,
        _: std::sync::Arc<tutti_engine::StreamFaults>,
    ) -> Result<tutti_engine::OpenedOutput<Never>, Self::Error> {
        Err(tutti_engine::TimerError::Thread(std::io::Error::other(
            "no thread",
        )))
    }
}

/// **A card whose stream will not open, after the timer stopped and the graph
/// moved to the card's rate, returns the session running on the timer**: the
/// graph is at the old rate again and the timer renders blocks.
#[test]
fn a_card_that_fails_to_open_after_the_stop_returns_the_session_running_on_the_timer() {
    let mut s = system();
    play(&s);
    let before = wait_past(&mut s, 0);
    let err = s
        .into_device_on(stereo(SampleRate::SR_44K1), UnopenableCard)
        .expect_err("the card's stream will not open");
    assert!(
        matches!(err.error().error(), Error::Device(_)),
        "{}",
        err.error()
    );
    let mut s = err.into_session();
    assert_eq!(s.sample_rate(), SampleRate::SR_48K, "the old rate again");
    let stream = s.stream_spec();
    assert_eq!(
        stream.sample_rate,
        SampleRate::SR_48K,
        "the old stream spec"
    );
    assert_eq!(stream.channels, ChannelLayout::STEREO);
    assert_eq!(
        stream.quantum,
        Some(tutti_engine::OutputQuantum::of(BLOCK)),
        "the old block"
    );
    wait_past(&mut s, before);
}

/// **A timer thread the OS refuses, after the card stopped, returns the session
/// running on the card**: the card's stream is started again on its driver and
/// the session's rate and spec are the ones it had.
#[test]
fn a_timer_that_fails_to_start_returns_the_session_running_on_the_card() {
    let (driver, stream) = ManualStreamDriver::new();
    let live = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(stereo(SampleRate::SR_48K), driver)
        .expect("builds with no device");
    let before = live.device().spec().clone();
    let (back, again) = ManualStreamDriver::new();
    let err = live
        .into_system_clock_on(UnstartableTimer, back)
        .expect_err("the timer thread will not start");
    assert!(
        matches!(err.error().error(), Error::Clock(_)),
        "{}",
        err.error()
    );
    let mut live = err.into_session();
    assert!(live.device().is_running(), "the card runs again");
    assert_eq!(live.device().graph_rate(), SampleRate::SR_48K);
    assert_eq!(
        *live.device().spec(),
        before,
        "the spec it had, block included"
    );
    assert!(
        again.render_block(Samples::new(64)).is_some(),
        "the card renders blocks again"
    );
    assert!(
        stream.render_block(Samples::new(64)).is_none(),
        "the old stream is gone"
    );
    let _ = live.update();
}

// --- What is not lost across a switch ---

/// **A fault the timer reported and no update read is reported after the
/// switch**: the card's start clears the record, so the session reads it first.
#[test]
fn a_fault_no_update_read_is_reported_after_the_switch() {
    let s = system();
    s.faults().record_overrun("the timer fell behind");
    let (driver, _card) = ManualStreamDriver::new();
    let mut live = s
        .into_device_on(stereo(SampleRate::SR_48K), driver)
        .unwrap_or_else(|e| panic!("switches: {}", e.error()));
    let events = live.update();
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::Fault(f) if f.kind == tutti_engine::StreamFaultKind::Overrun
        )),
        "{events:?}"
    );
}

/// **A restart's report no update read is reported after the switch to the
/// timer**: it was held for the next poll, which the card's session no longer
/// has.
#[test]
fn a_restart_report_no_update_read_is_reported_after_the_switch() {
    let (driver, _stream) = ManualStreamDriver::new();
    let mut live = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(stereo(SampleRate::SR_48K), driver)
        .expect("builds with no device");
    let (driver, _new) = ManualStreamDriver::new();
    live.restart_on(stereo(SampleRate::SR_44K1), driver, None, None)
        .expect("restarts");
    let mut system = live
        .into_system_clock()
        .unwrap_or_else(|e| panic!("switches: {}", e.error()));
    let events = system.update();
    assert!(
        events.iter().any(|e| matches!(
            e,
            SessionEvent::Device(tutti_session::DeviceEvent::Restarted(_))
        )),
        "{events:?}"
    );
}

// --- Two drivers never run the graph at once ---

/// Records which thread renders each block, then silence, and panics if two
/// blocks are in `process` at once.
#[derive(Clone)]
struct Renderer {
    log: tutti_fixtures::Record<std::thread::ThreadId>,
    inside: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl tutti_graph::Node for Renderer {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::source_unassigned(ChannelLayout::MONO)
    }

    fn prepare(&mut self, _: &tutti_graph::Prepare) {}

    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        assert!(
            !self.inside.swap(true, std::sync::atomic::Ordering::SeqCst),
            "two threads in process at once"
        );
        self.log
            .lock()
            .expect("log")
            .push(std::thread::current().id());
        io.output(0).fill(0.0);
        self.inside
            .store(false, std::sync::atomic::Ordering::SeqCst);
        tutti_graph::Status::Modified
    }

    fn reset(&mut self) {}
}

/// A card on a thread of its own, rendering a block every millisecond.
struct ThreadedCard;

struct ThreadedRunning {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl tutti_engine::RunningStream for ThreadedRunning {
    fn stop(self: Box<Self>) {}
}

impl Drop for ThreadedRunning {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl tutti_engine::StreamDriver for ThreadedCard {
    type Running = ThreadedRunning;
    type Error = tutti_cpal::DeviceError;

    fn open(
        self,
        spec: &OutputSpec,
        mut block: tutti_engine::OutputBlock,
        _: std::sync::Arc<tutti_engine::StreamFaults>,
    ) -> Result<tutti_engine::OpenedOutput<ThreadedRunning>, Self::Error> {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&stop);
        let channels = usize::from(spec.channels.count());
        let thread = std::thread::spawn(move || {
            let mut data = vec![0.0f32; 64 * channels];
            while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                block.render(&mut data);
                std::thread::yield_now();
            }
        });
        Ok(tutti_engine::OpenedOutput {
            stream: ThreadedRunning {
                stop,
                thread: Some(thread),
            },
            quantum: spec.quantum,
        })
    }
}

/// How many times the rendering thread changed, and whether one that left came
/// back: with two drivers that never overlap, once, and no.
fn hand_overs(log: &[std::thread::ThreadId]) -> (usize, bool) {
    let mut changes = 0;
    let mut seen = Vec::new();
    let mut back = false;
    for pair in log.windows(2) {
        if pair[0] != pair[1] {
            changes += 1;
        }
    }
    for id in log {
        if seen.last() != Some(id) {
            if seen.contains(id) {
                back = true;
            }
            seen.push(*id);
        }
    }
    (changes, back)
}

/// **The old driver's last block is rendered before the new one's first, in
/// both directions**: the thread that renders the graph changes exactly once
/// and the old one never renders again. (The engine has one renderer: a second
/// thread in `process` at once is the failure.)
#[test]
fn the_old_driver_renders_its_last_block_before_the_new_one_its_first() {
    let log = tutti_fixtures::Record::new();
    // The timer, then a card on a thread of its own.
    let mut s = system();
    let node = s.graph().add(ForkByClone(Renderer {
        log: log.clone(),
        inside: Default::default(),
    }));
    s.graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(node, 0)])
        .unwrap()
        .unfaded();
    let _ = s.update();
    wait_until("the timer to render", HANG_GUARD, || {
        !log.lock().unwrap().is_empty()
    });
    let mut live = s
        .into_device_on(stereo(SampleRate::SR_48K), ThreadedCard)
        .unwrap_or_else(|e| panic!("switches: {}", e.error()));
    let _ = live.update();
    let at = log.lock().unwrap().len();
    wait_until("the card to render", HANG_GUARD, || {
        log.lock().unwrap().len() > at + 3
    });
    let (changes, back) = hand_overs(&log.lock().unwrap());
    assert_eq!((changes, back), (1, false), "timer, then card");
    assert_eq!(live.faults().count(), 0, "no block overlapped another");

    // The card, then the timer.
    let mut system = live
        .into_system_clock()
        .unwrap_or_else(|e| panic!("switches: {}", e.error()));
    let _ = system.update();
    let at = log.lock().unwrap().len();
    wait_until("the timer to render again", HANG_GUARD, || {
        log.lock().unwrap().len() > at + 3
    });
    let (changes, back) = hand_overs(&log.lock().unwrap());
    assert_eq!(system.faults().count(), 0, "nor on the way back");
    assert_eq!(
        (changes, back),
        (2, false),
        "timer, card, timer: a new timer thread each"
    );
}

/// What an open MIDI port is across a switch: the handle stays live, the
/// port's input node stays the graph's, and the device stays connected.
#[cfg(feature = "midi-hardware")]
mod midi_ports {
    use tutti_midi_device::EndpointId;
    use tutti_midi_hardware::test_support::{FakeBackend, FakeCounters, endpoint};
    use tutti_session::{MidiInput, Peer, Presence};
    use tutti_ump::{Message, MidiGroup, System, Ump};

    use super::*;

    fn timer_with_port() -> (SystemSession, FakeCounters, MidiInput) {
        let (backend, fake) =
            FakeBackend::build_with(vec![endpoint(1, "Keys")], vec![], usize::MAX);
        let mut s = system_builder()
            .midi_hardware_over(backend, tutti_midi_hardware::MidiHardwareConfig::new())
            .build()
            .expect("builds");
        let _ = s.update();
        let keys = s.midi().devices().remove(0);
        let opened = {
            use std::task::{Context, Poll, Waker};
            let wait = s.open_midi_input(keys).expect("begins");
            match std::pin::pin!(wait).poll(&mut Context::from_waker(Waker::noop())) {
                Poll::Ready(outcome) => outcome.expect("a fake backend starts at once"),
                Poll::Pending => panic!("a fake backend's start is ready on the first poll"),
            }
        };
        let port = s.apply(opened).expect("opens");
        (s, fake, port)
    }

    fn tick() -> Ump {
        Message::System(MidiGroup::FIRST, System::TimingClock).into()
    }

    /// **An open input port keeps its handle, its input node and its device
    /// across a switch in both directions**: the handle still reads `Live`, it
    /// names the same node and the graph holds it, and the device still takes a
    /// message.
    #[test]
    fn an_open_input_port_survives_a_switch_to_the_card_and_back() {
        let (s, fake, port) = timer_with_port();
        let node = port.node();

        let (driver, _card) = ManualStreamDriver::new();
        let mut live = s
            .into_device_on(stereo(SampleRate::SR_48K), driver)
            .unwrap_or_else(|e| panic!("switches: {}", e.error()));
        let _ = live.update();
        assert_eq!(port.presence(), Presence::Live);
        assert!(live.graph_ref().contains(node), "the input node stays");
        assert!(fake.send_input(EndpointId::from_raw(1), tick(), Instant::now()));

        let mut back = live
            .into_system_clock()
            .unwrap_or_else(|e| panic!("switches: {}", e.error()));
        let _ = back.update();
        assert_eq!(port.presence(), Presence::Live);
        assert!(back.graph_ref().contains(node));
        assert!(fake.send_input(EndpointId::from_raw(1), tick(), Instant::now()));
    }
}
