//! `gesture`: one id across a drag's writes, an end per moved target landed together,
//! and what a click, a foreign write and a removed target record. Every figure is the
//! value a write was given, read back.

use std::sync::mpsc;
use std::time::Duration;

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

/// **A one-target drag over many writes is one gesture id, open until the drop**: each
/// read shows the newest value not ended, and the end carries the base before the first
/// write.
#[test]
fn a_drag_of_one_target_keeps_one_id_open_until_it_is_dropped() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut w = s.watch(lp.cutoff()).unwrap();
    let mut drag = s.gesture(lp.cutoff()).unwrap();

    drag.set(Hz::new(1_100.0)).unwrap();
    let first = w.changes().unwrap().unwrap();
    assert_eq!(first.value, Hz::new(1_100.0));
    assert_eq!(first.by, ChangedBy::App);
    let first = first.gesture.expect("a gesture's write is gestured");
    assert_eq!(first.id, drag.id());
    assert!(!first.ended);
    assert_eq!(first.base, Hz::new(1_000.0));

    drag.set(Hz::new(1_200.0)).unwrap();
    let second = w.changes().unwrap().unwrap();
    assert_eq!(second.value, Hz::new(1_200.0));
    let second = second.gesture.unwrap();
    assert_eq!((second.id, second.ended), (drag.id(), false));
    // The base is the one before this read's first change.
    assert_eq!(second.base, Hz::new(1_100.0));

    let id = drag.id();
    drop(drag);
    let end = w.changes().unwrap().unwrap();
    assert_eq!(end.value, Hz::new(1_200.0));
    let end = end.gesture.unwrap();
    assert_eq!((end.id, end.ended), (id, true));
    // What undoing the drag restores: the value before its first write.
    assert_eq!(end.base, Hz::new(1_000.0));
    assert!(w.changes().unwrap().is_none());
}

/// **An XY pair is one id and one end**: both watchers see the same id, not ended while
/// the gesture is open and ended after the drop, and the id is the gesture's.
#[test]
fn an_xy_pair_is_one_id_and_one_end() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut x = s.watch(lp.cutoff()).unwrap();
    let mut y = s.watch(lp.q()).unwrap();
    let mut both = s.watch((lp.cutoff(), lp.q())).unwrap();
    let mut drag = s.gesture((lp.cutoff(), lp.q())).unwrap();

    drag.set((Hz::new(2_000.0), Q::new(2.0))).unwrap();
    let (cx, cy) = (
        x.changes().unwrap().unwrap().gesture.unwrap(),
        y.changes().unwrap().unwrap().gesture.unwrap(),
    );
    assert_eq!((cx.id, cy.id), (drag.id(), drag.id()));
    assert!(!cx.ended && !cy.ended);

    let id = drag.id();
    drop(drag);
    let (cx, cy) = (
        x.changes().unwrap().unwrap().gesture.unwrap(),
        y.changes().unwrap().unwrap().gesture.unwrap(),
    );
    assert!(cx.ended && cy.ended);
    assert_eq!((cx.id, cy.id), (id, id));
    assert_eq!((cx.base, cy.base), (Hz::new(1_000.0), Q::new(0.75)));
    // One read of both targets shows the end of both: a drain never splits it.
    let (bx, by) = both.changes().unwrap();
    assert!(bx.unwrap().gesture.unwrap().ended);
    assert!(by.unwrap().gesture.unwrap().ended);
}

/// **A click records nothing**: a gesture that wrote nothing, or wrote the base, shows
/// no change at its drop.
#[test]
fn a_gesture_that_moved_nothing_records_nothing() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut w = s.watch(lp.cutoff()).unwrap();

    drop(s.gesture(lp.cutoff()).unwrap());
    assert!(w.changes().unwrap().is_none());

    let mut drag = s.gesture(lp.cutoff()).unwrap();
    drag.set(Hz::new(1_000.0)).unwrap();
    drop(drag);
    assert!(w.changes().unwrap().is_none());
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz::new(1_000.0));
}

/// **A drag that returns to where it began still ends**: the watcher is told the
/// gesture is over, with the value it began at.
#[test]
fn a_drag_back_to_its_base_still_ends() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut w = s.watch(lp.cutoff()).unwrap();
    let mut drag = s.gesture(lp.cutoff()).unwrap();
    drag.set(Hz::new(1_500.0)).unwrap();
    drag.set(Hz::new(1_000.0)).unwrap();
    drop(drag);
    let end = w.changes().unwrap().unwrap();
    assert_eq!(end.value, Hz::new(1_000.0));
    let end = end.gesture.unwrap();
    assert!(end.ended);
    assert_eq!(end.base, Hz::new(1_000.0));
}

/// **A write from another thread to a gesture's target lands ungestured**, and the
/// gesture's base for the end is the one before its own first write.
#[test]
fn a_write_from_another_thread_lands_ungestured() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut w = s.watch(lp.cutoff()).unwrap();
    let mut drag = s.gesture(lp.cutoff()).unwrap();
    drag.set(Hz::new(1_100.0)).unwrap();
    w.changes().unwrap();

    let values = s.values();
    std::thread::spawn(move || values.set(lp.cutoff().to(Hz::new(9_000.0))).unwrap())
        .join()
        .unwrap();
    let foreign = w.changes().unwrap().unwrap();
    assert_eq!(foreign.value, Hz::new(9_000.0));
    assert!(foreign.gesture.is_none());

    let id = drag.id();
    drop(drag);
    let end = w.changes().unwrap().unwrap();
    // The end reports the value the target has now.
    assert_eq!(end.value, Hz::new(9_000.0));
    let end = end.gesture.unwrap();
    assert_eq!((end.id, end.ended), (id, true));
    assert_eq!(end.base, Hz::new(1_000.0));
}

/// **A gesture on a removed target answers `NoSuchTarget` and records nothing**: when
/// opened over it, and when it is removed after the gesture opened.
#[test]
fn a_gesture_on_a_removed_target_answers_no_such_target_and_records_nothing() {
    let mut s = detached();
    let lp = filter(&mut s);
    let gone = filter(&mut s);
    let mut w = s.watch((lp.cutoff(), lp.q())).unwrap();
    let mut drag = s.gesture((lp.cutoff(), gone.cutoff())).unwrap();
    s.remove(gone).unwrap();

    let refused = drag.set((Hz::new(2_000.0), Hz::new(2_000.0))).unwrap_err();
    assert!(matches!(refused.kind, ValueErrorKind::NoSuchTarget));
    assert_eq!(refused.at, 1);
    // The whole batch landed nothing.
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz::new(1_000.0));
    drop(drag);
    assert_eq!(w.changes().unwrap(), (None, None));

    let opened = s.gesture(gone.cutoff()).unwrap_err();
    assert!(matches!(opened.kind, ValueErrorKind::NoSuchTarget));
}

/// **A gesture dropped inside a `modify` does not deadlock**, and its end lands with
/// that call's batch: the watcher then reads the value the call built, ended. The join
/// only guards a hang.
#[test]
fn a_gesture_dropped_inside_a_modify_ends_with_the_call() {
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        let mut s = detached();
        let lp = filter(&mut s);
        let mut w = s.watch(lp.cutoff()).unwrap();
        let mut drag = s.gesture(lp.cutoff()).unwrap();
        drag.set(Hz::new(1_100.0)).unwrap();
        let id = drag.id();
        w.changes().unwrap();

        s.set(lp.cutoff().modify(move |hz| {
            drop(drag);
            *hz + Hz::new(1.0)
        }))
        .unwrap();
        let end = w.changes().unwrap().unwrap();
        assert_eq!(end.value, Hz::new(1_101.0));
        let end = end.gesture.unwrap();
        assert_eq!((end.id, end.ended), (id, true));
        // The base is the one at the watcher's last read, before the call's write.
        assert_eq!(end.base, Hz::new(1_100.0));
        done.send(()).unwrap();
    });
    finished
        .recv_timeout(Duration::from_secs(120))
        .expect("the modify returned");
}

/// **A gesture whose session is gone answers `Closed`**, and dropping it records nothing.
#[test]
fn a_gesture_of_a_gone_session_is_closed() {
    let mut s = detached();
    let lp = filter(&mut s);
    let values = s.values();
    let mut drag = values.gesture(lp.cutoff()).unwrap();
    drop(s);
    let refused = drag.set(Hz::new(2_000.0)).unwrap_err();
    assert!(matches!(refused.kind, ValueErrorKind::Closed));
}
