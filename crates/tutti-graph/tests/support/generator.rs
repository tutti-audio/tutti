//! The random graph generator the render suites draw from: a seeded
//! description of kinds, wiring and edits, a pure function of the seed so a
//! failing case replays exactly.

#![allow(
    dead_code,
    reason = "each test crate includes this module and uses part of it"
)]

use std::collections::{BTreeMap, BTreeSet};

use tutti_buffers::ChannelLayout;
use tutti_graph::Node;
use tutti_graph::testing as common;
use tutti_graph::testing::Kind;
use tutti_graph::{
    Edge, EdgeId, EventEdge, EventInputPort, EventOutputPort, FeedbackFrom, GraphSpec, InputPort,
    NodeKey, NodeSpec, OutputPort, PortSource, Topology,
};
use tutti_rt::SplitMix64;
use tutti_units::{Gain, Samples};

pub(crate) const MAX_BLOCK: usize = 128;

/// A seeded stream: the graph is a pure function of the seed, so a failing
/// proptest case replays exactly.
#[derive(Clone)]
pub(crate) struct Rng(SplitMix64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(SplitMix64::new(seed))
    }
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.0.next_u64() % n.max(1)
    }
    pub(crate) fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
    pub(crate) fn pick<T: Copy>(&mut self, xs: &[T]) -> Option<T> {
        (!xs.is_empty()).then(|| xs[self.below(xs.len() as u64) as usize])
    }
}

#[derive(Clone)]
pub(crate) struct Desc {
    pub kinds: BTreeMap<NodeKey, Kind>,
    /// Creation order. Direct edges only ever run earlier → later in it, so
    /// every generated graph is acyclic without feedback.
    pub order: Vec<NodeKey>,
    pub spec: GraphSpec,
    /// Every key ever used. A removed key is never reused: a new unit at an
    /// old key must come with a new generation, or it *is* the old node as far
    /// as the value can tell (`Editor` enforces this with a counter).
    pub used: BTreeSet<NodeKey>,
    /// Keys `mutate_with_fades` crossfaded in its last round: where fades
    /// are likeliest to be running or waiting, so the next round aims
    /// replaces and `set_latency` there often enough to queue and cut them.
    pub recent: Vec<NodeKey>,
    /// The stream the graph was drawn from, positioned after it: a case's
    /// further draws (edits, lanes, modes) continue it.
    pub rng: Rng,
    /// The base of each gain lane: the editor's, not the spec's.
    pub bases: BTreeMap<EdgeId, Gain>,
    /// The range of each gain lane.
    pub gain_ranges: BTreeMap<EdgeId, tutti_graph::ParamRange>,
    /// The range of each node's mix port.
    pub port_ranges: BTreeMap<tutti_graph::ParamIn, tutti_graph::ParamRange>,
}

pub(crate) fn random_kind(rng: &mut Rng) -> Kind {
    // Latency-bearing nodes and multi-input sinks are weighted up: PDC only
    // happens where paths of different latency meet.
    match rng.below(17) {
        0 => Kind::Const {
            value: (rng.below(9) as f32 - 4.0) * 0.25 + 0.125,
            width: 1 + rng.below(2) as usize,
        },
        1 => Kind::Gain {
            gain: 0.25 + rng.below(8) as f32 * 0.25,
            width: 1 + rng.below(3) as usize,
        },
        2 | 10 | 11 => Kind::Sum {
            inputs: 1 + rng.below(3) as usize,
        },
        3 => Kind::Smooth,
        4 | 12 | 13 => Kind::Lag {
            latency: rng.below(24) as usize,
        },
        5 | 15 => Kind::Emitter {
            period: 3 + rng.below(40),
            phase: rng.below(10),
        },
        16 => burst_kind(rng),
        6 => consumer_kind(rng),
        7 | 14 => Kind::EventLag {
            latency: rng.below(30) as usize,
        },
        8 => Kind::EnvProbe,
        9 if rng.chance(50) => Kind::ThruInPlace {
            width: 1 + rng.below(2) as usize,
        },
        _ => Kind::Thru {
            width: 1 + rng.below(2) as usize,
        },
    }
}

/// An event sink, half the time declaring an input bound of a few events a
/// block (`Shape::event_input_capacity`), so its merge drops what is past it.
pub(crate) fn consumer_kind(rng: &mut Rng) -> Kind {
    let inputs = 1 + rng.below(2) as u16;
    if rng.chance(50) {
        Kind::BoundedConsumer {
            inputs,
            cap: 1 + rng.below(4) as u32,
        }
    } else {
        Kind::Consumer { inputs }
    }
}

/// An emitter declaring its event capacity, often below its burst, so the
/// writer refuses events every burst. Its period is at least `MAX_BLOCK`, so
/// it keeps its declared rate (see `Kind::Burst`).
pub(crate) fn burst_kind(rng: &mut Rng) -> Kind {
    Kind::Burst {
        period: MAX_BLOCK as u64 + rng.below(200),
        phase: rng.below(MAX_BLOCK as u64),
        burst: 1 + rng.below(6) as u32,
        cap: 1 + rng.below(4) as u32,
    }
}

pub(crate) fn shape(kind: &Kind) -> tutti_graph::Shape {
    common::TestNode::new(kind.clone()).shape()
}

pub(crate) fn audio_outs(desc: &Desc, among: &[NodeKey]) -> Vec<OutputPort> {
    among
        .iter()
        .flat_map(|&k| {
            (0..shape(&desc.kinds[&k]).audio_out().count())
                .map(move |port| OutputPort::new(k, port))
        })
        .collect()
}

pub(crate) fn event_outs(desc: &Desc, among: &[NodeKey]) -> Vec<EventOutputPort> {
    among
        .iter()
        .flat_map(|&k| {
            (0..shape(&desc.kinds[&k]).event_out).map(move |port| EventOutputPort::new(k, port))
        })
        .collect()
}

/// (Re)wire every input of the node at creation index `j`.
pub(crate) fn wire_node(desc: &mut Desc, j: usize, rng: &mut Rng) {
    let key = desc.order[j];
    let s = shape(&desc.kinds[&key]);
    let earlier: Vec<NodeKey> = desc.order[..j].to_vec();
    let all = desc.order.clone();
    let direct = audio_outs(desc, &earlier);
    let any = audio_outs(desc, &all);
    for port in 0..s.audio_in().count() {
        let at = InputPort::new(key, port);
        let roll = rng.below(100);
        let edge = if roll < 64 {
            rng.pick(&direct).map(|p| Edge::Direct(PortSource::Node(p)))
        } else if roll < 76 {
            let width = u64::from(desc.spec.topology.inputs.count());
            Some(Edge::Direct(PortSource::MainInput(rng.below(width) as u16)))
        } else if roll < 80 {
            // Silence is no edge.
            None
        } else if roll < 90 {
            // Feedback delays of at least a block, sometimes longer.
            let delay = Samples::new(MAX_BLOCK + 17 * rng.below(3) as usize);
            rng.pick(&any)
                .map(|from| Edge::Feedback(FeedbackFrom::new(from, delay)))
        } else {
            None
        };
        // Audio fan-in: a port now and then
        // takes several distinct sources (direct, main input or feedback), which
        // the compiled op sums in `Edge` order, silent addends skipped, and
        // a fold starts from `+0.0`. A hub is wider than `MAX_PORTS`
        // when enough earlier outputs exist (a chain of sum ops).
        let mut set: Vec<Edge> = edge.into_iter().collect();
        if rng.chance(35) {
            let hub = rng.chance(15);
            let tries = if hub {
                direct.len() as u64 + 3
            } else {
                1 + rng.below(3)
            };
            for _ in 0..tries {
                let extra = match rng.below(10) {
                    0 | 1 => {
                        let width = u64::from(desc.spec.topology.inputs.count());
                        Some(Edge::Direct(PortSource::MainInput(rng.below(width) as u16)))
                    }
                    2 => {
                        let delay = Samples::new(MAX_BLOCK + 17 * rng.below(3) as usize);
                        rng.pick(&any)
                            .map(|from| Edge::Feedback(FeedbackFrom::new(from, delay)))
                    }
                    _ => rng.pick(&direct).map(|p| Edge::Direct(PortSource::Node(p))),
                };
                if let Some(e) = extra
                    && !set.contains(&e)
                {
                    set.push(e);
                }
            }
        }
        set.sort();
        if set.is_empty() {
            desc.spec.topology.edges.remove(&at);
        } else {
            desc.spec.topology.edges.insert(at, set);
        }
    }
    let direct_ev = event_outs(desc, &earlier);
    let any_ev = event_outs(desc, &all);
    for port in 0..s.event_in {
        let at = EventInputPort::new(key, port);
        let mut seen = BTreeSet::new();
        let mut sources = Vec::new();
        // Up to six sources, so fan-in wider than the common two or three is
        // exercised — and now and then a hub fed by every earlier event port
        // (a > 64-wide fan-in has its own test in `events.rs`).
        let hub = rng.chance(30);
        let tries = if hub {
            direct_ev.len() as u64 * 2
        } else {
            1 + rng.below(6)
        };
        for _ in 0..tries {
            let e = if hub || rng.chance(85) {
                rng.pick(&direct_ev).map(EventEdge::Direct)
            } else {
                let delay = Samples::new(MAX_BLOCK + 17 * rng.below(3) as usize);
                rng.pick(&any_ev).map(|f| EventEdge::feedback(f, delay))
            };
            if let Some(e) = e
                && seen.insert(e.from())
            {
                sources.push(e);
            }
        }
        desc.spec.events.insert(at, sources);
    }
}

pub(crate) fn spec_for(kind: &Kind) -> NodeSpec {
    let s = shape(kind);
    NodeSpec::new("test", s.audio_in(), s.audio_out())
        .latency(s.latency.samples())
        .tail(s.tail)
}

pub(crate) fn wire_outputs(desc: &mut Desc, rng: &mut Rng) {
    let all = desc.order.clone();
    let outs = audio_outs(desc, &all);
    let width = u64::from(desc.spec.topology.inputs.count());
    desc.spec
        .topology
        .set_outputs((0..1 + rng.below(4)).map(|_| match rng.below(10) {
            0 => PortSource::Zero,
            1 => PortSource::MainInput(rng.below(width) as u16),
            _ => rng.pick(&outs).map_or(PortSource::Zero, PortSource::Node),
        }));
    common::declare_spec_outputs(&mut desc.spec);
    // A channel now and then sums several sources, aligned to the output time
    // each on its own.
    for ch in 0..desc.spec.topology.outputs.len() {
        if desc.spec.topology.outputs[ch].is_empty() || !rng.chance(35) {
            continue;
        }
        for _ in 0..1 + rng.below(3) {
            let extra = match rng.below(8) {
                0 => PortSource::MainInput(rng.below(width) as u16),
                _ => rng.pick(&outs).map_or(PortSource::Zero, PortSource::Node),
            };
            let set = &mut desc.spec.topology.outputs[ch];
            if extra != PortSource::Zero && !set.contains(&extra) {
                set.push(extra);
            }
        }
        desc.spec.topology.outputs[ch].sort();
    }
}

pub(crate) fn fresh_key(desc: &mut Desc, rng: &mut Rng) -> NodeKey {
    loop {
        let k = NodeKey(rng.below(1000));
        if desc.used.insert(k) {
            return k;
        }
    }
}

pub(crate) fn random_graph(seed: u64) -> Desc {
    let mut rng = Rng::new(seed);
    let mut desc = Desc {
        kinds: BTreeMap::new(),
        order: Vec::new(),
        used: BTreeSet::new(),
        recent: Vec::new(),
        rng: Rng::new(0),
        bases: BTreeMap::new(),
        gain_ranges: BTreeMap::new(),
        port_ranges: BTreeMap::new(),
        spec: GraphSpec::new(Topology {
            inputs: ChannelLayout::from_count(1 + u16::try_from(rng.below(3)).expect("below 3")),
            ..Topology::default()
        }),
    };
    let n = 2 + rng.below(11) as usize;
    // Some graphs are mostly event nodes, so that wide event fan-in (which
    // needs several earlier event outputs) turns up regularly.
    let event_heavy = rng.chance(25);
    for _ in 0..n {
        let key = fresh_key(&mut desc, &mut rng);
        let kind = if event_heavy && rng.chance(80) {
            match rng.below(5) {
                4 => burst_kind(&mut rng),
                0 | 1 => Kind::Emitter {
                    period: 3 + rng.below(40),
                    phase: rng.below(10),
                },
                2 => Kind::EventLag {
                    latency: rng.below(30) as usize,
                },
                _ => consumer_kind(&mut rng),
            }
        } else {
            random_kind(&mut rng)
        };
        desc.spec.topology.nodes.insert(key, spec_for(&kind));
        desc.kinds.insert(key, kind);
        desc.order.push(key);
    }
    for j in 0..n {
        wire_node(&mut desc, j, &mut rng);
    }
    wire_outputs(&mut desc, &mut rng);
    desc.rng = rng;
    desc
}

/// An edit of `desc`: remove a node, add one, regenerate one, rewire some
/// ports — keeping every other key, so their state must carry.
pub(crate) fn mutate(desc: &Desc, rng: &mut Rng) -> Desc {
    let mut d = desc.clone();
    if d.order.len() > 1 && rng.chance(35) {
        let victim = d.order.remove(rng.below(d.order.len() as u64) as usize);
        d.kinds.remove(&victim);
        d.spec.generations.remove(&victim);
        let t = &mut d.spec.topology;
        t.nodes.remove(&victim);
        t.edges.retain(|at, es| {
            es.retain(|e| {
                let from = match *e {
                    Edge::Direct(PortSource::Node(p))
                    | Edge::Feedback(FeedbackFrom { from: p, .. }) => Some(p.node()),
                    Edge::Direct(_) => None,
                };
                from != Some(victim)
            });
            at.node() != victim && !es.is_empty()
        });
        for s in &mut t.outputs {
            s.retain(|src| !matches!(src, PortSource::Node(p) if p.node() == victim));
        }
        d.spec.events.retain(|at, _| at.node() != victim);
        for v in d.spec.events.values_mut() {
            v.retain(|e| e.from().node() != victim);
        }
    }
    if rng.chance(40) {
        let key = fresh_key(&mut d, rng);
        let kind = random_kind(rng);
        d.spec.topology.nodes.insert(key, spec_for(&kind));
        d.kinds.insert(key, kind);
        d.order.push(key);
        let j = d.order.len() - 1;
        wire_node(&mut d, j, rng);
    }
    if rng.chance(35) {
        // Regenerate one node: a new unit (fresh state) at the same key, and
        // maybe a different kind — which can change its latency, and so every
        // PDC ring downstream of it.
        let j = rng.below(d.order.len() as u64) as usize;
        let key = d.order[j];
        let generation = d.spec.generation(key) + 1;
        d.spec.generations.insert(key, generation);
        if rng.chance(50) {
            let old = shape(&d.kinds[&key]);
            let kind = random_kind(rng);
            let new = shape(&kind);
            // Keep the port shape so edges stay valid; only swap when it
            // matches (a latency change, typically).
            if (old.audio_in(), old.audio_out(), old.event_in, old.event_out)
                == (new.audio_in(), new.audio_out(), new.event_in, new.event_out)
            {
                d.spec.topology.nodes.insert(key, spec_for(&kind));
                d.kinds.insert(key, kind);
            }
        }
    }
    for _ in 0..rng.below(3) {
        let j = rng.below(d.order.len() as u64) as usize;
        wire_node(&mut d, j, rng);
    }
    if rng.chance(30) {
        wire_outputs(&mut d, rng);
    }
    d
}

/// Block lengths for a run: fixed 1, 7, 64 or 100, or a different length
/// every block.
pub(crate) fn schedule(which: usize, seed: u64, frames: usize) -> Vec<usize> {
    let mut rng = Rng::new(seed ^ 0xB10C);
    let mut out = Vec::new();
    let mut total = 0;
    while total < frames {
        let n = match which {
            0 => 1,
            1 => 7,
            2 => 64,
            3 => 100,
            _ => 1 + rng.below(MAX_BLOCK as u64) as usize,
        };
        out.push(n);
        total += n;
    }
    out
}

/// Declared costs for `desc`'s nodes, a pure function of the seed: about
/// two in three declare `Cost::Light`, a few `Cost::Moderate`, the rest
/// nothing (`Cost::Heavy`) — so cheap siblings pack beside heavy tasks that keep the plan parallel.
pub(crate) fn random_costs(desc: &Desc, seed: u64) -> BTreeMap<NodeKey, tutti_graph::Cost> {
    let mut rng = Rng::new(seed ^ 0xC057);
    desc.kinds
        .keys()
        .filter_map(|&k| match rng.below(6) {
            0..=3 => Some((k, tutti_graph::Cost::Light)),
            4 => Some((k, tutti_graph::Cost::Moderate)),
            _ => None,
        })
        .collect()
}
