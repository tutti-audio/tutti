//! How long a graph rings after its input stops: the [`TailGraph`] trait and
//! the [`graph_tail`] walk.
//!
//! An offline render stops pulling when the last clip ends. A reverb does not
//! stop with it — a bounce that ends at the same moment truncates the decay.
//! This module answers "how many frames past the end must the render keep
//! pulling?", so a caller can extend the render by that much. Latency is not
//! part of the answer: a node's [`Tail`] excludes it, and a render trims the
//! graph's latency separately ([`latency`]).
//!
//! Pure graph math over [`TailGraph`] — no audio, no DSP — so it can be driven
//! by a real audio graph or a three-line test fixture, exactly as [`latency`]
//! is. Topology comes from [`LatencyGraph`]: one graph answers `nodes`,
//! `inputs` and `outputs` the same way whichever quantity is being walked, so
//! only the per-node figure is declared here.
//!
//! # How a tail composes
//!
//! ```text
//!   src ──▶ reverb(2 s) ──▶ reverb(3 s) ──▶ out      5 s: a cascade adds
//!
//!   src ──▶ reverb(2 s) ──▶ ┐
//!                           ├──▶ mixer ──▶ out       3 s: a merge takes the max
//!   src ──▶ reverb(3 s) ──▶ ┘
//! ```
//!
//! Cascading convolves the two responses, so their supports combine; summing
//! two paths leaves the longer one's support untouched. Expressed as *ring-out*
//! (see [`Tail`]) both are exact with no correction term, which is why this is
//! the same forward pass [`latency::plan`](crate::compile::latency::plan) runs —
//! `arrival = max(departures)`, `departure = arrival + own` — over a different
//! number.
//!
//! # Where it is an upper bound
//!
//! The convolution argument is a linear one. A saturator after a reverb does not
//! extend the decay, and a compressor with a slow release extends it by
//! something this walk cannot see. The figure is therefore an upper bound on a
//! graph with nonlinear nodes, not an identity. Over-estimating leaves silence
//! at the end of a bounce; under-estimating clips a decay, so the bound errs the
//! way that is recoverable.
//!
//! [`latency`]: crate::compile::latency

use crate::compile::latency::LatencyGraph;
use crate::compile::order::{Sorted, sort_nodes};
use std::collections::{HashMap, HashSet};
use tutti_units::{SampleRate, Samples, Seconds, Tail};

/// The upper bound on any single node's reported tail: sixty seconds.
///
/// A finite tail past this is not trusted as a figure: [`graph_tail`] counts
/// it as [`Tail::Unbounded`], for the reason
/// [`MAX_NODE_LATENCY`](crate::compile::latency::MAX_NODE_LATENCY) bounds a latency: a
/// node that declares garbage would otherwise size the render off it. A bad
/// tail is worse than a bad latency — a latency sizes a buffer, bounded by RAM,
/// while a tail sizes a file, bounded by the disk. It is counted unbounded, not
/// clamped: a clamp would cut a longer ring-out that someone meant and say
/// nothing, while unbounded makes the bounce's caller choose where to stop.
///
/// Six times the latency bound because the quantities differ in scale, not only
/// in kind. A latency past ~10 s is a bug by construction; a 30 s shimmer tail
/// is a patch someone chose, and clamping it at ten would truncate a decay
/// somebody meant. Sixty is past every measured unit (no Apple unit exceeds
/// ~21 s) while still bounding a garbage answer.
///
/// Like its sibling this is a duration, so the bound follows the graph's
/// rate: [`graph_tail`] takes the rate and converts it with
/// [`Seconds::to_samples_ceil`](tutti_units::Seconds::to_samples_ceil)
/// (2 880 000 frames at 48 kHz, 5 760 000 at 96 kHz), and
/// [`Editor::set_tail`](crate::Editor::set_tail) converts it at the editor's
/// prepared rate. A plugin host converts it at the plugin's own rate.
///
/// Applied per node, so a long chain can legitimately sum past it. That is the
/// same property the latency bound has: the bound rejects one bad answer, it
/// does not cap the graph.
///
/// **What it protects**: the length of a bounce. `graph_tail`'s `known` figure
/// sizes an export's tail, and a garbage finite figure would size a file by
/// it (bounded by the disk, not by an arithmetic overflow: sums saturate), so
/// `graph_tail` counts a node's finite tail past it as unbounded and
/// [`Editor::set_tail`](crate::Editor::set_tail) refuses one set by hand. It is not a way to declare a longer ring-out: a host that
/// has one declares `Tail::Unbounded` (the plugin hosts do, for any finite
/// figure past this), which claims nothing false.
pub const MAX_NODE_TAIL: Seconds = Seconds::new(60.0);

/// A [`LatencyGraph`] whose nodes may also keep producing after their input
/// stops.
///
/// Topology comes from the supertrait: a graph that can answer `nodes`,
/// `inputs` and `outputs` for latency answers them identically for tail, so
/// declaring them again would be two views of one graph free to drift.
pub trait TailGraph: LatencyGraph {
    /// Returns the tail `node` reports.
    ///
    /// [`Tail::Unknown`] is the honest answer for a node that has not been
    /// taught to report one; it is not [`Tail::None`], and [`graph_tail`] keeps
    /// them apart.
    fn tail(&self, node: Self::Node) -> Tail;
}

/// A graph's tail, and how much of the graph declined to say.
///
/// Three fields rather than a [`Tail`], because a graph's answer carries
/// something a node's does not: with [`Tail::Unknown`] the default for any node
/// that has not been taught to report, a typical graph is *partly* known.
/// Collapsing that to one word would throw away a figure the caller can use —
/// so the sum over nodes that answered is kept alongside the count that did not.
///
/// [`samples`](Self::samples) is the accessor that refuses to guess.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GraphTail {
    known: Samples,
    unknown_nodes: usize,
    unbounded: bool,
}

impl GraphTail {
    /// Returns the tail as a frame count, or `None` when there is no finite
    /// answer.
    ///
    /// `None` covers two cases that want opposite handling. A graph that never
    /// decays — a reverb reporting unbounded, a delay at full feedback — needs a
    /// caller-chosen place to stop. A graph whose nodes declined to answer needs
    /// a caller-chosen guess, and [`known`](Self::known) is what it would guess
    /// from. Neither decision is made here; a caller that wants either to be no
    /// tail writes `unwrap_or(Samples::ZERO)` and is seen to have decided.
    #[must_use]
    pub const fn samples(self) -> Option<Samples> {
        if self.unbounded || self.unknown_nodes > 0 {
            return None;
        }
        Some(self.known)
    }

    /// Returns the longest path over the nodes that answered, whatever the
    /// caveats.
    ///
    /// Always a number, and therefore only correct for a caller that has read
    /// [`is_unbounded`](Self::is_unbounded) and
    /// [`unknown_nodes`](Self::unknown_nodes) and decided they do not matter.
    /// Prefer [`samples`](Self::samples), which cannot be spent without that
    /// decision being visible.
    #[must_use]
    pub const fn known(self) -> Samples {
        self.known
    }

    /// Returns whether any node reaching an output never decays, or a feedback
    /// cycle rings.
    #[must_use]
    pub const fn is_unbounded(self) -> bool {
        self.unbounded
    }

    /// Returns how many nodes reaching an output reported [`Tail::Unknown`].
    ///
    /// Zero means every node that can affect the output answered, so
    /// [`samples`](Self::samples) is `Some` unless the graph is unbounded.
    #[must_use]
    pub const fn unknown_nodes(self) -> usize {
        self.unknown_nodes
    }

    /// Returns a frame count to render, given where the caller has chosen to
    /// stop.
    ///
    /// [`samples`](Self::samples) refuses to answer for a graph that never
    /// decays or one that did not fully report, because neither has a frame
    /// count. This spends that refusal against a caller-supplied bound, so a
    /// render always gets a number and the number the caller chose is visible in
    /// the call.
    ///
    /// `cap` is an argument rather than a default because there is no figure the
    /// engine could pick: how long to let an infinite reverb ring before cutting
    /// it is a decision about the *output*, not a property of the graph.
    ///
    /// **The two `None` cases resolve differently, because they mean opposite
    /// things.** Unbounded is spent at the cap: the graph genuinely never falls
    /// silent, so the cap is where the caller said to stop. A partly-unknown
    /// graph is spent at [`known`](Self::known), clamped by the cap: the nodes
    /// that *did* answer are evidence, and a node that said nothing is far more
    /// often one with no tail than one with a longer tail than everything
    /// measured. Capping an unknown graph would append silence — a convolver
    /// reporting 4 095 frames beside one node that stayed quiet is a 0.09-second
    /// tail, not an eight-second one.
    ///
    /// ```
    /// use tutti_graph::{NodeSpec, OutputPort, PortSource};
    /// use tutti_units::{SampleRate, Samples, Tail};
    /// use tutti_buffers::ChannelLayout;
    /// use tutti_graph::{graph_tail, NodeKey, Topology};
    ///
    /// let verb = NodeKey(1);
    /// let mut g = Topology::default();
    /// g.nodes.insert(
    ///     verb,
    ///     NodeSpec::new("reverb", ChannelLayout::EMPTY, ChannelLayout::MONO)
    ///         .tail(Tail::Unbounded),
    /// );
    /// g.set_outputs([PortSource::Node(OutputPort::new(verb, 0))]);
    ///
    /// let tail = graph_tail(&g, SampleRate::SR_48K);
    /// assert_eq!(tail.samples(), None); // never decays
    /// let eight_seconds = Samples::new(8 * 48_000);
    /// assert_eq!(tail.resolve(eight_seconds), eight_seconds);
    /// ```
    #[must_use]
    pub fn resolve(self, cap: Samples) -> Samples {
        if self.unbounded {
            return cap;
        }
        self.known.min(cap)
    }
}

/// Returns the tail `g` reports: the longest additive path from any node to an
/// output.
///
/// Only nodes that can reach an output count. A node wired to nothing cannot
/// affect the render however long it rings, so neither its tail nor its silence
/// changes the answer.
///
/// `rate` is the graph's sample rate, at which a node's finite figure is
/// compared with [`MAX_NODE_TAIL`].
///
/// Returns [`GraphTail::default`] — no tail, nothing unknown — for a graph whose
/// nodes all report [`Tail::None`], which is the common case. Allocates; run it
/// on the control thread.
pub fn graph_tail<G: TailGraph>(g: &G, rate: SampleRate) -> GraphTail {
    graph_tail_of(g, rate, |node| Owed::declared(g.tail(node)))
}

/// What a node adds to the graph's tail: what it owes if nothing more arrives
/// at it, and what it adds once input from upstream does.
///
/// A declared tail is both. An executor between blocks knows more of the first
/// (a node at rest owes nothing, one that reported owes its report) and no more
/// of the second than the ceiling, because a report is about the input already
/// heard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Owed {
    /// Owed when no upstream node still owes: it arrives to nothing.
    pub(crate) alone: Tail,
    /// Added after an upstream node that still owes.
    pub(crate) fed: Tail,
}

impl Owed {
    /// A node that owes `tail` whether or not anything reaches it.
    pub(crate) const fn declared(tail: Tail) -> Self {
        Self {
            alone: tail,
            fed: tail,
        }
    }
}

/// [`graph_tail`]'s fold with each node's figures given by `figure` instead of
/// [`TailGraph::tail`]: the walk, the bound and the cycle rule are the same
/// whatever the figures are (declared ceilings, or what the executor holds
/// between blocks). A node departs `alone` when nothing upstream owes (no
/// node there has a figure other than `Tail::None`), and `fed` after one that does; the unbounded and unknown
/// judgements are made on the figure applied to each node.
pub(crate) fn graph_tail_of<G: TailGraph>(
    g: &G,
    rate: SampleRate,
    figure: impl Fn(G::Node) -> Owed,
) -> GraphTail {
    let limit = MAX_NODE_TAIL.to_samples_ceil(rate);
    let figures: HashMap<G::Node, Owed> = g.nodes().map(|node| (node, figure(node))).collect();

    if figures.is_empty() {
        return GraphTail::default();
    }

    let live = reaching_output(g, &figures);
    let (order, had_cycle) = topological_order(g);

    // A node's own figure is `alone` when nothing upstream still owes and
    // `fed` otherwise. Upstream owes when a node there has a figure other
    // than `Tail::None`, with a frame count or not: an unknown or unbounded
    // tail adds nothing to the sum (`unknown_nodes` and `unbounded` carry it,
    // so the total stays the answer for the part of the graph that spoke),
    // but what it rings still excites the node after it. Arrival is the
    // longest departure upstream.
    let mut applied: HashMap<G::Node, Tail> = HashMap::with_capacity(order.len());
    let mut owes: HashSet<G::Node> = HashSet::with_capacity(order.len());
    let mut departure: HashMap<G::Node, Samples> = HashMap::with_capacity(order.len());
    for &node in &order {
        let sources = || g.inputs(node).filter_map(|s| s.feed.node());
        let at = sources()
            .map(|src| departure.get(&src).copied().unwrap_or_default())
            .max()
            .unwrap_or_default();
        let fed = sources().any(|src| owes.contains(&src));
        let owed = figures[&node];
        let tail = if fed { owed.fed } else { owed.alone };
        let own = tail
            .samples()
            .filter(|&s| s <= limit)
            .unwrap_or(Samples::ZERO);
        if fed || tail != Tail::None {
            owes.insert(node);
        }
        applied.insert(node, tail);
        departure.insert(node, at + own);
    }

    // A cycle carrying any tail never decays: each pass round the loop re-enters
    // a node that rings, so there is no pass on which the graph falls silent.
    // `latency`'s walk deliberately approximates a cycle instead, because a
    // misaligned graph is still renderable; an unbounded one is not, and saying
    // so is the whole point of the variant.
    let cycle_rings = had_cycle
        && order
            .iter()
            .filter(|n| live.contains(n))
            .any(|n| !matches!(applied.get(n), Some(Tail::None) | None));

    // A finite figure past `MAX_NODE_TAIL` is not trusted as a figure: it
    // counts as never decaying, so nothing cuts a ring-out to the bound.
    let unbounded = cycle_rings
        || live.iter().any(|n| match applied.get(n) {
            Some(Tail::Unbounded) => true,
            Some(Tail::Finite(frames)) => *frames > limit,
            _ => false,
        });

    let unknown_nodes = live
        .iter()
        .filter(|n| matches!(applied.get(n), Some(Tail::Unknown)))
        .count();

    let known = g
        .outputs()
        .filter_map(|s| s.feed.node())
        .map(|src| departure.get(&src).copied().unwrap_or_default())
        .max()
        .unwrap_or_default();

    GraphTail {
        known,
        unknown_nodes,
        unbounded,
    }
}

/// Every node that can reach an output, walking edges backwards from the taps.
fn reaching_output<G: TailGraph>(g: &G, figures: &HashMap<G::Node, Owed>) -> HashSet<G::Node> {
    let mut live = HashSet::with_capacity(figures.len());
    let mut stack: Vec<G::Node> = g.outputs().filter_map(|s| s.feed.node()).collect();

    while let Some(node) = stack.pop() {
        if !figures.contains_key(&node) || !live.insert(node) {
            continue;
        }
        stack.extend(g.inputs(node).filter_map(|s| s.feed.node()));
    }

    live
}

/// The nodes in dependency order, and whether a cycle stopped the sort.
///
/// The flag is the difference from [`latency`](crate::compile::latency)'s walk,
/// which swallows it: a cycle changes a tail's *answer* rather than its
/// accuracy, so it cannot be absorbed here. Cycle nodes are still appended in
/// node order, so the forward pass sees every node either way.
fn topological_order<G: TailGraph>(g: &G) -> (Vec<G::Node>, bool) {
    let nodes: Vec<G::Node> = g.nodes().collect();
    let Sorted { mut order, cyclic } =
        sort_nodes(&nodes, |node| g.inputs(node).filter_map(|s| s.feed.node()));
    let had_cycle = !cyclic.is_empty();
    order.extend(cyclic);
    (order, had_cycle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::latency::{ChannelFeed, Feed, PortFeed};

    const R: SampleRate = SampleRate::SR_48K;

    /// Minimal `TailGraph`: nodes are indices, edges name their source node.
    struct Toy {
        /// Per node: its tail, and the source feeding each input port.
        nodes: Vec<(Tail, Vec<Option<usize>>)>,
        /// PortSource feeding each output channel.
        outputs: Vec<Option<usize>>,
    }

    impl LatencyGraph for Toy {
        type Node = usize;

        fn nodes(&self) -> impl Iterator<Item = usize> {
            0..self.nodes.len()
        }

        fn latency(&self, _node: usize) -> Samples {
            Samples::ZERO
        }

        fn inputs(&self, node: usize) -> impl Iterator<Item = PortFeed<usize>> {
            self.nodes[node]
                .1
                .iter()
                .enumerate()
                .map(|(port, s)| PortFeed {
                    port: port as u16,
                    feed: s.map_or(Feed::None, Feed::Node),
                })
        }

        fn outputs(&self) -> impl Iterator<Item = ChannelFeed<usize>> {
            self.outputs
                .iter()
                .enumerate()
                .map(|(channel, s)| ChannelFeed {
                    channel,
                    feed: s.map_or(Feed::None, Feed::Node),
                })
        }
    }

    impl TailGraph for Toy {
        fn tail(&self, node: usize) -> Tail {
            self.nodes[node].0
        }
    }

    fn finite(n: usize) -> Tail {
        Tail::Finite(Samples::new(n))
    }

    /// Cascading convolves the two responses, so their ring-outs add. A chain
    /// that rings for 2 s and then feeds one that rings for 3 s rings for 5 s.
    #[test]
    fn a_cascade_sums_its_parts() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                (finite(2000), vec![Some(0)]),
                (finite(3000), vec![Some(1)]),
            ],
            outputs: vec![Some(2)],
        };
        assert_eq!(graph_tail(&g, R).samples(), Some(Samples::new(5000)));
    }

    /// Summing two paths leaves the longer one's decay untouched, so a merge
    /// takes the max rather than the sum — the property that separates this
    /// from a naive total over every node.
    #[test]
    fn parallel_paths_take_the_longer_tail() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                (finite(2000), vec![Some(0)]),
                (finite(3000), vec![Some(0)]),
                (Tail::None, vec![Some(1), Some(2)]),
            ],
            outputs: vec![Some(3)],
        };
        assert_eq!(graph_tail(&g, R).samples(), Some(Samples::new(3000)));
    }

    /// A graph cannot decay faster than its slowest part: one unbounded node
    /// reaching an output makes the whole graph unbounded, and `samples()`
    /// refuses to name a frame count for it.
    #[test]
    fn one_unbounded_node_makes_the_graph_unbounded() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                (Tail::Unbounded, vec![Some(0)]),
                (finite(3000), vec![Some(1)]),
            ],
            outputs: vec![Some(2)],
        };
        let t = graph_tail(&g, R);
        assert!(t.is_unbounded());
        assert_eq!(t.samples(), None);
    }

    /// An unbounded node wired to nothing cannot affect the render however long
    /// it rings, so it must not poison the bounce.
    #[test]
    fn an_unbounded_node_off_the_output_path_is_ignored() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                (finite(3000), vec![Some(0)]),
                // Rings forever, feeds nobody.
                (Tail::Unbounded, vec![Some(0)]),
            ],
            outputs: vec![Some(1)],
        };
        let t = graph_tail(&g, R);
        assert!(!t.is_unbounded(), "an orphan must not make the graph ring");
        assert_eq!(t.samples(), Some(Samples::new(3000)));
    }

    /// A feedback loop re-enters a ringing node on every pass, so there is no
    /// pass on which it falls silent. This deliberately diverges from
    /// `latency`, which approximates a cycle instead — a misaligned graph is
    /// still renderable, an unbounded one is not.
    #[test]
    fn a_feedback_cycle_is_unbounded_not_approximated() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                // 1 and 2 feed each other.
                (finite(1000), vec![Some(0), Some(2)]),
                (finite(1000), vec![Some(1)]),
            ],
            outputs: vec![Some(2)],
        };
        let t = graph_tail(&g, R);
        assert!(t.is_unbounded());
        assert_eq!(t.samples(), None);
    }

    /// A cycle of nodes that do not ring is not a tail: the loop carries
    /// nothing that decays, so the graph is still finite.
    #[test]
    fn a_cycle_of_tailless_nodes_is_not_unbounded() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                (Tail::None, vec![Some(0), Some(2)]),
                (Tail::None, vec![Some(1)]),
            ],
            outputs: vec![Some(2)],
        };
        assert!(!graph_tail(&g, R).is_unbounded());
    }

    /// A finite tail past the bound is not trusted as a figure and not cut to
    /// it: it counts as unbounded, so no bounce ends a ring-out at the bound.
    /// One exactly at the bound is still a figure. The bound is sixty seconds
    /// at the graph's rate: 2 880 000 frames at 48 kHz, 5 760 000 at 96 kHz,
    /// so a figure that is unbounded at 48 kHz is a figure at 96 kHz.
    #[test]
    fn a_tail_past_the_bound_counts_as_unbounded() {
        for (rate, bound) in [
            (SampleRate::SR_48K, 2_880_000),
            (SampleRate::SR_96K, 5_760_000),
        ] {
            assert_eq!(MAX_NODE_TAIL.to_samples_ceil(rate), Samples::new(bound));
            let alone = |frames: usize| Toy {
                nodes: vec![(Tail::Finite(Samples::new(frames)), vec![])],
                outputs: vec![Some(0)],
            };

            let t = graph_tail(&alone(bound + 1), rate);
            assert!(t.is_unbounded(), "{rate:?}");
            assert_eq!(t.samples(), None);
            assert_eq!(t.known(), Samples::ZERO);

            let t = graph_tail(&alone(bound), rate);
            assert!(!t.is_unbounded(), "{rate:?}");
            assert_eq!(t.samples(), Some(Samples::new(bound)));
        }
    }

    /// The same frame count is a figure at a rate that holds it and unbounded
    /// at one that does not: 4 000 000 frames is 83 s at 48 kHz and 41.7 s at
    /// 96 kHz.
    #[test]
    fn a_tail_is_judged_in_seconds_at_the_graphs_rate() {
        let g = Toy {
            nodes: vec![(Tail::Finite(Samples::new(4_000_000)), vec![])],
            outputs: vec![Some(0)],
        };
        assert!(graph_tail(&g, SampleRate::SR_48K).is_unbounded());
        let t = graph_tail(&g, SampleRate::SR_96K);
        assert_eq!(t.samples(), Some(Samples::new(4_000_000)));
    }

    /// A node that was never asked has said nothing, which is not the same as
    /// saying "no tail". The count is kept so a caller can tell the difference.
    #[test]
    fn an_unknown_node_is_not_a_silent_one() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                (Tail::Unknown, vec![Some(0)]),
                (finite(3000), vec![Some(1)]),
            ],
            outputs: vec![Some(2)],
        };
        let t = graph_tail(&g, R);
        assert_eq!(t.unknown_nodes(), 1);
        assert_eq!(t.samples(), None, "an unknown node cannot be read as zero");
        assert_eq!(
            t.known(),
            Samples::new(3000),
            "what the rest of the graph reported is still available"
        );
    }

    /// The common case: every node reports no tail, so the graph has none and
    /// nothing is left unknown.
    #[test]
    fn a_graph_with_no_tails_reports_none() {
        let g = Toy {
            nodes: vec![(Tail::None, vec![]), (Tail::None, vec![Some(0)])],
            outputs: vec![Some(1)],
        };
        let t = graph_tail(&g, R);
        assert_eq!(t.samples(), Some(Samples::ZERO));
        assert_eq!(t.unknown_nodes(), 0);
        assert!(!t.is_unbounded());
    }

    /// The diamond: one leg cascades, the other does not, and the answer is the
    /// longer leg. Pins that the walk sums *along* a path and maxes *across*
    /// them, which a single accumulator would get wrong.
    #[test]
    fn an_asymmetric_diamond_takes_the_longer_leg() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                (finite(1000), vec![Some(0)]),
                (finite(2000), vec![Some(1)]),
                (finite(2500), vec![Some(0)]),
                (Tail::None, vec![Some(2), Some(3)]),
            ],
            outputs: vec![Some(4)],
        };
        // Leg A: 1000 + 2000 = 3000. Leg B: 2500. The max is 3000.
        assert_eq!(graph_tail(&g, R).samples(), Some(Samples::new(3000)));
    }

    /// A graph that never decays is spent at exactly the cap: that is what the
    /// caller chose it for.
    #[test]
    fn resolving_an_unbounded_graph_uses_the_cap() {
        let g = Toy {
            nodes: vec![(Tail::None, vec![]), (Tail::Unbounded, vec![Some(0)])],
            outputs: vec![Some(1)],
        };
        let cap = Samples::new(384_000);
        assert_eq!(graph_tail(&g, R).resolve(cap), cap);
    }

    /// A partly-unknown graph resolves to what the nodes that answered reported,
    /// **not** to the cap.
    ///
    /// The two `None` cases of `samples()` mean opposite things, so collapsing
    /// them onto the cap would append silence: a convolver reporting 3000 frames
    /// beside one silent node is a 3000-frame tail, and rendering the full cap
    /// would write eight seconds of nothing after it.
    #[test]
    fn resolving_an_unknown_graph_uses_what_was_reported() {
        let g = Toy {
            nodes: vec![(Tail::Unknown, vec![]), (finite(3000), vec![Some(0)])],
            outputs: vec![Some(1)],
        };
        let t = graph_tail(&g, R);
        assert_eq!(t.samples(), None, "it cannot be spent without a decision");
        assert_eq!(
            t.resolve(Samples::new(384_000)),
            Samples::new(3000),
            "but the decision is not to believe the cap over the evidence"
        );
    }

    /// The cap bounds a reported tail as well as an unbounded one — a node that
    /// answers with more than the caller will render is still cut at the cap.
    #[test]
    fn the_cap_bounds_a_reported_tail_too() {
        let g = Toy {
            nodes: vec![(Tail::None, vec![]), (finite(100_000), vec![Some(0)])],
            outputs: vec![Some(1)],
        };
        assert_eq!(
            graph_tail(&g, R).resolve(Samples::new(48_000)),
            Samples::new(48_000)
        );
    }

    /// A fully-reported graph resolves to its own figure, untouched by a cap it
    /// does not reach.
    #[test]
    fn resolving_a_known_graph_ignores_a_larger_cap() {
        let g = Toy {
            nodes: vec![(Tail::None, vec![]), (finite(3000), vec![Some(0)])],
            outputs: vec![Some(1)],
        };
        let t = graph_tail(&g, R);
        assert_eq!(t.samples(), Some(Samples::new(3000)));
        assert_eq!(t.resolve(Samples::new(384_000)), Samples::new(3000));
    }

    /// A node adds its `alone` figure when nothing upstream owes and its `fed`
    /// figure after a branch that does: the mixer after an owing branch and a
    /// resting one owes `fed` after the owing one's 300, and the same mixer
    /// after the resting branch alone owes its `alone` figure, nothing.
    #[test]
    fn a_node_adds_its_fed_figure_only_after_an_upstream_that_owes() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                (Tail::None, vec![Some(0)]),
                (Tail::None, vec![Some(0)]),
                (Tail::None, vec![Some(1), Some(2)]),
            ],
            outputs: vec![Some(3)],
        };
        let owed = |n: usize| match n {
            1 => Owed {
                alone: finite(300),
                fed: finite(5000),
            },
            2 => Owed {
                alone: Tail::None,
                fed: finite(4000),
            },
            3 => Owed {
                alone: Tail::None,
                fed: finite(700),
            },
            _ => Owed::declared(Tail::None),
        };
        // Node 1 owes 300 alone; the mixer after it adds its fed 700.
        assert_eq!(
            graph_tail_of(&g, R, owed).samples(),
            Some(Samples::new(1000))
        );

        // With the owing branch resting too, nothing arrives and nothing is owed.
        let resting = |n: usize| match n {
            1 => Owed {
                alone: Tail::None,
                fed: finite(5000),
            },
            n => owed(n),
        };
        assert_eq!(graph_tail_of(&g, R, resting).samples(), Some(Samples::ZERO));
    }

    /// An upstream that owes a figure with no frame count (unknown or
    /// unbounded) still owes: the node after it adds its fed figure, directly
    /// and through a node with no tail between them.
    #[test]
    fn a_node_after_one_that_owes_no_frame_count_adds_its_fed_figure() {
        for upstream in [Tail::Unknown, Tail::Unbounded] {
            for nodes in [
                vec![(Tail::None, vec![]), (Tail::None, vec![Some(0)])],
                vec![
                    (Tail::None, vec![]),
                    (Tail::None, vec![Some(0)]),
                    (Tail::None, vec![Some(1)]),
                ],
            ] {
                let last = nodes.len() - 1;
                let g = Toy {
                    nodes,
                    outputs: vec![Some(last)],
                };
                let owed = |n: usize| match n {
                    0 => Owed::declared(upstream),
                    n if n == last => Owed {
                        alone: Tail::None,
                        fed: finite(5000),
                    },
                    _ => Owed::declared(Tail::None),
                };
                let tail = graph_tail_of(&g, R, owed);
                assert_eq!(tail.known(), Samples::new(5000), "{upstream:?}");
                assert_eq!(
                    (tail.unknown_nodes(), tail.is_unbounded()),
                    (
                        usize::from(upstream == Tail::Unknown),
                        upstream == Tail::Unbounded
                    ),
                    "{upstream:?}"
                );
            }
        }
    }

    /// An empty graph has no tail rather than an unknown one.
    #[test]
    fn an_empty_graph_has_no_tail() {
        let g = Toy {
            nodes: vec![],
            outputs: vec![],
        };
        assert_eq!(graph_tail(&g, R).samples(), Some(Samples::ZERO));
    }
}
