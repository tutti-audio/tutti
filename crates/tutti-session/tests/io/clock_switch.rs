//! A session changes clock: the system clock to a sound card and back, device-free
//! (the card is a `ManualStreamDriver`, the timer is real). What a switch
//! keeps, what it refuses before anything stops, and what it hands back.

use std::time::{Duration, Instant};

use tutti_cpal::{DeviceId, DeviceSelector};
use tutti_engine::{ManualStreamDriver, OutputSpec};
use tutti_graph::ForkByClone;
use tutti_nodes::testing::{Const, Through};
use tutti_session::{
    Error, InPort, LiveSession, OutPort, Root, Session, SessionEvent, SystemSession,
};
use tutti_transport::MotionEvent;
use tutti_types::{ChannelLayout, ChannelTopology, MaxBlock, SampleRate, Samples};

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
        .max_block(MaxBlock::of(Samples(BLOCK)))
        .system_clock()
}

fn system() -> SystemSession {
    system_builder().build().expect("a timer thread starts")
}

/// Wires a constant to both outputs and commits it.
fn wire_constant<C: tutti_session::Clock>(s: &mut Session<C>, level: f32) {
    let (node, _) = s.graph().add(ForkByClone(Const::mono(level)));
    for port in 0..2 {
        s.graph()
            .set_sources_hard(Root::output(port), [OutPort::new(node, 0)])
            .unwrap();
    }
    let _ = s.update();
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let limit = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < limit, "{what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn play<C: tutti_session::Clock>(s: &Session<C>) {
    s.transport()
        .motion()
        .try_send(MotionEvent::Play)
        .expect("room");
}

fn frame<C: tutti_session::Clock>(s: &Session<C>) -> u64 {
    s.playhead().frame().get()
}

/// Waits for the timer to have rendered past `than`, updating as a host does.
fn wait_past(s: &mut SystemSession, than: u64) -> u64 {
    wait_until("the timer never rendered another block", || {
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
    stream.render_block(64).expect("open");
    let block = stream.render_block(64).expect("open");
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
        .device(Default::default())
        .build_device_on(stereo(SampleRate::SR_48K), driver)
        .expect("builds with no device");
    wire_constant(&mut live, 0.25);
    stream.render_block(64).expect("open");
    let mut s = live
        .into_system_clock()
        .unwrap_or_else(|e| panic!("switches: {}", e.error()));
    assert!(
        stream.render_block(64).is_none(),
        "the card's stream stopped"
    );
    play(&s);
    let limit = Instant::now() + Duration::from_secs(10);
    let mut tap = loop {
        let _ = s.update();
        if let Some(tap) = s.open_heard_tap(Samples(4 * BLOCK)).unwrap().reader() {
            break tap;
        }
        assert!(
            Instant::now() < limit,
            "the audio side never took the graph"
        );
        std::thread::sleep(Duration::from_millis(2));
    };
    let mut heard = vec![0.0f32; 2 * BLOCK];
    wait_until("the constant never reached the tap", || {
        let _ = s.update();
        let frames = tap.pop_into(&mut heard).0;
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

/// **A root the new device cannot place is refused before the timer stops**:
/// six unassigned buses on a stereo card. The error is named, the card's stream
/// never opened, and the timer renders on.
#[test]
fn a_root_the_device_cannot_place_is_refused_before_the_timer_stops() {
    let mut s = system_builder()
        .outputs(ChannelLayout::from(6u16))
        .root_topology(ChannelTopology::unassigned(ChannelLayout::from(6u16)))
        .build()
        .expect("builds");
    play(&s);
    let before = wait_past(&mut s, 0);
    let (driver, stream) = ManualStreamDriver::new();
    let err = s
        .into_device_on(stereo(SampleRate::SR_48K), driver)
        .expect_err("a six-wide unassigned root has no place on two channels");
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

/// **A graph that cannot be re-prepared fails the switch after the stop and
/// the session comes back stopped**: the one failure that follows the stop. The
/// card's stream is not open, the timer does not render, and the session is
/// still usable.
#[test]
fn a_graph_that_no_longer_compiles_comes_back_stopped() {
    let mut s = system();
    play(&s);
    let (osc, _) = s.graph().add(ForkByClone(Const::mono(0.1)));
    let (lim, _) = s.graph().add(ForkByClone(Through::mono()));
    s.graph()
        .set_sources(InPort::new(lim, 0), [OutPort::new(osc, 0)])
        .unwrap();
    s.graph()
        .set_sources(Root::output(0), [OutPort::new(lim, 0)])
        .unwrap();
    let _ = s.update();
    wait_past(&mut s, 0);
    // An edit not yet committed feeds the node from a node it feeds.
    let (loop_back, _) = s.graph().add(ForkByClone(Through::mono()));
    s.graph()
        .set_sources(InPort::new(loop_back, 0), [OutPort::new(lim, 0)])
        .unwrap();
    s.graph()
        .set_sources(InPort::new(lim, 0), [OutPort::new(loop_back, 0)])
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
    let s = err.into_session();
    let stopped_at = frame(&s);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(frame(&s), stopped_at, "the timer is stopped");
    assert_eq!(s.faults().count(), 0, "and the session still answers");
}

/// **A rolling transport resumes on a whole block**: after the card's blocks,
/// the timer's first block starts where the card's last ended, so the frames
/// the timer has rendered since the switch are a whole number of its blocks.
#[test]
fn the_transport_resumes_where_the_last_block_ended_going_to_the_timer() {
    let (driver, stream) = ManualStreamDriver::new();
    let mut live = Session::builder()
        .device(Default::default())
        .build_device_on(stereo(SampleRate::SR_48K), driver)
        .expect("builds with no device");
    let _ = live.update();
    play(&live);
    for _ in 0..5 {
        stream.render_block(BLOCK).expect("open");
    }
    let timer_block = tutti_session::LIVE_MAX_BLOCK.samples().0 as u64;
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
    stream.render_block(64).expect("open");
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
    assert_eq!(live.transport().sample_rate(), SampleRate::SR_44K1);
    assert_eq!(live.device().graph_rate(), SampleRate::SR_44K1);
}

/// **The output policy carries over a clock switch, and a device the map does
/// not fit is refused before the timer stops**: as a device restart refuses
/// it, with the map named and kept (never reset to the identity). Once the
/// host sets a map the card has places for, the switch goes through and the
/// policy is in force on the card.
#[test]
fn the_output_policy_carries_over_and_a_card_the_map_does_not_fit_is_refused() {
    use tutti_engine::OutputMap;
    use tutti_session::OutputPolicy;
    use tutti_types::Bed;
    let mut s = system_builder()
        .outputs(ChannelLayout::from(6u16))
        .build()
        .expect("builds");
    let policy = OutputPolicy {
        monitor: Some(Bed::Stereo.topology()),
        map: OutputMap::channels([4, 5]).unwrap(),
        ..OutputPolicy::default()
    };
    s.policy::<OutputPolicy>().set(policy.clone()).unwrap();
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
        s.policy::<OutputPolicy>().get().map,
        policy.map,
        "never reset to the identity"
    );
    wait_past(&mut s, before);

    let mut fits = policy;
    fits.map = OutputMap::channels([2, 3]).unwrap();
    s.policy::<OutputPolicy>().set(fits.clone()).unwrap();
    let (driver, _card) = ManualStreamDriver::new();
    let mut live = s
        .into_device_on(four, driver)
        .unwrap_or_else(|e| panic!("outputs 3 and 4 are there: {}", e.error()));
    assert_eq!(live.policy::<OutputPolicy>().get().map, fits.map);
    assert_eq!(live.output_device().unwrap().topology.width().count(), 4);
}
