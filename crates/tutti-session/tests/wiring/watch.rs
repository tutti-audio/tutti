//! `watch` and `Watcher`: the newest change of each target since the last read, the
//! gesture a batch is, and what a removed target reads. Every figure is the value a
//! write was given, read back.

use std::panic::{AssertUnwindSafe, catch_unwind};

use tutti_buffers::ChannelLayout;
use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
use tutti_session::{ChangedBy, DetachedSession, Key, Session, ValueErrorKind};
use tutti_units::{Hz, Q, SampleRate};

fn detached() -> DetachedSession {
    Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds")
}

fn filter(s: &mut DetachedSession) -> Key<SvfFilterNode<f32>> {
    s.graph().add(SvfFilterNode::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(1_000.0))
            .q(Q::new(0.75)),
    ))
}

/// **A set shows one change with the value it landed**, once: the next read shows
/// nothing, and a lone write belongs to no gesture.
#[test]
fn a_set_shows_one_change_with_its_value() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut w = s.watch(lp.cutoff()).unwrap();
    assert!(w.changes().unwrap().is_none());

    s.set(lp.cutoff().to(Hz::new(2_000.0))).unwrap();
    let change = w.changes().unwrap().expect("the base moved");
    assert_eq!(change.value, Hz::new(2_000.0));
    assert_eq!(change.by, ChangedBy::App);
    assert!(change.gesture.is_none());
    assert!(w.changes().unwrap().is_none());
}

/// **A batch of two or more writes is one gesture**: its changes share one id, each
/// ended, each holding the base before the batch; the next batch is a later gesture.
#[test]
fn a_batch_shows_one_gesture_id_across_its_writes() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut w = s.watch((lp.cutoff(), lp.q())).unwrap();

    s.set((lp.cutoff().to(Hz::new(300.0)), lp.q().to(Q::new(2.0))))
        .unwrap();
    let (cutoff, q) = w.changes().unwrap();
    let (cutoff, q) = (cutoff.unwrap(), q.unwrap());
    assert_eq!(cutoff.value, Hz::new(300.0));
    assert_eq!(q.value, Q::new(2.0));
    let (cg, qg) = (cutoff.gesture.unwrap(), q.gesture.unwrap());
    assert_eq!(cg.id, qg.id);
    assert!(cg.ended && qg.ended);
    assert_eq!(cg.base, Hz::new(1_000.0));
    assert_eq!(qg.base, Q::new(0.75));

    s.set((lp.cutoff().to(Hz::new(400.0)), lp.q().to(Q::new(3.0))))
        .unwrap();
    let (cutoff, _) = w.changes().unwrap();
    let next = cutoff.unwrap().gesture.unwrap();
    assert!(next.id > cg.id);
    assert_eq!(next.base, Hz::new(300.0));
}

/// **Two edits of one target in one batch are one change**: the value the call built,
/// over the base before the batch.
#[test]
fn two_edits_of_one_target_are_one_change_over_the_first_base() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut w = s.watch(lp.cutoff()).unwrap();
    let plus_one = |h: &Hz| *h + Hz::new(1.0);
    s.set((lp.cutoff().modify(plus_one), lp.cutoff().modify(plus_one)))
        .unwrap();
    let change = w.changes().unwrap().unwrap();
    assert_eq!(change.value, Hz::new(1_002.0));
    assert_eq!(change.gesture.unwrap().base, Hz::new(1_000.0));
}

/// **A set equal to the base shows nothing**, alone, and beside a write that does move.
#[test]
fn a_set_equal_to_the_base_shows_nothing() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut w = s.watch((lp.cutoff(), lp.q())).unwrap();

    s.set(lp.cutoff().to(Hz::new(1_000.0))).unwrap();
    assert_eq!(w.changes().unwrap(), (None, None));

    s.set((lp.cutoff().to(Hz::new(1_000.0)), lp.q().to(Q::new(1.5))))
        .unwrap();
    let (cutoff, q) = w.changes().unwrap();
    assert!(cutoff.is_none());
    assert_eq!(q.unwrap().value, Q::new(1.5));
}

/// **Between two reads, the newest change is the one reported**, even when it brings
/// the target back to where the first read saw it.
#[test]
fn a_to_b_to_a_between_reads_reports_a() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut w = s.watch(lp.cutoff()).unwrap();
    s.set(lp.cutoff().to(Hz::new(2_000.0))).unwrap();
    s.set(lp.cutoff().to(Hz::new(1_000.0))).unwrap();
    assert_eq!(w.changes().unwrap().unwrap().value, Hz::new(1_000.0));
}

/// **A refused batch changes nothing a watcher sees**, and a panicking edit records
/// nothing and leaves the lock usable.
#[test]
fn a_refused_batch_and_a_panicking_edit_show_nothing() {
    let mut s = detached();
    let lp = filter(&mut s);
    let gone = filter(&mut s);
    s.remove(gone).unwrap();
    let mut w = s.watch(lp.cutoff()).unwrap();

    let refused = s
        .set((lp.cutoff().to(Hz::new(5.0)), gone.cutoff().to(Hz::new(5.0))))
        .unwrap_err();
    assert_eq!(refused.kind, ValueErrorKind::NoSuchTarget);
    assert!(w.changes().unwrap().is_none());

    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let _ = s.set(lp.cutoff().modify(|_| panic!("an edit that fails")));
    }));
    assert!(panicked.is_err());
    assert!(w.changes().unwrap().is_none());
    s.set(lp.cutoff().to(Hz::new(6.0))).unwrap();
    assert_eq!(w.changes().unwrap().unwrap().value, Hz::new(6.0));
}

/// **Watching a target the graph does not have fails at the call**, at its position.
#[test]
fn watching_a_removed_target_is_no_such_target_at_its_position() {
    let mut s = detached();
    let lp = filter(&mut s);
    let gone = filter(&mut s);
    s.remove(gone).unwrap();
    let e = s
        .watch((
            lp.cutoff(),
            None::<Key<SvfFilterNode<f32>>>.map(|k| k.q()),
            gone.cutoff(),
        ))
        .unwrap_err();
    assert_eq!(e.kind, ValueErrorKind::NoSuchTarget);
    assert_eq!(e.at, 2);
    assert!(e.target.unwrap().is(gone.cutoff()));
}

/// **A target removed after a change still reports that change, then reads `None`; once
/// every target is gone and read, the watcher ends `Closed`.**
#[test]
fn a_removed_target_reads_none_and_the_watcher_then_closes() {
    let mut s = detached();
    let (a, b) = (filter(&mut s), filter(&mut s));
    let mut w = s.watch((a.cutoff(), b.cutoff())).unwrap();
    s.set(a.cutoff().to(Hz::new(10.0))).unwrap();
    s.remove(a).unwrap();

    let (first, second) = w.changes().unwrap();
    assert_eq!(first.unwrap().value, Hz::new(10.0));
    assert!(second.is_none());
    assert_eq!(w.changes().unwrap(), (None, None));

    s.remove(b).unwrap();
    assert_eq!(w.changes().unwrap_err().kind, ValueErrorKind::Closed);
}

/// **A watcher of a session that is gone is `Closed`**, from its `Values` too.
#[test]
fn a_watcher_outlives_its_session_as_closed() {
    let mut s = detached();
    let lp = filter(&mut s);
    let values = s.values();
    let mut w = values.watch(lp.cutoff()).unwrap();
    drop(s);
    assert_eq!(w.changes().unwrap_err().kind, ValueErrorKind::Closed);
    assert_eq!(
        values.watch(lp.cutoff()).unwrap_err().kind,
        ValueErrorKind::Closed
    );
}

/// **A watch or a read of changes from inside a `modify` is `Reentrant`**.
#[test]
fn watching_inside_a_modify_is_reentrant() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut w = s.watch(lp.cutoff()).unwrap();
    let inside = |h: &Hz| {
        let watch = s.watch(lp.cutoff()).unwrap_err().kind;
        assert_eq!(watch, ValueErrorKind::Reentrant);
        *h + Hz::new(1.0)
    };
    s.set(lp.cutoff().modify(inside)).unwrap();
    assert_eq!(w.changes().unwrap().unwrap().value, Hz::new(1_001.0));
}

/// **Writers on two threads: after both are joined, the watcher's value is the value
/// the target reads.**
#[test]
fn racing_writers_leave_the_watcher_at_the_target_value() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut w = s.watch(lp.cutoff()).unwrap();
    let writers: Vec<_> = (0..2_u32)
        .map(|t| {
            let values = s.values();
            std::thread::spawn(move || {
                for n in 0..200_u32 {
                    let hz = Hz::new(f64::from(1 + t * 1_000 + n));
                    values.set(lp.cutoff().to(hz)).unwrap();
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    let seen = w.changes().unwrap().unwrap().value;
    assert_eq!(seen, s.get(lp.cutoff()).unwrap());
}
