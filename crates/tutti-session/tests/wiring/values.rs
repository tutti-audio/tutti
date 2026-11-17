//! The session's value verbs: `set` and `get` over targets, `Knobs`, and the errors they
//! answer. Every figure is an analytic one: a write is read back as written.

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use tutti_nodes::{
    CompressorControls, CompressorNode, KeySource, MultibandCompressorNode, SvfFilterNode, SvfType,
};
use tutti_session::{
    Assign, DetachedSession, Knobs, ParamTarget, Session, SetRefusal, TargetInfo, ValueErrorKind,
};
use tutti_types::{
    BusTopology, ChannelLayout, Db, Hz, Q, SampleRate, Seconds, UnitAddr, UnitParam,
};

fn detached() -> DetachedSession {
    Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds")
}

fn filter(s: &mut DetachedSession, cutoff: f32) -> tutti_session::Key<SvfFilterNode<f32>> {
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
fn multiband(s: &mut DetachedSession) -> tutti_session::Key<MultibandCompressorNode> {
    s.graph()
        .add(MultibandCompressorNode::new(ChannelLayout::STEREO, 2).unwrap())
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

/// **A target lands once, with its final value**: the cell is written for each target of
/// a batch, so a read after the call sees the last item's value for each.
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
    for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
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
    let e = s.set(lp.cutoff().modify(|_| Hz(f32::NAN))).unwrap_err();
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
/// through the session and through a `Knobs`; a swapped-in node that declares no params
/// has no such param either.
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
        assert_eq!(e.target.expect("it names the key").node(), lp.erase());
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
