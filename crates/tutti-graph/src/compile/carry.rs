//! What a plan takes over from the one it replaces: every old-to-new match of
//! the state the executor holds, made once, by key. The stager and the apply
//! read a [`Carry`] and match nothing themselves.

use std::collections::{BTreeMap, BTreeSet};

use crate::compile::ids::{
    AudioDelayId, AudioFeedbackId, EventDelayId, EventFeedbackId, Id, PlanEventTapId, PlanMeterId,
    PlanTapId, Table, TableSlice,
};
use crate::compile::plan::{AudioDelay, AudioDelayKey, EventDelayKey, Plan, PlanId};
use crate::compile::program::{BySlot, Program};
use crate::node::Prepare;
use crate::{EdgeId, EventInputPort, MainOutputBank, PortSource};

/// An old delay or feedback FIFO that has no successor: its queue flushes
/// into `at`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Vanished<I> {
    /// The old plan's id of the FIFO.
    pub(crate) from: I,
    /// The sink its queue flushes into.
    pub(crate) at: EventInputPort,
}

/// For each item of a plan, the old plan's item it continues, by id.
///
/// A sample-rate change carries no ring, feedback or FIFO (time-based state
/// has no meaning at another rate): they all vanish. Taps, meters and lanes
/// are not time, and carry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Carry {
    /// The plan it is against, `None` for no plan: a delta applies only
    /// over the plan it was compiled against.
    pub(crate) prev: Option<PlanId>,
    /// Each PDC ring's predecessor of the same key.
    pub(crate) audio_rings: Table<AudioDelayId, Option<AudioDelayId>>,
    /// Each event delay's predecessor of the same key.
    pub(crate) event_rings: Table<EventDelayId, Option<EventDelayId>>,
    /// Each audio feedback ring's predecessor of the same key.
    pub(crate) audio_fb: Table<AudioFeedbackId, Option<AudioFeedbackId>>,
    /// Each event feedback FIFO's predecessor of the same key.
    pub(crate) event_fb: Table<EventFeedbackId, Option<EventFeedbackId>>,
    /// Each tap's predecessor of the same id.
    pub(crate) taps: Table<PlanTapId, Option<PlanTapId>>,
    /// Each event tap's predecessor of the same id.
    pub(crate) event_taps: Table<PlanEventTapId, Option<PlanEventTapId>>,
    /// Each meter's predecessor of the same id.
    pub(crate) meters: Table<PlanMeterId, Option<PlanMeterId>>,
    /// Each sum lane's predecessor on the same edge, by index: a plain addend
    /// has none.
    pub(crate) lanes: Vec<Option<usize>>,
    /// `(new, old)`: a ring with no predecessor of its key that starts from a
    /// copy of the old ring, which stays (a main output change's channels).
    pub(crate) copy: Vec<(AudioDelayId, AudioDelayId)>,
    /// The old event delays with no successor, in key order.
    pub(crate) vanished_delays: Vec<Vanished<EventDelayId>>,
    /// The old event feedback with no successor, in key order.
    pub(crate) vanished_feedback: Vec<Vanished<EventFeedbackId>>,
}

/// The ids of the rows of `table` by their keys.
fn keyed<I: Id, T, K: Ord>(table: &TableSlice<I, T>, key: impl Fn(&T) -> K) -> BTreeMap<K, I> {
    table.iter_enumerated().map(|(i, t)| (key(t), i)).collect()
}

/// For each row of `new`, the row of `old` with its key; none when `carries`
/// is false.
fn matched<I: Id, T, K: Ord>(
    new: &TableSlice<I, T>,
    old: &BTreeMap<K, I>,
    carries: bool,
    key: impl Fn(&T) -> K,
) -> Table<I, Option<I>> {
    new.iter()
        .map(|t| old.get(&key(t)).copied().filter(|_| carries))
        .collect()
}

/// The rows of `old`, in key order, that no row of `new` continues: all of
/// them when `carries` is false.
fn unmatched<'a, I: Id, T, K: Ord>(
    new: &TableSlice<I, T>,
    old: &'a BTreeMap<K, I>,
    carries: bool,
    key: impl Fn(&T) -> K,
) -> impl Iterator<Item = (&'a K, I)> {
    let kept: BTreeSet<K> = new.iter().map(key).collect();
    old.iter()
        .filter(move |(k, _)| !(carries && kept.contains(*k)))
        .map(|(k, &i)| (k, i))
}

impl Carry {
    /// What `program`, prepared as `prepare`, takes over from `prev`.
    pub(crate) fn between(
        prev: Option<&Plan>,
        program: &Program<BySlot>,
        prepare: &Prepare,
        bank: Option<MainOutputBank>,
    ) -> Self {
        let old = prev.map(|p| &p.program);
        // Time-based state carries only at the rate it was made for.
        let time = prev.is_none_or(|p| p.prepare.sample_rate() == prepare.sample_rate());
        let old_audio = old
            .map(|o| keyed(&o.audio_delays, |d| d.key))
            .unwrap_or_default();
        let old_events = old
            .map(|o| keyed(&o.event_delays, |d| d.key))
            .unwrap_or_default();
        let old_audio_fb = old
            .map(|o| keyed(&o.audio_feedback, |f| f.key))
            .unwrap_or_default();
        let old_event_fb = old
            .map(|o| keyed(&o.event_feedback, |f| f.key))
            .unwrap_or_default();
        let old_taps = old.map(|o| keyed(&o.taps, |k| *k)).unwrap_or_default();
        let old_event_taps = old
            .map(|o| keyed(&o.event_taps, |k| *k))
            .unwrap_or_default();
        let old_meters = old
            .map(|o| keyed(&o.meters, |(k, _)| *k))
            .unwrap_or_default();
        let old_lanes: BTreeMap<EdgeId, usize> = old
            .iter()
            .flat_map(|o| o.sum_lanes.iter().enumerate())
            .filter_map(|(i, id)| Some(((*id)?, i)))
            .collect();

        let by_source = old
            .map(|o| ring_by_source(&o.audio_delays))
            .unwrap_or_default();
        let copy = program
            .audio_delays
            .iter_enumerated()
            .filter(|(_, d)| time && !old_audio.contains_key(&d.key))
            .filter_map(|(n, d)| {
                copy_origin(bank, &d.key, &old_audio, &by_source).map(|from| (n, from))
            })
            .collect();
        let vanished_delays = unmatched(&program.event_delays, &old_events, time, |d| d.key)
            .filter_map(|(key, from)| match key {
                EventDelayKey::Edge { at, .. } => Some(Vanished { from, at: *at }),
                _ => None,
            })
            .collect();
        let vanished_feedback = unmatched(&program.event_feedback, &old_event_fb, time, |f| f.key)
            .map(|(key, from)| Vanished { from, at: key.at })
            .collect();

        Self {
            prev: prev.map(|p| p.id.0),
            audio_rings: matched(&program.audio_delays, &old_audio, time, |d| d.key),
            event_rings: matched(&program.event_delays, &old_events, time, |d| d.key),
            audio_fb: matched(&program.audio_feedback, &old_audio_fb, time, |f| f.key),
            event_fb: matched(&program.event_feedback, &old_event_fb, time, |f| f.key),
            taps: matched(&program.taps, &old_taps, true, |k| *k),
            event_taps: matched(&program.event_taps, &old_event_taps, true, |k| *k),
            meters: matched(&program.meters, &old_meters, true, |(k, _)| *k),
            lanes: program
                .sum_lanes
                .iter()
                .map(|id| id.and_then(|id| old_lanes.get(&id)).copied())
                .collect(),
            copy,
            vanished_delays,
            vanished_feedback,
        }
    }
}

/// The longest ring that aligns each source to an output, whichever channel it
/// fed; of equal lengths, the lowest key. The choice is a function of the set
/// of rings, not of their order in the plan: what a source on a new channel
/// starts from, with none of its history zero-filled.
fn ring_by_source(
    delays: &TableSlice<AudioDelayId, AudioDelay>,
) -> BTreeMap<PortSource, AudioDelayId> {
    let mut by_source: BTreeMap<PortSource, AudioDelayId> = BTreeMap::new();
    for (i, d) in delays.iter_enumerated() {
        let AudioDelayKey::Output { from, .. } = d.key else {
            continue;
        };
        let better = |j: &AudioDelayId| {
            let best = &delays[*j];
            (d.len, std::cmp::Reverse(d.key)) > (best.len, std::cmp::Reverse(best.key))
        };
        if by_source.get(&from).is_none_or(better) {
            by_source.insert(from, i);
        }
    }
    by_source
}

/// The old ring an output channel's ring starts from when no old ring has its
/// own key: the old main output's channel `c` for the bank's channel `base + c`,
/// and, in a main output change, for a main output channel of a source the old
/// plan aligned on another channel, that channel's ring (a source's delayed
/// signal does not depend on the channel it feeds).
fn copy_origin(
    bank: Option<MainOutputBank>,
    key: &AudioDelayKey,
    old: &BTreeMap<AudioDelayKey, AudioDelayId>,
    by_source: &BTreeMap<PortSource, AudioDelayId>,
) -> Option<AudioDelayId> {
    let AudioDelayKey::Output { channel, from } = *key else {
        return None;
    };
    match bank {
        Some(bank) if usize::from(channel) >= bank.base => {
            let was = channel - u16::try_from(bank.base).ok()?;
            old.get(&AudioDelayKey::Output { channel: was, from })
                .copied()
        }
        // Only in a main output change: any other recompile keeps a ring by its key.
        Some(_) if !old.contains_key(key) => by_source.get(&from).copied(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::ids::{Table, id_at};
    use crate::testing::{Kind, prepare, shapes_of, spec_of};
    use crate::{
        Edge, EdgeId, EventEdge, EventOutputPort, FeedbackFrom, GraphSpec, InputPort, MeterId,
        MeterSpec, NodeKey, OutputPort, PortSink, PortSource, TapId, TapSpec, compile,
    };
    use tutti_units::Samples;

    fn ring(channel: u16, from: PortSource, len: usize) -> AudioDelay {
        AudioDelay {
            key: AudioDelayKey::Output { channel, from },
            len: Samples::new(len),
        }
    }

    fn source(node: u64) -> PortSource {
        PortSource::Node(OutputPort::new(NodeKey(node), 0))
    }

    /// The longest ring of a source is the one a new channel copies, and of
    /// equal lengths the lowest key, whatever order the plan lists them in:
    /// two renders of one commit never copy different rings.
    #[test]
    fn the_longest_ring_of_a_source_wins_and_the_lowest_key_breaks_a_tie() {
        let s = source(1);
        let delays = [
            ring(3, s, 20),
            ring(2, s, 30),
            ring(1, s, 30),
            ring(0, s, 10),
            ring(0, source(2), 5),
        ];
        let by = ring_by_source(&Table::from(delays.to_vec()));
        assert_eq!(
            by[&s],
            id_at(2),
            "channel 1: of the two 30-frame rings, the lower key"
        );
        assert_eq!(
            by[&source(2)],
            id_at(4),
            "a source of its own has its own ring"
        );
        let mut order: Vec<usize> = (0..delays.len()).collect();
        for _ in 0..delays.len() {
            order.rotate_left(1);
            let shuffled: Table<AudioDelayId, AudioDelay> =
                order.iter().map(|&i| delays[i]).collect();
            let by = ring_by_source(&shuffled);
            assert_eq!(
                shuffled[by[&s]].key, delays[2].key,
                "the same ring in any listing order"
            );
        }
    }

    /// A ring that is not an output's is no source's origin.
    #[test]
    fn only_output_rings_are_origins() {
        let at = crate::InputPort::new(NodeKey(9), 0);
        let delays = [AudioDelay {
            key: AudioDelayKey::Edge {
                at,
                from: source(1),
            },
            len: Samples::new(40),
        }];
        assert!(ring_by_source(&Table::from(delays.to_vec())).is_empty());
    }

    const BLOCK: usize = 64;

    fn out(node: u64) -> PortSource {
        PortSource::Node(OutputPort::new(NodeKey(node), 0))
    }

    /// A graph with every kind of state a plan carries: PDC rings (audio and
    /// event), audio and event feedback, a tap, a meter and a driven lane.
    /// Two emitters each fan into a sink beside a late path, so each has an
    /// event delay; the sink `40` runs before `14`, which reads it, so the
    /// plan emits `40`'s delay first and `14`'s second, the reverse of their
    /// key order. `trimmed` drops those two edges and the feedback edges.
    fn graph(trimmed: bool) -> (GraphSpec, crate::Shapes) {
        let kinds: BTreeMap<NodeKey, Kind> = [
            (
                1,
                Kind::Const {
                    value: 1.0,
                    width: 1,
                },
            ),
            (
                2,
                Kind::Gain {
                    gain: 0.5,
                    width: 1,
                },
            ),
            (3, Kind::Lag { latency: 8 }),
            (
                4,
                Kind::Const {
                    value: 2.0,
                    width: 1,
                },
            ),
            (5, Kind::Sum { inputs: 2 }),
            (
                6,
                Kind::GainDriver {
                    period: 64,
                    latency: 0,
                },
            ),
            (
                12,
                Kind::Emitter {
                    period: 16,
                    phase: 0,
                },
            ),
            (13, Kind::EventLag { latency: 40 }),
            (14, Kind::Consumer { inputs: 1 }),
            (15, Kind::EventLag { latency: 0 }),
            (
                20,
                Kind::Emitter {
                    period: 16,
                    phase: 1,
                },
            ),
            (
                21,
                Kind::Emitter {
                    period: 16,
                    phase: 2,
                },
            ),
            (40, Kind::EventLag { latency: 0 }),
        ]
        .into_iter()
        .map(|(k, kind)| (NodeKey(k), kind))
        .collect();
        let mut spec = GraphSpec::new(crate::Topology::default());
        for (k, kind) in &kinds {
            spec.topology.nodes.insert(*k, spec_of(kind));
        }
        let at = |node: u64, port| InputPort::new(NodeKey(node), port);
        let t = &mut spec.topology;
        t.connect(at(2, 0), Edge::Direct(out(1)));
        t.connect(at(3, 0), Edge::Direct(out(2)));
        t.connect(at(5, 0), Edge::Direct(out(3)));
        t.connect(at(5, 1), Edge::Direct(out(4)));
        t.set_outputs(vec![out(5)]);
        t.meters
            .insert(MeterId::of(1), MeterSpec::raw(vec![out(5)]));
        t.taps.insert(TapId::of(1), TapSpec::aligned(vec![out(4)]));
        let lane = EdgeId {
            sink: PortSink::Port(at(5, 1)),
            from: Edge::Direct(out(4)),
        };
        spec.connect_edge_gain(lane, EventOutputPort::new(NodeKey(6), 0));
        let from = |node: u64| EventOutputPort::new(NodeKey(node), 0);
        let into = |node: u64| crate::EventInputPort::new(NodeKey(node), 0);
        for (sink, source) in [(13, 12), (40, 13), (14, 40), (15, 40)] {
            spec.connect_events(into(sink), EventEdge::Direct(from(source)));
        }
        if !trimmed {
            spec.connect_events(into(40), EventEdge::Direct(from(21)));
            spec.connect_events(into(14), EventEdge::Direct(from(20)));
            spec.topology.connect(
                at(2, 0),
                Edge::Feedback(FeedbackFrom::one_block(
                    OutputPort::new(NodeKey(5), 0),
                    Samples::new(BLOCK),
                )),
            );
            spec.connect_events(into(13), EventEdge::feedback(from(15), Samples::new(BLOCK)));
        }
        let shapes = shapes_of(&kinds);
        (spec, shapes)
    }

    /// Two compiles against one plan give the same delta, carry included, and
    /// the old FIFOs no key continues are listed in key order, not in the
    /// order the plan emitted them. The graph exercises every table: each has
    /// a row that carries.
    #[test]
    fn a_delta_is_a_function_of_its_graph_and_the_plan_it_replaces() {
        let (before, shapes) = graph(false);
        let (prev, _) = compile(
            &before.validate().expect("valid"),
            &shapes,
            &prepare(BLOCK),
            None,
        )
        .expect("compiles");
        let (after, shapes) = graph(true);
        let valid = after.validate().expect("valid");
        let (_, first) = compile(&valid, &shapes, &prepare(BLOCK), Some(&prev)).expect("compiles");
        let (_, second) = compile(&valid, &shapes, &prepare(BLOCK), Some(&prev)).expect("compiles");
        assert_eq!(first, second);

        let carry = &first.carry;
        let carried = |t: &[Option<usize>]| t.iter().flatten().count();
        assert!(carried(&carry.lanes) > 0, "a lane carries");
        assert!(carry.taps.iter().flatten().count() > 0, "a tap carries");
        assert!(carry.meters.iter().flatten().count() > 0, "a meter carries");
        assert!(
            carry.audio_rings.iter().flatten().count() > 0,
            "a ring carries"
        );
        assert!(
            carry.event_rings.iter().flatten().count() > 0,
            "a FIFO carries"
        );
        assert_eq!(carry.vanished_feedback.len(), 1, "the event feedback goes");
        assert_eq!(carry.audio_fb.len(), 0, "and the audio feedback with it");

        let keys: Vec<_> = carry
            .vanished_delays
            .iter()
            .map(|v| prev.program.event_delays[v.from].key)
            .collect();
        assert_eq!(keys.len(), 2, "both emitters' delays vanish: {keys:?}");
        assert!(keys[0] < keys[1], "in key order: {keys:?}");
        assert!(
            carry.vanished_delays[0].from > carry.vanished_delays[1].from,
            "though the plan emitted them the other way round"
        );
    }
}
