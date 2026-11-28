//! An event tap opened on a session's graph: the MIDI events a node's port
//! carries, each with its frame, copied out for a thread of the caller's.
//!
//! The graph's own suite (`tutti-graph/tests/event_tap.rs`) holds the op: frames
//! across block sizes, alignment, loss, a missing port, the parallel render.
//! This is the session's half: the open marks the graph edited, a close stops
//! the copy, a reader dropped first is closed by the `Reclaim` phase, and the
//! taps are opened again at the graph's new rate.

use crate::committed::committed;
use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_graph::{
    Cx, Event, EventCapacity, EventTapOptions, Io, MissingSource, Node, Prepare, Shape, Status,
    TapAlign, TapEvent, Unforkable,
};
use tutti_session::{CloseError, Committed, EventInputPort, EventOutputPort, Session};
use tutti_ump::Ump;
use tutti_units::{Frame, SampleRate};

/// A note-on on every frame that is a multiple of 4, its own frame in word 0.
struct Every4;

impl Every4 {
    const SHAPE: Shape = Shape::EMPTY.events(0, 1);
    const OUT: tutti_graph::OutEvents = Self::SHAPE
        .event_output(0)
        .expect("one event output is declared");
}

impl Node for Every4 {
    fn shape(&self) -> Shape {
        Self::SHAPE
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        for at in cx.env.offsets() {
            let f = cx.env.frame_at(at).get();
            if f.is_multiple_of(4) {
                let _ = io
                    .event_out(Self::OUT)
                    .push(Event::midi(at, Ump([f as u32, 0, 0, 0])));
            }
        }
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn session() -> (Session, EventOutputPort) {
    let mut s = Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .outputs(ChannelLayout::MONO)
        .sample_rate(SampleRate::SR_48K)
        .block_dates(tutti_session::BlockDates::Frames)
        .build()
        .expect("builds");
    let key = s.graph().add(Unforkable(Every4));
    let _ = s.update();
    (s, key.event_output(0))
}

fn frames(n: usize, s: &mut Session) {
    let mut out = [0.0f32];
    for _ in 0..n {
        s.render(&mut InterleavedMut::one_frame(&mut out).unwrap());
    }
}

/// Blocks and updates until the graph has resumed from a re-prepare: its second
/// half lands once a block has run and an update has collected.
fn resumed(s: &mut Session) {
    for _ in 0..16 {
        frames(1, s);
        let _ = s.update();
        if s.commit() != Committed::Retry {
            return;
        }
    }
    panic!("the graph never resumed");
}

fn drain(tap: &mut tutti_graph::EventTapReader) -> Vec<u64> {
    let mut buf = [TapEvent {
        frame: Frame::new(0),
        ump: Ump::default(),
    }; 64];
    let n = tap.pop_into(&mut buf);
    buf[..n]
        .iter()
        .map(|e| {
            assert_eq!(
                u64::from(e.ump.0[0]),
                e.frame.get(),
                "the packet is its own frame"
            );
            e.frame.get()
        })
        .collect()
}

/// The whole point of the seam: a host gets the reader from the graph and it
/// sees the events rendered since the open, each on its frame, at the graph's
/// rate.
#[test]
fn opening_a_tap_yields_the_events_rendered_since_the_open() {
    let (mut s, source) = session();
    frames(1, &mut s);
    let mut tap = s
        .graph()
        .taps()
        .open_event(
            source,
            EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
        )
        .expect("the port exists");
    assert_eq!(tap.rate(), SampleRate::SR_48K);
    let _ = s.update();
    frames(15, &mut s);
    assert_eq!(drain(&mut tap), vec![4, 8, 12]);
    assert_eq!(tap.take_lost(), 0);
}

/// A port the graph does not have is refused, and nothing is opened.
#[test]
fn opening_refuses_a_port_the_graph_does_not_have() {
    let (mut s, source) = session();
    assert_eq!(
        s.graph()
            .taps()
            .open_event(
                EventOutputPort::new(source.node(), 3),
                EventTapOptions::new(EventCapacity::of(8)).align(TapAlign::Raw)
            )
            .err(),
        Some(MissingSource::EventPort {
            node: source.node(),
            port: 3,
            width: 1
        })
    );
}

/// Closing stops the copy: only what rendered while the tap was open is read.
#[test]
fn closing_the_tap_stops_the_copy() {
    let (mut s, source) = session();
    let mut tap = s
        .graph()
        .taps()
        .open_event(
            source,
            EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
        )
        .expect("the port exists");
    let _ = s.update();
    frames(6, &mut s);
    s.graph().taps().close_event(tap.id()).unwrap();
    assert_eq!(
        s.graph().taps().close_event(tap.id()),
        Err(CloseError::EventTap(tap.id())),
        "a closed event tap is not open"
    );
    let _ = s.update();
    frames(8, &mut s);
    assert_eq!(drain(&mut tap), vec![0, 4], "only what rendered while open");
}

/// A tap whose reader was dropped is closed by the next `Reclaim` phase and
/// leaves the plan at the commit after it.
#[test]
fn a_dropped_reader_leaves_the_plan_at_the_reclaim_phase() {
    let (mut s, source) = session();
    let tap = s
        .graph()
        .taps()
        .open_event(
            source,
            EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
        )
        .expect("the port exists");
    assert_eq!(committed(&s.update()), 1, "the tap's plan");
    drop(tap);
    assert_eq!(
        committed(&s.update()),
        0,
        "the reclaim of this update closed it; the plan is the next commit's"
    );
    assert_eq!(committed(&s.update()), 1, "the tap's op left the plan");
}

/// After the graph is re-prepared at another rate the taps are opened again:
/// each new reader replaces the tap of its id, is on the new rate, and the
/// reader it supersedes is fed no more.
#[test]
fn reopening_replaces_the_tap_at_the_new_rate() {
    let (mut s, source) = session();
    let mut old = s
        .graph()
        .taps()
        .open_event(
            source,
            EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
        )
        .expect("the port exists");
    let _ = s.update();
    frames(5, &mut s);
    assert_eq!(drain(&mut old), vec![0, 4]);
    s.set_sample_rate(SampleRate::SR_44K1).unwrap();
    resumed(&mut s);
    // What the old reader heard while the graph re-prepared is not the point.
    let _ = drain(&mut old);
    let mut new = s
        .graph()
        .taps()
        .reopen_event(old.id())
        .expect("the tap is open");
    let _ = s.update();
    assert_eq!(new.id(), old.id());
    assert_eq!(old.rate(), SampleRate::SR_48K);
    assert_eq!(new.rate(), SampleRate::SR_44K1);
    frames(16, &mut s);
    assert_eq!(
        drain(&mut old),
        Vec::<u64>::new(),
        "the superseded reader is fed no more"
    );
    assert!(!drain(&mut new).is_empty(), "the new reader is");
}

/// A node with one event input that does nothing with it.
struct Hears;

impl Node for Hears {
    fn shape(&self) -> Shape {
        Shape::EMPTY.events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

/// A tap opened on an event input (`key.event_input(0)`) copies what the node
/// hears there: the events its source wrote, on their frames, and the tap
/// survives a re-rate through `reopen_event`. A port past the node's inputs
/// is refused as an input.
#[test]
fn a_tap_of_an_event_input_hears_what_the_node_hears() {
    let (mut s, source) = session();
    let hears = s.graph().add(Unforkable(Hears));
    s.graph()
        .connect(source, hears.event_input(0))
        .expect("both ports exist");
    assert_eq!(
        s.graph()
            .taps()
            .open_event(
                EventInputPort::new(hears.erase(), 1),
                EventTapOptions::new(EventCapacity::of(8)).align(TapAlign::Raw)
            )
            .err(),
        Some(MissingSource::EventInputPort {
            node: hears.erase(),
            port: 1,
            width: 1
        })
    );
    let mut tap = s
        .graph()
        .taps()
        .open_event(
            hears.event_input(0),
            EventTapOptions::new(EventCapacity::of(64)).align(TapAlign::Raw),
        )
        .expect("the input exists");
    let _ = s.update();
    frames(16, &mut s);
    assert_eq!(drain(&mut tap), vec![0, 4, 8, 12]);
    s.set_sample_rate(SampleRate::SR_44K1).unwrap();
    resumed(&mut s);
    let mut new = s
        .graph()
        .taps()
        .reopen_event(tap.id())
        .expect("the tap is open");
    let _ = s.update();
    assert_eq!(new.rate(), SampleRate::SR_44K1);
    frames(8, &mut s);
    assert!(
        !drain(&mut new).is_empty(),
        "the reopened tap still hears it"
    );
}
