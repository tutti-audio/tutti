//! The session's node modes (doc 013, "Bypass and mute are graph state", M2's
//! session half): `ModeTarget`, `key.mode()`, over the executor's
//! `NodeMode` (M1, `tutti-graph/tests/mixing/mode.rs`, pins the fades, the bypass
//! line and the parking; this suite pins what the session adds: the write
//! reaches the running unit, is a control and not an edit, reads back through
//! `get`, and is refused for a node that is not there).
//!
//! Every figure is analytic: a constant `A` through a node that halves it, so
//! the active output is `A / 2`, the bypassed output is `A` (the node has no
//! latency, so its aligned input is its input) and the muted output is `+0.0`,
//! each exact in `f32`.

use tutti_buffers::ChannelLayout;
use tutti_buffers::InterleavedMut;
use tutti_graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_nodes::testing::Const;
use tutti_session::{InputPort, Key, MainOutput, NodeMode, OutputPort, Session, ValueErrorKind};

const A: f32 = 0.25;
const BLOCK: usize = 64;
/// More than `DECLICK` (240 frames at 48 kHz): a mode set before these blocks has
/// finished its fade by the last one.
const SETTLE_BLOCKS: usize = 6;

/// `out = in * 0.5`: a node whose output differs from its input, so a bypass
/// is audible.
#[derive(Clone)]
struct Halve;

impl Node for Halve {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for (o, &x) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *o = x * 0.5;
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

struct Rig {
    s: Session,
    halve: Key<ForkByClone<Halve>>,
}

/// `Const(A) -> Halve -> output 0`, committed once.
fn rig() -> Rig {
    let mut s = Session::builder().build().expect("builds");
    let g = s.graph();
    let a = g.add(ForkByClone(Const::mono(A)));
    let halve = g.add(ForkByClone(Halve));
    g.set_sources(InputPort::new(halve, 0), [OutputPort::new(a, 0)])
        .unwrap()
        .unfaded();
    g.set_sources(MainOutput::channel(0), [OutputPort::new(halve, 0)])
        .unwrap()
        .unfaded();
    let _ = s.update();
    assert!(!s.graph_ref().has_uncommitted_edits());
    Rig { s, halve }
}

impl Rig {
    /// Runs `update` and enough blocks for a fade to finish; returns the last
    /// block's channel 0.
    fn settled(&mut self) -> Vec<f32> {
        let _ = self.s.update();
        let mut block = vec![f32::NAN; BLOCK * 2];
        for _ in 0..SETTLE_BLOCKS {
            self.s
                .render(&mut InterleavedMut::new(&mut block, ChannelLayout::STEREO).unwrap());
        }
        block.iter().step_by(2).copied().collect()
    }
}

fn all_are(block: &[f32], want: f32) -> bool {
    block.iter().all(|x| x.to_bits() == want.to_bits())
}

/// Each mode's steady output, through the session: active `A / 2`, bypassed
/// `A`, muted `+0.0`, and active again `A / 2`.
#[test]
fn each_mode_reaches_the_running_node() {
    let mut r = rig();
    assert!(all_are(&r.settled(), A * 0.5), "active halves");

    r.s.set(r.halve.mode().to(NodeMode::Bypassed)).unwrap();
    assert!(all_are(&r.settled(), A), "bypassed passes the input");

    r.s.set(r.halve.mode().to(NodeMode::Muted)).unwrap();
    assert!(all_are(&r.settled(), 0.0), "muted is +0.0");

    r.s.set(r.halve.mode().to(NodeMode::Active)).unwrap();
    assert!(all_are(&r.settled(), A * 0.5), "active again");
}

/// A mode reads back through `get` the moment it is written, before any
/// update, and is `Active` for a node never given one.
#[test]
fn mode_reads_back_what_was_set() {
    let mut r = rig();
    assert_eq!(r.s.get(r.halve.mode()).unwrap(), NodeMode::Active);
    r.s.set(r.halve.mode().to(NodeMode::Bypassed)).unwrap();
    assert_eq!(r.s.get(r.halve.mode()).unwrap(), NodeMode::Bypassed);
    r.s.set(r.halve.mode().modify(|_| NodeMode::Muted)).unwrap();
    assert_eq!(r.s.get(r.halve.mode()).unwrap(), NodeMode::Muted);
    let _ = r.settled();
    assert_eq!(r.s.get(r.halve.mode()).unwrap(), NodeMode::Muted);
}

/// A mode is a control: setting one marks nothing edited.
#[test]
fn a_mode_is_not_an_edit() {
    let mut r = rig();
    r.s.set(r.halve.mode().to(NodeMode::Bypassed)).unwrap();
    let _ = r.s.update();
    assert!(
        !r.s.graph_ref().has_uncommitted_edits(),
        "a mode marks nothing edited"
    );
    let _ = r.settled();
}

/// A node that is no longer in the graph is refused at the call, naming the
/// mode target, whatever the value, and its mode is no longer readable.
#[test]
fn a_removed_node_is_refused() {
    let mut r = rig();
    r.s.set(r.halve.mode().to(NodeMode::Muted)).unwrap();
    r.s.remove(r.halve).unwrap();
    let refused = r.s.set(r.halve.mode().to(NodeMode::Bypassed)).unwrap_err();
    assert_eq!(refused.kind, ValueErrorKind::NoSuchTarget);
    assert!(refused.target.is_some_and(|t| t.is(r.halve.mode())));
    assert_eq!(
        r.s.get(r.halve.mode()).unwrap_err().kind,
        ValueErrorKind::NoSuchTarget
    );
}
