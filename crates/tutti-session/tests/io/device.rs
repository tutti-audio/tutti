//! A session on a device, device-free: `build_device_on` and `restart_on`
//! over `ManualStreamDriver`s standing for the devices.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tutti_buffers::InterleavedMut;
use tutti_platform::Instant;
use tutti_rt::Mutex;

use tutti_buffers::{ChannelLayout, ChannelTopology, MaxBlock};
use tutti_cpal::{DeviceDirection, DeviceId, DeviceSelector, Snapshot, SnapshotSource};
use tutti_engine::{BackendErrorKind, ManualStreamDriver, OutputQuantum, OutputSpec};
use tutti_graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_nodes::testing::Osc;
use tutti_nodes::{LimiterConfig, LimiterControls, LimiterNode};
use tutti_session::{
    Backoff, DeviceEvent, DevicePolicy, DeviceRestart, DeviceWatch, InputPort, LiveSession,
    MainOutput, OutputPort, Phase, PhaseCx, RestartOutcome, Session, SessionEvent, Workers,
};
use tutti_units::{Db, Hz, SampleRate, Samples};

fn spec_at(rate: f64) -> OutputSpec {
    OutputSpec::new(
        SampleRate::of(rate),
        ChannelLayout::STEREO,
        tutti_buffers::SampleFormat::F32,
    )
}

/// **The session publishes the meter and the taps its callback feeds**, not
/// disconnected ones: a host opens a tap with `Graph::open_heard_tap` and
/// sees the frames the callback renders, and reads the level they carry from
/// `Session::take_main_output_meter`.
#[test]
fn the_session_publishes_the_meter_and_taps_its_callback_feeds() {
    let (driver, stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .expect("builds with no device");
    let dc = session
        .graph()
        .add(ForkByClone(tutti_nodes::testing::Const::mono(0.5)));
    session
        .graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(dc, 0)])
        .unwrap()
        .unfaded();
    let mut tap = session
        .graph()
        .taps()
        .open_main_output(Samples::new(1_024))
        .expect("the sources exist");
    let _ = session.update();
    let mut meter = session
        .take_main_output_meter()
        .expect("declared at the build");
    stream
        .render_block(Samples::new(64))
        .expect("the stream is open");
    let mut frames = 0;
    let mut one = [0.0f32; 2];
    while tap
        .pop_into(InterleavedMut::new(&mut one, tap.layout()).unwrap())
        .unwrap()
        .get()
        == 1
    {
        assert_eq!(one, [0.5, 0.0]);
        frames += 1;
    }
    assert_eq!(frames, 64, "the callback's block reached the session's tap");
    let level = meter.read()[0].peak.get();
    assert!(
        (level - 0.5).abs() < 1e-6,
        "the callback's block was measured: {level}"
    );
}

/// **The session's load meter is the one its callback's engine feeds**:
/// each block the device renders is one block measured.
#[test]
fn the_session_reads_the_load_its_callback_measures() {
    let (driver, stream) = ManualStreamDriver::new();
    let session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .expect("builds with no device");
    for _ in 0..3 {
        stream
            .render_block(Samples::new(64))
            .expect("the stream is open");
    }
    let blocks = session.blocks();
    assert_eq!(blocks.get().blocks, 3);
}

/// **A rerate from 48 kHz to 96 kHz keeps the published compensation in
/// step with the plan.** A 1 kHz sine dry on output 0 and through a
/// lookahead limiter on output 1: the dry channel pre-rolls by the
/// limiter's latency, a time, so in samples it doubles with the rate. After
/// the restart, before any block at the new rate, the published table and
/// total are the figures of the plan the audio side holds (the re-prepare's
/// resumed plan), and they are the limiter's new latency.
#[test]
fn a_rerate_keeps_the_published_compensation_in_step_with_the_plan() {
    let (driver, old) = ManualStreamDriver::new();
    let mut s = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .expect("builds with no device");
    let osc = s.graph().add(ForkByClone(Osc::sine(Hz::new(1_000.0))));
    let lim = s.graph().add(LimiterNode::<f32>::from(
        LimiterConfig::new(ChannelTopology::MONO).controls(LimiterControls {
            threshold: Db::new(0.0),
            ceiling: Db::new(0.0),
            ..LimiterControls::DEFAULT
        }),
    ));
    s.graph()
        .set_sources(InputPort::new(lim, 0), [OutputPort::new(osc, 0)])
        .unwrap()
        .unfaded();
    s.graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(osc, 0)])
        .unwrap()
        .unfaded();
    s.graph()
        .set_sources(MainOutput::channel(1), [OutputPort::new(lim, 0)])
        .unwrap()
        .unfaded();
    let _ = s.update();
    old.render_block(Samples::new(480)).expect("open");
    let _ = s.update();

    let in_step = |s: &LiveSession, what: &str| -> Samples {
        let lim = s.graph_ref().node_latency(lim).unwrap();
        assert_eq!(s.latency(), lim, "{what}: the plan compensates the limiter");
        assert_eq!(
            s.compensation().to_vec().first().copied(),
            Some(lim),
            "{what}: dry pre-rolls"
        );
        lim
    };
    let at48 = in_step(&s, "48 kHz");
    assert!(at48 > Samples::new(0), "a lookahead to compensate");

    let (driver, new) = ManualStreamDriver::new();
    s.restart_on(spec_at(96_000.0), driver, None, None)
        .expect("restarts");
    assert!(!old.is_open() && new.is_open(), "on the new device");
    assert_eq!(s.sample_rate(), SampleRate::SR_96K);
    let at96 = in_step(&s, "96 kHz, before the first block");
    assert_eq!(at96, at48 * 2, "the lookahead is a time");

    for _ in 0..4 {
        new.render_block(Samples::new(960)).expect("open");
        let _ = s.update();
    }
    in_step(&s, "96 kHz, after");
}

// ---- the device watch ----

/// A source the test feeds by hand.
#[derive(Clone, Default)]
struct Fake(Arc<Mutex<VecDeque<Snapshot>>>);

impl Fake {
    fn show(&self, outputs: &[&str], default: Option<&str>) {
        self.0.lock().push_back(Snapshot::from_names(
            outputs.iter().copied(),
            [],
            default,
            None,
        ));
    }
}

impl SnapshotSource for Fake {
    fn poll(&mut self) -> Option<Snapshot> {
        self.0.lock().pop_front()
    }
}

fn id(name: &str) -> DeviceId {
    DeviceId::new(name)
}

/// Runs the `Poll` phase at `base + ms`, as an update would.
fn poll(session: &mut LiveSession, base: Instant, ms: u64) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    session.run_phase(
        Phase::Poll,
        PhaseCx::at(base + Duration::from_millis(ms)),
        &mut out,
    );
    out
}

fn wants(events: &[SessionEvent]) -> Vec<DeviceSelector> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Device(DeviceEvent::RestartWanted(r)) => Some(r.device.clone()),
            _ => None,
        })
        .collect()
}

fn policy() -> DevicePolicy {
    DevicePolicy {
        follow_default: true,
        reconnect: true,
        backoff: Backoff {
            base: Duration::from_millis(500),
            max: Duration::from_secs(4),
            quiet: Duration::from_secs(10),
        },
        ..DevicePolicy::default()
    }
}

/// **Appearing, vanishing and a moved default are session events**, the same
/// headless and in an app, with no policy on: nothing restarts.
#[test]
fn the_watch_reports_what_the_host_lists() {
    let (driver, _stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    let fake = Fake::default();
    session.watch_devices_on(
        fake.clone(),
        DevicePolicy::default(),
        DeviceSelector::Default,
        Some(id("A")),
    );
    let base = Instant::now();
    assert!(poll(&mut session, base, 0).is_empty(), "no snapshot yet");

    fake.show(&["A"], Some("A"));
    let events = poll(&mut session, base, 10);
    assert!(
        matches!(
            events.as_slice(),
            [
                SessionEvent::Device(DeviceEvent::Appeared {
                    direction: DeviceDirection::Output,
                    ..
                }),
                SessionEvent::Device(DeviceEvent::DefaultChanged {
                    direction: DeviceDirection::Output,
                    ..
                }),
            ]
        ),
        "{events:?}"
    );

    fake.show(&["A", "B"], Some("B"));
    let events = poll(&mut session, base, 20);
    assert!(
        matches!(
            events.as_slice(),
            [SessionEvent::Device(DeviceEvent::Appeared { id, .. }), SessionEvent::Device(DeviceEvent::DefaultChanged { .. })] if id.name() == "B"
        ),
        "{events:?}"
    );
    assert!(wants(&events).is_empty(), "the policy is off");

    fake.show(&["B"], Some("B"));
    let events = poll(&mut session, base, 30);
    assert!(
        matches!(
            events.as_slice(),
            [SessionEvent::Device(DeviceEvent::Lost { id, .. })] if id.name() == "A"
        ),
        "{events:?}"
    );
}

/// **The default moving restarts a stream that follows it, and the restart's
/// outcome says the rate moved** (the host reopens its mic on it).
#[test]
fn following_the_default_restarts_and_reports_the_rate() {
    let (driver, _stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(44_100.0), driver)
        .unwrap();
    let fake = Fake::default();
    session.watch_devices_on(
        fake.clone(),
        policy(),
        DeviceSelector::Default,
        Some(id("A")),
    );
    let base = Instant::now();
    fake.show(&["A", "B"], Some("A"));
    assert!(wants(&poll(&mut session, base, 0)).is_empty());

    fake.show(&["A", "B"], Some("B"));
    let events = poll(&mut session, base, 100);
    assert_eq!(wants(&events), [DeviceSelector::Default]);

    // The host runs it (here on a device-free driver at 48 kHz).
    let (driver, new) = ManualStreamDriver::new();
    session
        .restart_on(spec_at(48_000.0), driver, None, None)
        .unwrap();
    assert!(new.is_open());
    let events = poll(&mut session, base, 110);
    let [SessionEvent::Device(DeviceEvent::Restarted(r))] = events.as_slice() else {
        panic!("{events:?}");
    };
    assert!(r.rate_changed());
    assert_eq!(
        (r.previous_rate, r.rate),
        (SampleRate::SR_44K1, SampleRate::SR_48K)
    );

    // Another restart at the same rate is not a rate change.
    let (driver, _s) = ManualStreamDriver::new();
    session
        .restart_on(spec_at(48_000.0), driver, None, None)
        .unwrap();
    let events = poll(&mut session, base, 120);
    let [SessionEvent::Device(DeviceEvent::Restarted(r))] = events.as_slice() else {
        panic!("{events:?}");
    };
    assert!(!r.rate_changed());
}

/// **The session never overrides an explicit choice.** A stream the host
/// put on "A" stays silent while "A" is gone, whatever else is the default,
/// and is asked back onto "A" only when it returns.
#[test]
fn an_explicit_device_is_not_replaced_by_the_default() {
    let (driver, stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    let fake = Fake::default();
    session.watch_devices_on(
        fake.clone(),
        policy(),
        DeviceSelector::named("A"),
        Some(id("A")),
    );
    let base = Instant::now();
    fake.show(&["A", "B"], Some("B"));
    assert!(wants(&poll(&mut session, base, 0)).is_empty());
    fake.show(&["B"], Some("B"));
    stream.fail(
        BackendErrorKind::DeviceGone,
        &"device not available",
        Instant::now(),
    );
    assert!(
        wants(&poll(&mut session, base, 5_000)).is_empty(),
        "A gone, B is not a substitute"
    );
    fake.show(&["A", "B"], Some("B"));
    assert_eq!(
        wants(&poll(&mut session, base, 10_000)),
        [DeviceSelector::named("A")]
    );
}

/// **A disconnect fault is a loss the policy answers**, once.
#[test]
fn a_disconnect_fault_asks_for_a_restart_once() {
    let (driver, stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    let fake = Fake::default();
    session.watch_devices_on(
        fake.clone(),
        policy(),
        DeviceSelector::Default,
        Some(id("A")),
    );
    let base = Instant::now();
    fake.show(&["A", "B"], Some("A"));
    poll(&mut session, base, 0);
    stream.fail(
        BackendErrorKind::DeviceGone,
        &"device not available",
        Instant::now(),
    );
    let events = poll(&mut session, base, 10);
    assert!(events.iter().any(|e| matches!(e, SessionEvent::Fault(_))));
    assert_eq!(wants(&events), [DeviceSelector::Default]);
    assert!(
        wants(&poll(&mut session, base, 20)).is_empty(),
        "held by the backoff"
    );
}

/// **A flapping device cannot cause a restart storm** through the session.
#[test]
fn a_flapping_device_is_not_restarted_on_every_change() {
    let (driver, stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    let fake = Fake::default();
    session.watch_devices_on(
        fake.clone(),
        policy(),
        DeviceSelector::named("A"),
        Some(id("A")),
    );
    let base = Instant::now();
    fake.show(&["A"], Some("A"));
    poll(&mut session, base, 0);
    let mut asked = 0;
    for k in 0..100u64 {
        if k % 2 == 0 {
            stream.fail(
                BackendErrorKind::DeviceGone,
                &"device not available",
                Instant::now(),
            );
            fake.show(&[], None);
        } else {
            fake.show(&["A"], Some("A"));
        }
        asked += wants(&poll(&mut session, base, 100 * (k + 1))).len();
    }
    assert!(asked >= 1, "it did come back");
    assert!(asked <= 6, "{asked} restarts in 10 s of flapping");
}

/// **A restart that fails leaves the old spec and graph rate**, is reported,
/// and the policy asks again only after its backoff.
#[test]
fn a_refused_restart_keeps_the_old_spec_and_is_retried_later() {
    let (driver, _stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    let fake = Fake::default();
    session.watch_devices_on(
        fake.clone(),
        policy(),
        DeviceSelector::Default,
        Some(id("A")),
    );
    let base = Instant::now();
    fake.show(&["A", "B"], Some("A"));
    poll(&mut session, base, 0);
    fake.show(&["A", "B"], Some("B"));
    assert_eq!(wants(&poll(&mut session, base, 100)).len(), 1);

    // The restart is refused (a block past the engine's capacity), before the stream stops.
    let (driver, _s) = ManualStreamDriver::new();
    let err = session.restart_on(
        spec_at(96_000.0),
        driver,
        Some(MaxBlock::of(Samples::new(16_384))),
        None,
    );
    assert!(err.is_err());
    assert_eq!(
        session.device().spec().sample_rate,
        SampleRate::SR_48K,
        "old spec kept"
    );
    assert_eq!(session.device().graph_rate(), SampleRate::SR_48K);
    let events = poll(&mut session, base, 150);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::Device(DeviceEvent::RestartFailed(_)))),
        "{events:?}"
    );
    assert!(wants(&events).is_empty(), "inside the backoff");
    // Still on A while the default is B: asked again once the wait is over.
    assert_eq!(wants(&poll(&mut session, base, 700)).len(), 1);
}

/// **A restart that fails after the stream stopped is not forgotten**: the
/// stream is dead, so when the default is back on the device it was on, the
/// policy asks again (the device it "runs on" is not playing).
#[test]
fn a_restart_that_stopped_the_stream_is_retried() {
    let (driver, _stream) = ManualStreamDriver::new();
    let mut live = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    let fake = Fake::default();
    live.watch_devices_on(
        fake.clone(),
        policy(),
        DeviceSelector::Default,
        Some(id("A")),
    );
    let base = Instant::now();
    fake.show(&["A", "B"], Some("A"));
    poll(&mut live, base, 0);
    fake.show(&["A", "B"], Some("B"));
    assert_eq!(wants(&poll(&mut live, base, 100)).len(), 1);

    // A restart onto a stereo device that declares a six-channel placement is
    // refused by the re-rate hook, which runs after the stop: the stream stays
    // stopped and the old spec stays.
    let (replacement, _replacement_stream) = ManualStreamDriver::new();
    let err = live.restart_on(
        spec_at(48_000.0),
        replacement,
        None,
        Some(ChannelTopology::default_for(ChannelLayout::clamped(6)).unwrap()),
    );
    assert!(err.is_err());
    assert!(!live.device().is_running(), "stopped");
    assert_eq!(
        live.device().spec().sample_rate,
        SampleRate::SR_48K,
        "old spec kept"
    );
    assert_eq!(live.device().graph_rate(), SampleRate::SR_48K);
    let events = poll(&mut live, base, 150);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::Device(DeviceEvent::RestartFailed(_)))),
        "{events:?}"
    );

    // The default goes back to A, which the dead stream was on.
    fake.show(&["A", "B"], Some("A"));
    assert_eq!(wants(&poll(&mut live, base, 700)).len(), 1);
}

/// **A stale `RestartWanted` cannot override an explicit choice.** The
/// policy asks for `Default`; before it runs, the host restarts onto
/// another device. The wanted restart is dropped and the choice stays.
#[test]
fn a_stale_wanted_restart_does_not_replace_a_later_choice() {
    let (driver, _stream) = ManualStreamDriver::new();
    let mut live = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    let fake = Fake::default();
    live.watch_devices_on(
        fake.clone(),
        policy(),
        DeviceSelector::Default,
        Some(id("A")),
    );
    let base = Instant::now();
    fake.show(&["A", "X"], Some("A"));
    poll(&mut live, base, 0);
    fake.show(&["A", "X"], Some("X"));
    let wanted = poll(&mut live, base, 100);
    let [DeviceSelector::Default] = wants(&wanted).as_slice() else {
        panic!("{wanted:?}");
    };
    // The host chooses X first (this device-free engine cannot open it, but
    // the choice is made).
    let _ = live.restart(DeviceRestart::new(DeviceSelector::named("X")));
    // The policy's own request, as the event carried it, is stale now.
    let [stale] = wanted
        .into_iter()
        .filter_map(|e| match e {
            SessionEvent::Device(DeviceEvent::RestartWanted(r)) => Some(r),
            _ => None,
        })
        .collect::<Vec<_>>()
        .try_into()
        .expect("one request");
    assert!(
        live.restart(stale).unwrap() == RestartOutcome::Stale,
        "a stale request is dropped, not run"
    );
    assert_eq!(
        live.watch_devices_want(),
        Some(DeviceSelector::named("X")),
        "the choice stands"
    );
}

/// **A fault after a restart is reported** even when the count equals the
/// one the last poll saw (a restart clears the record).
#[test]
fn a_fault_after_a_restart_is_reported_again() {
    let (driver, _first) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    let _ = session.update();
    let base = Instant::now();
    let (driver, second) = ManualStreamDriver::new();
    session
        .restart_on(spec_at(48_000.0), driver, None, None)
        .unwrap();
    second.fail(
        BackendErrorKind::DeviceGone,
        &"device not available",
        Instant::now(),
    );
    let events = poll(&mut session, base, 0);
    assert!(events.iter().any(|e| matches!(e, SessionEvent::Fault(_))));
    let (driver, third) = ManualStreamDriver::new();
    session
        .restart_on(spec_at(48_000.0), driver, None, None)
        .unwrap();
    third.fail(
        BackendErrorKind::DeviceGone,
        &"device not available",
        Instant::now(),
    );
    let events = poll(&mut session, base, 20);
    assert!(
        events.iter().any(|e| matches!(e, SessionEvent::Fault(_))),
        "{events:?}"
    );
}

/// **Held restarts are declined**: nothing is asked while held.
#[test]
fn a_host_can_hold_restarts() {
    let (driver, _stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    let fake = Fake::default();
    session.watch_devices_on(
        fake.clone(),
        policy(),
        DeviceSelector::Default,
        Some(id("A")),
    );
    let base = Instant::now();
    session.pause_device_restarts(true);
    fake.show(&["A", "B"], Some("A"));
    poll(&mut session, base, 0);
    fake.show(&["A", "B"], Some("B"));
    assert!(wants(&poll(&mut session, base, 100)).is_empty());
    session.pause_device_restarts(false);
    assert_eq!(wants(&poll(&mut session, base, 200)).len(), 1);
}

/// **A run of backend errors that stalls the stream is a lost stream.** The
/// first error is a `Backend` fault (reported, not a loss); errors still
/// arriving after the stall grace (500 ms, `tutti-cpal`'s `STALL_GRACE`) are
/// promoted to `Disconnected`, reported again, and the policy answers it.
/// This is how ALSA, which says `DeviceNotAvailable` rarely, loses a stream.
/// The errors carry the instants the grace is measured between.
#[test]
fn a_stalled_backend_run_becomes_a_lost_stream() {
    let (driver, stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    let fake = Fake::default();
    session.watch_devices_on(
        fake.clone(),
        policy(),
        DeviceSelector::Default,
        Some(id("A")),
    );
    let base = Instant::now();
    fake.show(&["A", "B"], Some("A"));
    poll(&mut session, base, 0);

    let (kind, message) = (BackendErrorKind::Other, "POLLERR");
    let run_start = Instant::now();
    stream.fail(kind, &message, run_start);
    let events = poll(&mut session, base, 10);
    assert!(events.iter().any(
        |e| matches!(e, SessionEvent::Fault(f) if f.kind == tutti_engine::StreamFaultKind::Backend)
    ));
    assert!(
        wants(&events).is_empty(),
        "a backend error alone is not a loss"
    );

    stream.fail(kind, &message, run_start + Duration::from_millis(600));
    let events = poll(&mut session, base, 700);
    assert!(events.iter().any(|e| matches!(e, SessionEvent::Fault(f) if f.kind == tutti_engine::StreamFaultKind::Disconnected)), "{events:?}");
    assert_eq!(wants(&events), [DeviceSelector::Default]);
}

/// **`render_worker_start` reaches the live pool**: with four participants
/// the hook runs once for each of the three helpers, with indices 1..=3.
#[test]
fn the_render_worker_start_hook_runs_once_per_live_helper() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::clone(&seen);
    let (driver, stream) = ManualStreamDriver::new();
    let session = Session::builder()
        .render_workers(Workers::of(4))
        .render_worker_start(move |i| hook.lock().push(i))
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .expect("builds with no device");
    assert_eq!(indices_after(&seen, 3), [1, 2, 3]);
    // Dropping the session and the stream drops the pool, which joins its
    // helpers: no hook runs after this.
    drop(session);
    drop(stream);
    assert_eq!(seen.lock().len(), 3, "once per helper");
}

/// **`Workers::PerCore` reaches the live pool as one participant per core the
/// OS reports**: the hook runs once for each helper, indices `1..cores`, and
/// not at all on a one-core box (no pool).
#[test]
fn per_core_workers_start_a_helper_for_every_core_but_the_callbacks() {
    let cores = Workers::PerCore.threads();
    assert_eq!(
        cores,
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let hook = Arc::clone(&seen);
    let (driver, stream) = ManualStreamDriver::new();
    let live = Session::builder()
        .render_workers(Workers::PerCore)
        .render_worker_start(move |i| hook.lock().push(i))
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .expect("builds with no device");
    let helpers: Vec<usize> = (1..cores).collect();
    assert_eq!(indices_after(&seen, helpers.len()), helpers);
    // Dropping the session and the stream drops the pool, which joins its
    // helpers: no hook runs after this.
    drop(live);
    drop(stream);
    assert_eq!(seen.lock().len(), helpers.len(), "once per helper");
}

/// Wait until `seen` holds `n` indices (a hang guard of a minute), then
/// return them sorted.
fn indices_after(seen: &Mutex<Vec<usize>>, n: usize) -> Vec<usize> {
    let t = Instant::now();
    while seen.lock().len() < n && t.elapsed() < Duration::from_secs(60) {
        std::thread::yield_now();
    }
    let mut v = seen.lock().clone();
    v.sort_unstable();
    v
}

/// **The heard tap follows a restart onto a device of another width.** A tap
/// open across the change keeps counting its blocks lost (its width is the old
/// one's); one opened after it takes the new width and is fed.
#[test]
fn the_heard_tap_reopens_at_the_width_of_the_device_after_a_restart() {
    let (driver, _old) = ManualStreamDriver::new();
    let mut s = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .expect("builds with no device");
    let across = s.open_heard_tap(Samples::new(1_024));
    assert_eq!(across.width(), 2);

    let six = OutputSpec::new(
        SampleRate::SR_48K,
        ChannelLayout::from(6u16),
        tutti_buffers::SampleFormat::F32,
    );
    let (driver, new) = ManualStreamDriver::new();
    s.restart_on(six, driver, None, None).expect("restarts");
    new.render_block(Samples::new(64)).expect("open");
    assert_eq!(
        across.available(),
        Samples::new(0),
        "the old width gets nothing"
    );
    assert_eq!(
        across.take_lost(),
        Samples::new(64),
        "and counts the block lost"
    );

    drop(across);
    let after = s.open_heard_tap(Samples::new(1_024));
    assert_eq!(after.width(), 6, "the device's width now");
    new.render_block(Samples::new(64)).expect("open");
    assert_eq!(after.available(), Samples::new(64));
    assert_eq!(after.take_lost(), Samples::new(0));
}

/// **A restart before the first update has no plan to resume**: the hook
/// completes, and the session then sends its plan and renders it on the new
/// device.
#[test]
fn a_restart_before_the_first_update_then_renders() {
    let (driver, _old) = ManualStreamDriver::new();
    let mut s = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .expect("builds with no device");
    let dc = s
        .graph()
        .add(ForkByClone(tutti_nodes::testing::Const::mono(0.5)));
    s.graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(dc, 0)])
        .unwrap()
        .unfaded();
    let (driver, new) = ManualStreamDriver::new();
    s.restart_on(spec_at(48_000.0), driver, None, None)
        .expect("restarts");
    let _ = s.update();
    new.render_block(Samples::new(64)).expect("open");
    let block = new.render_block(Samples::new(64)).expect("open");
    assert_eq!(block.len(), 64 * 2);
    assert_eq!(block[0], 0.5, "the first channel carries the constant");
}

/// **The session lists the outputs once**: the first `list_outputs_once`
/// starts its listing and the second starts nothing, so the second fake never
/// runs. The device is one the host supplied, so `device_name` is `Ok(None)`,
/// not an error.
#[test]
fn the_session_lists_the_outputs_once() {
    let (driver, _stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    assert_eq!(
        session.device().device_name().unwrap(),
        None,
        "supplied, by design"
    );

    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fake = || {
        let calls = std::sync::Arc::clone(&calls);
        tutti_cpal::DeviceListing::from_fn(move || {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Vec::new())
        })
    };
    session.list_outputs_once(fake());
    let deadline = tutti_platform::Instant::now() + std::time::Duration::from_secs(20);
    while !session
        .update()
        .iter()
        .any(|e| matches!(e, SessionEvent::Device(DeviceEvent::OutputsListed(_))))
    {
        assert!(tutti_platform::Instant::now() < deadline, "no list arrived");
        std::thread::yield_now();
    }
    session.list_outputs_once(fake());
    for _ in 0..3 {
        assert!(
            !session
                .update()
                .iter()
                .any(|e| matches!(e, SessionEvent::Device(DeviceEvent::OutputsListed(_)))),
            "a second listing answers"
        );
    }
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the second listing never starts"
    );
}

/// A declared main output is placed on the device it is declared for, and a restart to
/// a device that cannot place it is refused rather than rendering silence: four
/// buses on a six-wide device restart onto another six-wide one, and not onto
/// a stereo one.
#[test]
fn a_restart_onto_a_device_that_cannot_place_the_declared_main_output_is_refused() {
    let wide = |rate| {
        OutputSpec::new(
            SampleRate::of(rate),
            ChannelLayout::from(6u16),
            tutti_buffers::SampleFormat::F32,
        )
    };
    let (driver, _stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .main_output_topology(
            tutti_buffers::ChannelTopology::unassigned(ChannelLayout::QUAD).unwrap(),
        )
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(wide(48_000.0), driver)
        .expect("four buses place on six channels");
    assert_eq!(session.graph_ref().outputs(), 4);

    let (driver, _new) = ManualStreamDriver::new();
    session
        .restart_on(wide(44_100.0), driver, None, None)
        .expect("another six-wide device takes it");
    assert_eq!(session.graph_ref().outputs(), 4, "the device is no floor");

    let (driver, _stereo) = ManualStreamDriver::new();
    let refused = session.restart_on(spec_at(48_000.0), driver, None, None);
    assert!(
        matches!(
            &refused,
            Err(e) if matches!(
                e.error(),
                tutti_session::Error::Unplaceable {
                    device,
                    source: tutti_buffers::FoldError::Unplaced { channel: 2, .. },
                    ..
                } if device.count() == 2
            )
        ),
        "{refused:?}"
    );
}

/// A restart takes what its own request states about the new device: the
/// session reads `Stated` after a restart that says how the device is wired
/// and `Default` after one that does not (another device is wired another
/// way), and a statement that is not the new device's width is refused before
/// the graph moves.
#[test]
fn a_restart_states_or_defaults_the_new_devices_placement() {
    use tutti_buffers::{Bed, Placement};
    let six = OutputSpec::new(
        SampleRate::SR_48K,
        ChannelLayout::from(6u16),
        tutti_buffers::SampleFormat::F32,
    );
    let (driver, _stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    assert_eq!(
        session.output_device().map(|d| d.placement),
        Some(Placement::Default)
    );

    let (driver, _new) = ManualStreamDriver::new();
    let wired = Bed::Surround51.topology();
    session
        .restart_on(six.clone(), driver, None, Some(wired))
        .unwrap();
    let placed = session.output_device().unwrap();
    assert_eq!(
        (placed.placement, placed.channels),
        (Placement::Stated, tutti_buffers::ChannelSet::from(wired))
    );

    // A statement of the wrong width is refused, and the old one stands.
    let (driver, _new) = ManualStreamDriver::new();
    let refused = session.restart_on(six.clone(), driver, None, Some(Bed::Stereo.topology()));
    assert!(
        matches!(
            &refused,
            Err(e) if matches!(
                e.error(),
                tutti_session::Error::Output(tutti_buffers::FoldError::Buffer(
                    tutti_buffers::BufferError::WidthMismatch { .. }
                ))
            )
        ),
        "{refused:?}"
    );
    assert_eq!(
        session.output_device().map(|d| d.placement),
        Some(Placement::Stated)
    );

    // The same width again with nothing stated: the default again.
    let (driver, _new) = ManualStreamDriver::new();
    session.restart_on(six, driver, None, None).unwrap();
    assert_eq!(
        session.output_device().map(|d| d.placement),
        Some(Placement::Default)
    );
}

/// A declaration the device refused is asked again of the next device: it is a
/// `Refused` status on stereo and `Applied` after a restart onto six channels.
#[test]
fn a_refused_declaration_is_retried_on_a_restart() {
    use tutti_session::MainOutputTopologyStatus;
    let buses = tutti_buffers::ChannelTopology::unassigned(ChannelLayout::QUAD).unwrap();
    let (driver, _stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .unwrap();
    assert!(session.declare_main_output_topology(buses).is_err());
    assert!(matches!(
        session.main_output_topology_status(),
        MainOutputTopologyStatus::Refused(_)
    ));
    assert_eq!(session.graph_ref().outputs(), 2);

    let wide = OutputSpec::new(
        SampleRate::SR_48K,
        ChannelLayout::from(6u16),
        tutti_buffers::SampleFormat::F32,
    );
    let (driver, _new) = ManualStreamDriver::new();
    session.restart_on(wide, driver, None, None).unwrap();
    assert_eq!(
        session.main_output_topology_status(),
        MainOutputTopologyStatus::Applied
    );
    assert_eq!(session.main_output_topology(), &buses);
    assert_eq!(session.graph_ref().outputs(), 4);
}

/// A declaration that waited for a main output crossfade and is refused when it
/// applies, because the device it finds no longer places it, is a `Refused`
/// status that a later widening does not erase, and a restart asks again.
#[test]
fn a_waiting_declaration_the_device_no_longer_places_is_refused_and_retried_on_a_restart() {
    use tutti_session::MainOutputTopologyStatus;
    let wide = OutputSpec::new(
        SampleRate::SR_48K,
        ChannelLayout::from(6u16),
        tutti_buffers::SampleFormat::F32,
    );
    let buses = tutti_buffers::ChannelTopology::unassigned(ChannelLayout::QUAD).unwrap();
    let stereo = tutti_buffers::Bed::Stereo.topology();
    let (driver, stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(wide.clone(), driver)
        .unwrap();
    // Something is heard, so the old main output has outputs to fade; the main output in
    // force fits both devices, and the change to it starts a fade.
    let dc = session
        .graph()
        .add(ForkByClone(tutti_nodes::testing::Const::mono(0.5)));
    session
        .graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(dc, 0)])
        .unwrap()
        .unfaded();
    let _ = session.update();
    session.declare_main_output_topology(stereo).unwrap();
    let _ = session.update();
    stream.render_block(Samples::new(64)).expect("open");
    // Four buses place on six channels, so the declaration is taken, and waits.
    session.declare_main_output_topology(buses).unwrap();
    assert_eq!(
        session.main_output_topology_status(),
        MainOutputTopologyStatus::Pending
    );
    assert_eq!(session.requested_main_output_topology(), &buses);

    // The device changes under it to one that cannot place four buses.
    let (driver, narrow) = ManualStreamDriver::new();
    session
        .restart_on(spec_at(48_000.0), driver, None, None)
        .expect("the main output in force places on stereo");
    for _ in 0..5 {
        narrow.render_block(Samples::new(64)).expect("open");
        let _ = session.update();
    }
    assert!(
        matches!(
            session.main_output_topology_status(),
            MainOutputTopologyStatus::Refused(tutti_graph::MainOutputError::Fold(_))
        ),
        "{:?}",
        session.main_output_topology_status()
    );
    assert_eq!(
        session.main_output_topology(),
        &stereo,
        "the main output in force stays"
    );

    let (driver, _wide) = ManualStreamDriver::new();
    session.restart_on(wide, driver, None, None).unwrap();
    assert_eq!(
        session.main_output_topology_status(),
        MainOutputTopologyStatus::Applied
    );
    assert_eq!(
        session.main_output_topology(),
        &buses,
        "the restart asked again"
    );
    assert_eq!(session.graph_ref().outputs(), 4);
}

/// A source whose render always panics.
#[derive(Clone)]
struct AlwaysPanics;

impl Node for AlwaysPanics {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        panic!("a node that always fails");
    }

    fn reset(&mut self) {}
}

/// **A panic in the render reaches the app as a device fault**: the callback
/// returns normally with silence, and the next `update` reports
/// `SessionEvent::Fault` of kind `CallbackPanicked`, once.
#[test]
fn a_panic_in_the_render_is_a_device_fault_event() {
    let (driver, stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_at(48_000.0), driver)
        .expect("builds with no device");
    let node = session.graph().add(ForkByClone(AlwaysPanics));
    session
        .graph()
        .set_sources(MainOutput::channel(0), [OutputPort::new(node, 0)])
        .unwrap()
        .unfaded();
    let _ = session.update();

    let block = stream
        .render_block(Samples::new(64))
        .expect("the stream is open");
    assert!(
        block.iter().all(|&s| s == 0.0),
        "the panicked block is silent"
    );

    let faulted = |events: &[SessionEvent]| {
        events
            .iter()
            .filter(|e| {
                matches!(e, SessionEvent::Fault(f)
                    if f.kind == tutti_engine::StreamFaultKind::CallbackPanicked)
            })
            .count()
    };
    assert_eq!(faulted(&session.update()), 1);
    assert_eq!(faulted(&session.update()), 0, "reported once");
}

/// Drives a watch with no stream through the public path: nothing listed asks
/// for nothing; once the device is listed, `retry_start` runs the start once
/// with the selector, on a stream that then renders. Returns what the start
/// was asked for.
fn start_when_listed(retry_startup: bool) -> Vec<DeviceSelector> {
    let fake = Fake::default();
    let mut watch = DeviceWatch::without_stream(
        fake.clone(),
        DevicePolicy {
            retry_startup,
            ..policy()
        },
        DeviceSelector::Default,
    );
    let asked = Arc::new(Mutex::new(Vec::new()));
    let (driver, stream) = ManualStreamDriver::new();
    let mut driver = Some(driver);
    let mut start = |watch: &mut DeviceWatch| {
        let mut events = Vec::new();
        let asked = Arc::clone(&asked);
        let driver = &mut driver;
        let ran = watch.retry_start(&mut events, |selector| {
            asked.lock().push(selector);
            Session::builder()
                .device(tutti_cpal::DeviceSelector::Default)
                .build_device_on(spec_at(48_000.0), driver.take().expect("one start"))
        });
        (ran, events)
    };

    fake.show(&[], None);
    let (ran, events) = start(&mut watch);
    assert!(ran.is_none(), "no device is listed: no start");
    assert!(wants(&events).is_empty());
    assert!(asked.lock().is_empty());

    fake.show(&["A"], Some("A"));
    let (ran, _) = start(&mut watch);
    if retry_startup {
        let live = ran
            .expect("the device is listed: a start is due")
            .expect("the start builds");
        assert!(
            stream.render_block(Samples::new(64)).is_some(),
            "the stream runs after the start"
        );
        // The session takes the same source over.
        let mut live = live;
        live.watch_devices(watch.into_source(), policy());
    } else {
        assert!(ran.is_none(), "retry_startup is off: no start");
        assert!(
            stream.render_block(Samples::new(64)).is_none(),
            "no stream was started"
        );
    }
    std::mem::take(&mut *asked.lock())
}

/// **A start that failed for want of a device is retried when the device is
/// listed**, once, with the selector the host asked for.
#[test]
fn a_start_with_no_device_is_retried_once_the_device_is_listed() {
    assert_eq!(start_when_listed(true), [DeviceSelector::Default]);
}

/// With `retry_startup` off the same listing starts nothing: the retry is the
/// policy's, not the watch's.
#[test]
fn a_listed_device_starts_nothing_with_retry_startup_off() {
    assert!(start_when_listed(false).is_empty());
}

/// **A restart onto a device the map does not fit is refused, and the map
/// is kept**, never reset to the identity: the error names the map and the
/// device, the old device is as it was, and once the host sets a map the new
/// device has places for, the same restart goes through.
#[test]
fn a_restart_onto_a_device_the_map_does_not_fit_is_refused_and_keeps_the_map() {
    use tutti_buffers::Bed;
    use tutti_engine::OutputMap;
    use tutti_session::OutputPolicy;
    let spec = |channels: usize| {
        OutputSpec::new(
            SampleRate::SR_48K,
            ChannelLayout::try_from(channels).unwrap(),
            tutti_buffers::SampleFormat::F32,
        )
    };
    let (driver, _stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec(6), driver)
        .unwrap();
    let policy = OutputPolicy {
        monitor: Some(Bed::Stereo.topology()),
        map: OutputMap::channels([4, 5]).unwrap(),
        ..OutputPolicy::default()
    };
    session
        .set(session.policy::<OutputPolicy>().to(policy.clone()))
        .unwrap();
    let _ = session.update();

    let (driver, _four) = ManualStreamDriver::new();
    let refused = session.restart_on(spec(4), driver, None, None);
    match refused {
        Err(e) => match e.error() {
            tutti_session::Error::MapUnplaceable { map, device, .. } => {
                assert_eq!(*map, policy.map);
                assert_eq!(device.count(), 4);
            }
            other => panic!("expected the map named, got {other:?}"),
        },
        Ok(_) => panic!("the map has no output 4 on a four-channel device"),
    }
    assert_eq!(
        session.get(session.policy::<OutputPolicy>()).unwrap().map,
        policy.map,
        "never reset to the identity"
    );
    assert_eq!(session.output_device().unwrap().channels.width().count(), 6);

    // A map the new device has places for, and the restart goes through.
    let mut fits = policy;
    fits.map = OutputMap::channels([2, 3]).unwrap();
    session
        .set(session.policy::<OutputPolicy>().to(fits))
        .unwrap();
    let (driver, _four) = ManualStreamDriver::new();
    session
        .restart_on(spec(4), driver, None, None)
        .expect("outputs 3 and 4 are there");
    assert_eq!(session.output_device().unwrap().channels.width().count(), 4);
}

/// A session asked for 512-frame callbacks on a device whose stream runs at
/// `rate`: the spec says 512.
fn spec_with_quantum(rate: f64, frames: usize) -> OutputSpec {
    spec_at(rate).with_quantum(OutputQuantum::of(frames))
}

/// The restart requests with a callback size in `events`.
fn quantum_requests(events: &[SessionEvent]) -> Vec<OutputQuantum> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Device(DeviceEvent::RestartWanted(r)) => r.quantum,
            _ => None,
        })
        .collect()
}

/// A manual driver whose stream reports `opened` as its callback size, whatever
/// the spec asked for.
struct OpensAt {
    inner: ManualStreamDriver,
    opened: OutputQuantum,
}

impl tutti_engine::StreamDriver for OpensAt {
    type Running = <ManualStreamDriver as tutti_engine::StreamDriver>::Running;
    type Error = std::convert::Infallible;

    fn open(
        self,
        spec: &OutputSpec,
        block: tutti_engine::OutputBlock,
        faults: std::sync::Arc<tutti_engine::StreamFaults>,
    ) -> Result<tutti_engine::OpenedOutput<Self::Running>, Self::Error> {
        let opened = self.inner.open(spec, block, faults)?;
        Ok(tutti_engine::OpenedOutput {
            quantum: Some(self.opened),
            ..opened
        })
    }
}

/// A silent source that keeps the `Prepare` it was last given: what the graph
/// prepares its nodes for.
#[derive(Clone)]
struct Prepared(Arc<Mutex<Option<Prepare>>>);

impl Prepared {
    /// Inserts the node into `session`'s graph, which prepares it.
    fn insert(session: &mut LiveSession) -> Self {
        let seen = Self(Arc::new(Mutex::new(None)));
        session.graph().add(ForkByClone(seen.clone()));
        seen
    }

    /// The largest block the graph last prepared the node for, if it was asked.
    fn quantum(&self) -> Option<MaxBlock> {
        self.0.lock().as_ref().and_then(Prepare::quantum)
    }
}

impl Node for Prepared {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, p: &Prepare) {
        *self.0.lock() = Some(*p);
    }
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A stream that opened at another size than the graph is prepared for is
/// still asked to move.** The device was asked for 512 frames and its stream
/// reports, and delivers, 480: the stream's spec says 480 and the graph is
/// prepared for 512, the size requested. The rule compares what is delivered
/// with what the graph is prepared for, so it asks for a restart onto 480; read
/// against the stream's spec it would find delivered and stated equal and ask
/// for nothing.
#[test]
fn a_stream_that_opened_at_another_size_than_the_graph_is_prepared_for_is_still_asked_to_move() {
    let (inner, stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(
            spec_with_quantum(48_000.0, 512),
            OpensAt {
                inner,
                opened: OutputQuantum::of(480),
            },
        )
        .unwrap();
    let prepared = Prepared::insert(&mut session);
    assert_eq!(
        session.device().spec().quantum,
        Some(OutputQuantum::of(480)),
        "the stream says what it opened at"
    );
    assert_eq!(
        prepared.quantum(),
        Some(MaxBlock::of(Samples::new(512))),
        "the graph is prepared for the request"
    );

    let base = Instant::now();
    let mut asked = Vec::new();
    let mut ms = 0;
    while asked.is_empty() && ms < 100 {
        stream.render_block(Samples::new(480)).expect("open");
        ms += 1;
        asked.extend(quantum_requests(&poll(&mut session, base, ms)));
    }
    assert_eq!(
        asked,
        [OutputQuantum::of(480)],
        "asked to move to the size delivered"
    );

    // The host runs it; the graph and the stream then agree, and callbacks of
    // that size ask for nothing.
    let (driver, new) = ManualStreamDriver::new();
    session
        .restart_on(spec_with_quantum(48_000.0, 480), driver, None, None)
        .unwrap();
    assert_eq!(prepared.quantum(), Some(MaxBlock::of(Samples::new(480))));
    let mut later = poll(&mut session, base, 200);
    for step in 0..50 {
        new.render_block(Samples::new(480)).expect("open");
        later.extend(poll(&mut session, base, 201 + step));
    }
    assert!(quantum_requests(&later).is_empty(), "{later:?}");
}

/// **Callbacks that settle at another size than the one asked for move the
/// graph and the stream to it, once.** The device was opened asking for 512
/// frames and the backend delivers 480: the session asks for a restart onto
/// 480 after the delivered size holds, the host runs it, the graph is
/// prepared for 480 and the stream's spec says 480, and callbacks of 480
/// change nothing more.
#[test]
fn callbacks_that_settle_at_another_size_move_the_graph_and_the_stream_to_it() {
    let (driver, stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_with_quantum(48_000.0, 512), driver)
        .unwrap();
    let prepared = Prepared::insert(&mut session);
    let base = Instant::now();
    let mut asked = Vec::new();
    let mut ms = 0;
    while asked.is_empty() && ms < 100 {
        stream.render_block(Samples::new(480)).expect("open");
        ms += 1;
        asked.extend(quantum_requests(&poll(&mut session, base, ms)));
    }
    assert_eq!(
        asked,
        [OutputQuantum::of(480)],
        "asked for what it settled at"
    );
    assert_eq!(
        prepared.quantum(),
        Some(MaxBlock::of(Samples::new(512))),
        "nothing moved until the host runs the restart"
    );

    // The host runs it, on a device-free driver delivering what the request
    // names.
    let (driver, new) = ManualStreamDriver::new();
    session
        .restart_on(spec_with_quantum(48_000.0, 480), driver, None, None)
        .unwrap();
    assert_eq!(prepared.quantum(), Some(MaxBlock::of(Samples::new(480))));
    assert_eq!(
        session.device().spec().quantum,
        Some(OutputQuantum::of(480))
    );

    let mut later = poll(&mut session, base, 200);
    for step in 0..50 {
        new.render_block(Samples::new(480)).expect("open");
        later.extend(poll(&mut session, base, 201 + step));
    }
    assert!(
        quantum_requests(&later).is_empty(),
        "callbacks of the prepared size change nothing: {later:?}"
    );
    assert!(
        later
            .iter()
            .all(|e| !matches!(e, SessionEvent::Device(DeviceEvent::CallbackSizeVaries))),
        "{later:?}"
    );
}

/// **Callbacks of the size the graph is prepared for never ask for a
/// restart.**
#[test]
fn callbacks_of_the_prepared_size_never_ask_for_a_restart() {
    let (driver, stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_with_quantum(48_000.0, 512), driver)
        .unwrap();
    let base = Instant::now();
    let mut events = Vec::new();
    for ms in 0..100 {
        stream.render_block(Samples::new(512)).expect("open");
        events.extend(poll(&mut session, base, ms));
    }
    assert!(events.is_empty(), "{events:?}");
}

/// **Callbacks that keep changing size are reported once and restart
/// nothing.**
#[test]
fn callbacks_that_never_settle_are_reported_varying_once() {
    let (driver, stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_with_quantum(48_000.0, 512), driver)
        .unwrap();
    let base = Instant::now();
    let mut events = Vec::new();
    for ms in 0..200u64 {
        let frames = if ms % 2 == 0 { 400 } else { 480 };
        stream.render_block(Samples::new(frames)).expect("open");
        events.extend(poll(&mut session, base, ms));
    }
    let varying = events
        .iter()
        .filter(|e| matches!(e, SessionEvent::Device(DeviceEvent::CallbackSizeVaries)))
        .count();
    assert_eq!(varying, 1, "{events:?}");
    assert!(quantum_requests(&events).is_empty(), "{events:?}");
}

/// **A device that keeps delivering another size after each restart is asked
/// twice, then reported varying once, then left alone.** The host restarts
/// onto each asked size and the device then delivers a third size: two
/// requests in total, `CallbackSizeVaries` once, then silence.
#[test]
fn a_device_that_keeps_changing_size_after_restarts_is_asked_twice_then_reported() {
    let (driver, mut stream) = ManualStreamDriver::new();
    let mut session = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(spec_with_quantum(48_000.0, 512), driver)
        .unwrap();
    let base = Instant::now();
    let mut ms = 0;
    let mut asked = Vec::new();
    let mut varying = 0;
    // The device delivers 480, then 441 once restarted onto 480, then 400
    // once restarted onto 441, for as long as it is polled.
    for (delivered, asked_size) in [(480, Some(480)), (441, Some(441)), (400, None)] {
        for _ in 0..40 {
            stream.render_block(Samples::new(delivered)).expect("open");
            ms += 1;
            let events = poll(&mut session, base, ms);
            asked.extend(quantum_requests(&events));
            varying += events
                .iter()
                .filter(|e| matches!(e, SessionEvent::Device(DeviceEvent::CallbackSizeVaries)))
                .count();
        }
        if let Some(size) = asked_size {
            let (driver, next) = ManualStreamDriver::new();
            session
                .restart_on(spec_with_quantum(48_000.0, size), driver, None, None)
                .unwrap();
            stream = next;
        }
    }
    assert_eq!(asked, [OutputQuantum::of(480), OutputQuantum::of(441)]);
    assert_eq!(varying, 1, "reported once, and no third request");
}
