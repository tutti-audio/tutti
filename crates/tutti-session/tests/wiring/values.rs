//! The session's value verbs: `set` and `get` over targets, `Knobs`, and the errors they
//! answer. Every figure is an analytic one: a write is read back as written.

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use tutti_graph::{
    Cx, FadeLaw, Io, Node, ParamNode, ParamSet, Params, Prepare, Shape, Status, Unforkable,
};
use tutti_nodes::{
    CompressorControls, CompressorNode, KeySource, MultibandCompressorNode, SvfFilterNode, SvfType,
};
use tutti_session::{
    Assign, DetachedSession, Knobs, ParamTarget, Session, SetRefusal, TargetInfo, ValueErrorKind,
};
use tutti_types::{BusTopology, ChannelLayout, Param};
use tutti_units::{Db, Hz, Q, SampleRate, Seconds, UnitAddr, UnitParam};

fn detached() -> DetachedSession {
    Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds")
}

fn filter(s: &mut DetachedSession, cutoff: f64) -> tutti_session::Key<SvfFilterNode<f32>> {
    s.graph()
        .add(SvfFilterNode::<f32>::new(
            SvfType::LowPass,
            Hz(cutoff),
            Q(0.7),
        ))
        .0
}

fn compressor(s: &mut DetachedSession) -> tutti_session::Key<CompressorNode> {
    s.graph()
        .add(CompressorNode::new(
            BusTopology::MONO,
            KeySource::Sidechain,
            CompressorControls::DEFAULT,
        ))
        .0
}

/// A node whose `Seconds` params are `f64` cells.
fn multiband(s: &mut DetachedSession) -> tutti_session::Key<MultibandCompressorNode<f32>> {
    s.graph()
        .add(MultibandCompressorNode::<f32>::new(ChannelLayout::STEREO, 2).unwrap())
        .0
}

fn plus_one(h: &Hz) -> Hz {
    Hz(h.0 + 1.0)
}

/// **A batch runs its `modify`s in argument order, each over the value the call has built
/// so far for its target**: two edits add two, and an assignment then an edit gives the
/// edit of the assigned value.
#[test]
fn a_batch_runs_in_argument_order_over_the_value_built_so_far() {
    let mut s = detached();
    let lp = filter(&mut s, 1_000.0);
    let cutoff = lp.cutoff();

    s.set((cutoff.modify(plus_one), cutoff.modify(plus_one)))
        .unwrap();
    assert_eq!(s.get(cutoff).unwrap(), Hz(1_002.0));

    s.set((cutoff.to(Hz(1.0)), cutoff.modify(plus_one)))
        .unwrap();
    assert_eq!(s.get(cutoff).unwrap(), Hz(2.0));

    // An edit then an assignment: the assignment is the final value.
    s.set((cutoff.modify(plus_one), cutoff.to(Hz(50.0))))
        .unwrap();
    assert_eq!(s.get(cutoff).unwrap(), Hz(50.0));
}

/// **A batch over several targets leaves each at its final value**: a read after the call
/// sees, for each target, what its last item built.
#[test]
fn a_batch_over_several_targets_lands_each_final_value() {
    let mut s = detached();
    let lp = filter(&mut s, 1_000.0);
    s.set((
        lp.cutoff().to(Hz(300.0)),
        lp.q().to(Q(2.0)),
        lp.cutoff().modify(plus_one),
    ))
    .unwrap();
    assert_eq!(s.get((lp.cutoff(), lp.q())).unwrap(), (Hz(301.0), Q(2.0)));
}

/// **A batch is atomic, and `at` is the first failing item's position with a `None`
/// counted**: nothing lands when a later item fails, and a failure later in the argument
/// than the first one does not move `at`.
#[test]
fn a_failing_item_lands_nothing_and_at_counts_a_none() {
    let mut s = detached();
    let lp = filter(&mut s, 1_000.0);
    let gone = filter(&mut s, 1_000.0);
    s.graph().remove(gone).unwrap();
    let nothing: Option<Assign<ParamTarget<Hz>>> = None;

    let e = s
        .set((
            lp.cutoff().to(Hz(2.0)),
            nothing.clone(),
            gone.cutoff().to(Hz(3.0)),
        ))
        .unwrap_err();
    assert_eq!(e.at, 2, "the None is position 1");
    assert_eq!(e.kind, ValueErrorKind::NoSuchTarget);
    assert!(e.target.expect("it names the target").is(gone.cutoff()));
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz(1_000.0), "nothing landed");

    let e = s
        .set((nothing, gone.cutoff().to(Hz(3.0)), gone.q().to(Q(1.0))))
        .unwrap_err();
    assert_eq!(e.at, 1, "the first failing item, not the last");

    // Positions flatten nested shapes in order.
    let e = s
        .set((
            [lp.cutoff().to(Hz(2.0)), lp.cutoff().to(Hz(3.0))],
            vec![lp.q().to(Q(1.0))],
            gone.cutoff().to(Hz(3.0)),
        ))
        .unwrap_err();
    assert_eq!(e.at, 3);
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz(1_000.0));
}

/// **A value the target refuses fails the call at its item and lands nothing**: a NaN is
/// not a cutoff, and the item before it does not land either.
#[test]
fn a_non_finite_value_is_refused_and_lands_nothing() {
    let mut s = detached();
    let lp = filter(&mut s, 1_000.0);
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let e = s
            .set((lp.q().to(Q(2.0)), lp.cutoff().to(Hz(bad))))
            .unwrap_err();
        assert_eq!(e.at, 1);
        assert_eq!(e.kind, ValueErrorKind::Refused(SetRefusal::NotFinite));
        assert_eq!(
            s.get(lp.q()).unwrap(),
            Q(0.7),
            "the first item did not land"
        );
    }
    // An edit that builds a non-finite value is refused too.
    let e = s.set(lp.cutoff().modify(|_| Hz(f64::NAN))).unwrap_err();
    assert_eq!(e.kind, ValueErrorKind::Refused(SetRefusal::NotFinite));
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz(1_000.0));
}

/// **A value call from inside a `modify` is `Reentrant`**, whether it goes through the
/// session or a `Knobs`, a read or a write, and the call that holds the lock still
/// lands.
#[test]
fn a_value_call_inside_a_modify_is_reentrant() {
    let mut s = detached();
    let lp = filter(&mut s, 1_000.0);
    let knobs = s.knobs();
    let seen = Cell::new(0);
    let inside = |h: &Hz| {
        let kinds = [
            s.get(lp.cutoff()).unwrap_err().kind,
            s.set(lp.cutoff().to(Hz(1.0))).unwrap_err().kind,
            knobs.get(lp.cutoff()).unwrap_err().kind,
            knobs.set(lp.cutoff().to(Hz(1.0))).unwrap_err().kind,
        ];
        seen.set(
            kinds
                .iter()
                .filter(|k| **k == ValueErrorKind::Reentrant)
                .count(),
        );
        Hz(h.0 + 1.0)
    };
    s.set(lp.cutoff().modify(inside)).unwrap();
    assert_eq!(seen.get(), 4);
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz(1_001.0));
    // The lock is free again.
    s.set(lp.cutoff().to(Hz(5.0))).unwrap();
}

/// **A panic in a `modify` lands nothing and leaves the value lock usable**: the batch's
/// earlier items are not written, and the next call, through the session and through a
/// `Knobs`, takes the lock.
#[test]
fn a_panic_in_a_modify_lands_nothing_and_leaves_the_lock_usable() {
    let mut s = detached();
    let lp = filter(&mut s, 1_000.0);
    let knobs = s.knobs();
    let unwound = catch_unwind(AssertUnwindSafe(|| {
        let _ = s.set((
            lp.q().to(Q(3.0)),
            lp.cutoff()
                .modify(|_| std::panic::panic_any("an edit that fails")),
        ));
    }));
    assert!(unwound.is_err());
    assert_eq!(s.get((lp.cutoff(), lp.q())).unwrap(), (Hz(1_000.0), Q(0.7)));
    s.set(lp.cutoff().to(Hz(2.0))).unwrap();
    knobs.set(lp.q().to(Q(1.0))).unwrap();
    assert_eq!(knobs.get((lp.cutoff(), lp.q())).unwrap(), (Hz(2.0), Q(1.0)));
}

/// **A node that is gone is `NoSuchTarget`, naming its key**, on a read and on a write,
/// through the session and through a `Knobs`.
#[test]
fn a_removed_node_is_no_such_target_and_names_its_key() {
    let mut s = detached();
    let lp = filter(&mut s, 1_000.0);
    let knobs = s.knobs();
    s.graph().remove(lp).unwrap();
    for e in [
        s.get(lp.cutoff()).unwrap_err(),
        knobs.get(lp.cutoff()).unwrap_err(),
        s.set(lp.cutoff().to(Hz(1.0))).unwrap_err(),
        knobs.set(lp.cutoff().to(Hz(1.0))).unwrap_err(),
        s.get(lp.cutoff().info()).unwrap_err(),
    ] {
        assert_eq!(e.kind, ValueErrorKind::NoSuchTarget);
        assert_eq!(e.at, 0);
        assert_eq!(e.target.expect("it names the key").node(), Some(lp.erase()));
    }
}

/// **`Knobs` answers `Closed` once the session is gone**, and the whole call's position
/// is 0.
#[test]
fn knobs_are_closed_once_the_session_is_gone() {
    let mut s = detached();
    let lp = filter(&mut s, 1_000.0);
    let knobs = s.knobs();
    let other = knobs.clone();
    assert_eq!(knobs.get(lp.cutoff()).unwrap(), Hz(1_000.0));
    drop(s);
    for e in [
        knobs.get(lp.cutoff()).unwrap_err(),
        other.set(lp.cutoff().to(Hz(1.0))).unwrap_err(),
    ] {
        assert_eq!(e.kind, ValueErrorKind::Closed);
        assert_eq!(e.at, 0);
        assert!(e.target.is_none());
    }
}

/// **A `Seconds` param is not narrowed to an `f32`**: a figure an `f32` cannot hold is
/// read back exactly, through the session and through a `Knobs`, and an edit sees it
/// whole.
#[test]
fn a_seconds_param_keeps_its_full_width() {
    let mut s = detached();
    let mb = multiband(&mut s);
    let attack = mb.attack_at(1);
    let wide = 0.1 + 1e-9;
    assert_ne!(f64::from(wide as f32), wide, "an f32 cannot hold it");

    s.set(attack.to(Seconds(wide))).unwrap();
    assert_eq!(s.get(attack).unwrap(), Seconds(wide));
    s.knobs().set(attack.to(Seconds(wide + 1e-9))).unwrap();
    assert_eq!(s.get(attack).unwrap(), Seconds(wide + 1e-9));
    s.set(attack.modify(|a| Seconds(a.0 + 1e-9))).unwrap();
    assert_eq!(s.get(attack).unwrap(), Seconds(wide + 2e-9));
}

/// **A write reaches the node's own cell**, and a read answers what the cell holds.
#[test]
fn a_write_and_a_read_go_through_the_nodes_cell() {
    let mut s = detached();
    let (lp, controls) = s.graph().add(SvfFilterNode::<f32>::new(
        SvfType::LowPass,
        Hz(1_000.0),
        Q(0.7),
    ));
    s.set(lp.cutoff().to(Hz(2_500.0))).unwrap();
    assert_eq!(controls.get(UnitParam::Cutoff), Some(2_500.0));
    controls.set(UnitParam::Q, 4.0);
    assert_eq!(s.get(lp.q()).unwrap(), Q(4.0));
}

/// **A read answers in the argument's shape**: a target its value, a tuple a tuple (nested
/// too), an array an array, a `Vec` a `Vec`, an `Option` an `Option`.
#[test]
fn a_read_answers_in_the_shape_of_its_argument() {
    let mut s = detached();
    let lp = filter(&mut s, 1_000.0);
    let comp = compressor(&mut s);
    s.set((lp.q().to(Q(2.0)), comp.threshold().to(Db(-12.0))))
        .unwrap();
    let (cut, (q, th)) = s.get((lp.cutoff(), (lp.q(), comp.threshold()))).unwrap();
    assert_eq!((cut, q, th), (Hz(1_000.0), Q(2.0), Db(-12.0)));
    assert_eq!(
        s.get([lp.cutoff(), lp.cutoff()]).unwrap(),
        [Hz(1_000.0), Hz(1_000.0)]
    );
    assert_eq!(s.get(vec![lp.q()]).unwrap(), vec![Q(2.0)]);
    assert_eq!(s.get(Some(lp.q())).unwrap(), Some(Q(2.0)));
    assert_eq!(s.get(None::<ParamTarget<Q>>).unwrap(), None);
    assert_eq!(s.get(()).unwrap(), ());
    // Twelve items are one tuple.
    let c = lp.cutoff();
    assert_eq!(
        s.get((c, c, c, c, c, c, c, c, c, c, c, c)).unwrap().11,
        Hz(1_000.0)
    );
}

/// **An assignment with its target erased writes like the typed one**, in a list built at
/// run time of mixed types.
#[test]
fn an_erased_assignment_writes_like_a_typed_one() {
    let mut s = detached();
    let lp = filter(&mut s, 1_000.0);
    let list = vec![lp.cutoff().to(Hz(700.0)).erase(), lp.q().to(Q(1.5)).erase()];
    s.set(list.clone()).unwrap();
    assert_eq!(s.get((lp.cutoff(), lp.q())).unwrap(), (Hz(700.0), Q(1.5)));
    s.set((lp.cutoff().modify(plus_one), list[0].clone()))
        .unwrap();
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz(700.0));
}

/// **`info` reads the address and the value the node was added with.**
#[test]
fn info_reads_the_address_and_the_default() {
    let mut s = detached();
    let lp = filter(&mut s, 1_000.0);
    s.set(lp.cutoff().to(Hz(5.0))).unwrap();
    assert_eq!(
        s.get(lp.cutoff().info()).unwrap(),
        TargetInfo {
            param: UnitAddr::first(UnitParam::Cutoff),
            default: Hz(1_000.0),
        }
    );
}

/// **`Knobs` is `Clone + Send`**, and a thread holding one writes what the session reads.
#[test]
fn knobs_are_clone_and_send_and_write_across_threads() {
    fn assert_clone_send<T: Clone + Send>() {}
    assert_clone_send::<Knobs>();
    assert_clone_send::<Knobs<String>>();

    let mut s = detached();
    let lp = filter(&mut s, 1_000.0);
    let knobs = s.knobs();
    std::thread::spawn(move || knobs.set(lp.cutoff().to(Hz(123.0))).unwrap())
        .join()
        .unwrap();
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz(123.0));
}

/// A node whose `Cutoff` is a `Seconds`, where a filter's is a `Hz`: the same address in
/// another unit.
#[derive(Clone)]
struct Slow {
    cutoff: Param<Seconds>,
}

impl Node for Slow {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(self.cutoff.load().0 as f32);
        Status::Modified
    }
    fn reset(&mut self) {}
}

impl ParamNode for Slow {
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.cutoff.detach();
        fork
    }
}

tutti_graph::declare_params!([] Slow, n { Cutoff => Seconds = &n.cutoff });

fn swap<N: tutti_session::SessionNode>(
    s: &mut DetachedSession,
    key: tutti_session::Key<SvfFilterNode<f32>>,
    incoming: N,
) {
    s.graph()
        .replace(key, incoming, Seconds(0.0), FadeLaw::EqualPower)
        .unwrap_or_else(|_| panic!("a detached graph swaps at once"));
}

/// **A typed key outlives a swap to another node type as `NoSuchTarget`**: the old key's
/// `Hz` cutoff names no param of the node now at its address (a `Seconds` one, or none),
/// on a read, a write and an `info`, and in a batch the error is at the stale item.
#[test]
fn a_key_of_a_swapped_out_node_type_is_no_such_target() {
    let mut s = detached();
    let stale = filter(&mut s, 1_000.0);
    let kept = filter(&mut s, 1_000.0);
    let bare = filter(&mut s, 1_000.0);
    swap(
        &mut s,
        stale,
        Params(Slow {
            cutoff: Param::new(Seconds(1.0)),
        }),
    );
    swap(
        &mut s,
        bare,
        Unforkable(SvfFilterNode::<f32>::new(
            SvfType::LowPass,
            Hz(1_000.0),
            Q(0.7),
        )),
    );
    for key in [stale, bare] {
        let e = s.set(key.cutoff().to(Hz(5.0))).unwrap_err();
        assert_eq!(e.kind, ValueErrorKind::NoSuchTarget);
        assert_eq!(
            e.target.expect("it names the key").node(),
            Some(key.erase())
        );
        assert_eq!(
            s.get(key.cutoff()).unwrap_err().kind,
            ValueErrorKind::NoSuchTarget
        );
        assert_eq!(
            s.get(key.cutoff().info()).unwrap_err().kind,
            ValueErrorKind::NoSuchTarget
        );
        let e = s
            .set((kept.cutoff().to(Hz(7.0)), key.cutoff().to(Hz(5.0))))
            .unwrap_err();
        assert_eq!((e.at, e.kind), (1, ValueErrorKind::NoSuchTarget));
        assert_eq!(s.get(kept.cutoff()).unwrap(), Hz(1_000.0), "nothing landed");
    }
}

/// **A value call on one session from inside a `modify` of another is not `Reentrant`**:
/// each session has its own value lock.
#[test]
fn a_modify_may_call_another_session() {
    let mut a = detached();
    let mut b = detached();
    let fa = filter(&mut a, 100.0);
    let fb = filter(&mut b, 200.0);
    let seen = Cell::new(None);
    a.set(fa.cutoff().modify(|h| {
        seen.set(Some(b.get(fb.cutoff()).map_err(|e| e.kind)));
        *h
    }))
    .unwrap();
    assert_eq!(seen.take().unwrap().unwrap(), Hz(200.0));
}

/// **An `f32` param reads back the bits it was given**, subnormals and the last mantissa
/// bit included.
#[test]
fn an_f32_param_round_trips_bit_for_bit() {
    let mut s = detached();
    let lp = filter(&mut s, 1_000.0);
    for bits in [0x3f80_0001u32, 0x0000_0001, 0x7f7f_ffff, 0x1234_5678] {
        let v = f32::from_bits(bits);
        s.set(lp.q().to(Q(v))).unwrap();
        assert_eq!(s.get(lp.q()).unwrap().0.to_bits(), bits);
    }
}

/// **A `Seconds` that is not a finite number, or that an `f32` cell cannot hold, is
/// refused, and an `f64` cell takes what an `f32` cannot**.
#[test]
fn a_seconds_value_the_cell_cannot_hold_is_refused() {
    let mut s = detached();
    let mb = multiband(&mut s);
    let comp = compressor(&mut s);
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for refused in [
            s.set(mb.attack_at(0).to(Seconds(bad))).unwrap_err(),
            s.set(comp.attack().to(Seconds(bad))).unwrap_err(),
        ] {
            assert_eq!(refused.kind, ValueErrorKind::Refused(SetRefusal::NotFinite));
        }
    }
    let before = s.get(comp.attack()).unwrap();
    let e = s
        .set((
            comp.release().to(Seconds(0.5)),
            comp.attack().to(Seconds(1e300)),
        ))
        .unwrap_err();
    assert_eq!(
        (e.at, e.kind),
        (1, ValueErrorKind::Refused(SetRefusal::OutOfRange))
    );
    assert_eq!(s.get(comp.attack()).unwrap(), before, "no infinity stored");
    s.set(mb.attack_at(0).to(Seconds(1e300))).unwrap();
    assert_eq!(s.get(mb.attack_at(0)).unwrap(), Seconds(1e300));
}

/// **An erased assignment is checked like a typed one**: a NaN through `AnyAssign` is
/// refused at its position, and the item before it does not land.
#[test]
fn an_erased_non_finite_assignment_is_refused() {
    let mut s = detached();
    let lp = filter(&mut s, 1_000.0);
    let e = s
        .set(vec![
            lp.q().to(Q(2.0)).erase(),
            lp.cutoff().to(Hz(f64::NAN)).erase(),
        ])
        .unwrap_err();
    assert_eq!(
        (e.at, e.kind),
        (1, ValueErrorKind::Refused(SetRefusal::NotFinite))
    );
    assert_eq!(s.get(lp.q()).unwrap(), Q(0.7));
}

/// **A read that fails inside a `Vec` is at its flattened position.**
#[test]
fn a_failing_read_inside_a_vec_is_at_its_position() {
    let mut s = detached();
    let lp = filter(&mut s, 1_000.0);
    let gone = filter(&mut s, 1_000.0);
    s.graph().remove(gone).unwrap();
    let e = s
        .get((
            lp.cutoff(),
            vec![lp.cutoff(), lp.cutoff()],
            vec![gone.cutoff()],
        ))
        .unwrap_err();
    assert_eq!((e.at, e.kind), (3, ValueErrorKind::NoSuchTarget));
}
