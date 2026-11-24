//! `Node::poll_parked`: the executor answers a parked node's handoffs without
//! rendering it. Called once per block for a parked node, never for a node
//! that is not parked, and `process` is not called meanwhile. A real recall
//! (a `tutti_rt` handoff) is answered under `RtScope`.

use tutti_graph::testing as common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::prepare;
use tutti_buffers::ChannelLayout;
use tutti_graph::{
    Cx, Editor, Executor, Io, Node, NodeKey, NodeMode, OutputPort, PortSource, Prepare, Shape,
    Status, Unforkable,
};
use tutti_rt::{Handoff, Rt, handoff};

const NODE: NodeKey = NodeKey(1);
const BLOCK: usize = 32;

/// A source that holds a value on a handoff, counting its calls and its
/// parked polls. `poll_parked` and `process` both answer the handoff the way
/// a plugin client does: `Rt::poll`.
struct Holder {
    rt: Rt<u32>,
    processed: Arc<AtomicUsize>,
    parked: Arc<AtomicUsize>,
    returned: Arc<AtomicUsize>,
}

impl Node for Holder {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.processed.fetch_add(1, Ordering::Relaxed);
        let returned = &self.returned;
        let _ = self.rt.poll(|_| {
            returned.fetch_add(1, Ordering::Relaxed);
        });
        io.output(0).fill(0.25);
        Status::Modified
    }
    fn poll_parked(&mut self) {
        self.parked.fetch_add(1, Ordering::Relaxed);
        let returned = &self.returned;
        let _ = self.rt.poll(|_| {
            returned.fetch_add(1, Ordering::Relaxed);
        });
    }
    fn reset(&mut self) {}
}

struct Rig {
    ed: Editor,
    exec: Executor,
    hand: Handoff<u32>,
    processed: Arc<AtomicUsize>,
    parked: Arc<AtomicUsize>,
    returned: Arc<AtomicUsize>,
}

fn rig() -> Rig {
    let (hand, rt) = handoff::<u32>();
    let (processed, parked, returned) = (Arc::default(), Arc::default(), Arc::default());
    let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
    ed.insert(
        NODE,
        "holder",
        Unforkable(Holder {
            rt,
            processed: Arc::clone(&processed),
            parked: Arc::clone(&parked),
            returned: Arc::clone(&returned),
        }),
    );
    ed.wire_outputs(vec![PortSource::Node(OutputPort::new(NODE, 0))])
        .expect("the sources exist");
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    Rig {
        ed,
        exec,
        hand,
        processed,
        parked,
        returned,
    }
}

fn render(exec: &mut Executor) {
    common::block(exec, BLOCK, 1);
}

fn apply(r: &mut Rig) {
    r.ed.commit().expect("commits");
    r.exec.apply_pending();
    r.ed.collect();
}

/// Renders until the node is parked for good: its fade done, `process` no
/// longer called.
fn park(r: &mut Rig) {
    r.ed.set_mode(NODE, NodeMode::Bypassed).expect("a node");
    apply(r);
    for _ in 0..40 {
        render(&mut r.exec);
    }
}

fn count(c: &AtomicUsize) -> usize {
    c.load(Ordering::Relaxed)
}

/// **A parked node is polled once per block and not processed; a node that is
/// not parked is never polled.**
#[test]
fn poll_parked_runs_for_a_parked_node_and_process_does_not() {
    let mut r = rig();
    for _ in 0..4 {
        render(&mut r.exec);
    }
    assert_eq!(count(&r.parked), 0, "an active node is never polled parked");
    assert_eq!(count(&r.processed), 4);

    park(&mut r);
    let (p0, q0) = (count(&r.processed), count(&r.parked));
    for _ in 0..5 {
        render(&mut r.exec);
    }
    assert_eq!(count(&r.processed), p0, "a parked node is not processed");
    assert_eq!(count(&r.parked), q0 + 5, "polled once per parked block");

    r.ed.set_mode(NODE, NodeMode::Active).expect("a node");
    apply(&mut r);
    render(&mut r.exec);
    let q1 = count(&r.parked);
    for _ in 0..5 {
        render(&mut r.exec);
    }
    assert_eq!(
        count(&r.parked),
        q1,
        "an unparked node is not polled parked"
    );
    assert_eq!(count(&r.processed), p0 + 6);
}

/// **A recall raised while the node is parked is answered by the next block,
/// without allocating**, and the handed-back value is what was lent.
#[test]
fn a_recall_is_answered_by_a_parked_node_without_allocating() {
    let mut r = rig();
    assert!(r.hand.lend(7).is_ok());
    render(&mut r.exec);
    park(&mut r);
    assert!(r.hand.recall().is_none(), "the node holds it");
    assert_eq!(count(&r.returned), 0);
    render(&mut r.exec);
    assert_eq!(count(&r.returned), 1, "stopped once on its way back");
    assert_eq!(r.hand.take(), Some(7), "returned to the control side");
}

/// Four holders on four outputs, two of them bypassed, rendered for a while on
/// `workers` participants: every output block's bits, and each holder's
/// `(processed, parked)` counts.
fn four_holders(workers: usize) -> (Vec<Vec<u32>>, Vec<(usize, usize)>) {
    let (mut ed, mut exec) = Editor::new(prepare(BLOCK));
    common::pooled(&mut exec, workers);
    let mut counts = Vec::new();
    let mut outputs = Vec::new();
    for k in 1..=4u64 {
        let (_hand, rt) = handoff::<u32>();
        let (processed, parked) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        ed.insert(
            NodeKey(k),
            "holder",
            Unforkable(Holder {
                rt,
                processed: Arc::clone(&processed),
                parked: Arc::clone(&parked),
                returned: Arc::default(),
            }),
        );
        outputs.push(PortSource::Node(OutputPort::new(NodeKey(k), 0)));
        counts.push((processed, parked));
    }
    ed.wire_outputs(outputs).expect("the sources exist");
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    for k in [2u64, 4] {
        ed.set_mode(NodeKey(k), NodeMode::Bypassed).expect("a node");
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    let mut heard = Vec::new();
    for _ in 0..60 {
        heard.extend(common::bits(&common::block(&mut exec, BLOCK, 4)));
    }
    (
        heard,
        counts.iter().map(|(p, q)| (count(p), count(q))).collect(),
    )
}

/// **Parked polls run on the pool, and the render is bit for bit the one-participant
/// one:** the two bypassed holders are polled every parked block, the other two
/// never, on every worker count.
#[test]
fn a_parked_poll_on_the_pool_renders_what_the_one_participant_executor_does() {
    let (_, serial_counts) =
        common::agree_across_workers(common::WORKERS, four_holders, Clone::clone);
    assert!(
        serial_counts[1].1 > 0 && serial_counts[3].1 > 0,
        "polled parked"
    );
    assert_eq!(serial_counts[0].1, 0, "an active node is not polled parked");
    assert!(serial_counts[1].0 < 60, "a parked node is not processed");
}
