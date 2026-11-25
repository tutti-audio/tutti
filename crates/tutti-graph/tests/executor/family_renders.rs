//! The graph families of `tests/support/families.rs` under the one-participant
//! invariant of `invariants.rs`: whatever the family, the block schedule and
//! the pool width, every output sample and the late, unrouted and dropped
//! counts are those of an executor with no pool.
//!
//! - **Crossfades**: replaces that fade over the running node, queue behind
//!   a running fade, supersede a waiting one and are cut by a removal, a hard
//!   regenerate or a `set_latency`.
//! - **Weighted and driven edge gains**: weights on the edges, a fader moved
//!   between blocks, lanes that drive them, ranges that clamp them.
//! - **Modes and mixes**: bypass and mute, a node's mix, a lane driving it,
//!   and nodes declaring their own bypass.
//! - **Bus edges**: folds and copies lowered to lanes, between nodes and into
//!   the main output.
//!
//! One test reaches every feature of every family over a fixed range of seeds.

use proptest::prelude::*;
use tutti_buffers::Float;
use tutti_graph::testing::{self as common, Harness, Word, input_signal};
use tutti_graph::{EdgeId, ParamRange};
use tutti_units::Gain;

use crate::families::*;
use crate::generator::{Desc, MAX_BLOCK, Rng, mutate, random_graph, schedule};
use crate::invariants::{Trace, workers};

/// Renders `blocks` through `harness`, recording each into `trace`.
fn render<F: Word>(harness: &mut Harness<F>, blocks: &[usize], frame: &mut u64, trace: &mut Trace)
where
    F::Bits: Into<u64>,
{
    for &n in blocks {
        trace.push(&harness.block(n, &input_signal::<F>(*frame, n)));
        *frame += n as u64;
    }
}

/// Renders until the editor has room for another commit, as a host does on
/// `Backpressure`. (A crossfade holds no commit, so this only waits for the
/// queue itself; `Harness::switch` applies and collects each.)
fn wait_for_credit(harness: &mut Harness, frame: &mut u64, trace: &mut Trace) {
    loop {
        harness.editor.collect();
        if harness.editor.in_flight() < tutti_graph::QUEUE_CAPACITY {
            return;
        }
        render(harness, &[64], frame, trace);
    }
}

/// Moves a random weighted edge's fader to a random weight, `percent` of the
/// blocks.
fn move_fader<F: Float>(desc: &mut Desc, harness: &mut Harness<F>, rng: &mut Rng, percent: u64) {
    let ids: Vec<EdgeId> = desc.spec.gains.keys().copied().collect();
    if rng.chance(percent)
        && let Some(id) = rng.pick(&ids)
    {
        let w = Gain::new(WEIGHTS[rng.below(WEIGHTS.len() as u64) as usize]);
        harness.set_edge_gain(id, w);
        desc.bases.insert(id, w);
    }
}

/// Eight rounds of edits a random 1-300 frames apart, each crossfading one to
/// three nodes (every kind, events included) over 1-250 frames. Afterwards
/// every crossfade has come back on the fade-return ring.
fn crossfaded(seed: u64, which: usize, workers: usize) -> Trace {
    let mut desc = random_graph(seed);
    let mut rng = desc.rng.clone();
    let mut harness = Harness::with_workers(MAX_BLOCK, workers);
    harness.switch(&desc.spec.validate().expect("valid"), &desc.kinds);
    let (mut trace, mut frame) = (Trace::default(), 0u64);
    let first = schedule(which, seed, 100);
    render(&mut harness, &first, &mut frame, &mut trace);
    for round in 0..8u64 {
        let (next, fades) = mutate_with_fades(&desc, &mut rng);
        wait_for_credit(&mut harness, &mut frame, &mut trace);
        harness.switch_with_fades(&next.spec.validate().expect("valid"), &next.kinds, &fades);
        let frames = 1 + rng.below(300) as usize;
        let blocks = schedule(which, seed.wrapping_add(round), frames);
        render(&mut harness, &blocks, &mut frame, &mut trace);
        desc = next;
    }
    // Long enough for every fade, waiting ones included, to end.
    let rest = schedule(which, seed, 8 * 260);
    render(&mut harness, &rest, &mut frame, &mut trace);
    harness.editor.collect();
    assert_eq!(harness.editor.in_flight(), 0);
    assert_eq!(
        harness.editor.fades_in_flight(),
        0,
        "a crossfade never came back"
    );
    trace.finish(&harness)
}

/// Weights on a third of the edges (feedback ones, fan-ins, one-edge sinks,
/// output channels, and chains behind PDC included), a fader moved between
/// blocks (`set_edge_gain`, so ramps of every block length in the schedule),
/// and an edit halfway that rewires, removes and regenerates nodes (lane state
/// carried by edge, entries of vanished edges dropped, new ones added).
fn weighted(seed: u64, which: usize, workers: usize) -> Trace {
    let mut desc = random_graph(seed);
    let mut rng = desc.rng.clone();
    weigh(&mut desc, &mut rng);
    let mut harness = Harness::with_workers(MAX_BLOCK, workers);
    desc.start(&mut harness);
    harness.switch(
        &desc.spec.validate().expect("generated graphs are valid"),
        &desc.kinds,
    );
    let blocks = schedule(which, seed, 900);
    let half = blocks.len() / 2;
    let (mut trace, mut frame) = (Trace::default(), 0u64);
    for (i, &n) in blocks.iter().enumerate() {
        if i == half {
            let mut edited = mutate(&desc, &mut rng);
            prune_gains(&mut edited);
            weigh(&mut edited, &mut rng);
            edited.start(&mut harness);
            harness.switch(&edited.spec.validate().expect("valid"), &edited.kinds);
            desc = edited;
        }
        move_fader(&mut desc, &mut harness, &mut rng, 40);
        render(&mut harness, &[n], &mut frame, &mut trace);
    }
    trace.finish(&harness)
}

/// The same at `f64`, with the control lanes moving: edge weights moved
/// between blocks (ramps of every block length), nodes' mixes moved and driven
/// by lanes.
fn moving_f64(seed: u64, which: usize, workers: usize) -> Trace {
    let mut desc = random_graph(seed);
    let mut rng = desc.rng.clone();
    weigh(&mut desc, &mut rng);
    drive_mixes(&mut desc, &mut rng);
    let mut harness = Harness::<f64>::with_format(MAX_BLOCK);
    common::pooled(&mut harness.exec, workers);
    desc.start(&mut harness);
    harness.switch(
        &desc.spec.validate().expect("generated graphs are valid"),
        &desc.kinds,
    );
    let (mut trace, mut frame) = (Trace::default(), 0u64);
    for &n in &schedule(which, seed, 900) {
        move_fader(&mut desc, &mut harness, &mut rng, 40);
        if rng.chance(20)
            && let Some((key, mix)) = pick_mix(&desc, &mut rng)
        {
            harness.set_mix(key, mix);
        }
        render(&mut harness, &[n], &mut frame, &mut trace);
    }
    trace.finish(&harness)
}

/// Lanes of several periods and latencies drive about half the weighted edges
/// (at ports and output channels, behind PDC, in fan-ins and one-edge sinks),
/// with ranges that clamp, a range moved between blocks
/// (`set_edge_gain_range`), a fader moved under a driven lane, and an edit
/// halfway that rewires, removes and regenerates nodes and re-draws every
/// source (a source that changes or goes is declicked, state carried by edge).
fn driven_gains(seed: u64, which: usize, workers: usize) -> Trace {
    let mut desc = random_graph(seed);
    let mut rng = desc.rng.clone();
    weigh(&mut desc, &mut rng);
    drive(&mut desc, &mut rng);
    let mut harness = Harness::with_workers(MAX_BLOCK, workers);
    desc.start(&mut harness);
    harness.switch(
        &desc.spec.validate().expect("generated graphs are valid"),
        &desc.kinds,
    );
    let blocks = schedule(which, seed, 900);
    let half = blocks.len() / 2;
    let (mut trace, mut frame) = (Trace::default(), 0u64);
    for (i, &n) in blocks.iter().enumerate() {
        if i == half {
            let mut edited = mutate(&desc, &mut rng);
            prune_gains(&mut edited);
            weigh(&mut edited, &mut rng);
            drive(&mut edited, &mut rng);
            edited.start(&mut harness);
            harness.switch(&edited.spec.validate().expect("valid"), &edited.kinds);
            desc = edited;
        }
        let ids: Vec<EdgeId> = desc.spec.gains.keys().copied().collect();
        if rng.chance(30)
            && let Some(id) = rng.pick(&ids)
        {
            let (min, max) = LANE_RANGES[rng.below(LANE_RANGES.len() as u64) as usize];
            let range = ParamRange::of(min, max);
            harness.set_edge_gain_range(id, range);
            desc.gain_ranges.insert(id, range);
        }
        move_fader(&mut desc, &mut harness, &mut rng, 30);
        render(&mut harness, &[n], &mut frame, &mut trace);
    }
    trace.finish(&harness)
}

/// About one block in five a node (any: latent ones, event ones, sources,
/// sinks, in-place ones, sums of several inputs, nodes behind PDC) is set to a
/// random mode, so toggles land mid-fade, during the warm-up, while parked and
/// in blocks of every length in the schedule; an edit halfway rewires, removes
/// and regenerates nodes (a kept node's mode carries, a regenerated one starts
/// steady in the spec's mode).
fn moded(seed: u64, which: usize, workers: usize) -> Trace {
    let mut desc = random_graph(seed);
    let mut rng = desc.rng.clone();
    let mut harness = Harness::with_workers(MAX_BLOCK, workers);
    desc.start(&mut harness);
    harness.switch(
        &desc.spec.validate().expect("generated graphs are valid"),
        &desc.kinds,
    );
    let blocks = schedule(which, seed, 1400);
    let half = blocks.len() / 2;
    let (mut trace, mut frame) = (Trace::default(), 0u64);
    for (i, &n) in blocks.iter().enumerate() {
        if i == half {
            let edited = mutate(&desc, &mut rng);
            edited.start(&mut harness);
            harness.switch(&edited.spec.validate().expect("valid"), &edited.kinds);
            desc = edited;
        }
        if rng.chance(20)
            && let Some((key, mode)) = pick_mode(&desc, &mut rng)
        {
            harness.set_mode(key, mode);
        }
        render(&mut harness, &[n], &mut frame, &mut trace);
    }
    trace.finish(&harness)
}

/// The modes run with a third of the nodes declaring their own bypass: the
/// executor reads `Bypassed` as `Active` for them (called every block, no
/// blend, no park), while `Muted` and a toggle back stay the executor's.
fn own_bypassed(seed: u64, which: usize, workers: usize) -> Trace {
    let mut desc = random_graph(seed);
    let mut rng = desc.rng.clone();
    own_bypass(&mut desc, &mut rng);
    let mut harness = Harness::with_workers(MAX_BLOCK, workers);
    desc.start(&mut harness);
    harness.switch(
        &desc.spec.validate().expect("generated graphs are valid"),
        &desc.kinds,
    );
    let (mut trace, mut frame) = (Trace::default(), 0u64);
    for &n in &schedule(which, seed, 1400) {
        if rng.chance(20)
            && let Some((key, mode)) = pick_mode(&desc, &mut rng)
        {
            harness.set_mode(key, mode);
        }
        render(&mut harness, &[n], &mut frame, &mut trace);
    }
    trace.finish(&harness)
}

/// About one block in four a node (any: latent ones, event ones, sources,
/// sinks, in-place ones, sums, nodes behind PDC) gets a random mix, and one in
/// eight of those blocks also a random mode, so a mix meets a steady Active
/// node, a ramp, a fade, the warm-up and a parked node, in blocks of every
/// length in the schedule; an edit halfway rewires, removes and regenerates
/// nodes (a kept node's mix carries, a regenerated one starts at the spec's).
fn mixed(seed: u64, which: usize, workers: usize) -> Trace {
    let mut desc = random_graph(seed);
    let mut rng = desc.rng.clone();
    let mut harness = Harness::with_workers(MAX_BLOCK, workers);
    desc.start(&mut harness);
    harness.switch(
        &desc.spec.validate().expect("generated graphs are valid"),
        &desc.kinds,
    );
    let blocks = schedule(which, seed, 1400);
    let half = blocks.len() / 2;
    let (mut trace, mut frame) = (Trace::default(), 0u64);
    for (i, &n) in blocks.iter().enumerate() {
        if i == half {
            let edited = mutate(&desc, &mut rng);
            edited.start(&mut harness);
            harness.switch(&edited.spec.validate().expect("valid"), &edited.kinds);
            desc = edited;
        }
        if rng.chance(25)
            && let Some((key, mix)) = pick_mix(&desc, &mut rng)
        {
            harness.set_mix(key, mix);
            if rng.chance(8)
                && let Some((key, mode)) = pick_mode(&desc, &mut rng)
            {
                harness.set_mode(key, mode);
            }
        }
        render(&mut harness, &[n], &mut frame, &mut trace);
    }
    trace.finish(&harness)
}

/// Lanes of several periods and latencies drive the mix of about a third of
/// the nodes (latent ones, event ones, sources, sinks, in-place ones, nodes
/// behind PDC), with ranges that clamp, a mix moved under a driven lane, a
/// range moved between blocks, and modes (a driven mix meets a fade, the
/// warm-up and a parked node); an edit halfway rewires, removes and
/// regenerates nodes and re-draws every source (a source that changes or goes
/// is declicked, a regenerated node's lane starts at its value).
fn driven_mixes(seed: u64, which: usize, workers: usize) -> Trace {
    let mut desc = random_graph(seed);
    let mut rng = desc.rng.clone();
    drive_mixes(&mut desc, &mut rng);
    let mut harness = Harness::with_workers(MAX_BLOCK, workers);
    desc.start(&mut harness);
    harness.switch(
        &desc.spec.validate().expect("generated graphs are valid"),
        &desc.kinds,
    );
    let blocks = schedule(which, seed, 1400);
    let half = blocks.len() / 2;
    let (mut trace, mut frame) = (Trace::default(), 0u64);
    for (i, &n) in blocks.iter().enumerate() {
        if i == half {
            let mut edited = mutate(&desc, &mut rng);
            prune_mix_lanes(&mut edited);
            drive_mixes(&mut edited, &mut rng);
            edited.start(&mut harness);
            harness.switch(&edited.spec.validate().expect("valid"), &edited.kinds);
            desc = edited;
        }
        if rng.chance(20)
            && let Some((key, mix)) = pick_mix(&desc, &mut rng)
        {
            harness.set_mix(key, mix);
        }
        if rng.chance(15)
            && let Some(key) = rng.pick(&desc.order)
        {
            let (min, max) = LANE_RANGES[rng.below(LANE_RANGES.len() as u64) as usize];
            let range = ParamRange::of(min, max);
            harness.set_mix_range(key, range);
            desc.port_ranges.insert(mix_port(key), range);
        }
        if rng.chance(8)
            && let Some((key, mode)) = pick_mode(&desc, &mut rng)
        {
            harness.set_mode(key, mode);
        }
        render(&mut harness, &[n], &mut frame, &mut trace);
    }
    trace.finish(&harness)
}

/// A bus graph's lowered spec rendered through block schedule `which`.
fn bused(graph: &BusGraph, which: usize, seed: u64, workers: usize) -> Trace {
    let lowered = graph
        .spec
        .lowered(&common::shapes_of(&graph.kinds))
        .expect("every kept bus edge lowers");
    let valid = lowered.spec.validate().expect("generated graphs are valid");
    let mut harness = Harness::with_workers(MAX_BLOCK, workers);
    harness.switch(&valid, &graph.kinds);
    let (mut trace, mut frame) = (Trace::default(), 0u64);
    let blocks = schedule(which, seed, 500);
    render(&mut harness, &blocks, &mut frame, &mut trace);
    trace.finish(&harness)
}

fn bus_edges(seed: u64, which: usize, workers: usize) -> Trace {
    bused(&bus_graph(seed), which, seed, workers)
}

fn main_output_bus_edges(seed: u64, which: usize, workers: usize) -> Trace {
    bused(&main_output_bus_graph(seed), which, seed, workers)
}

/// A family renders `(seed, block schedule, participants)`.
type Family = fn(u64, usize, usize) -> Trace;

/// The families, by name.
const FAMILIES: [(&str, Family); 10] = [
    ("crossfades", crossfaded),
    ("weighted edges", weighted),
    ("weights and mixes at f64", moving_f64),
    ("driven edge gains", driven_gains),
    ("modes", moded),
    ("own bypass", own_bypassed),
    ("mixes", mixed),
    ("driven mixes", driven_mixes),
    ("bus edges", bus_edges),
    ("main output bus edges", main_output_bus_edges),
];

proptest! {
    #![proptest_config(ProptestConfig { cases: tutti_fixtures::cases(1600), ..ProptestConfig::default() })]

    /// Every family renders alike on any participant count, at every block
    /// schedule.
    #[test]
    fn every_family_renders_alike_on_any_participant_count(
        family in 0..FAMILIES.len(),
        seed in any::<u64>(),
        which in 0usize..5,
        workers in workers(),
    ) {
        let (name, render) = FAMILIES[family];
        prop_assert_eq!(render(seed, which, 1), render(seed, which, workers), "{}", name);
    }
}

/// Over a fixed range of seeds the generators draw every feature the property
/// above claims to hold, each in at least `MIN` of the graphs.
#[test]
fn the_generators_reach_every_feature_of_every_family() {
    const MIN: usize = 10;
    let mut reached = Features::default();
    for seed in 0..512u64 {
        let mut desc = random_graph(seed);
        reached.add(desc.features());
        let mut rng = desc.rng.clone();
        weigh(&mut desc, &mut rng);
        drive(&mut desc, &mut rng);
        drive_mixes(&mut desc, &mut rng);
        own_bypass(&mut desc, &mut rng);
        reached.add(desc.features());
        let (after, fades) = mutate_with_fades(&desc, &mut rng);
        reached.add(fade_features(&desc, &after, &fades));
        reached.add(bus_graph(seed).features());
        reached.add(main_output_bus_graph(seed).features());
    }
    for (name, n) in &reached.0 {
        eprintln!("{name}: {n}");
    }
    for (name, &n) in &reached.0 {
        assert!(n >= MIN, "only {n} graphs hold {name}");
    }
}
