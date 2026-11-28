//! A patch's wires: what `Session::patch` does to the wires between the nodes it
//! declares. A new wire is connected, one left out is disconnected, a kept one
//! is untouched, a changed initial gain is written and a set one wins, a wire
//! the declared nodes cannot make is refused before anything changes, and an
//! edge `Graph::connect` made is never the patch's.

use tutti_buffers::{ChannelLayout, InterleavedMut, MaxBlock};
use tutti_graph::{Cx, EventInputPort, Io, Node, Prepare, Shape, Status, declare_params};
use tutti_session::{
    ChangedBy, ConnectError, InputPort, MainOutput, Mention, NodeDecl, OutputPort, PatchError,
    Session, WireEnds, WireStart,
};
use tutti_units::{Gain, Hz, Samples};

#[path = "../support/echo_node.rs"]
mod echo_node;
use echo_node::EchoConfig;

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
            .events(1, 1)
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
            .events(1, 1)
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

/// **A declared wire removed by hand is made again by the next patch**, with
/// its initial gain.
#[test]
fn a_wire_removed_by_hand_is_made_again_with_its_gain() {
    let mut s = session();
    let patch = || {
        (
            NodeDecl::new("src", src()),
            NodeDecl::new("pass", pass()),
            Mention::new("src")
                .connect(Mention::new("pass"))
                .unfaded()
                .initial_gain(amp(0.5)),
        )
    };
    s.patch(patch()).unwrap();
    let (out, input) = (
        OutputPort::new(src_node(&s), 0),
        InputPort::new(pass_node(&s), 0),
    );
    s.graph().disconnect(out, input).unwrap().unfaded();
    assert!(fed(&s).is_empty());

    let patched = s.patch(patch()).unwrap();

    assert!(patched.dormant.is_empty());
    assert_eq!(fed(&s), [out]);
    let target = s.graph_ref().gain_target(out, input).unwrap();
    assert_eq!(s.get(target).unwrap(), amp(0.5));
}

/// **A wire that closes a cycle of direct edges is refused**, naming it, with
/// nothing changed; a patch that turns a wire around is not a cycle.
#[test]
fn a_wire_closing_a_cycle_is_refused_with_nothing_changed() {
    let mut s = session();
    let nodes = || {
        (
            NodeDecl::new("src", src()),
            NodeDecl::new("pass", pass()),
            NodeDecl::new("other", pass()),
        )
    };
    let forward = || Mention::new("pass").connect(Mention::new("other"));
    let back = || Mention::new("other").connect(Mention::new("pass"));

    let empty = s.graph_ref().node_count();
    let refused = s.patch((nodes(), forward(), back()));
    assert!(matches!(refused.unwrap_err(), PatchError::Cycle { .. }));
    assert_eq!(s.graph_ref().node_count(), empty);

    s.patch((nodes(), forward())).unwrap();
    let nodes_now = s.graph_ref().node_count();
    let refused = s.patch((nodes(), forward(), back()));
    assert!(matches!(refused.unwrap_err(), PatchError::Cycle { .. }));
    assert_eq!(s.graph_ref().node_count(), nodes_now);

    s.patch((nodes(), back())).unwrap();
}

/// **A cycle through event or bus wires is refused too**, as one of audio
/// is: the graph's own search reads them all.
#[test]
fn an_event_or_bus_cycle_is_refused_with_nothing_changed() {
    let mut s = session();
    let empty = s.graph_ref().node_count();
    let nodes = || (NodeDecl::new("src", src()), NodeDecl::new("pass", pass()));
    let (a, b) = nodes();
    let events = (
        a.event_output(0).connect(b.event_input(0)),
        b.event_output(0).connect(a.event_input(0)),
    );
    let refused = s.patch((a, b, events));
    assert!(matches!(refused.unwrap_err(), PatchError::Cycle { .. }));
    assert_eq!(s.graph_ref().node_count(), empty);

    let (a, b) = (
        NodeDecl::new("pass", pass()),
        NodeDecl::new("other", pass()),
    );
    let buses = (
        a.bus_output(0).connect(b.bus_input(0)),
        b.bus_output(0).connect(a.bus_input(0)),
    );
    let refused = s.patch((a, b, buses));
    assert!(matches!(refused.unwrap_err(), PatchError::Cycle { .. }));
    assert_eq!(s.graph_ref().node_count(), empty);
}

/// **A node the patch drops takes its hand-made edges out of the cycle
/// search**: `c -> b -> a` is wired by hand, `b` is dropped, and the declared
/// `a -> c` closes no cycle once `b` is gone.
#[test]
fn a_dropped_nodes_hand_made_edges_close_no_cycle() {
    let mut s = session();
    let nodes = || {
        (
            NodeDecl::new("a", pass()),
            NodeDecl::new("b", pass()),
            NodeDecl::new("c", pass()),
        )
    };
    s.patch(nodes()).unwrap();
    let node = |s: &Session, key| s.key::<PassConfig>(&key).unwrap().erase();
    let (a, b, c) = (node(&s, "a"), node(&s, "b"), node(&s, "c"));
    s.graph().connect(c, b).unwrap();
    s.graph().connect(b, a).unwrap();

    let patched = s.patch((
        NodeDecl::new("a", pass()),
        NodeDecl::new("c", pass()),
        Mention::new("a").connect(Mention::new("c")),
    ));

    assert_eq!(patched.unwrap().removed, ["b"]);
}

/// The block of the loop sessions, and a delay of two of them.
const BLOCK: usize = 64;
const DELAY: usize = 2 * BLOCK;

/// A mono session whose largest block is `BLOCK` frames.
fn looped() -> Session {
    Session::builder()
        .outputs(ChannelLayout::MONO)
        .outputs_floor(ChannelLayout::MONO)
        .max_block(MaxBlock::of(Samples::new(BLOCK)))
        .build()
        .expect("builds")
}

/// `echo -> pass` direct and `pass -> echo` fed back by `delay` frames, at
/// gain `gain`, the pass on the main output: every wire unfaded.
fn echo_loop(
    delay: usize,
    gain: f64,
) -> impl tutti_session::IntoPatch<&'static str, tutti_session::Graph> {
    (
        NodeDecl::new("echo", EchoConfig::new(ChannelLayout::MONO)),
        NodeDecl::new("pass", pass()),
        Mention::new("echo").connect(Mention::new("pass")).unfaded(),
        Mention::new("pass")
            .connect(Mention::new("echo"))
            .feedback(Samples::new(delay))
            .unfaded()
            .initial_gain(amp(gain)),
        Mention::new("pass").connect(MainOutput).unfaded(),
    )
}

/// `blocks` blocks of the mono main output, after an update.
fn rendered(s: &mut Session, blocks: usize) -> Vec<f32> {
    let _ = s.update();
    let mut out = Vec::new();
    for _ in 0..blocks {
        let mut block = vec![f32::NAN; BLOCK];
        s.render(&mut InterleavedMut::new(&mut block, ChannelLayout::MONO).unwrap());
        out.extend(block);
    }
    out
}

/// **A feedback wire closes a loop that renders its analytic echo.** `echo`
/// adds a unit impulse at frame 0 and `pass` carries it to the output; `pass`
/// fed back into `echo` by `DELAY` frames at gain `g` (no node of the loop has
/// latency) is a train of impulses at `k * DELAY` of amplitude `g^k`, and
/// nothing between.
#[test]
fn a_feedback_wire_closes_a_loop_that_echoes_at_its_delay() {
    let mut s = looped();
    s.patch(echo_loop(DELAY, 0.5)).unwrap();

    let out = rendered(&mut s, 6);

    let mut want = vec![0.0f32; 6 * BLOCK];
    want[0] = 1.0;
    want[DELAY] = 0.5;
    want[2 * DELAY] = 0.25;
    assert_eq!(out, want);
}

/// **`initial_gain` on a feedback wire is the loop gain, and a set on its
/// target moves it**: the target of the ports resolves to the feedback edge.
#[test]
fn a_feedback_wires_gain_target_moves_the_loop_gain() {
    let mut s = looped();
    s.patch(echo_loop(DELAY, 0.5)).unwrap();
    let echo = s.key::<EchoConfig>(&"echo").unwrap().erase();
    let pass = s.key::<PassConfig>(&"pass").unwrap().erase();
    let target = s
        .graph_ref()
        .gain_target(OutputPort::new(pass, 0), InputPort::new(echo, 0))
        .unwrap();
    assert_eq!(s.get(target).unwrap(), amp(0.5));
    s.set(target.to(amp(0.25))).unwrap();

    let out = rendered(&mut s, 4);

    assert_eq!(out[DELAY], 0.25);
}

/// **A changed delay makes the wire again**: the edge of the old delay is
/// gone, and the render echoes at the new one only.
#[test]
fn a_changed_delay_remakes_the_edge() {
    let mut s = looped();
    s.patch(echo_loop(DELAY, 0.5)).unwrap();
    let _ = s.update();

    s.patch(echo_loop(3 * BLOCK, 0.5)).unwrap();

    let out = rendered(&mut s, 7);
    assert_eq!(out[DELAY], 0.0, "nothing at the old delay");
    assert_eq!(out[3 * BLOCK], 0.5);
}

/// **A delay shorter than the largest block is refused at the patch**, naming
/// the wire, and nothing changes.
#[test]
fn a_delay_shorter_than_the_block_is_refused_with_nothing_changed() {
    let mut s = looped();
    let nodes = s.graph_ref().node_count();

    let refused = s.patch(echo_loop(BLOCK - 1, 0.5));

    let PatchError::FeedbackTooShort {
        ends,
        delay,
        max_block,
    } = refused.unwrap_err()
    else {
        panic!("a feedback refusal");
    };
    assert_eq!(ends, WireEnds::Main("pass", Some("echo")));
    assert_eq!(
        (delay, max_block),
        (Samples::new(BLOCK - 1), Samples::new(BLOCK))
    );
    assert_eq!(s.graph_ref().node_count(), nodes);
    s.patch(echo_loop(BLOCK, 0.5))
        .expect("a delay of one block is the shortest");
}

/// **A pair of ports holds one edge**: a direct and a feedback wire on the
/// same ports are refused, declared together or against an edge the graph
/// holds, with nothing changed.
#[test]
fn a_direct_and_a_feedback_wire_on_one_pair_of_ports_are_refused() {
    let mut s = looped();
    let nodes = || {
        (
            NodeDecl::new("echo", EchoConfig::new(ChannelLayout::MONO)),
            NodeDecl::new("pass", pass()),
        )
    };
    let direct = || Mention::new("echo").connect(Mention::new("pass"));
    let delayed = || {
        Mention::new("echo")
            .connect(Mention::new("pass"))
            .feedback(Samples::new(DELAY))
    };
    let empty = s.graph_ref().node_count();

    let refused = s.patch((nodes(), direct(), delayed()));
    assert!(matches!(refused.unwrap_err(), PatchError::TwoEdges { .. }));
    let refused = s.patch((
        nodes(),
        delayed(),
        Mention::new("echo")
            .connect(Mention::new("pass"))
            .feedback(Samples::new(DELAY + 1)),
    ));
    assert!(matches!(refused.unwrap_err(), PatchError::TwoEdges { .. }));
    assert_eq!(s.graph_ref().node_count(), empty);

    // An edge made by hand holds the ports too.
    s.patch(nodes()).unwrap();
    let echo = s.key::<EchoConfig>(&"echo").unwrap().erase();
    let pass = s.key::<PassConfig>(&"pass").unwrap().erase();
    s.graph().connect(echo, pass).unwrap().unfaded();
    let refused = s.patch((nodes(), delayed()));
    assert!(matches!(refused.unwrap_err(), PatchError::TwoEdges { .. }));
    assert_eq!(
        s.graph_ref().sources(InputPort::new(pass, 0)).unwrap(),
        [OutputPort::new(echo, 0)]
    );

    // A patch that takes the direct wire out may put the feedback one there.
    s.graph().disconnect(echo, pass).unwrap().unfaded();
    s.patch((nodes(), delayed())).unwrap();
}

/// **A feedback wire is a source of its sink**: `Graph::sources` lists the
/// source of the edge the loop closes.
#[test]
fn a_feedback_wire_is_listed_as_a_source() {
    let mut s = looped();
    s.patch(echo_loop(DELAY, 1.0)).unwrap();
    let echo = s.key::<EchoConfig>(&"echo").unwrap().erase();
    let pass = s.key::<PassConfig>(&"pass").unwrap().erase();

    let sources = s.graph_ref().sources(InputPort::new(echo, 0)).unwrap();

    assert_eq!(sources, [OutputPort::new(pass, 0)]);
}

/// **An event wire can be fed back**: `src -> pass` by event and `pass ->
/// src` by event is a cycle of direct edges, refused, unless one is a
/// feedback wire; with one the patch is taken.
#[test]
fn an_event_feedback_wire_closes_an_event_loop() {
    let mut s = session();
    let (a, b) = (NodeDecl::new("src", src()), NodeDecl::new("pass", pass()));
    let forward = a.event_output(0).connect(b.event_input(0));
    let back = b
        .event_output(0)
        .connect(a.event_input(0))
        .feedback(Samples::new(1_024));
    let looped = (
        a.event_output(0).connect(b.event_input(0)),
        b.event_output(0).connect(a.event_input(0)),
    );

    assert!(matches!(
        s.patch((
            NodeDecl::new("src", src()),
            NodeDecl::new("pass", pass()),
            looped
        ))
        .unwrap_err(),
        PatchError::Cycle { .. }
    ));
    s.patch((a, b, forward, back)).unwrap();

    let sink = EventInputPort::new(src_node(&s), 0);
    let source = tutti_graph::EventOutputPort::new(pass_node(&s), 0);
    assert_eq!(s.graph_ref().sources(sink).unwrap(), [source]);
}

/// What feeds `node`'s input 0 now: an edge fading out is no source.
fn sources_of(s: &Session, node: tutti_graph::NodeKey) -> Vec<OutputPort> {
    s.graph_ref().sources(InputPort::new(node, 0)).unwrap()
}

/// **A patch that turns a wire around is valid, and cuts the old edge**: the
/// compile counts an edge that is only fading out, so `a -> b` left to fade
/// beside the new `b -> a` would be a cycle the commit refuses. The old edge
/// goes at once, the new one is live, and the next update commits.
#[test]
fn turning_a_wire_around_cuts_the_old_edge_and_commits() {
    let mut s = session();
    let nodes = || (NodeDecl::new("a", pass()), NodeDecl::new("b", pass()));
    s.patch((nodes(), Mention::new("a").connect(Mention::new("b"))))
        .unwrap();
    let _ = s.update();
    let node = |s: &Session, key| s.key::<PassConfig>(&key).unwrap().erase();
    let (a, b) = (node(&s, "a"), node(&s, "b"));
    assert_eq!(sources_of(&s, b), [OutputPort::new(a, 0)]);

    s.patch((nodes(), Mention::new("b").connect(Mention::new("a"))))
        .unwrap();

    assert!(sources_of(&s, b).is_empty(), "the old edge is cut");
    let events = s.update();
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, tutti_session::SessionEvent::CommitRefused(_))),
        "{events:?}"
    );
    assert_eq!(
        s.graph_ref().sources(InputPort::new(a, 0)).unwrap(),
        [OutputPort::new(b, 0)]
    );
}

/// **Swapping which wire of a loop is the feedback one is valid**: `a -> b`
/// direct and `b -> a` fed back, then `a -> b` fed back and `b -> a` direct.
/// The old direct edge, fading, would close a cycle with the new direct one,
/// so it is cut.
#[test]
fn swapping_the_feedback_wire_of_a_loop_cuts_the_old_direct_edge_and_commits() {
    let mut s = looped();
    let patch = |forward_delayed: bool| {
        let forward = Mention::new("a").connect(Mention::new("b"));
        let back = Mention::new("b").connect(Mention::new("a"));
        let delayed = Samples::new(DELAY);
        let (forward, back) = if forward_delayed {
            (forward.feedback(delayed), back)
        } else {
            (forward, back.feedback(delayed))
        };
        (
            NodeDecl::new("a", pass()),
            NodeDecl::new("b", pass()),
            forward,
            back,
        )
    };
    s.patch(patch(false)).unwrap();
    let _ = s.update();

    s.patch(patch(true)).unwrap();

    let events = s.update();
    assert!(
        events
            .iter()
            .all(|e| !matches!(e, tutti_session::SessionEvent::CommitRefused(_))),
        "{events:?}"
    );
}

/// **A removed feedback wire fades out**, like a direct one: its echo decays
/// along the declick instead of cutting. The loop echoes a unit impulse at
/// frame 0 into frame `DELAY`; the wire is removed before the first frame
/// is rendered, so frame `j` of the render is frame `j` of the fade, and the
/// echo at `DELAY` is weighted `1 - s((DELAY + 1) / 241)`, `s` the declick's
/// smootherstep (240 frames at 48 kHz), rendered a frame at a time. The next echo is past the fade: none.
#[test]
fn a_removed_feedback_wire_fades_its_echo_out() {
    const SPAN: usize = 240;
    let smooth = |j: usize| {
        let x = (j + 1) as f64 / (SPAN + 1) as f64;
        x * x * x * (6.0 * x * x - 15.0 * x + 10.0)
    };
    let mut s = looped();
    let patch = |feedback: Option<bool>| {
        (
            NodeDecl::new("echo", EchoConfig::new(ChannelLayout::MONO)),
            NodeDecl::new("pass", pass()),
            Mention::new("echo").connect(Mention::new("pass")).unfaded(),
            feedback.map(|unfaded| {
                let wire = Mention::new("pass")
                    .connect(Mention::new("echo"))
                    .feedback(Samples::new(DELAY));
                if unfaded { wire.unfaded() } else { wire }
            }),
            Mention::new("pass").connect(MainOutput).unfaded(),
        )
    };
    // Made with no fade in, declared again with the fade out it will leave by.
    s.patch(patch(Some(true))).unwrap();
    s.patch(patch(Some(false))).unwrap();
    let _ = s.update();

    s.patch(patch(None)).unwrap();
    let _ = s.update();

    let out: Vec<f32> = (0..3 * DELAY)
        .map(|_| {
            let mut frame = [f32::NAN];
            s.render(&mut InterleavedMut::new(&mut frame, ChannelLayout::MONO).unwrap());
            frame[0]
        })
        .collect();
    assert_eq!(out[0], 1.0);
    let want = 1.0 - smooth(DELAY);
    assert!(
        (f64::from(out[DELAY]) - want).abs() < 1e-6,
        "{} against {want}",
        out[DELAY]
    );
    assert!(out[DELAY] > 0.0 && out[DELAY] < 1.0);
    assert_eq!(out[2 * DELAY], 0.0, "the next echo is past the fade");
}
