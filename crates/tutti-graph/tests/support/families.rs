//! The graph families the executor suites draw beside `generator.rs`'s random
//! graphs: crossfades, weighted and driven edge gains, modes, mixes, driven
//! mixes, bus edges and bus edges into the main output. Each draws from the
//! `Rng` it is handed, and each summarises what a case drew as [`Features`].

#![allow(
    dead_code,
    reason = "each test crate includes this module and uses part of it"
)]

use std::collections::BTreeMap;

use tutti_buffers::{ChannelLayout, ChannelTopology, Float};
use tutti_graph::testing::{self as common, Harness, Kind};
use tutti_graph::{
    Bus, BusOutputPort, BusRole, Buses, Edge, EdgeGain, EdgeId, EventOutputPort, GraphSpec,
    InputPort, NodeBus, NodeKey, OutputPort, PortSink, PortSource, Topology,
};
use tutti_units::{Gain, Samples};

use crate::generator::{Desc, Rng, mutate, random_kind, shape, spec_for};

impl Desc {
    /// Starts `harness`'s lanes and mix ports at the values this description
    /// holds, for the entries and nodes the spec has.
    pub(crate) fn start<F: Float>(&self, harness: &mut Harness<F>) {
        for id in self.spec.gains.keys() {
            if let Some(&base) = self.bases.get(id) {
                harness.start_edge_gain(*id, base);
            }
            if let Some(&range) = self.gain_ranges.get(id) {
                harness.start_edge_gain_range(*id, range);
            }
        }
        for (&at, &range) in &self.port_ranges {
            if self.spec.topology.nodes.contains_key(&at.node) {
                harness.start_param_range(at, range);
            }
        }
    }
}

// ---- crossfades -------------------------------------------------------------

/// Whether a unit of `b` may crossfade from one of `a`: the same shape in
/// everything but the tail (`Editor::replace`'s rule, spelled out so the
/// generator does not borrow the editor's).
pub(crate) fn fits(a: &Kind, b: &Kind) -> bool {
    let (a, b) = (shape(a), shape(b));
    (
        a.audio_in(),
        a.audio_out(),
        a.event_in,
        a.event_out,
        a.latency,
        a.in_place,
    ) == (
        b.audio_in(),
        b.audio_out(),
        b.event_in,
        b.event_out,
        b.latency,
        b.in_place,
    ) && a.event_resolution == b.event_resolution
        && a.event_capacity == b.event_capacity
        && a.event_input_capacity == b.event_input_capacity
}

fn random_fade(rng: &mut Rng) -> tutti_graph::Fade {
    let curve = if rng.chance(50) {
        tutti_graph::FadeLaw::EqualPower
    } else {
        tutti_graph::FadeLaw::EqualAmplitude
    };
    // Mostly shorter than the rounds between edits, often longer, so fades
    // end mid-block, run across edits, and queue behind each other.
    tutti_graph::Fade::new(Samples::new(1 + rng.below(250) as usize)).law(curve)
}

/// An edit that replaces one to three running nodes with a crossfade — the
/// same kind with fresh state, or another kind that fits — sometimes on top
/// of an ordinary `mutate`, which can remove, rewire or regenerate (without a
/// fade, latency and all) a node that is fading.
pub(crate) fn mutate_with_fades(
    desc: &Desc,
    rng: &mut Rng,
) -> (Desc, BTreeMap<NodeKey, tutti_graph::Fade>) {
    let mut d = if rng.chance(40) {
        mutate(desc, rng)
    } else {
        desc.clone()
    };
    let mut fades = BTreeMap::new();
    for _ in 0..1 + rng.below(3) {
        // Still running as `desc` had it: not removed or regenerated above.
        let running: Vec<NodeKey> = desc
            .order
            .iter()
            .copied()
            .filter(|k| {
                d.kinds.contains_key(k)
                    && d.spec.generation(*k) == desc.spec.generation(*k)
                    && !fades.contains_key(k)
                    // A unit whose declared latency was set by hand cannot
                    // fade: a new unit reports its own (`FadeShape`).
                    && desc.spec.topology.nodes[k].latency
                        == shape(&desc.kinds[k]).latency.samples()
            })
            .collect();
        let recent: Vec<NodeKey> = running
            .iter()
            .copied()
            .filter(|k| desc.recent.contains(k))
            .collect();
        let among = if !recent.is_empty() && rng.chance(40) {
            &recent
        } else {
            &running
        };
        let Some(key) = rng.pick(among) else { break };
        let old = desc.kinds[&key].clone();
        let mut kind = old.clone();
        if rng.chance(50) {
            let other = random_kind(rng);
            if fits(&old, &other) {
                kind = other;
            }
        }
        let generation = d.spec.generation(key) + 1;
        d.spec.generations.insert(key, generation);
        d.spec.topology.nodes.insert(key, spec_for(&kind));
        d.kinds.insert(key, kind);
        fades.insert(key, random_fade(rng));
    }
    // `set_latency` on kept nodes — idle, fading, or with a fade waiting
    // (the proptest's schedule decides which): a new declared latency with
    // no new generation, which cuts a fade there.
    if rng.chance(35) {
        for _ in 0..1 + rng.below(2) {
            let kept: Vec<NodeKey> = desc
                .order
                .iter()
                .copied()
                .filter(|k| {
                    d.kinds.contains_key(k)
                        && d.spec.generation(*k) == desc.spec.generation(*k)
                        && !fades.contains_key(k)
                })
                .collect();
            // Mostly a key crossfaded last round, where a fade runs or
            // waits; now and then any kept key.
            let recent: Vec<NodeKey> = kept
                .iter()
                .copied()
                .filter(|k| desc.recent.contains(k))
                .collect();
            let among = if !recent.is_empty() && rng.chance(70) {
                &recent
            } else {
                &kept
            };
            let Some(key) = rng.pick(among) else { break };
            let node = d.spec.topology.nodes.get_mut(&key).expect("kept");
            node.latency = Samples::new(rng.below(41) as usize);
        }
    }
    d.recent = fades.keys().copied().collect();
    (d, fades)
}

// ---- weighted edge gains ----------------------------------------------------

/// The weights the generator picks from: unity (the plain add), zero (never
/// skipped by value), negative (polarity), and fractions that do not round
/// exactly.
pub(crate) const WEIGHTS: [f64; 9] = [1.0, 0.5, -0.75, 0.0, 1.5, 0.3, 0.1, -1.0, 2.0];

/// Whether `id` names an edge of `t`.
pub(crate) fn has_edge(t: &Topology, id: EdgeId) -> bool {
    match id.sink {
        PortSink::Port(at) => t.edges.get(&at).is_some_and(|e| e.contains(&id.from)),
        PortSink::Output(ch) => match id.from {
            Edge::Direct(s) => t
                .outputs
                .get(usize::from(ch))
                .is_some_and(|v| v.contains(&s)),
            Edge::Feedback(_) => false,
        },
    }
}

/// Drops the entries of edges `mutate` removed or rewired.
pub(crate) fn prune_gains(desc: &mut Desc) {
    let t = &desc.spec.topology;
    desc.spec.gains.retain(|id, _| has_edge(t, *id));
}

/// Gives about a third of the edges without an entry one.
pub(crate) fn weigh(desc: &mut Desc, rng: &mut Rng) {
    let t = &desc.spec.topology;
    let mut ids: Vec<EdgeId> = t
        .edges
        .iter()
        .flat_map(|(&at, es)| {
            es.iter().map(move |&from| EdgeId {
                sink: PortSink::Port(at),
                from,
            })
        })
        .collect();
    for (ch, sources) in t.outputs.iter().enumerate() {
        ids.extend(sources.iter().map(|&s| EdgeId {
            sink: PortSink::Output(ch as u16),
            from: Edge::Direct(s),
        }));
    }
    for id in ids {
        if rng.chance(33) && !desc.spec.gains.contains_key(&id) {
            let w = WEIGHTS[rng.below(WEIGHTS.len() as u64) as usize];
            desc.spec.set_edge_gain(id, EdgeGain::default());
            desc.bases.insert(id, Gain::new(w));
        }
    }
}

// ---- driven edge gains ------------------------------------------------------

/// The first key of the lane nodes: `fresh_key` draws below 1000, and
/// `mutate` touches only the nodes of `order`, so a lane node is never
/// wired, removed or regenerated by it.
const LANE_KEYS: u64 = 1_000_000;

/// The latencies the lane nodes declare: none, a few frames, and past a
/// block (an early lane gets an event ring, a late one makes the sink wait).
const LANE_LATENCIES: [usize; 4] = [0, 16, 40, 150];

/// The ranges a driven lane is clamped to.
pub(crate) const LANE_RANGES: [(f64, f64); 4] =
    [(-8.0, 8.0), (-1.0, 1.0), (0.0, 0.5), (0.5, -0.25)];

/// Adds lane nodes (once) and, for about half the entries, a source among
/// them (or none: a source that goes, declicked) and a range.
pub(crate) fn drive(desc: &mut Desc, rng: &mut Rng) {
    if !desc.kinds.contains_key(&NodeKey(LANE_KEYS)) {
        for (i, &latency) in LANE_LATENCIES.iter().enumerate() {
            let key = NodeKey(LANE_KEYS + i as u64);
            let kind = Kind::GainDriver {
                period: 8 + 9 * i as u64,
                latency,
            };
            desc.spec.topology.nodes.insert(key, spec_for(&kind));
            desc.kinds.insert(key, kind);
            desc.used.insert(key);
        }
    }
    let ids: Vec<EdgeId> = desc.spec.gains.keys().copied().collect();
    for id in ids {
        if !rng.chance(50) {
            continue;
        }
        let g = desc.spec.gains.get_mut(&id).expect("listed");
        g.source = rng.chance(80).then(|| {
            EventOutputPort::new(
                NodeKey(LANE_KEYS + rng.below(LANE_LATENCIES.len() as u64)),
                0,
            )
        });
        let (min, max) = LANE_RANGES[rng.below(LANE_RANGES.len() as u64) as usize];
        desc.gain_ranges
            .insert(id, tutti_graph::ParamRange::of(min, max));
    }
}

// ---- modes ------------------------------------------------------------------

/// A node and a mode for it. A third of the draws are `Active`, so a toggle
/// back is as common as a toggle away.
pub(crate) fn pick_mode(desc: &Desc, rng: &mut Rng) -> Option<(NodeKey, tutti_graph::NodeMode)> {
    use tutti_graph::NodeMode;
    let key = rng.pick(&desc.order)?;
    let mode = match rng.below(3) {
        0 => NodeMode::Active,
        1 => NodeMode::Bypassed,
        _ => NodeMode::Muted,
    };
    Some((key, mode))
}

// ---- mixes ------------------------------------------------------------------

/// A node and a mix for it. Half the draws are an end (steady WET or DRY, the
/// cases that copy or leave), the rest are between.
pub(crate) fn pick_mix(desc: &Desc, rng: &mut Rng) -> Option<(NodeKey, tutti_units::Mix)> {
    let key = rng.pick(&desc.order)?;
    let mix = match rng.below(8) {
        0 | 1 => 1.0,
        2 | 3 => 0.0,
        4 => 0.5,
        5 => 0.25,
        6 => 0.875,
        _ => rng.below(1000) as f64 / 999.0,
    };
    Some((key, tutti_units::Mix::new(mix)))
}

// ---- driven mixes -----------------------------------------------------------

/// The keys of the mix lane nodes (`Kind::MixDriver`), clear of every other
/// property's.
const MIX_LANE_KEYS: u64 = 2_000_000;

/// The address of `key`'s mix.
pub(crate) fn mix_port(key: NodeKey) -> tutti_graph::ParamIn {
    tutti_graph::ParamIn {
        node: key,
        param: tutti_graph::NodeMix::ADDR,
    }
}

/// Adds the mix lane nodes (once) and, for about a third of the nodes, a
/// source among them (or none: a source that goes, declicked) and a range.
pub(crate) fn drive_mixes(desc: &mut Desc, rng: &mut Rng) {
    if !desc.kinds.contains_key(&NodeKey(MIX_LANE_KEYS)) {
        for (i, &latency) in LANE_LATENCIES.iter().enumerate() {
            let key = NodeKey(MIX_LANE_KEYS + i as u64);
            let kind = Kind::MixDriver {
                period: 8 + 9 * i as u64,
                latency,
            };
            desc.spec.topology.nodes.insert(key, spec_for(&kind));
            desc.kinds.insert(key, kind);
            desc.used.insert(key);
        }
    }
    for key in desc.order.clone() {
        if !rng.chance(33) {
            continue;
        }
        let at = mix_port(key);
        if rng.chance(80) {
            let from = EventOutputPort::new(
                NodeKey(MIX_LANE_KEYS + rng.below(LANE_LATENCIES.len() as u64)),
                0,
            );
            desc.spec.connect_param(at, from);
        } else {
            desc.spec.params.remove(&at);
        }
        if rng.chance(50) {
            let (min, max) = LANE_RANGES[rng.below(LANE_RANGES.len() as u64) as usize];
            desc.port_ranges
                .insert(at, tutti_graph::ParamRange::of(min, max));
        }
    }
}

/// Drops the lanes of nodes `mutate` removed.
pub(crate) fn prune_mix_lanes(desc: &mut Desc) {
    let nodes = &desc.spec.topology.nodes;
    desc.spec
        .params
        .retain(|at, _| nodes.contains_key(&at.node));
}

// ---- bus edges --------------------------------------------------------------

/// A graph of nodes declaring buses, wired by bus edges (and some plain
/// ones). Every bus edge kept lowers, so the value a test renders is the
/// lowered spec's.
pub(crate) struct BusGraph {
    pub spec: GraphSpec,
    pub kinds: BTreeMap<NodeKey, Kind>,
}

fn bus_of(width: usize, named: bool) -> Bus {
    let layout = ChannelLayout::from_count(width as u16);
    let topology = if named {
        ChannelTopology::default_for(layout).expect("fits a bus")
    } else {
        ChannelTopology::unassigned(layout).expect("fits a bus")
    };
    Bus::new(topology, BusRole::Main)
}

pub(crate) fn bus_graph(seed: u64) -> BusGraph {
    bus_graph_from(&mut Rng::new(seed))
}

fn bus_graph_from(rng: &mut Rng) -> BusGraph {
    let main_inputs = 1 + rng.below(6) as usize;
    let mut spec = GraphSpec::default();
    spec.topology.inputs = ChannelLayout::from_count(main_inputs as u16);
    let mut kinds: BTreeMap<NodeKey, Kind> = BTreeMap::new();
    let mut widths: Vec<(NodeKey, usize)> = Vec::new();
    let nodes = 3 + rng.below(6) as usize;
    for j in 1..=nodes as u64 {
        let node = NodeKey(j);
        let width = rng.pick(&[1usize, 1, 2, 2, 6]).expect("a width");
        let named = rng.chance(75);
        let source = j == 1 || rng.chance(15);
        let inner = if width == 1 && !source && rng.chance(30) {
            Kind::Lag {
                latency: 1 + rng.below(20) as usize,
            }
        } else {
            Kind::Gain {
                gain: rng.pick(&[0.5f32, 0.75, 1.0, 0.3]).expect("a gain"),
                width,
            }
        };
        let kind = Kind::Bused {
            inner: Box::new(inner),
            buses_in: Box::new(Buses::main(bus_of(width, named).topology)),
            buses_out: Box::new(Buses::main(bus_of(width, named).topology)),
        };
        spec.topology.nodes.insert(node, spec_for(&kind));
        kinds.insert(node, kind);
        if source {
            for c in 0..width as u16 {
                spec.topology.connect(
                    InputPort::new(node, c),
                    Edge::Direct(PortSource::MainInput(c % main_inputs as u16)),
                );
            }
        } else {
            // One or two bus edges from earlier nodes, kept when they lower.
            for _ in 0..1 + rng.below(2) {
                let Some((from, _)) = rng.pick(&widths) else {
                    continue;
                };
                let mut trial = spec.clone();
                trial.connect_bus(
                    BusOutputPort::Node(NodeBus::new(from, 0)),
                    NodeBus::new(node, 0),
                );
                if trial.lowered(&common::shapes_of(&kinds)).is_ok() {
                    spec = trial;
                }
            }
            // And now and then an ordinary edge into the same sink.
            if rng.chance(30)
                && let Some((from, w)) = rng.pick(&widths)
            {
                let mut trial = spec.clone();
                trial.topology.connect(
                    InputPort::new(node, rng.below(width as u64) as u16),
                    Edge::Direct(PortSource::Node(OutputPort::new(
                        from,
                        rng.below(w as u64) as u16,
                    ))),
                );
                if trial.lowered(&common::shapes_of(&kinds)).is_ok() {
                    spec = trial;
                }
            }
        }
        widths.push((node, width));
    }
    let &(last, width) = widths.last().expect("a node");
    spec.topology.set_outputs(
        (0..width as u16)
            .map(|port| PortSource::Node(OutputPort::new(last, port)))
            .collect::<Vec<_>>(),
    );
    BusGraph { spec, kinds }
}

/// `bus_graph(seed)` with a declared main output and bus edges into it. The
/// main output is 1, 2 or 6 channels, named or unassigned; its ordinary output sources past its width
/// are dropped, and a main output edge is kept when the spec still lowers (an
/// unfoldable pair, or a lane on an ordinary output source, is not).
pub(crate) fn main_output_bus_graph(seed: u64) -> BusGraph {
    let mut rng = Rng::new(seed);
    let mut graph = bus_graph_from(&mut rng);
    let width = rng.pick(&[1usize, 2, 2, 6]).expect("a width");
    let main_output = bus_of(width, rng.chance(75)).topology;
    graph.spec.main_output = Some(main_output);
    graph.spec.topology.outputs.truncate(width);
    let nodes: Vec<NodeKey> = graph.kinds.keys().copied().collect();
    for _ in 0..1 + rng.below(3) {
        let Some(node) = rng.pick(&nodes) else {
            continue;
        };
        let mut trial = graph.spec.clone();
        trial.connect_bus_to_main_output(BusOutputPort::Node(NodeBus::new(node, 0)));
        if trial.lowered(&common::shapes_of(&graph.kinds)).is_ok() {
            graph.spec = trial;
        }
    }
    graph
}

// ---- what a case drew -------------------------------------------------------

/// What generated cases hold, by name: how many cases drew each feature.
#[derive(Debug, Default)]
pub(crate) struct Features(pub BTreeMap<&'static str, usize>);

impl Features {
    /// Counts `name` once when `held`; an unheld feature is still listed.
    pub(crate) fn note(&mut self, name: &'static str, held: bool) {
        *self.0.entry(name).or_default() += usize::from(held);
    }

    /// Adds `other`'s counts to these.
    pub(crate) fn add(&mut self, other: Self) {
        for (name, n) in other.0 {
            *self.0.entry(name).or_default() += n;
        }
    }
}

impl Desc {
    /// What this graph holds: wiring, node kinds, weights, lanes and ranges.
    pub(crate) fn features(&self) -> Features {
        let mut f = Features::default();
        let t = &self.spec.topology;
        let shapes: Vec<tutti_graph::Shape> = self.kinds.values().map(shape).collect();
        let edges = || t.edges.values().flatten();
        f.note(
            "audio feedback",
            edges().any(|e| matches!(e, Edge::Feedback(_))),
        );
        f.note(
            "event feedback",
            self.spec
                .events
                .values()
                .flatten()
                .any(|e| matches!(e, tutti_graph::EventEdge::Feedback { .. })),
        );
        f.note(
            "audio fan-in at a port",
            t.edges.values().any(|e| e.len() > 1),
        );
        f.note(
            "audio fan-in past four",
            t.edges.values().any(|e| e.len() > 4),
        );
        f.note("summed output", t.outputs.iter().any(|s| s.len() > 1));
        f.note(
            "several inputs and outputs",
            t.inputs.count() > 1 && t.outputs.len() > 2,
        );
        f.note(
            "event fan-in",
            self.spec.events.values().any(|e| e.len() > 1),
        );
        f.note(
            "event fan-in past three",
            self.spec.events.values().any(|e| e.len() > 3),
        );
        f.note(
            "latent nodes",
            shapes.iter().any(|s| s.latency.samples().get() > 0),
        );
        f.note(
            "event nodes",
            shapes.iter().any(|s| s.event_in + s.event_out > 0),
        );
        f.note(
            "source nodes",
            shapes.iter().any(|s| s.audio_in().count() == 0),
        );
        f.note(
            "wide nodes",
            shapes.iter().any(|s| s.audio_out().count() > 1),
        );
        f.note("in-place nodes", shapes.iter().any(|s| s.in_place));
        f.note(
            "bypass nodes",
            self.kinds
                .values()
                .any(|k| matches!(k, Kind::Thru { .. } | Kind::ThruInPlace { .. })),
        );
        f.note(
            "nodes declaring their own bypass",
            self.kinds
                .values()
                .any(|k| matches!(k, Kind::OwnBypass { .. })),
        );
        f.note(
            "writers past their declared capacity, read",
            self.kinds.iter().any(|(k, kind)| {
                matches!(kind, Kind::Burst { burst, cap, .. } if burst > cap)
                    && self
                        .spec
                        .events
                        .values()
                        .flatten()
                        .any(|e| e.from().node() == *k)
            }),
        );
        f.note(
            "bounded event inputs",
            self.kinds
                .values()
                .any(|k| matches!(k, Kind::BoundedConsumer { .. })),
        );
        f.note("weighted edges", !self.spec.gains.is_empty());
        f.note(
            "negative weights",
            self.bases.values().any(|b| b.get() < 0.0),
        );
        f.note("zero weights", self.bases.values().any(|b| b.get() == 0.0));
        f.note(
            "driven edge gains",
            self.spec.gains.values().any(|g| g.source.is_some()),
        );
        f.note("clamped lanes", !self.gain_ranges.is_empty());
        f.note("driven mixes", !self.spec.params.is_empty());
        f.note("clamped mixes", !self.port_ranges.is_empty());
        f
    }
}

/// What an edit that crossfades `fades` changed in `before`: kinds swapped,
/// event and in-place nodes faded, a declared latency set.
pub(crate) fn fade_features(
    before: &Desc,
    after: &Desc,
    fades: &BTreeMap<NodeKey, tutti_graph::Fade>,
) -> Features {
    let mut f = Features::default();
    let faded = || fades.keys().map(|k| (k, shape(&after.kinds[k])));
    f.note(
        "kind swaps",
        fades.keys().any(|k| after.kinds[k] != before.kinds[k]),
    );
    f.note(
        "faded event nodes",
        faded().any(|(_, s)| s.event_in + s.event_out > 0),
    );
    f.note("faded in-place nodes", faded().any(|(_, s)| s.in_place));
    f.note(
        "set_latency on kept nodes",
        before.spec.topology.nodes.iter().any(|(k, was)| {
            after.spec.topology.nodes.get(k).is_some_and(|now| {
                now.latency != was.latency
                    && after.spec.generation(*k) == before.spec.generation(*k)
            })
        }),
    );
    f
}

/// Gives about a third of the nodes their own bypass.
pub(crate) fn own_bypass(desc: &mut Desc, rng: &mut Rng) {
    for key in desc.order.clone() {
        if rng.chance(34) {
            let inner = desc.kinds.remove(&key).expect("a kind");
            desc.kinds.insert(
                key,
                Kind::OwnBypass {
                    inner: Box::new(inner),
                },
            );
        }
    }
}

impl BusGraph {
    /// What the graph's bus edges lower to: weighted folds, unity copies, and
    /// lanes beside ordinary edges, between nodes and into the main output.
    pub(crate) fn features(&self) -> Features {
        let lowering = self
            .spec
            .lowered(&common::shapes_of(&self.kinds))
            .expect("every kept bus edge lowers");
        let mut f = Features::default();
        let outputs = || {
            lowering
                .lanes
                .iter()
                .filter(|(id, _)| matches!(id.sink, PortSink::Output(_)))
        };
        f.note("bus edges", !self.spec.bus_edges.is_empty());
        f.note("weighted bus lanes", !lowering.weights().is_empty());
        f.note(
            "unity bus lanes",
            lowering
                .lanes
                .iter()
                .any(|(id, w)| matches!(id.sink, PortSink::Port(_)) && w.get() == 1.0),
        );
        f.note(
            "bus lanes beside ordinary edges",
            lowering.lanes.keys().any(|id| match id.sink {
                PortSink::Port(at) => self
                    .spec
                    .topology
                    .edges
                    .get(&at)
                    .is_some_and(|edges| !edges.is_empty()),
                PortSink::Output(_) => false,
            }),
        );
        f.note(
            "main output edges",
            !self.spec.main_output_sources.is_empty(),
        );
        f.note(
            "weighted main output lanes",
            outputs().any(|(_, w)| w.get() != 1.0),
        );
        f.note(
            "unity main output lanes",
            outputs().any(|(_, w)| w.get() == 1.0),
        );
        f.note(
            "main output lanes beside ordinary sources",
            outputs().any(|(id, _)| match id.sink {
                PortSink::Output(c) => self
                    .spec
                    .topology
                    .outputs
                    .get(usize::from(c))
                    .is_some_and(|o| !o.is_empty()),
                PortSink::Port(_) => false,
            }),
        );
        f
    }
}
