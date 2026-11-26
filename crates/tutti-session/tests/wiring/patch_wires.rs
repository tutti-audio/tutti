//! A patch's wires: what `Session::patch` does to the wires between the nodes it
//! declares. A new wire is connected, one left out is disconnected, a kept one
//! is untouched, a changed initial gain is written and a set one wins, a wire
//! the declared nodes cannot make is refused before anything changes, and an
//! edge `Graph::connect` made is never the patch's.

use tutti_buffers::{ChannelLayout, InterleavedMut};
use tutti_graph::{Cx, EventInputPort, Io, Node, Prepare, Shape, Status, declare_params};
use tutti_session::{
    ChangedBy, ConnectError, InputPort, MainOutput, Mention, NodeDecl, OutputPort, PatchError,
    Session, WireStart,
};
use tutti_units::{Gain, Hz};

/// A constant source `level / 4` wide `layout`, with a cutoff to give a patch
/// a param to change beside a gain.
#[derive(Clone)]
pub struct Src {
    layout: ChannelLayout,
    level: u64,
    cells: SrcCells,
}

declare_params!([] Src, SrcConfig, SrcCells {
    required { layout: ChannelLayout }
    params { Cutoff => cutoff: Hz = Hz::new(1_000.0) }
    options { level: u64 = 4 }
});

impl From<SrcConfig> for Src {
    fn from(config: SrcConfig) -> Self {
        Self {
            layout: config.layout,
            level: config.level,
            cells: SrcCells::from_config(&config),
        }
    }
}

impl Node for Src {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(self.layout)
            .events(0, 1)
            .param_ports(self.declared_ports())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for c in 0..usize::from(self.layout.count()) {
            io.output(c).fill(self.level as f32 * 0.25);
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        self.declared_base(port)
    }
}

/// A pass-through `layout` wide: a sink to wire into.
#[derive(Clone)]
pub struct Pass {
    layout: ChannelLayout,
    cells: PassCells,
}

declare_params!([] Pass, PassConfig, PassCells {
    required { layout: ChannelLayout }
    params { Cutoff => cutoff: Hz = Hz::new(1_000.0) }
});

impl From<PassConfig> for Pass {
    fn from(config: PassConfig) -> Self {
        Self {
            layout: config.layout,
            cells: PassCells::from_config(&config),
        }
    }
}

impl Node for Pass {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(self.layout)
            .events(1, 0)
            .param_ports(self.declared_ports())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for c in 0..usize::from(self.layout.count()) {
            outs.get(c).copy_from_slice(ins.get(c));
        }
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        self.declared_base(port)
    }
}

fn session() -> Session {
    Session::builder().build().expect("builds")
}

fn src() -> SrcConfig {
    SrcConfig::new(ChannelLayout::MONO)
}

fn pass() -> PassConfig {
    PassConfig::new(ChannelLayout::MONO)
}

fn amp(x: f64) -> Gain {
    Gain::new(x)
}

fn src_node(s: &Session) -> tutti_graph::NodeKey {
    s.key::<SrcConfig>(&"src").unwrap().erase()
}

fn pass_node(s: &Session) -> tutti_graph::NodeKey {
    s.key::<PassConfig>(&"pass").unwrap().erase()
}

/// What feeds the pass's input 0.
fn fed(s: &Session) -> Vec<OutputPort> {
    s.graph_ref()
        .sources(InputPort::new(pass_node(s), 0))
        .unwrap()
}

/// The last block's samples of a one-channel render, once a moved weight has
/// finished its one-block ramp.
fn settled(s: &mut Session) -> f32 {
    let _ = s.update();
    let mut block = vec![f32::NAN; 128];
    for _ in 0..2 {
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
    }
    block[126]
}

/// **A wire the patch declares is connected**: the sink's sources are the
/// declared source's output.
#[test]
fn a_new_wire_is_connected() {
    let mut s = session();
    let (a, b) = (NodeDecl::new("src", src()), NodeDecl::new("pass", pass()));
    let wire = (&a).connect(&b);

    s.patch((a, b, wire)).unwrap();

    assert_eq!(fed(&s), [OutputPort::new(src_node(&s), 0)]);
}

/// **A wire the last patch declared and this one does not is disconnected**,
/// and the nodes stay.
#[test]
fn a_wire_left_out_is_disconnected() {
    let mut s = session();
    let (a, b) = (NodeDecl::new("src", src()), NodeDecl::new("pass", pass()));
    let wire = Mention::new("src").connect(Mention::new("pass"));
    s.patch((a, b, wire)).unwrap();
    assert_eq!(fed(&s).len(), 1);

    s.patch((NodeDecl::new("src", src()), NodeDecl::new("pass", pass())))
        .unwrap();

    assert!(fed(&s).is_empty());
}

/// **A kept wire is untouched**: a patch that declares it again makes no edit.
#[test]
fn a_kept_wire_is_untouched() {
    let mut s = session();
    let patch = || {
        (
            NodeDecl::new("src", src()),
            NodeDecl::new("pass", pass()),
            Mention::new("src").connect(Mention::new("pass")),
        )
    };
    s.patch(patch()).unwrap();
    let _ = s.update();
    assert!(!s.graph_ref().has_uncommitted_edits());

    let patched = s.patch(patch()).unwrap();

    assert!(patched.dormant.is_empty());
    assert!(!s.graph_ref().has_uncommitted_edits());
    assert_eq!(fed(&s).len(), 1);
}

/// **A wire declared twice is one wire**: the sink has one source, and leaving
/// both out removes it.
#[test]
fn a_wire_declared_twice_is_one_wire() {
    let mut s = session();
    let wire = || Mention::new("src").connect(Mention::new("pass"));
    s.patch((
        NodeDecl::new("src", src()),
        NodeDecl::new("pass", pass()),
        wire(),
        wire(),
    ))
    .unwrap();
    assert_eq!(fed(&s).len(), 1);
}

/// **A changed initial gain is written, in the patch's one gesture with a param
/// change**, and reaches the render.
#[test]
fn a_changed_initial_gain_is_written_in_one_gesture_with_a_param() {
    let mut s = session();
    let patch = |gain: f64, cutoff: f64| {
        (
            NodeDecl::new("src", src().cutoff(Hz::new(cutoff))),
            NodeDecl::new("pass", pass()),
            Mention::new("src")
                .connect(Mention::new("pass"))
                .unfaded()
                .initial_gain(amp(gain)),
            Mention::new("pass").connect(MainOutput).unfaded(),
        )
    };
    s.patch(patch(0.5, 1_000.0)).unwrap();
    assert_eq!(settled(&mut s), 0.5);
    let key = s.key::<SrcConfig>(&"src").unwrap();
    let target = s
        .graph_ref()
        .gain_target(
            OutputPort::new(src_node(&s), 0),
            InputPort::new(pass_node(&s), 0),
        )
        .unwrap();
    let mut watcher = s.watch((key.cutoff(), target)).unwrap();

    s.patch(patch(0.25, 2_000.0)).unwrap();

    let (cutoff, gain) = watcher.changes().unwrap();
    let (cutoff, gain) = (cutoff.unwrap(), gain.unwrap());
    assert_eq!(gain.value, amp(0.25));
    assert_eq!(gain.by, ChangedBy::App);
    assert_eq!(cutoff.gesture.unwrap().id, gain.gesture.unwrap().id);
    assert_eq!(settled(&mut s), 0.25);
}

/// **A gain that was set wins over an initial that did not change**.
#[test]
fn a_set_gain_wins_over_an_unchanged_initial() {
    let mut s = session();
    let patch = || {
        (
            NodeDecl::new("src", src()),
            NodeDecl::new("pass", pass()),
            Mention::new("src")
                .connect(Mention::new("pass"))
                .unfaded()
                .initial_gain(amp(0.5)),
            Mention::new("pass").connect(MainOutput).unfaded(),
        )
    };
    s.patch(patch()).unwrap();
    let target = s
        .graph_ref()
        .gain_target(
            OutputPort::new(src_node(&s), 0),
            InputPort::new(pass_node(&s), 0),
        )
        .unwrap();
    s.set(target.to(amp(0.125))).unwrap();

    s.patch(patch()).unwrap();

    assert_eq!(settled(&mut s), 0.125);
}

/// **A wire that names a key the patch does not declare is refused**, naming
/// it, and a node the same patch declares is not added.
#[test]
fn an_undeclared_mention_is_refused_with_nothing_changed() {
    let mut s = session();
    let nodes = s.graph_ref().node_count();

    let refused = s.patch((
        NodeDecl::new("src", src()),
        Mention::new("src").connect(Mention::new("nowhere")),
    ));

    assert_eq!(refused.unwrap_err(), PatchError::Undeclared("nowhere"));
    assert_eq!(s.graph_ref().node_count(), nodes);
    assert!(s.key::<SrcConfig>(&"src").is_err());
}

/// **A wire to a port the declared node does not have is refused**, with the
/// graph's cause, and the nodes the same patch adds are not added.
#[test]
fn a_bad_port_is_refused_with_nothing_changed() {
    let mut s = session();
    s.patch(NodeDecl::new("old", pass())).unwrap();
    let nodes = s.graph_ref().node_count();
    let (a, b) = (NodeDecl::new("src", src()), NodeDecl::new("pass", pass()));
    let wire = a.output(3).connect(b.input(0));

    let refused = s.patch((NodeDecl::new("old", pass()), a, b, wire));

    let PatchError::Wire { cause, .. } = refused.unwrap_err() else {
        panic!("a wire refusal");
    };
    assert!(matches!(cause, ConnectError::NoSuchOutPort(p) if p.index() == 3));
    assert_eq!(s.graph_ref().node_count(), nodes);
    assert!(s.key::<SrcConfig>(&"src").is_err());
}

/// **A node that is added again gets its declared wires again**: removed by
/// hand, the node is re-added and the wire is made on the new node.
#[test]
fn a_readded_nodes_wires_are_made_again() {
    let mut s = session();
    let patch = || {
        (
            NodeDecl::new("src", src()),
            NodeDecl::new("pass", pass()),
            Mention::new("src").connect(Mention::new("pass")),
        )
    };
    s.patch(patch()).unwrap();
    let old = src_node(&s);
    let _ = s.remove(old).unwrap();

    let patched = s.patch(patch()).unwrap();

    assert_eq!(patched.readded, ["src"]);
    assert_ne!(src_node(&s), old);
    assert_eq!(fed(&s), [OutputPort::new(src_node(&s), 0)]);
}

/// **A wire whose port is on the declared node and not on the running one is
/// dormant**, and is made by the next patch once the swap has landed.
#[test]
fn a_wire_is_dormant_until_its_nodes_swap_lands() {
    let mut s = session();
    s.patch((NodeDecl::new("src", src()), NodeDecl::new("pass", pass())))
        .unwrap();
    let _ = s.update();
    let wider = || SrcConfig::new(ChannelLayout::STEREO);
    let patch = |decl: NodeDecl<&'static str, SrcConfig>| {
        let wire = decl.output(1).connect(Mention::new("pass").input(0));
        (decl, NodeDecl::new("pass", pass()), wire)
    };

    let refused = s.patch(patch(NodeDecl::new("src", wider()))).unwrap();
    assert_eq!(refused.unconverged.len(), 1);
    assert!(matches!(
        refused.dormant.as_slice(),
        [(_, ConnectError::NoSuchOutPort(_))]
    ));
    assert!(fed(&s).is_empty());

    let taken = s
        .patch(patch(NodeDecl::new("src", wider()).unfaded()))
        .unwrap();
    assert_eq!(taken.rebuilt, ["src"]);
    assert!(taken.dormant.is_empty());
    assert_eq!(fed(&s), [OutputPort::new(src_node(&s), 1)]);
}

/// **An edge `Graph::connect` made is not the patch's**: a patch that declares
/// both nodes and no wire leaves it.
#[test]
fn an_imperative_edge_survives_a_patch() {
    let mut s = session();
    let declare = |s: &mut Session| {
        s.patch((NodeDecl::new("src", src()), NodeDecl::new("pass", pass())))
            .unwrap()
    };
    declare(&mut s);
    let (a, b) = (src_node(&s), pass_node(&s));
    s.graph()
        .connect(OutputPort::new(a, 0), InputPort::new(b, 0))
        .unwrap();

    declare(&mut s);

    assert_eq!(fed(&s), [OutputPort::new(a, 0)]);
}

/// **An event wire is connected by the event ports it names**: the sink's event
/// input is fed by the source's event output.
#[test]
fn an_event_wire_is_connected() {
    let mut s = session();
    let (a, b) = (NodeDecl::new("src", src()), NodeDecl::new("pass", pass()));
    let wire = a.event_output(0).connect(b.event_input(0));

    s.patch((a, b, wire)).unwrap();

    let sink = EventInputPort::new(pass_node(&s), 0);
    let source = tutti_graph::EventOutputPort::new(src_node(&s), 0);
    assert_eq!(s.graph_ref().sources(sink).unwrap(), [source]);
}
