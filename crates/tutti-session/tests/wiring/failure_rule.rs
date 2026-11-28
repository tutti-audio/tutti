//! The one failure rule (doc 013, "The session graph's surface"): a call that
//! names a key or port that does not exist fails at the call with a named
//! error, never later at the commit and never as silence.

use tutti_buffers::ChannelLayout;
use tutti_graph::{Cx, ForkByClone, Io, Node, NodeKey, Prepare, SampleRates, Shape, Status};
use tutti_nodes::testing::{Const, Through};
use tutti_session::{
    Committed, ConnectError, DetachedSession, EventInputPort, EventOutputPort, Feeder, InputPort,
    MainOutput, OutputPort, ReplaceRefused, Replacement, Session, SetRateError,
};
use tutti_units::SampleRate;

const RATE: SampleRate = SampleRate::SR_48K;
const GHOST: NodeKey = NodeKey(u64::MAX);

fn detached() -> DetachedSession {
    Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(RATE)
        .expect("builds")
}

/// **Every query names the node that is not there.** A key no node has is an
/// error carrying that key, where a count of 0, a latency of 0, `Active` or
/// `Mix::WET` would have read as an answer.
#[test]
fn a_query_of_a_node_that_is_not_there_names_it() {
    let s = detached();
    let g = s.graph_ref();
    let named = |e: tutti_session::NoSuchNode| e.node();
    assert_eq!(named(g.node_inputs(GHOST).unwrap_err()), GHOST);
    assert_eq!(named(g.node_outputs(GHOST).unwrap_err()), GHOST);
    assert_eq!(named(g.node_event_inputs(GHOST).unwrap_err()), GHOST);
    assert_eq!(named(g.node_latency(GHOST).unwrap_err()), GHOST);
    assert_eq!(named(g.node_tail(GHOST).unwrap_err()), GHOST);
    assert_eq!(named(g.node_buses(GHOST).unwrap_err()), GHOST);
}

/// **A node that is there answers.** The same queries on a real node are `Ok`,
/// so the refusal above is the missing key and nothing else.
#[test]
fn a_query_of_a_node_that_is_there_answers() {
    let mut s = detached();
    let through = s
        .graph()
        .add(ForkByClone(Through::new(ChannelLayout::STEREO)));
    let g = s.graph_ref();
    assert_eq!(g.node_inputs(through).unwrap(), 2);
    assert_eq!(g.node_outputs(through).unwrap(), 2);
    assert_eq!(g.node_event_inputs(through).unwrap(), 0);
    assert!(g.node_buses(through).is_ok());
}

/// **A wire taken out names a node that is not there, and says when an edge
/// is merely absent.** `Ok(false)` is "no such edge", and an error is "no such
/// node or port": a typo in a key is not a disconnect that found nothing.
#[test]
fn disconnect_and_sources_refuse_a_node_that_is_not_there() {
    let mut s = detached();
    let a = s.graph().add(ForkByClone(Const::frame(&[0.5])));
    let g = s.graph();

    let ghost_out = OutputPort::new(GHOST, 0);
    let ghost_in = InputPort::new(GHOST, 0);
    assert_eq!(
        g.disconnect(ghost_out, MainOutput::channel(0)).err(),
        Some(ConnectError::NoSuchNode(GHOST))
    );
    assert_eq!(
        g.disconnect(a.output(0), ghost_in).err(),
        Some(ConnectError::NoSuchNode(GHOST))
    );
    assert_eq!(
        g.set_sources(ghost_in, []).err(),
        Some(ConnectError::NoSuchNode(GHOST))
    );
    assert_eq!(g.sources(ghost_in), Err(ConnectError::NoSuchNode(GHOST)));
    assert_eq!(
        g.disconnect(a.event_output(0), EventInputPort::new(GHOST, 0))
            .err(),
        Some(ConnectError::NoSuchEventOut(a.event_output(0))),
        "the source side is checked first: a constant has no event output"
    );
    assert_eq!(
        g.sources(EventInputPort::new(GHOST, 0)),
        Err(ConnectError::NoSuchNode(GHOST))
    );
    assert_eq!(
        g.disconnect(
            EventOutputPort::new(GHOST, 0),
            EventInputPort::new(a.erase(), 0)
        )
        .err(),
        Some(ConnectError::NoSuchNode(GHOST))
    );
    assert_eq!(
        g.bus_sources(tutti_session::BusInputPort::new(GHOST, 0)),
        Err(ConnectError::NoSuchNode(GHOST))
    );

    // Real ends with no edge between them: absent, not an error.
    assert!(
        !g.disconnect(a.output(0), MainOutput::channel(0))
            .unwrap()
            .was_connected()
    );
    assert!(g.set_sources(MainOutput::channel(0), []).is_ok());
    assert_eq!(g.sources(MainOutput::channel(0)), Ok(Vec::new()));
    // A port a real node does not have is a named error too.
    assert_eq!(
        g.sources(InputPort::new(a.erase(), 9)),
        Err(ConnectError::NoSuchInPort(InputPort::new(a.erase(), 9)))
    );
}

/// **A replace of a node that is not there is refused at the call even while
/// the graph re-prepares.** Asked `Busy` first, it would have been parked to
/// fail on a later update; it is `NoSuchNode` now and nothing waits.
#[test]
fn a_replace_of_a_missing_node_is_refused_during_a_re_prepare() {
    let mut s = detached();
    let _side = s.graph().take_audio_side();
    let knob = s.graph().add(ForkByClone(Const::frame(&[1.0])));
    let _ = s.update();
    s.set_sample_rate(SampleRate::SR_44K1).unwrap();
    assert_eq!(s.commit(), Committed::Retry, "mid re-prepare");

    let refused = s
        .graph()
        .replace(GHOST, Replacement::new(ForkByClone(Const::frame(&[2.0]))))
        .expect_err("a node that is not there");
    assert_eq!(refused, ReplaceRefused::NoSuchNode(GHOST));
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 0);

    // The node that is there waits, as before.
    s.graph()
        .replace(knob, Replacement::new(ForkByClone(Const::frame(&[2.0]))))
        .expect("the node is there");
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 1);
}

/// **A tap or meter source whose port index no `u16` holds is named, not
/// dropped.** Dropped, `open` would have opened a tap with fewer lanes than
/// the caller asked for.
#[test]
fn a_tap_source_past_what_a_port_index_holds_is_refused() {
    use tutti_graph::{MissingSource, TapAlign, TapOptions};
    use tutti_units::Samples;
    let mut s = detached();
    let a = s.graph().add(ForkByClone(Const::frame(&[0.5])));
    let far = OutputPort::new(a.erase(), 70_000);
    let refused = s
        .graph()
        .taps()
        .open_ports(
            &[a.output(0), far],
            TapOptions::new(Samples::new(64)).align(TapAlign::Raw),
        )
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(refused, MissingSource::Port { node, .. } if node == a.erase()));
    let refused = s
        .graph()
        .meters()
        .open(&[far], TapAlign::Raw)
        .map(|_| ())
        .unwrap_err();
    assert!(matches!(refused, MissingSource::Port { .. }));
}

/// A source of silence that runs at `rates` only.
#[derive(Clone)]
struct Rated(SampleRates);

impl Node for Rated {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).with_rates(self.0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// A node that takes events and one that makes them.
#[derive(Clone)]
struct Events {
    inputs: u16,
    outputs: u16,
}

impl Node for Events {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).events(self.inputs, self.outputs)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **A feed into a node that is not there is refused, naming it, and the feeds
/// stand.** Accepted, the call would have kept an entry for a node that never
/// reads it. A node removed is as missing as one never added.
#[test]
fn set_feed_refuses_a_sink_that_is_not_there_and_changes_nothing() {
    let mut s = detached();
    let g = s.graph();
    let sink = g.add(ForkByClone(Events {
        inputs: 1,
        outputs: 0,
    }));
    let gone = g.add(ForkByClone(Events {
        inputs: 1,
        outputs: 0,
    }));
    let source = g.add(ForkByClone(Events {
        inputs: 0,
        outputs: 1,
    }));
    let fed = vec![source.event_output(0)];
    g.set_feed(sink, Feeder::Routes, fed.clone()).unwrap();
    g.set_feed(gone, Feeder::Routes, fed.clone()).unwrap();
    s.remove(gone).unwrap();
    let g = s.graph();
    let edges = g.sources(sink.event_input(0)).unwrap();

    for missing in [GHOST, gone.erase()] {
        let refused = g.set_feed(missing, Feeder::Routes, fed.clone());
        assert_eq!(refused.unwrap_err().node(), missing);
        let refused = g.set_feed(missing, Feeder::Routes, Vec::new());
        assert_eq!(refused.unwrap_err().node(), missing, "an empty feed too");
    }
    assert_eq!(
        g.feed(sink, Feeder::Routes).unwrap(),
        fed,
        "the feed stands"
    );
    assert_eq!(g.sources(sink.event_input(0)).unwrap(), edges);
}

/// **A read of the feeds of a node that is not there names it.** An empty
/// answer reads as "nothing feeds it"; a node that is there and fed by nothing
/// still answers empty.
#[test]
fn feed_and_feeds_refuse_a_node_that_is_not_there() {
    let mut s = detached();
    let g = s.graph();
    let sink = g.add(ForkByClone(Events {
        inputs: 1,
        outputs: 0,
    }));
    assert_eq!(g.feed(GHOST, Feeder::Routes).unwrap_err().node(), GHOST);
    assert_eq!(g.feeds(GHOST).unwrap_err().node(), GHOST);
    assert!(g.feed(sink, Feeder::Routes).unwrap().is_empty());
    assert!(g.feeds(sink).unwrap().is_empty());
}

/// **A graph re-rates only under nodes that run at the new rate, and a refusal
/// names the node and its rates and leaves the rate alone.** Both ends of a
/// node's range are accepted.
#[test]
fn set_sample_rate_refuses_a_rate_a_node_does_not_run_at_and_keeps_the_rate() {
    let mut s = detached();
    let rates = SampleRates::of(SampleRate::SR_44K1, SampleRate::SR_48K);
    let limited = s.graph().add(ForkByClone(Rated(rates)));
    s.graph()
        .add(ForkByClone(Through::new(ChannelLayout::STEREO)));

    let refused = s.set_sample_rate(SampleRate::SR_96K).unwrap_err();
    assert_eq!(
        refused,
        SetRateError::RateUnsupported {
            node: limited.erase(),
            supported: rates,
            rate: SampleRate::SR_96K,
        }
    );
    assert_eq!(s.sample_rate(), RATE, "unchanged");
    assert_eq!(
        s.commit(),
        Committed::Sent,
        "nothing was sent: no re-prepare pends"
    );

    // Below the range is refused the same way, and both ends are accepted.
    assert!(matches!(
        s.set_sample_rate(SampleRate::SR_22K05),
        Err(SetRateError::RateUnsupported { .. })
    ));
    s.set_sample_rate(SampleRate::SR_44K1).unwrap();
    assert_eq!(s.sample_rate(), SampleRate::SR_44K1);
    s.set_sample_rate(SampleRate::SR_48K).unwrap();
    assert_eq!(s.sample_rate(), SampleRate::SR_48K);
}
