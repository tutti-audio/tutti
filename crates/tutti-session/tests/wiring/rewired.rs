//! A commit that moves a node's audio wiring is reported once, by the update
//! that sends it: a port gaining its first source or consumer, or losing its
//! last. A commit that moves nothing reports nothing, and a node it leaves
//! alone is not listed.

use tutti_buffers::{BusTopology, ChannelLayout};
use tutti_graph::{
    Bus, BusRole, Buses, Cx, ForkByClone, Io, Node, NodeKey, Prepare, Shape, Status, Wired,
    WiredMask,
};
use tutti_nodes::testing::Const;
use tutti_session::{OutPort, Root, Session, SessionEvent};

/// A stereo main bus and a mono aux bus on each side: ports 0 and 1 are the
/// main bus, port 2 the aux.
#[derive(Clone)]
struct Bused;

impl Node for Bused {
    fn shape(&self) -> Shape {
        let side = |role| {
            let stereo = BusTopology::unassigned(ChannelLayout::STEREO).unwrap();
            let mono = BusTopology::unassigned(ChannelLayout::MONO).unwrap();
            Buses::NONE
                .with(Bus::main(stereo))
                .and_then(|b| b.with(Bus::new(mono, role)))
                .unwrap()
        };
        Shape::buses(side(BusRole::Sidechain), side(BusRole::Aux))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Silent
    }
    fn reset(&mut self) {}
}

fn wired(inputs: u64, outputs: u64) -> Wired {
    Wired {
        inputs: WiredMask(inputs),
        outputs: WiredMask(outputs),
    }
}

/// The `Rewired` events, by node.
fn rewired(events: &[SessionEvent]) -> Vec<(NodeKey, Wired, Wired)> {
    let mut out: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::Rewired { node, was, now } => Some((*node, *was, *now)),
            _ => None,
        })
        .collect();
    out.sort_by_key(|r| r.0);
    out
}

/// **A connect and the removal of the last source each report once, and only
/// the nodes with a bus to activate that they moved.** `a -> x.aux`, `x.aux ->
/// output 0`, with `idle` (also bused) untouched and `a` (no aux bus) never
/// listed: the connecting update reports `x` gaining a source and a
/// consumer on its aux port; an update that changes nothing reports nothing;
/// cutting `a -> x` reports `x` losing its source; removing `x` reports
/// nothing, there is no node left to notify.
#[test]
fn a_connect_and_the_loss_of_the_last_source_are_each_reported_once() {
    let mut s = Session::builder().build().expect("builds");
    let g = s.graph();
    let a = g.add(ForkByClone(Const::mono(0.5)));
    let x = g.add(ForkByClone(Bused));
    let _idle = g.add(ForkByClone(Bused));
    assert_eq!(rewired(&s.update()), [], "unwired nodes: nothing moved");

    let g = s.graph();
    g.connect(OutPort::new(a, 0), x.input(2)).unwrap().unfaded();
    g.connect(x.out(2), Root::output(0)).unwrap().unfaded();
    let events = s.update();
    assert_eq!(
        rewired(&events),
        [(x.erase(), wired(0, 0), wired(0b100, 0b100))]
    );
    assert_eq!(rewired(&s.update()), [], "reported once");

    let g = s.graph();
    g.disconnect(OutPort::new(a, 0), x.input(2))
        .unwrap()
        .unfaded();
    assert_eq!(
        rewired(&s.update()),
        [(x.erase(), wired(0b100, 0b100), wired(0, 0b100))],
        "the last source going is reported"
    );

    s.graph().remove(x).expect("there");
    assert_eq!(
        rewired(&s.update()),
        [],
        "a removed node has nothing to tell"
    );
}
