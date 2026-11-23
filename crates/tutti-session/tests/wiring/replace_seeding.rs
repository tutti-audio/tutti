//! `Graph::replace` seeds the incoming node's params from the outgoing node's:
//! an address both declare in one unit at one initial value keeps the value it
//! was set to, a changed declaration takes its new initial, and an address the
//! incoming node lacks is dropped. The seeding is no write an app made.

use tutti_buffers::{ChannelLayout, InterleavedMut};
use tutti_graph::{
    Cx, DeclaredUnit, Io, Node, ParamNode, ParamSet, Params, Prepare, Shape, Status,
};
use tutti_rt::Param;
use tutti_session::{DetachedSession, Replacement, Session, SessionEvent, ValueErrorKind};
use tutti_units::{Hz, Q, SampleRate, Seconds, UnitParam};

fn detached() -> DetachedSession {
    Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds")
}

/// A node with a `Hz` cutoff and a `Q`, the second declared only when `with_q`.
#[derive(Clone)]
struct Tone {
    cutoff: Param<Hz>,
    q: Param<Q>,
    with_q: bool,
}

impl Tone {
    fn new(cutoff: f64, q: f64) -> Self {
        Self {
            cutoff: Param::new(Hz::new(cutoff)),
            q: Param::new(Q::new(q)),
            with_q: true,
        }
    }

    /// The same node declaring only its cutoff.
    fn without_q(mut self) -> Self {
        self.with_q = false;
        self
    }
}

impl Node for Tone {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

impl ParamNode for Tone {
    fn param_set(&self) -> ParamSet {
        if self.with_q {
            return self.declared_set();
        }
        ParamSet::builder()
            .param(
                DeclaredUnit::of::<Hz>(),
                UnitParam::Cutoff,
                self.cutoff.cell(),
            )
            .build()
    }
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.cutoff.detach();
        fork.q.detach();
        fork
    }
}

tutti_graph::declare_params!([] Tone, n { Cutoff => Hz = &n.cutoff, Q => Q = &n.q });

/// A node whose cutoff is in seconds: the address `Tone`'s cutoff has, in
/// another unit.
#[derive(Clone)]
struct Delay {
    cutoff: Param<Seconds>,
}

impl Node for Delay {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

impl ParamNode for Delay {
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.cutoff.detach();
        fork
    }
}

tutti_graph::declare_params!([] Delay, n { Cutoff => Seconds = &n.cutoff });

/// **A replace by a node declared the same keeps the value a param was set
/// to**: 800 Hz set on the outgoing cutoff reads 800 Hz on the incoming one.
#[test]
fn a_replace_with_the_same_declaration_keeps_the_set_value() {
    let mut s = detached();
    let tone = s.graph().add(Params(Tone::new(1_000.0, 0.7)));
    s.set(tone.cutoff().to(Hz::new(800.0))).unwrap();

    let tone = s
        .graph()
        .replace(tone, Replacement::new(Params(Tone::new(1_000.0, 0.7))))
        .unwrap_or_else(|_| panic!("a detached graph swaps at once"));

    assert_eq!(s.get(tone.cutoff()).unwrap(), Hz::new(800.0));
    assert_eq!(s.get(tone.q()).unwrap(), Q::new(0.7), "the rest stays");
}

/// **A param whose declared initial changed takes the new initial, and another
/// in the same replace whose declaration did not change keeps its value.**
#[test]
fn a_changed_declaration_takes_its_new_initial_beside_an_unchanged_one() {
    let mut s = detached();
    let tone = s.graph().add(Params(Tone::new(1_000.0, 0.7)));
    s.set((tone.cutoff().to(Hz::new(800.0)), tone.q().to(Q::new(2.0))))
        .unwrap();

    let tone = s
        .graph()
        .replace(tone, Replacement::new(Params(Tone::new(1_000.0, 3.0))))
        .unwrap_or_else(|_| panic!("a detached graph swaps at once"));

    assert_eq!(s.get(tone.cutoff()).unwrap(), Hz::new(800.0), "unchanged");
    assert_eq!(s.get(tone.q()).unwrap(), Q::new(3.0), "changed");
}

/// **A value the incoming param does not take is not seeded**: a cell holding
/// a NaN (a write never leaves one) stays at the incoming initial, never
/// clamped.
#[test]
fn a_value_a_write_refuses_is_not_seeded() {
    let mut s = detached();
    let outgoing = Tone::new(1_000.0, 0.7);
    let cutoff = outgoing.cutoff.clone();
    let tone = s.graph().add(Params(outgoing));
    cutoff.store(Hz::new(f64::NAN));

    let tone = s
        .graph()
        .replace(tone, Replacement::new(Params(Tone::new(1_000.0, 0.7))))
        .unwrap_or_else(|_| panic!("a detached graph swaps at once"));

    assert_eq!(s.get(tone.cutoff()).unwrap(), Hz::new(1_000.0));
}

/// **An address in another unit is not seeded**: the cutoff of `Tone` in Hz
/// does not become the cutoff of `Delay` in seconds, and the key of the node
/// that was replaced names no param of the new type.
#[test]
fn an_address_in_another_unit_is_not_seeded() {
    let mut s = detached();
    let tone = s.graph().add(Params(Tone::new(1_000.0, 0.7)));
    s.set(tone.cutoff().to(Hz::new(800.0))).unwrap();

    let delay = s
        .graph()
        .replace(
            tone,
            Replacement::new(Params(Delay {
                cutoff: Param::new(Seconds::new(0.25)),
            })),
        )
        .unwrap_or_else(|_| panic!("a detached graph swaps at once"));

    assert_eq!(s.get(delay.cutoff()).unwrap(), Seconds::new(0.25));
    let refused = s.get(tone.cutoff()).unwrap_err();
    assert_eq!(refused.kind, ValueErrorKind::NoSuchTarget);
}

/// **An address the incoming node lacks is dropped**: a call on it is
/// `NoSuchTarget`, and the address the node still has keeps its value.
#[test]
fn an_address_the_incoming_node_lacks_is_no_such_target() {
    let mut s = detached();
    let tone = s.graph().add(Params(Tone::new(1_000.0, 0.7)));
    s.set((tone.cutoff().to(Hz::new(800.0)), tone.q().to(Q::new(2.0))))
        .unwrap();

    let tone = s
        .graph()
        .replace(
            tone,
            Replacement::new(Params(Tone::new(1_000.0, 0.7).without_q())),
        )
        .unwrap_or_else(|_| panic!("a detached graph swaps at once"));

    assert_eq!(s.get(tone.cutoff()).unwrap(), Hz::new(800.0));
    let refused = s.set(tone.q().to(Q::new(1.0))).unwrap_err();
    assert_eq!(refused.kind, ValueErrorKind::NoSuchTarget);
    assert!(refused.target.expect("it names the key").is(tone.q()));
}

/// **A replace that waits for a re-prepare seeds the same way**: the key reads
/// the seeded values from the call on, and they are still there when the swap
/// lands.
#[test]
fn a_parked_replace_seeds_the_same_way() {
    let mut s = detached();
    let mut side = s.graph().take_audio_side();
    let tone = s.graph().add(Params(Tone::new(1_000.0, 0.7)));
    assert!(s.update().is_empty());
    s.set((tone.cutoff().to(Hz::new(800.0)), tone.q().to(Q::new(2.0))))
        .unwrap();

    s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
    let tone = s
        .graph()
        .replace(tone, Replacement::new(Params(Tone::new(1_000.0, 3.0))))
        .expect("the node is in the graph");
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 1, "parked");
    assert_eq!(s.get(tone.cutoff()).unwrap(), Hz::new(800.0));
    assert_eq!(s.get(tone.q()).unwrap(), Q::new(3.0));

    let mut out = [0.0f32];
    let mut landed = false;
    for _ in 0..16 {
        side.tick(InterleavedMut::one_frame(&mut out).unwrap());
        landed |= s
            .update()
            .iter()
            .any(|e| matches!(e, SessionEvent::Replaced { .. }));
        if landed {
            break;
        }
    }
    assert!(landed, "the swap lands once the graph resumes");
    assert_eq!(s.get(tone.cutoff()).unwrap(), Hz::new(800.0));
    assert_eq!(s.get(tone.q()).unwrap(), Q::new(3.0));
}

/// **A second replace of a waiting node seeds from the first one's node**: a
/// value set while the first waited reaches the second.
#[test]
fn a_replace_behind_a_waiting_one_seeds_from_it() {
    let mut s = detached();
    let _side = s.graph().take_audio_side();
    let tone = s.graph().add(Params(Tone::new(1_000.0, 0.7)));
    assert!(s.update().is_empty());
    s.graph().set_sample_rate(SampleRate::SR_44K1).unwrap();
    let tone = s
        .graph()
        .replace(tone, Replacement::new(Params(Tone::new(1_000.0, 0.7))))
        .expect("the node is in the graph");
    s.set(tone.cutoff().to(Hz::new(800.0))).unwrap();
    let tone = s
        .graph()
        .replace(tone, Replacement::new(Params(Tone::new(1_000.0, 0.7))))
        .expect("the node is in the graph");
    assert_eq!(s.graph_ref().testing().waiting_replacements(), 2);
    assert_eq!(s.get(tone.cutoff()).unwrap(), Hz::new(800.0));
}

/// **The seeding is no write an app made**: a watcher on the seeded param sees
/// no change from the replace.
#[test]
fn seeding_is_no_change_a_watcher_sees() {
    let mut s = detached();
    let tone = s.graph().add(Params(Tone::new(1_000.0, 0.7)));
    let mut w = s.watch(tone.cutoff()).unwrap();
    s.set(tone.cutoff().to(Hz::new(800.0))).unwrap();
    assert!(w.changes().unwrap().is_some(), "the set is seen once");

    let tone = s
        .graph()
        .replace(tone, Replacement::new(Params(Tone::new(1_000.0, 0.7))))
        .unwrap_or_else(|_| panic!("a detached graph swaps at once"));

    assert_eq!(s.get(tone.cutoff()).unwrap(), Hz::new(800.0), "it seeded");
    assert!(w.changes().unwrap().is_none(), "and no change was seen");
}
