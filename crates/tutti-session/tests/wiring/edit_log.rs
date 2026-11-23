//! `edit_log`: undo steps as marks, each with the saved values at its landing. Every
//! figure is a value a write was given, read back through a restore of a mark's `after`.

use tutti_buffers::ChannelLayout;
use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
use tutti_session::{ChangedBy, DetachedSession, EditLog, EditMark, Key, Session};
use tutti_units::{Hz, Q, SampleRate, UndoSteps};

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

fn marks(log: &mut EditLog<&'static str>) -> Vec<EditMark<&'static str>> {
    log.drain().collect()
}

/// **One undo step is a gesture's marks**: a drag's open writes and its end share its id,
/// a batch of two targets shares one, and a single write has none.
#[test]
fn a_gesture_a_batch_and_a_single_write_are_one_step_each() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut log = s.edit_log().unwrap();

    s.set(lp.cutoff().to(Hz::new(1_500.0))).unwrap();
    let single = marks(&mut log);
    assert_eq!(single.len(), 1);
    assert_eq!((single[0].gesture, single[0].ended), (None, true));
    assert_eq!(single[0].by, ChangedBy::App);

    s.set((lp.cutoff().to(Hz::new(1_600.0)), lp.q().to(Q::new(1.0))))
        .unwrap();
    let batch = marks(&mut log);
    assert_eq!(batch.len(), 2);
    assert!(batch[0].gesture.is_some());
    assert_eq!(batch[0].gesture, batch[1].gesture);
    assert!(batch.iter().all(|m| m.ended));

    let mut drag = s.gesture(lp.cutoff()).unwrap();
    let id = drag.id();
    drag.set(Hz::new(1_700.0)).unwrap();
    drag.set(Hz::new(1_800.0)).unwrap();
    drop(drag);
    let drag = marks(&mut log);
    assert_eq!(drag.len(), 3);
    assert!(drag.iter().all(|m| m.gesture == Some(id)));
    assert_eq!(
        drag.iter().map(|m| m.ended).collect::<Vec<_>>(),
        [false, false, true]
    );
}

/// **`after` is the saved values at the landing, however late the drain**: set A, set B,
/// drain, and the first mark's `after` puts A back.
#[test]
fn a_late_drain_still_sees_the_values_as_they_were_at_the_landing() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut log = s.edit_log().unwrap();
    s.set(lp.cutoff().to(Hz::new(1_200.0))).unwrap();
    s.set(lp.cutoff().to(Hz::new(1_400.0))).unwrap();
    let drained = marks(&mut log);
    assert_eq!(drained.len(), 2);

    s.set(&drained[0].after).unwrap();
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz::new(1_200.0));
    s.set(&drained[1].after).unwrap();
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz::new(1_400.0));
}

/// **A capture while a gesture is open holds its targets at their base**: the open
/// write's `after` puts back the value before the drag, though the drag moved on.
#[test]
fn an_open_gesture_captures_its_targets_at_their_base() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut log = s.edit_log().unwrap();
    let mut drag = s.gesture(lp.cutoff()).unwrap();
    drag.set(Hz::new(1_100.0)).unwrap();
    drag.set(Hz::new(1_200.0)).unwrap();
    let open = marks(&mut log);
    assert_eq!(open.len(), 2);
    assert!(open.iter().all(|m| !m.ended));
    for mark in &open {
        s.set(&mark.after).unwrap();
        assert_eq!(s.get(lp.cutoff()).unwrap(), Hz::new(1_000.0));
    }
    drop(drag);
}

/// **The cap merges the oldest undrained steps**: five single sets under `cap(2)`
/// merge three away, keep two, and the count reads 3.
#[test]
fn the_cap_coalesces_the_oldest_steps_and_counts_them() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut log = s.edit_log().unwrap().cap(2);
    assert_eq!(log.coalesced(), UndoSteps::ZERO);
    for hz in [1_100.0, 1_200.0, 1_300.0, 1_400.0, 1_500.0] {
        s.set(lp.cutoff().to(Hz::new(hz))).unwrap();
    }
    assert_eq!(log.coalesced(), UndoSteps::new(3));
    let kept = marks(&mut log);
    assert_eq!(kept.len(), 2);
    s.set(&kept[0].after).unwrap();
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz::new(1_400.0));
    assert_eq!(log.coalesced(), UndoSteps::new(3));
}

/// **A drain holds no lock while it is iterated**: a `set` inside the loop completes, and
/// its mark is the next drain's.
#[test]
fn a_set_inside_the_drain_loop_completes() {
    let mut s = detached();
    let lp = filter(&mut s);
    let mut log = s.edit_log().unwrap();
    s.set(lp.cutoff().to(Hz::new(1_100.0))).unwrap();
    for _ in log.drain() {
        s.set(lp.cutoff().to(Hz::new(1_900.0))).unwrap();
    }
    let next = marks(&mut log);
    assert_eq!(next.len(), 1);
    s.set(&next[0].after).unwrap();
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz::new(1_900.0));
}

/// **A restore's marks are `Restore`, in one gesture**: an app reads them as a non-undo
/// step.
#[test]
fn a_restore_lands_marks_by_restore() {
    let mut s = detached();
    let lp = filter(&mut s);
    let saved = s.saved_values().unwrap();
    s.set((lp.cutoff().to(Hz::new(2_000.0)), lp.q().to(Q::new(2.0))))
        .unwrap();
    let mut log = s.edit_log().unwrap();
    s.set(&saved).unwrap();
    let restored = marks(&mut log);
    assert!(restored.len() >= 2);
    assert!(restored.iter().all(|m| m.by == ChangedBy::Restore));
    assert!(restored[0].gesture.is_some());
    assert!(restored.iter().all(|m| m.gesture == restored[0].gesture));
}

/// **A log sees only what lands after it is made**, and dropping it ends the recording.
#[test]
fn a_log_records_from_its_creation() {
    let mut s = detached();
    let lp = filter(&mut s);
    s.set(lp.cutoff().to(Hz::new(1_100.0))).unwrap();
    let mut log = s.edit_log().unwrap();
    assert!(marks(&mut log).is_empty());
    drop(log);
    s.set(lp.cutoff().to(Hz::new(1_200.0))).unwrap();
}
