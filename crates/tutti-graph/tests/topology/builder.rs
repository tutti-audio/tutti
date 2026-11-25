//! `GraphBuilder`: what it builds is what a host writing the `GraphSpec` by
//! hand builds, and its fan-out calls follow the rules its docs state. Those
//! rules are written out independently below ([`net_rule`]) and each render
//! is pinned to the figure they give in closed form.

use tutti_graph::testing::WireDeclared;

use tutti_buffers::{Planes, PlanesMut};
use tutti_fixtures::signal::bits;
use tutti_graph::EdgeId;
use tutti_graph::testing as common;

use common::{Kind, TestNode, prepare};
use tutti_buffers::ChannelLayout;
use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, Editor, EventEdge, EventInputPort, EventOutputPort, ForkByClone, GraphBuilder, Io, Node,
    Prepare, Renderer, Shape, Status, TransportPosition, Unforkable,
};
use tutti_graph::{Edge, FeedbackFrom, GraphSpec, InputPort, OutputPort, PortSource};
use tutti_units::{Beat, Frame, Samples};

fn signal(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| ((i * 7919 % 97) as f32 - 48.0) / 48.0)
        .collect()
}

/// `ins` inputs, `outs` outputs; output `c` is `c + 1` plus the sum of the
/// inputs. Any width, so the fan-out rules can be walked over a grid, and
/// every output is a small integer while the main inputs are silent, so
/// what a wiring renders is known exactly ([`net_rule::constants`]).
#[derive(Clone)]
struct Width {
    ins: usize,
    outs: usize,
}

impl Node for Width {
    fn shape(&self) -> Shape {
        Shape::buses(
            tutti_graph::Buses::unassigned(ch(self.ins)),
            tutti_graph::Buses::unassigned(ch(self.outs)),
        )
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let frames = io.frames();
        let (ins, mut outs) = io.split();
        for i in 0..frames {
            let sum: f32 = (0..self.ins).map(|c| ins.get(c)[i]).sum();
            for c in 0..self.outs {
                outs.get(c)[i] = (c + 1) as f32 + sum;
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// [`Width`] as a graph node.
fn node(ins: usize, outs: usize) -> ForkByClone<Width> {
    ForkByClone(Width { ins, outs })
}

fn ch(n: usize) -> ChannelLayout {
    ChannelLayout::try_from(n).expect("a test width is small")
}

/// The wiring `g` builds: the built editor's spec.
fn wiring(g: GraphBuilder) -> GraphSpec {
    g.build(prepare(64)).expect("builds").0.spec().clone()
}

/// What the builder feeds `node`'s input `port`. An absent edge reads
/// silence, as an explicit `Zero` does.
fn source_of(spec: &GraphSpec, node: NodeKey, port: u16) -> PortSource {
    match spec
        .topology
        .edges
        .get(&InputPort::new(node, port))
        .map(Vec::as_slice)
    {
        Some([Edge::Direct(s)]) => *s,
        Some([Edge::Feedback(_)]) => panic!("no feedback in these graphs"),
        Some(several) => panic!("one source per port here, got {several:?}"),
        None => PortSource::Zero,
    }
}

/// Every input of every node, and every main output, is fed from where
/// `want` (one entry per node, in the order added: its width and what feeds
/// each input) and `outputs` say.
fn assert_wired(
    spec: &GraphSpec,
    want: &[(NodeKey, net_rule::Node)],
    outputs: &[PortSource],
    case: &str,
) {
    for (key, n) in want {
        for (c, s) in n.sources.iter().enumerate() {
            assert_eq!(
                source_of(spec, *key, c as u16),
                *s,
                "{case}: {key:?} input {c}"
            );
        }
    }
    assert_eq!(
        spec.topology.outputs,
        outputs
            .iter()
            .map(|s| if *s == PortSource::Zero {
                vec![]
            } else {
                vec![*s]
            })
            .collect::<Vec<_>>(),
        "{case}: main outputs"
    );
}

/// Assert `got` holds, on every frame, the constant each output channel of
/// `want`'s wiring carries.
fn assert_renders(
    got: &[Vec<f32>],
    want: &[(NodeKey, net_rule::Node)],
    outputs: &[PortSource],
    frames: usize,
    case: &str,
) {
    let values = net_rule::constants(want, outputs);
    assert_eq!(got.len(), values.len(), "{case}: channel count");
    for (c, (ch, v)) in got.iter().zip(&values).enumerate() {
        assert_eq!(ch.len(), frames, "{case}: channel {c} length");
        assert!(
            ch.iter().all(|x| x.to_bits() == v.to_bits()),
            "{case}: channel {c} should be {v} on every frame, got {:?}",
            &ch[..ch.len().min(4)]
        );
    }
}

/// The builder's fan-out rules, written out independently over [`Width`]
/// nodes.
mod net_rule {
    use super::*;

    /// A node: its output width and what feeds each of its inputs.
    pub(super) struct Node {
        pub outs: usize,
        pub sources: Vec<PortSource>,
    }

    fn port(node: NodeKey, port: usize) -> PortSource {
        PortSource::Node(OutputPort::new(node, port as u16))
    }

    /// `pipe(a, b)`: input `c` of `b` reads `a`'s port
    /// `c % a_outs`; silence when `a` has no outputs.
    pub(super) fn pipe_all(a: NodeKey, a_outs: usize, b_ins: usize) -> Vec<PortSource> {
        (0..b_ins)
            .map(|c| {
                if a_outs > 0 {
                    port(a, c % a_outs)
                } else {
                    PortSource::Zero
                }
            })
            .collect()
    }

    /// `pipe_output(n)`: main output `c` reads `n`'s port
    /// `c % n_outs`; silence when `n` has no outputs.
    pub(super) fn pipe_output(n: NodeKey, n_outs: usize, outputs: usize) -> Vec<PortSource> {
        (0..outputs)
            .map(|c| {
                if n_outs > 0 {
                    port(n, c % n_outs)
                } else {
                    PortSource::Zero
                }
            })
            .collect()
    }

    /// `pipe_input(n)`: input `c` reads main input `c % main_inputs`;
    /// silence when there are none.
    pub(super) fn pipe_input(main_inputs: usize, ins: usize) -> Vec<PortSource> {
        (0..ins)
            .map(|c| {
                if main_inputs > 0 {
                    PortSource::MainInput((c % main_inputs) as u16)
                } else {
                    PortSource::Zero
                }
            })
            .collect()
    }

    /// `chain` over `widths`: the first node takes `pipe_input` (when
    /// there are main inputs; otherwise its inputs stay silent), each later
    /// one reads what fed main output `i % outputs` (silence with no
    /// outputs), and each takes over the outputs with `pipe_output`.
    pub(super) fn chain(
        main_inputs: usize,
        outputs: usize,
        widths: &[(usize, usize)],
    ) -> (Vec<(NodeKey, Node)>, Vec<PortSource>) {
        let mut nodes = Vec::new();
        let mut outs = vec![PortSource::Zero; outputs];
        for (k, &(ins, w)) in widths.iter().enumerate() {
            let key = NodeKey(k as u64);
            let sources = if k == 0 {
                if main_inputs > 0 {
                    pipe_input(main_inputs, ins)
                } else {
                    vec![PortSource::Zero; ins]
                }
            } else {
                (0..ins)
                    .map(|i| {
                        if outputs > 0 {
                            outs[i % outputs]
                        } else {
                            PortSource::Zero
                        }
                    })
                    .collect()
            };
            nodes.push((key, Node { outs: w, sources }));
            outs = pipe_output(key, w, outputs);
        }
        (nodes, outs)
    }

    /// What each main output carries with silent main inputs: a
    /// [`Width`] node's output `c` is `c + 1` plus the sum of its inputs,
    /// evaluated in the order the nodes were added (every source precedes
    /// the node it feeds in these graphs).
    pub(super) fn constants(nodes: &[(NodeKey, Node)], outputs: &[PortSource]) -> Vec<f32> {
        let mut value: Vec<(NodeKey, Vec<f32>)> = Vec::new();
        let read = |value: &[(NodeKey, Vec<f32>)], s: &PortSource| match s {
            PortSource::Node(from) => {
                value
                    .iter()
                    .find(|(k, _)| *k == from.node())
                    .expect("source precedes")
                    .1[usize::from(from.port())]
            }
            _ => 0.0,
        };
        for (key, n) in nodes {
            let sum: f32 = n.sources.iter().map(|s| read(&value, s)).sum();
            let outs = (0..n.outs).map(|c| (c + 1) as f32 + sum).collect();
            value.push((*key, outs));
        }
        outputs.iter().map(|s| read(&value, s)).collect()
    }
}

/// The same graph written twice — through the builder, and by hand through
/// `Editor::insert` and the wiring calls — gives equal specs, equal plans and a
/// bit-identical render. It covers every kind of wiring the builder writes:
/// a main input, a node-to-node edge, a feedback edge, a main output
/// fed by `set_output` and one by `connect_output`, and an event edge.
#[test]
fn builder_builds_what_a_hand_written_spec_builds() {
    let pre = prepare(64);
    let fb_delay = Samples::new(64);

    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::STEREO);
    let mix = g.add(Unforkable(TestNode::new(Kind::Sum { inputs: 2 })));
    let lp = g.add(Unforkable(TestNode::new(Kind::Smooth)));
    let emit = g.add(Unforkable(TestNode::new(Kind::Emitter {
        period: 50,
        phase: 7,
    })));
    let fold = g.add(Unforkable(TestNode::new(Kind::Consumer { inputs: 1 })));
    g.connect_input(0, mix, 0)
        .feedback(lp, 0, mix, 1, fb_delay)
        .connect(mix, 0, lp, 0)
        .set_output(0, PortSource::Node(OutputPort::new(lp, 0)))
        .connect_output(fold, 0, 1)
        .connect_events(emit, 0, fold, 0);
    assert_eq!(
        [mix, lp, emit, fold],
        [NodeKey(0), NodeKey(1), NodeKey(2), NodeKey(3)],
        "keys in the order nodes were added"
    );
    let mut built = g.renderer(pre).expect("builds");

    let (mut ed, exec) = Editor::new(pre);
    let test_node = std::any::type_name::<TestNode>();
    ed.insert(
        NodeKey(0),
        test_node,
        Unforkable(TestNode::new(Kind::Sum { inputs: 2 })),
    );
    ed.insert(
        NodeKey(1),
        test_node,
        Unforkable(TestNode::new(Kind::Smooth)),
    );
    ed.insert(
        NodeKey(2),
        test_node,
        Unforkable(TestNode::new(Kind::Emitter {
            period: 50,
            phase: 7,
        })),
    );
    ed.insert(
        NodeKey(3),
        test_node,
        Unforkable(TestNode::new(Kind::Consumer { inputs: 1 })),
    );
    ed.resize_inputs(ChannelLayout::MONO);
    let at = |node: u64, port: u16| InputPort::new(NodeKey(node), port);
    let from = |node: u64, port: u16| OutputPort::new(NodeKey(node), port);
    ed.connect_edge(EdgeId::into_port(
        at(0, 0),
        Edge::Direct(PortSource::MainInput(0)),
    ))
    .unwrap();
    ed.connect_edge(EdgeId::into_port(
        at(0, 1),
        Edge::Feedback(FeedbackFrom::new(from(1, 0), fb_delay)),
    ))
    .unwrap();
    ed.connect_edge(EdgeId::into_port(
        at(1, 0),
        Edge::Direct(PortSource::Node(from(0, 0))),
    ))
    .unwrap();
    ed.wire_declared_outputs(vec![
        PortSource::Node(from(1, 0)),
        PortSource::Node(from(3, 0)),
    ])
    .unwrap();
    ed.connect_events(
        EventInputPort::new(NodeKey(3), 0),
        EventEdge::Direct(EventOutputPort::new(NodeKey(2), 0)),
    )
    .unwrap();
    ed.commit().expect("commits");
    let mut by_hand = Renderer::new(ed, exec);
    by_hand.render(1); // installs the plan the builder installed eagerly

    assert_eq!(built.editor().spec(), by_hand.editor().spec());
    assert_eq!(
        **built.executor().plan().expect("installed"),
        **by_hand.executor().plan().expect("installed")
    );

    // The hand-built side has rendered one silent frame to install its
    // plan; render the same frame on the built side, so both clocks (and so
    // the emitter's phase) line up before the comparison.
    built.render_input(Planes::new(&[&[0.0]]).unwrap());
    let input = signal(2_000);
    let a = built.render_input(Planes::new(&[&input]).unwrap());
    let b = by_hand.render_input(Planes::new(&[&input]).unwrap());
    assert_eq!(a.len(), 2);
    for c in 0..2 {
        assert_eq!(bits(&a[c]), bits(&b[c]), "channel {c}");
        assert!(a[c].iter().any(|&x| x != 0.0), "channel {c} is not silent");
    }
}

/// `pipe` connects ports in order, with its fan-out rule — over a
/// grid of widths, including a source with no outputs, checked against the
/// rule written out ([`net_rule::pipe_all`]) and by the figure it renders.
#[test]
fn pipe_connects_ports_in_order_as_net_pipe_all() {
    for outs in 0..=3 {
        for ins in 1..=3 {
            let case = format!("{outs} outputs → {ins} inputs");
            let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
            let (a, b) = (g.add(node(0, outs)), g.add(node(ins, 1)));
            g.pipe(a, b).pipe_output(b);
            let want = [
                (
                    a,
                    net_rule::Node {
                        outs,
                        sources: vec![],
                    },
                ),
                (
                    b,
                    net_rule::Node {
                        outs: 1,
                        sources: net_rule::pipe_all(a, outs, ins),
                    },
                ),
            ];
            let outputs = net_rule::pipe_output(b, 1, 1);
            let mut r = g.renderer(prepare(64)).expect("builds");
            assert_wired(r.editor().spec(), &want, &outputs, &case);

            // And it renders what that wiring carries.
            let got = r.render(100);
            assert_renders(&got, &want, &outputs, 100, &case);
        }
    }
    // The plain case, spelled out: port c → port c.
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let (a, b) = (g.add(node(0, 3)), g.add(node(3, 1)));
    g.pipe(a, b);
    let spec = wiring(g);
    for c in 0..3 {
        assert_eq!(
            source_of(&spec, b, c),
            PortSource::Node(OutputPort::new(a, c))
        );
    }
}

/// `pipe_output` fans a node out over the main outputs: output `c` reads port `c % width`. Mono feeds
/// every channel; stereo into six **wraps** (L R L R L R), it does not clamp
/// to the last channel; a wider node's extra ports go unused; a node with no
/// outputs feeds silence. Checked structurally and by the figure it renders.
#[test]
fn pipe_output_fans_out_as_net_does() {
    for (w, outs) in [(0, 2), (1, 1), (1, 2), (1, 6), (2, 6), (3, 2), (6, 2)] {
        let case = format!("{w}-wide node → {outs} outputs");
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ch(outs));
        let key = g.add(node(0, w));
        g.pipe_output(key);
        let want = [(
            key,
            net_rule::Node {
                outs: w,
                sources: vec![],
            },
        )];
        let outputs = net_rule::pipe_output(key, w, outs);
        let mut r = g.renderer(prepare(64)).expect("builds");
        assert_wired(r.editor().spec(), &want, &outputs, &case);

        let got = r.render(70);
        assert_renders(&got, &want, &outputs, 70, &case);
    }
    // Stereo into six, spelled out.
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ch(6));
    let key = g.add(node(0, 2));
    g.pipe_output(key);
    let ports: Vec<PortSource> = (0..6)
        .map(|c| PortSource::Node(OutputPort::new(key, c % 2)))
        .collect();
    assert_eq!(
        wiring(g).topology.outputs,
        ports
            .iter()
            .map(|s| if *s == PortSource::Zero {
                vec![]
            } else {
                vec![*s]
            })
            .collect::<Vec<_>>()
    );
}

/// `pipe_input` reads main input `c % inputs`, and silence with no main
/// inputs.
#[test]
fn pipe_input_wraps_as_net_does() {
    for main_inputs in 0..=3 {
        for ins in 1..=3 {
            let case = format!("{main_inputs} main_inputs → {ins} inputs");
            let mut g = GraphBuilder::new(ch(main_inputs), ChannelLayout::MONO);
            let key = g.add(node(ins, 1));
            g.pipe_input(key).pipe_output(key);
            let want = [(
                key,
                net_rule::Node {
                    outs: 1,
                    sources: net_rule::pipe_input(main_inputs, ins),
                },
            )];
            assert_wired(&wiring(g), &want, &net_rule::pipe_output(key, 1, 1), &case);
        }
    }
}

/// `chain` extends a series: the first node reads the
/// main inputs, each later one reads what fed the main outputs
/// (wrapping), and every one takes over the outputs. Across width changes,
/// with and without main inputs, and to the bit in render.
#[test]
fn chain_extends_the_series_as_net_does() {
    for main_inputs in [0, 1, 2] {
        let widths = [(1, 1), (1, 3), (2, 2), (3, 1), (1, 2)];
        let case = format!("{main_inputs} main_inputs");
        let mut g = GraphBuilder::new(ch(main_inputs), ChannelLayout::STEREO);
        for (ins, outs) in widths {
            g.chain(node(ins, outs));
        }
        let (want, outputs) = net_rule::chain(main_inputs, 2, &widths);
        let mut r = g.renderer(prepare(64)).expect("builds");
        assert_wired(r.editor().spec(), &want, &outputs, &case);

        let got = r.render(70);
        assert_renders(&got, &want, &outputs, 70, &case);
    }
}

/// `chain` takes an `IntoNode` with controls too, and `add_with_controls` hands back what
/// `IntoNode` returns.
#[test]
fn chain_and_add_take_nodes() {
    struct Tagged;
    impl tutti_graph::IntoNode for Tagged {
        type Controls = u32;
        fn into_parts(self) -> tutti_graph::NodeParts<u32> {
            tutti_graph::NodeParts::new(
                Box::new(TestNode::new(Kind::Gain {
                    gain: 2.0,
                    width: 1,
                })),
                7,
                None,
                None,
            )
        }
    }
    let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
    let first = g.chain(Unforkable(TestNode::new(Kind::Gain {
        gain: 3.0,
        width: 1,
    })));
    let (second, controls) = g.add_with_controls(Tagged);
    g.pipe(first, second).pipe_output(second);
    assert_eq!((first, second, controls), (NodeKey(0), NodeKey(1), 7));
    let out = g
        .renderer(prepare(64))
        .expect("builds")
        .render_input(Planes::new(&[&[1.0; 10]]).unwrap());
    assert_eq!(out, vec![vec![6.0; 10]]);
}

/// An out-of-range port is a panic at the call, not a graph
/// that fails later.
#[test]
#[should_panic(expected = "port 1 is out of range")]
fn an_out_of_range_port_panics_at_the_call() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let (a, b) = (g.add(node(0, 1)), g.add(node(1, 1)));
    g.connect(a, 0, b, 1);
}

/// A graph the editor refuses is refused by `build` with the editor's own
/// error: a feedback delay shorter than the block.
#[test]
fn build_returns_the_editors_error() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let a = g.add(node(1, 1));
    g.feedback(a, 0, a, 0, Samples::new(16)).pipe_output(a);
    assert!(matches!(
        g.build(prepare(64)),
        Err(tutti_graph::CommitError::Compile(
            tutti_graph::CompileError::FeedbackTooShort { .. }
        ))
    ));
}

/// The renderer asks for the transport once per block, at the block's first
/// frame, in blocks of the chosen length (the last one short), and
/// interleaves frame by frame.
#[test]
fn renderer_drives_blocks_and_interleaves() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let key = g.add(node(0, 2));
    g.pipe_output(key);
    let mut r = g.renderer(prepare(256)).expect("builds");
    r.set_block(Samples::new(100));
    let seen = tutti_fixtures::Record::new();
    let log = seen.clone();
    r.set_transport_fn(move |frame| {
        log.lock().unwrap().push(frame);
        TransportPosition::new(
            true,
            tutti_units::Bpm::of(120.0),
            Beat::new(frame.get() as f64),
            None,
        )
    });
    let out = r.render_interleaved(250);
    assert_eq!(
        seen.lock().unwrap().as_slice(),
        [Frame::new(0), Frame::new(100), Frame::new(200)]
    );
    assert_eq!(out.len(), 500);
    assert!(out.chunks(2).all(|f| f == [1.0, 2.0]));

    // `render_into` writes where it is told, and advances the same clock.
    let (mut l, mut rr) = (vec![0.0; 30], vec![0.0; 30]);
    r.render_into(PlanesMut::new(&mut [&mut l, &mut rr]).unwrap());
    assert_eq!((l, rr), (vec![1.0; 30], vec![2.0; 30]));
    assert_eq!(r.executor().frame(), Frame::new(280));
}

/// Silence is the empty set and `PortSource::Zero` is never stored: every writer
/// normalises, so two topologies built the old ways (a stored `Zero`, an
/// absent key) compare and hash equal.
#[test]
fn every_writer_normalises_silence_to_the_empty_set() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let hash = |t: &tutti_graph::Topology| {
        let mut h = DefaultHasher::new();
        t.hash(&mut h);
        h.finish()
    };
    let at = InputPort::new(NodeKey(0), 0);

    // A builder: a fresh one is empty, a port set to silence is no key.
    let fresh = || {
        let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::STEREO);
        let a = g.add(node(1, 1));
        (g, a)
    };
    let (g, a) = fresh();
    assert_eq!(wiring(g).topology.outputs, vec![vec![], vec![]]);
    let (mut g, _) = fresh();
    g.set_source(a, 0, PortSource::MainInput(0));
    assert_eq!(wiring(g).topology.edges.len(), 1);
    let (mut g, _) = fresh();
    g.set_source(a, 0, PortSource::MainInput(0));
    g.disconnect(a, 0);
    assert!(wiring(g).topology.edges.is_empty(), "cleared port: no key");
    let (mut g, _) = fresh();
    g.set_output(1, PortSource::Zero);
    assert_eq!(wiring(g).topology.outputs[1], Vec::<PortSource>::new());

    // `Topology`'s writers.
    let mut t = tutti_graph::Topology::default();
    t.set_outputs([PortSource::Zero, PortSource::Node(OutputPort::new(a, 0))]);
    assert_eq!(t.outputs[0], Vec::<PortSource>::new());
    t.connect(at, Edge::Direct(PortSource::Zero));
    assert!(t.edges.is_empty(), "connecting silence stores nothing");

    // The two old ways agree: "no key" and "never wired" are one value.
    let mut wired_then_cleared = tutti_graph::Topology::default();
    wired_then_cleared.connect(at, Edge::Direct(PortSource::MainInput(0)));
    wired_then_cleared.disconnect(at, Edge::Direct(PortSource::MainInput(0)));
    assert_eq!(wired_then_cleared, tutti_graph::Topology::default());
    assert_eq!(
        hash(&wired_then_cleared),
        hash(&tutti_graph::Topology::default())
    );

    // `Editor::remove` leaves an emptied channel empty.
    let (mut ed, _exec) = Editor::new(prepare(64));
    ed.insert(NodeKey(1), "w", node(0, 1));
    ed.wire_declared_outputs([PortSource::Node(OutputPort::new(NodeKey(1), 0))])
        .unwrap();
    ed.remove(NodeKey(1));
    assert_eq!(ed.spec().topology.outputs, vec![Vec::<PortSource>::new()]);
}

/// `connect`, `connect_input`, `connect_output` and `feedback` **add** a source
/// to a port's set (the graph sums it) and `set_source`, `set_sources` and
/// `set_output` replace it; `disconnect_from` removes one. Figures: a `Width`
/// node's output `c` is `c + 1` plus its inputs, so `one` is 1, `two`'s port 1
/// is 2, and `sink` reads `1 + (sum of what feeds it)`.
#[test]
fn connect_adds_and_set_source_replaces() {
    // A fresh graph per check: `one` (key 0), `two` (1), `sink` (2).
    let scene = || {
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
        let one = g.add(node(0, 1));
        let two = g.add(node(0, 2));
        let sink = g.add(node(1, 1));
        (g, one, two, sink)
    };
    let (_, one, two, sink) = scene();
    let at = InputPort::new(sink, 0);
    let from_one = PortSource::Node(OutputPort::new(one, 0));
    let from_two = PortSource::Node(OutputPort::new(two, 1));
    // What `sink`'s input 0 holds after `ops` ran on a fresh graph.
    let held = |ops: &dyn Fn(&mut GraphBuilder)| {
        let (mut g, ..) = scene();
        ops(&mut g);
        wiring(g)
            .topology
            .edges
            .get(&at)
            .cloned()
            .unwrap_or_default()
    };
    let both = || {
        let mut sorted = vec![Edge::Direct(from_one), Edge::Direct(from_two)];
        sorted.sort();
        sorted
    };
    let wire_both = |g: &mut GraphBuilder| {
        g.connect(one, 0, sink, 0).connect(two, 1, sink, 0);
    };

    assert_eq!(
        held(&wire_both),
        both(),
        "two sources, in the graph's order"
    );
    assert_eq!(
        held(&|g| {
            wire_both(g);
            g.connect(two, 1, sink, 0);
        })
        .len(),
        2,
        "the same source twice is one"
    );

    let (mut g, ..) = scene();
    wire_both(&mut g);
    g.connect_output(sink, 0, 0);
    let mut r = g.renderer(prepare(64)).expect("builds");
    assert_eq!(r.render(8)[0], vec![4.0; 8], "1 + 1 + 2");

    let replaced = |g: &mut GraphBuilder| {
        wire_both(g);
        g.set_source(sink, 0, from_one);
    };
    assert_eq!(
        held(&replaced),
        vec![Edge::Direct(from_one)],
        "set_source replaces"
    );
    let set_both = |g: &mut GraphBuilder| {
        replaced(g);
        g.set_sources(sink, 0, &[from_two, from_one]);
    };
    assert_eq!(held(&set_both), both(), "set_sources: exactly these");
    let removed = |g: &mut GraphBuilder| {
        set_both(g);
        g.disconnect_from(sink, 0, from_two);
    };
    assert_eq!(
        held(&removed),
        vec![Edge::Direct(from_one)],
        "the others stay"
    );
    assert_eq!(
        held(&|g| {
            removed(g);
            g.disconnect_from(sink, 0, from_two);
        })
        .len(),
        1,
        "removing what is not there is a no-op"
    );

    let (mut g, ..) = scene();
    removed(&mut g);
    g.disconnect_from(sink, 0, from_two);
    // Output channels sum too: `sink` (2) and `one` (1) into channel 0.
    g.connect_output(sink, 0, 0).connect_output(one, 0, 0);
    let mut r = g.renderer(prepare(64)).expect("builds");
    assert_eq!(r.editor().spec().topology.outputs[0].len(), 2);
    assert_eq!(r.render(8)[0], vec![3.0; 8], "sink (1 + 1) plus one (1)");

    // A feedback edge is added beside the direct one, not instead of it.
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let one = g.add(node(0, 1));
    let sink = g.add(node(1, 1));
    g.connect(one, 0, sink, 0)
        .feedback(sink, 0, sink, 0, Samples::new(64));
    let at = InputPort::new(sink, 0);
    let spec = wiring(g);
    let edges = &spec.topology.edges[&at];
    assert_eq!(edges.len(), 2);
    assert!(matches!(edges[0], Edge::Direct(_)));
    assert!(matches!(edges[1], Edge::Feedback(_)));
}

/// **A built mix needs a pass.** A mono node with a stereo output names no
/// placement for its dry path, so a mix short of WET set on the builder is
/// refused at `build` with the reason, and WET builds.
#[test]
fn a_built_mix_short_of_wet_on_a_node_with_no_pass_is_refused() {
    let build = |mix: tutti_units::Mix| {
        let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::STEREO);
        let split = g.add(node(1, 2));
        g.connect_input(0, split, 0)
            .connect_output(split, 0, 0)
            .connect_output(split, 1, 1)
            .mix(split, mix);
        (split, g.build(prepare(64)).map(|_| ()))
    };
    let (split, refused) = build(tutti_units::Mix::new(0.5));
    assert_eq!(
        refused,
        Err(tutti_graph::CommitError::Unplaced {
            node: split,
            error: tutti_graph::PassError::Unplaced {
                inputs: 1,
                outputs: 2
            },
        })
    );
    assert_eq!(build(tutti_units::Mix::WET).1, Ok(()), "WET needs no pass");
}
