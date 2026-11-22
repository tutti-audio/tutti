//! Event ports on typed keys (doc 013, "The session graph's surface"; doc 030
//! step 2c): `key.event_input(0)` and `key.event_output(0)` are the main (index 0)
//! event ports, `event_input(i)` and `event_output(i)` take an index, and
//! the one `connect`, `disconnect`, `sources`, and `set_sources`
//! cover an event edge as they do an audio one.
//!
//! The figures are the graph's own: which edges it holds.

use tutti_buffers::ChannelLayout;
use tutti_graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_session::{ConnectError, DetachedSession, EventInputPort, EventOutputPort, Session};
use tutti_units::SampleRate;

/// A node with `IN` event inputs and `OUT` event outputs and nothing else.
#[derive(Clone)]
struct Events<const IN: u16, const OUT: u16>;

impl<const IN: u16, const OUT: u16> Node for Events<IN, OUT> {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_events(IN, OUT)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn detached() -> DetachedSession {
    Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds")
}

/// **An event edge is connected, read back and disconnected by the same calls
/// as an audio one**: `connect` adds it (once, however often it is asked
/// for), `sources` lists what feeds the port in the graph's order, and
/// `disconnect` says whether it was there.
#[test]
fn an_event_edge_is_connected_read_back_and_disconnected() {
    let mut s = detached();
    let g = s.graph();
    let src = g.add(ForkByClone(Events::<0, 2>));
    let sink = g.add(ForkByClone(Events::<1, 0>));

    let connection = g.connect(src.event_output(0), sink.event_input(0)).unwrap();
    assert!(
        connection.edges().is_empty(),
        "an event edge is no audio edge"
    );
    g.connect(src.event_output(0), sink.event_input(0)).unwrap();
    assert_eq!(
        g.sources(sink.event_input(0)).unwrap(),
        [src.event_output(0)],
        "once"
    );

    g.connect(src.event_output(1), sink.event_input(0)).unwrap();
    assert_eq!(
        g.sources(sink.event_input(0)).unwrap(),
        [src.event_output(0), src.event_output(1)],
        "events merge: a second source joins the first"
    );

    assert!(
        g.disconnect(src.event_output(0), sink.event_input(0))
            .unwrap()
            .was_connected()
    );
    assert!(
        !g.disconnect(src.event_output(0), sink.event_input(0))
            .unwrap()
            .was_connected(),
        "gone"
    );
    assert_eq!(
        g.sources(sink.event_input(0)).unwrap(),
        [src.event_output(1)]
    );
}

/// **Port 0 is the main event port, and an index names another**: an edge
/// onto input 1 is no source of input 0.
#[test]
fn event_port_zero_is_the_main_port_and_an_index_names_another() {
    let mut s = detached();
    let g = s.graph();
    let src = g.add(ForkByClone(Events::<0, 1>));
    let sink = g.add(ForkByClone(Events::<2, 0>));
    assert_eq!(sink.event_input(0), sink.event_input(0));
    assert_eq!(src.event_output(0), src.event_output(0));
    assert_eq!(sink.event_input(0).port, 0);

    g.connect(src.event_output(0), sink.event_input(1)).unwrap();
    assert!(
        g.sources(sink.event_input(0)).unwrap().is_empty(),
        "the main input is unfed"
    );
    assert_eq!(
        g.sources(sink.event_input(1)).unwrap(),
        [src.event_output(0)]
    );
}

/// **A port the node does not have, a node that is not there, and a node
/// feeding itself are refused with the port named, and nothing is stored.**
#[test]
fn a_missing_event_port_is_refused_and_stores_nothing() {
    let mut s = detached();
    let g = s.graph();
    let src = g.add(ForkByClone(Events::<0, 1>));
    let sink = g.add(ForkByClone(Events::<1, 0>));
    let both = g.add(ForkByClone(Events::<1, 1>));
    let gone = g.add(ForkByClone(Events::<1, 1>));
    s.remove(gone).unwrap();
    let _ = s.update();
    let g = s.graph();

    assert_eq!(
        g.connect(src.event_output(0), sink.event_input(1)).err(),
        Some(ConnectError::NoSuchEventIn(sink.event_input(1)))
    );
    assert_eq!(
        g.connect(src.event_output(1), sink.event_input(0)).err(),
        Some(ConnectError::NoSuchEventOut(src.event_output(1)))
    );
    // A sink with no event output has no output 0 either.
    assert_eq!(
        g.connect(sink.event_output(0), src.event_input(0)).err(),
        Some(ConnectError::NoSuchEventOut(sink.event_output(0)))
    );
    assert_eq!(
        g.connect(both.event_output(0), both.event_input(0)).err(),
        Some(ConnectError::Itself(both.erase()))
    );
    assert_eq!(
        g.connect(src.event_output(0), gone.event_input(0)).err(),
        Some(ConnectError::NoSuchNode(gone.erase()))
    );
    assert!(
        !s.graph_ref().has_uncommitted_edits(),
        "a refusal stores nothing"
    );
}

/// **`set_sources` sets exactly what feeds an event input, and
/// an empty set takes it all**: a source listed is kept, one not listed
/// goes, an empty set clears.
#[test]
fn set_sources_covers_event_inputs() {
    let mut s = detached();
    let g = s.graph();
    let a = g.add(ForkByClone(Events::<0, 1>));
    let b = g.add(ForkByClone(Events::<0, 1>));
    let sink = g.add(ForkByClone(Events::<1, 0>));
    let input = sink.event_input(0);

    g.set_sources(input, [a.event_output(0), b.event_output(0)])
        .unwrap();
    let mut held = g.sources(input).unwrap();
    held.sort();
    let mut both = [a.event_output(0), b.event_output(0)];
    both.sort();
    assert_eq!(held, both);
    g.set_sources(input, [b.event_output(0)]).unwrap();
    assert_eq!(g.sources(input).unwrap(), [b.event_output(0)]);

    g.set_sources(input, []).unwrap();
    assert!(g.sources(input).unwrap().is_empty());

    // The refusal names the port and changes nothing.
    assert_eq!(
        g.set_sources(input, [EventOutputPort::new(a, 5)]).err(),
        Some(ConnectError::NoSuchEventOut(EventOutputPort::new(a, 5)))
    );
    assert_eq!(
        g.set_sources(EventInputPort::new(sink, 3), [a.event_output(0)])
            .err(),
        Some(ConnectError::NoSuchEventIn(EventInputPort::new(sink, 3)))
    );
    assert!(g.sources(input).unwrap().is_empty());
}
