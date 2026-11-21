//! A session on the system clock: an OS timer thread renders the graph, with
//! no device and no host pulling blocks.

use std::time::Duration;

use tutti_buffers::InterleavedMut;
use tutti_platform::Instant;

use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_engine::StreamFaultKind;
use tutti_graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_nodes::testing::Const;
use tutti_session::{OutPort, Root, Session, SessionEvent, SystemSession};
use tutti_units::{SampleRate, Samples};

const BLOCK: usize = 480;

fn system() -> SystemSession {
    Session::builder()
        .sample_rate(SampleRate::SR_48K)
        .max_block(MaxBlock::of(Samples::new(BLOCK)))
        .system_clock()
        .build()
        .expect("a timer thread starts")
}

/// **A system session renders its graph with no device and no host.** A
/// constant wired to both outputs reaches the heard tap, in whole stereo
/// frames, and the playhead moves in whole timer blocks: the timer thread is
/// the only thing that called the engine.
#[test]
fn a_system_session_renders_its_graph_with_no_device() {
    let mut system = system();
    let s = &mut system;
    let level = s.graph().add(ForkByClone(Const::mono(0.25)));
    for port in 0..2 {
        s.graph()
            .set_sources(Root::output(port), [OutPort::new(level, 0)])
            .unwrap()
            .unfaded();
    }
    let limit = Instant::now() + Duration::from_secs(10);
    let mut tap = loop {
        let _ = s.update();
        if let Some(tap) = s.open_output_tap(Samples::new(4 * BLOCK)).unwrap().reader() {
            break tap;
        }
        assert!(
            Instant::now() < limit,
            "the audio side never took the graph"
        );
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(tap.width(), 2);

    s.play().expect("room");
    let mut heard = vec![0.0f32; 2 * BLOCK];
    loop {
        let _ = s.update();
        let frames = tap
            .pop_into(InterleavedMut::new(&mut heard, tap.layout()).unwrap())
            .unwrap()
            .get();
        if frames > 0 && heard[..2 * frames].iter().all(|&x| x == 0.25) {
            break;
        }
        assert!(Instant::now() < limit, "the constant never reached the tap");
        std::thread::sleep(Duration::from_millis(2));
    }
    let head = s.playhead();
    assert!(head.playing(), "the transport rolls on the timer's blocks");
    assert_eq!(head.frame().get() % BLOCK as u64, 0);
}

/// **A timer that has fallen behind by nothing has recorded no fault**: the
/// fault count of a fresh system session is zero.
#[test]
fn a_fresh_system_session_has_no_faults() {
    let system = system();
    assert_eq!(system.faults().count(), 0);
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

/// **A fault of the timer's stream reaches the app as a session event, as a
/// device's does**: a node that panics on the timer thread is contained, and
/// the updates report `SessionEvent::Fault` of kind `CallbackPanicked`
/// exactly once. Counted by kind: the timer may record `Overrun` faults of its
/// own on a loaded machine, which are other events.
#[test]
fn a_panic_on_the_timer_thread_is_reported_once_as_a_fault_event() {
    let mut system = system();
    let node = system.graph().add(ForkByClone(AlwaysPanics));
    system
        .graph()
        .set_sources(Root::output(0), [OutPort::new(node, 0)])
        .unwrap()
        .unfaded();
    let panics = |events: &[SessionEvent]| {
        events
            .iter()
            .filter(|e| {
                matches!(e, SessionEvent::Fault(f) if f.kind == StreamFaultKind::CallbackPanicked)
            })
            .count()
    };
    let limit = Instant::now() + Duration::from_secs(10);
    let mut reported = 0;
    while reported == 0 {
        reported += panics(&system.update());
        assert!(Instant::now() < limit, "the panic was never reported");
        std::thread::sleep(Duration::from_millis(2));
    }
    for _ in 0..20 {
        reported += panics(&system.update());
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(reported, 1, "the panic is one event");
    assert!(system.faults().count() >= 1);
}

/// **A fault that only bumps the count is not the last fault again**: after a
/// panic was reported, an overrun is reported as an `Overrun` and the panic is
/// not reported a second time. (The record keeps no message for an overrun
/// beside a latched panic; reading the record's last message on every count
/// bump re-reported the panic.)
#[test]
fn an_overrun_after_a_reported_panic_is_its_own_event_and_not_the_panic_again() {
    let mut system = system();
    let faults = system.faults();
    let kinds = |events: &[SessionEvent]| -> Vec<StreamFaultKind> {
        events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Fault(f) => Some(f.kind),
                _ => None,
            })
            .collect()
    };
    faults.record_panic("a callback panicked");
    let first = kinds(&system.update());
    assert_eq!(
        first
            .iter()
            .filter(|k| **k == StreamFaultKind::CallbackPanicked)
            .count(),
        1,
        "{first:?}"
    );
    faults.record_overrun("the timer fell behind");
    let second = kinds(&system.update());
    assert!(
        !second.contains(&StreamFaultKind::CallbackPanicked),
        "the panic is not reported again: {second:?}"
    );
    assert!(second.contains(&StreamFaultKind::Overrun), "{second:?}");
}

/// **A fresh session on any real-time clock reports no fault event**: the
/// first update after the build has nothing to say of the stream.
#[test]
fn a_fresh_system_session_reports_no_fault_event() {
    let mut system = system();
    let events = system.update();
    assert!(
        events.iter().all(|e| !matches!(e, SessionEvent::Fault(_))),
        "{events:?}"
    );
}
