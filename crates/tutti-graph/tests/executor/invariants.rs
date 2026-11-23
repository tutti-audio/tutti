//! Invariants of the executor over the random graphs of
//! `tests/support/generator.rs`, which mix every test node: generators, gains,
//! sums, delays that put PDC rings on the shorter paths, bypass nodes, event
//! emitters, event delays and order-sensitive consumers with fan-in, and
//! feedback edges of both kinds.
//!
//! - **A render is the one-participant render.** Whatever the graph, the block
//!   schedule (1 frame, odd sizes, ragged), the pool width, the commands
//!   scheduled by frame and by beat, the transport changing inside blocks and a
//!   recompile halfway: every output sample, and the late, unrouted and dropped
//!   counts, are those of an executor with no pool.
//! - **A tap reads what the output holds**, sample for sample.
//! - **The graph reports its declared figures.** Its latency, the compensation
//!   of each output channel and its tail are the nodes' declared ones along the
//!   longest paths.
//!
//! The closed forms of sums, gains and delay alignment on built graphs are
//! `tests/render/lti.rs`'s; the figures of single mechanisms are the area's
//! hand-written tests'.

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use tutti_buffers::InterleavedMut;
use tutti_graph::testing::{self as common, Harness, bits, input_signal};
use tutti_graph::{
    Edge, EventEdge, EventInputPort, NodeKey, PortSource, ScheduleError, TapId, TapOptions,
    TapSpec, TransportChanges, compile, graph_tail,
};
use tutti_units::{At, Beat, Frame, SampleRate, Samples, Tail};

use crate::generator::{Desc, MAX_BLOCK, Rng, mutate, random_costs, random_graph, schedule, shape};
use crate::script::Script;

/// What a run produced: each block's output, bit for bit, and the executor's
/// counts.
#[derive(Debug, Default, PartialEq)]
struct Trace {
    blocks: Vec<Vec<Vec<u32>>>,
    late: u64,
    unrouted: u64,
    dropped: u64,
}

impl Trace {
    fn push(&mut self, block: &[Vec<f32>]) {
        self.blocks.push(bits(block));
    }

    fn finish(mut self, harness: &Harness) -> Self {
        self.late = harness.exec.late_command_count();
        self.unrouted = harness.exec.unrouted_command_count();
        self.dropped = harness.exec.dropped_event_count();
        self
    }
}

/// The pool widths a case runs at besides one participant: the seed's pick from
/// `common::WORKERS`.
fn parallel(seed: u64) -> usize {
    common::workers_for(seed)
}

fn event_ports(desc: &Desc) -> Vec<EventInputPort> {
    desc.kinds
        .keys()
        .flat_map(|&k| {
            (0..shape(&desc.kinds[&k]).event_in).map(move |port| EventInputPort::new(k, port))
        })
        .collect()
}

/// The graph rendered through block schedule `which`, its nodes declaring
/// costs when `costed` (so cheap siblings pack into one task).
fn plain(seed: u64, which: usize, workers: usize, costed: bool) -> Trace {
    let desc = random_graph(seed);
    let mut harness = Harness::with_workers(MAX_BLOCK, workers);
    if costed {
        harness.costs = random_costs(&desc, seed);
    }
    harness.switch(
        &desc.spec.validate().expect("generated graphs are valid"),
        &desc.kinds,
    );
    let (mut trace, mut frame) = (Trace::default(), 0u64);
    for n in schedule(which, seed, 700) {
        trace.push(&harness.block(n, &input_signal(frame, n)));
        frame += n as u64;
    }
    trace.finish(&harness)
}

/// The graph under a played transport and commands scheduled all through the
/// run, on every event input: on emitters' frames (ties), on each other's, in
/// the past, at the next block, and by beat, ahead of the playhead and behind
/// it. With `inside`, each block is cut at up to three frames where the
/// transport changes (a seek, a start or stop, a tempo step, a loop edit);
/// without, the graph is recompiled halfway, so some commands lose their node
/// and the rings and slots that survive by key carry their state.
fn scripted(seed: u64, which: usize, workers: usize, inside: bool) -> Trace {
    let desc = random_graph(seed);
    let mut harness = Harness::with_workers(MAX_BLOCK, workers);
    harness.switch(
        &desc.spec.validate().expect("generated graphs are valid"),
        &desc.kinds,
    );
    let ports = event_ports(&desc);
    let mut rng = Rng::new(seed ^ 0xC0DE);
    let mut script = Script {
        beat: 0.0,
        playing: !inside,
        tempo: 1_200.0,
        looping: None,
    };
    let blocks = schedule(which, seed, 900);
    let half = blocks.len() / 2;
    let (mut trace, mut frame) = (Trace::default(), 0u64);
    for (i, &n) in blocks.iter().enumerate() {
        if !inside && i == half {
            let edited = mutate(&desc, &mut rng);
            harness.switch(&edited.spec.validate().expect("valid"), &edited.kinds);
        }
        let mut bounds = vec![0, n];
        if inside {
            let mut cuts: Vec<usize> = (0..rng.below(4))
                .filter(|_| n > 1)
                .map(|_| 1 + rng.below(n as u64 - 1) as usize)
                .collect();
            cuts.sort_unstable();
            cuts.dedup();
            bounds.splice(1..1, cuts);
        }
        let start = script.block(bounds[1] - bounds[0], &mut rng);
        let mut changes = TransportChanges::NONE;
        for w in bounds[1..].windows(2) {
            let to = script.block(w[1] - w[0], &mut rng);
            let at = tutti_graph::Offset::of(Samples::new(w[0]), Samples::new(n));
            changes.push(at, to).expect("ordered, distinct, few");
        }
        if rng.chance(30) {
            let beats: Vec<f64> = std::iter::once(start.beat().get())
                .chain(changes.as_slice().iter().map(|c| c.to.beat().get()))
                .collect();
            for _ in 0..1 + rng.below(3) {
                let Some(to) = rng.pick(&ports) else { break };
                let at = match rng.below(6) {
                    0 => At::NextBlock,
                    1 => At::Frame(Frame::new(rng.below(frame + 1))),
                    2 => At::Frame(Frame::new(frame + 3 * rng.below(100))),
                    // Around a piece's start: behind it by up to 0.01 beat,
                    // ahead by up to 0.05.
                    _ => {
                        let b = beats[rng.below(beats.len() as u64) as usize];
                        At::Beat(Beat::new(
                            (b + (rng.below(600) as f64 - 100.0) / 10_000.0).max(0.0),
                        ))
                    }
                };
                let kind =
                    tutti_graph::EventKind::Midi(tutti_ump::Ump([rng.below(1000) as u32, 0, 0, 0]));
                match harness.editor.schedule(at, to, kind) {
                    // Pending beats hold credit; a command may also have lost
                    // its node in the recompile.
                    Ok(_) | Err(ScheduleError::Backpressure | ScheduleError::NoSuchPort { .. }) => {
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        }
        let a = harness.block_with_changes(n, &input_signal(frame, n), &start, &changes);
        trace.push(&a);
        frame += n as u64;
    }
    trace.finish(&harness)
}

/// Three graphs, each an edit of the last, rendered in turn.
fn recompiled(seed: u64, which: usize, workers: usize) -> Trace {
    let first = random_graph(seed);
    let mut rng = Rng::new(seed ^ 0x5EED);
    let second = mutate(&first, &mut rng);
    let third = mutate(&second, &mut rng);
    let mut harness = Harness::with_workers(MAX_BLOCK, workers);
    let (mut trace, mut frame) = (Trace::default(), 0u64);
    for (i, desc) in [&first, &second, &third].into_iter().enumerate() {
        harness.switch(
            &desc.spec.validate().expect("mutations stay valid"),
            &desc.kinds,
        );
        for n in schedule(which, seed.wrapping_add(i as u64), 300) {
            trace.push(&harness.block(n, &input_signal(frame, n)));
            frame += n as u64;
        }
    }
    trace.finish(&harness)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: tutti_fixtures::cases(256), ..ProptestConfig::default() })]

    /// A render is the one-participant render, at every block schedule.
    #[test]
    fn a_parallel_render_is_the_one_participant_render(seed in any::<u64>(), which in 0usize..5) {
        prop_assert_eq!(plain(seed, which, 1, false), plain(seed, which, parallel(seed), false));
    }

    /// The same when nodes declare costs and the compiler packs the cheap ones
    /// into tasks.
    #[test]
    fn a_coarsened_plan_renders_as_one_participant_does(seed in any::<u64>(), which in 0usize..5) {
        prop_assert_eq!(plain(seed, which, 1, true), plain(seed, which, parallel(seed), true));
    }

    /// The same under commands scheduled by frame and by beat, and a recompile
    /// halfway.
    #[test]
    fn scheduled_commands_land_alike_on_any_participant_count(seed in any::<u64>(), which in 0usize..5) {
        prop_assert_eq!(scripted(seed, which, 1, false), scripted(seed, which, parallel(seed), false));
    }

    /// The same with the transport changing inside blocks: a beat resolves
    /// against the transport in force where playback reaches it.
    #[test]
    fn transport_changes_inside_blocks_land_alike_on_any_participant_count(seed in any::<u64>(), which in 0usize..5) {
        prop_assert_eq!(scripted(seed, which, 1, true), scripted(seed, which, parallel(seed), true));
    }

    /// The same across recompiles: nodes, rings and feedback slots that survive
    /// by key carry their state.
    #[test]
    fn recompiles_carry_state_alike_on_any_participant_count(seed in any::<u64>(), which in 0usize..5) {
        prop_assert_eq!(recompiled(seed, which, 1), recompiled(seed, which, parallel(seed)));
    }

    /// A tap changes nothing and reads exactly what it names: with an aligned
    /// tap of every output's source and an outputs tap open, the lanes of each
    /// are the output channels, sample for sample, at every block schedule,
    /// one participant and parallel. The outputs tap reads every channel,
    /// summed ones included; the ports tap names the source of a one-source
    /// channel only (a tap of ports names no sum), so it is compared on those.
    #[test]
    fn an_aligned_tap_of_the_outputs_is_the_output(seed in any::<u64>(), which in 0usize..5) {
        let mut traces = Vec::new();
        for workers in [1, parallel(seed)] {
            let desc = random_graph(seed);
            let outputs: Vec<PortSource> = desc
                .spec
                .topology
                .outputs
                .iter()
                .map(|s| match s[..] {
                    [one] => one,
                    _ => PortSource::Zero,
                })
                .collect();
            let summed: Vec<bool> = desc.spec.topology.outputs.iter().map(|s| s.len() > 1).collect();
            let channels = outputs.len();
            let mut spec = desc.spec.clone();
            spec.topology.taps.insert(TapId::of(1), TapSpec::aligned(outputs.clone()));
            spec.topology.taps.insert(TapId::of(2), TapSpec::Outputs { width: channels });
            let valid = spec.validate().expect("generated graphs are valid");
            let mut harness = Harness::with_workers(MAX_BLOCK, workers);
            // `open_tap` refuses a source the editor's spec lacks, and the
            // harness compiles `spec` itself: give the editor the same nodes.
            harness.editor.spec_mut().topology.nodes = spec.topology.nodes.clone();
            harness.editor.spec_mut().topology.inputs = spec.topology.inputs;
            let mut tap = harness.editor.open_tap(TapId::of(1), outputs, TapOptions::new(Samples::new(1 << 16))).expect("the sources exist");
            let mut heard = harness.editor.open_heard_tap(TapId::of(2), channels, Samples::new(1 << 16)).expect("the sources exist");
            harness.switch(&valid, &desc.kinds);
            let width = tap.width();
            prop_assert_eq!(heard.width(), channels);
            let (mut trace, mut frame) = (Trace::default(), 0u64);
            for n in schedule(which, seed, 700) {
                let out = harness.block(n, &input_signal(frame, n));
                let mut got = vec![0.0f32; n * width];
                prop_assert_eq!(tap.pop_into(InterleavedMut::new(&mut got, tap.layout()).unwrap()).unwrap().get(), n);
                for (c, channel) in out.iter().enumerate().filter(|(c, _)| !summed[*c]) {
                    let lane: Vec<f32> = got.iter().skip(c).step_by(width).copied().collect();
                    prop_assert_eq!(bits(&[lane]), bits(std::slice::from_ref(channel)), "tap lane {} at frame {}", c, frame);
                }
                let mut got = vec![0.0f32; n * channels];
                prop_assert_eq!(heard.pop_into(InterleavedMut::new(&mut got, heard.layout()).unwrap()).unwrap().get(), n);
                for (c, channel) in out.iter().enumerate() {
                    let lane: Vec<f32> = got.iter().skip(c).step_by(channels).copied().collect();
                    prop_assert_eq!(bits(&[lane]), bits(std::slice::from_ref(channel)), "outputs tap lane {} at frame {}", c, frame);
                }
                prop_assert_eq!(tap.take_lost().get(), 0);
                prop_assert_eq!(heard.take_lost().get(), 0);
                trace.push(&out);
                frame += n as u64;
            }
            traces.push(trace.finish(&harness));
        }
        prop_assert_eq!(&traces[0], &traces[1]);
    }
}

// ---- declared figures ------------------------------------------------------

/// The nodes `node`'s arrival waits for: the sources of its audio ports and of
/// its event ports, direct ones. A feedback edge carries last block's value and
/// waits for nothing.
fn sources(desc: &Desc, node: NodeKey) -> BTreeSet<NodeKey> {
    let audio = desc
        .spec
        .topology
        .edges
        .iter()
        .filter(|(at, _)| at.node() == node)
        .flat_map(|(_, es)| es)
        .filter_map(|e| match e {
            Edge::Direct(PortSource::Node(p)) => Some(p.node()),
            _ => None,
        });
    let events = desc
        .spec
        .events
        .iter()
        .filter(|(at, _)| at.node() == node)
        .flat_map(|(_, es)| es)
        .filter_map(|e| match e {
            EventEdge::Direct(from) => Some(from.node()),
            _ => None,
        });
    audio.chain(events).collect()
}

/// The frames before `node`'s output is aligned with the main input: its
/// arrival (the latest departure among its sources) and its declared latency.
fn departure(desc: &Desc, node: NodeKey) -> usize {
    let arrival = sources(desc, node)
        .into_iter()
        .map(|s| departure(desc, s))
        .max()
        .unwrap_or(0);
    arrival + desc.spec.topology.nodes[&node].latency.get()
}

/// What a channel's sources depart at: a main input and silence at 0.
fn channel_departure(desc: &Desc, c: usize) -> usize {
    desc.spec.topology.outputs[c]
        .iter()
        .map(|s| match s {
            PortSource::Node(p) => departure(desc, p.node()),
            _ => 0,
        })
        .max()
        .unwrap_or(0)
}

/// The nodes that reach an output through direct audio edges, with the longest
/// sum of declared tails along a path from each.
fn tails(desc: &Desc) -> BTreeMap<NodeKey, usize> {
    fn walk(desc: &Desc, node: NodeKey, seen: &mut BTreeMap<NodeKey, usize>) -> usize {
        if let Some(&t) = seen.get(&node) {
            return t;
        }
        let own = match desc.spec.topology.nodes[&node].tail {
            Tail::Finite(s) => s.get(),
            _ => 0,
        };
        let below = desc
            .spec
            .topology
            .edges
            .iter()
            .filter(|(at, _)| at.node() == node)
            .flat_map(|(_, es)| es)
            .filter_map(|e| match e {
                Edge::Direct(PortSource::Node(p)) => Some(walk(desc, p.node(), seen)),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        seen.insert(node, own + below);
        own + below
    }
    let mut seen = BTreeMap::new();
    for s in desc.spec.topology.outputs.iter().flatten() {
        if let PortSource::Node(p) = s {
            walk(desc, p.node(), &mut seen);
        }
    }
    seen
}

proptest! {
    #![proptest_config(ProptestConfig { cases: tutti_fixtures::cases(512), ..ProptestConfig::default() })]

    /// The plan's latency is the latest departure among the output sources, and
    /// each channel's compensation is how far it departs before that, each node
    /// departing its declared latency after the latest of its sources; the
    /// graph's tail is the longest sum of declared tails along a path to an
    /// output, and it is unbounded exactly when a node that reaches an output
    /// never decays.
    #[test]
    fn the_graph_reports_the_figures_its_nodes_declare(seed in any::<u64>()) {
        let desc = random_graph(seed);
        let valid = desc.spec.validate().expect("generated graphs are valid");
        let (plan, _) = compile(&valid, &common::shapes_of(&desc.kinds), &common::prepare(MAX_BLOCK), None).expect("compiles");
        let channels = desc.spec.topology.outputs.len();
        let latency = (0..channels).map(|c| channel_departure(&desc, c)).max().unwrap_or(0);
        prop_assert_eq!(plan.total_latency().samples(), Samples::new(latency));
        let compensation: Vec<Samples> = (0..channels)
            .map(|c| Samples::new(latency - channel_departure(&desc, c)))
            .collect();
        prop_assert_eq!(plan.compensation(), &compensation[..]);

        let reaching = tails(&desc);
        let tail = graph_tail(&desc.spec.topology, SampleRate::SR_48K);
        let unbounded = reaching
            .keys()
            .any(|k| desc.spec.topology.nodes[k].tail == Tail::Unbounded);
        prop_assert_eq!(tail.is_unbounded(), unbounded);
        if !unbounded {
            let longest = reaching.values().copied().max().unwrap_or(0);
            prop_assert_eq!(tail.known(), Samples::new(longest));
        }
    }
}

/// The figures above are not vacuous: over a fixed range of seeds the graphs
/// hold compensation, a tail along a path, an unbounded tail, and event sources
/// that hold a node's arrival back.
#[test]
fn the_declared_figures_are_exercised() {
    let (mut compensated, mut tailed, mut unbounded, mut by_events) = (0, 0, 0, 0);
    for seed in 0..512u64 {
        let desc = random_graph(seed);
        let reaching = tails(&desc);
        compensated += usize::from((0..desc.spec.topology.outputs.len()).any(|c| {
            channel_departure(&desc, c)
                < (0..desc.spec.topology.outputs.len())
                    .map(|d| channel_departure(&desc, d))
                    .max()
                    .unwrap_or(0)
        }));
        tailed += usize::from(reaching.values().any(|&t| t > 0));
        unbounded += usize::from(
            reaching
                .keys()
                .any(|k| desc.spec.topology.nodes[k].tail == Tail::Unbounded),
        );
        by_events += usize::from(desc.spec.events.iter().any(|(at, es)| {
            es.iter().any(|e| matches!(e, EventEdge::Direct(from) if departure(&desc, from.node()) > 0 && departure(&desc, at.node()) > 0))
        }));
    }
    assert!(
        compensated > 50,
        "only {compensated} graphs hold compensation"
    );
    assert!(tailed > 50, "only {tailed} graphs hold a tail");
    assert!(
        unbounded > 50,
        "only {unbounded} graphs hold an unbounded tail"
    );
    assert!(
        by_events > 20,
        "only {by_events} graphs delay a node by an event source"
    );
}
