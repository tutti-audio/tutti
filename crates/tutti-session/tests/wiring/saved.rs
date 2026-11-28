//! `saved_values` and `set(&saved)`: what a capture holds, what a restore lands
//! and as whom, and what a removed node does to it. Every figure is the value a
//! write was given, read back.

use tutti_buffers::ChannelLayout;
use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
use tutti_session::{ChangedBy, Key, OutputPolicy, Session, ValueErrorKind};
use tutti_tempo::TempoMap;
use tutti_units::{Bpm, Hz, Q, SampleRate};

fn detached() -> Session {
    Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .outputs(ChannelLayout::MONO)
        .sample_rate(SampleRate::SR_48K)
        .block_dates(tutti_session::BlockDates::Frames)
        .build()
        .expect("builds")
}

fn filter(s: &mut Session) -> Key<SvfFilterNode<f32>> {
    s.graph().add(SvfFilterNode::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(1_000.0))
            .q(Q::new(0.75)),
    ))
}

/// **A restore of two or more targets is one gesture by `Restore`**: each target's
/// watcher shows the saved value, the one id, ended, over the base before the restore.
#[test]
fn a_restore_of_two_params_is_one_ended_gesture_by_restore() {
    let mut s = detached();
    let lp = filter(&mut s);
    let saved = s.saved_values().unwrap();
    s.set((lp.cutoff().to(Hz::new(300.0)), lp.q().to(Q::new(2.0))))
        .unwrap();
    let mut w = s.watch((lp.cutoff(), lp.q())).unwrap();

    s.set(&saved).unwrap();

    let (cutoff, q) = w.changes().unwrap();
    let (cutoff, q) = (cutoff.unwrap(), q.unwrap());
    assert_eq!((cutoff.value, q.value), (Hz::new(1_000.0), Q::new(0.75)));
    assert_eq!((cutoff.by, q.by), (ChangedBy::Restore, ChangedBy::Restore));
    let (cg, qg) = (cutoff.gesture.unwrap(), q.gesture.unwrap());
    assert_eq!(cg.id, qg.id);
    assert!(cg.ended && qg.ended);
    assert_eq!((cg.base, qg.base), (Hz::new(300.0), Q::new(2.0)));
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz::new(1_000.0));
    assert_eq!(s.get(lp.q()).unwrap(), Q::new(0.75));
}

/// **A restore moves only what differs**: a target already at its saved value is no
/// change, and the one that moved is by `Restore` with no gesture.
#[test]
fn a_restore_changes_only_the_target_that_differs() {
    let mut s = detached();
    let lp = filter(&mut s);
    let saved = s.saved_values().unwrap();
    s.set(lp.cutoff().to(Hz::new(300.0))).unwrap();
    let mut w = s.watch((lp.cutoff(), lp.q())).unwrap();

    s.set(&saved).unwrap();

    let (cutoff, q) = w.changes().unwrap();
    assert!(q.is_none(), "q was at its saved value");
    let cutoff = cutoff.unwrap();
    assert_eq!(
        (cutoff.value, cutoff.by),
        (Hz::new(1_000.0), ChangedBy::Restore)
    );
    assert!(cutoff.gesture.is_none());
}

/// **A session's own value is saved and restored**: the tempo map written after the
/// capture is replaced by the one captured.
#[test]
fn a_restore_puts_the_tempo_map_back() {
    let s = detached();
    let tempo = s.transport_handle().tempo();
    s.set(tempo.to(TempoMap::constant(Bpm::of(90.0)).unwrap()))
        .unwrap();
    let saved = s.saved_values().unwrap();
    s.set(tempo.to(TempoMap::constant(Bpm::of(140.0)).unwrap()))
        .unwrap();

    s.set(&saved).unwrap();

    assert_eq!(s.get(tempo).unwrap().constant_tempo(), Some(Bpm::of(90.0)));
}

/// **A node removed after the capture fails the restore at its item's position and
/// lands nothing**; `retain_live` drops it and the same call then restores the rest.
#[test]
fn a_removed_node_fails_the_restore_until_retain_live_drops_it() {
    let mut s = detached();
    let lp = filter(&mut s);
    let gone = filter(&mut s);
    let mut saved = s.saved_values().unwrap();
    s.set((
        lp.cutoff().to(Hz::new(300.0)),
        gone.cutoff().to(Hz::new(400.0)),
    ))
    .unwrap();
    s.remove(gone).unwrap();

    let e = s.set(&saved).unwrap_err();
    assert_eq!(e.kind, ValueErrorKind::NoSuchTarget);
    let first_gone = saved
        .iter()
        .position(|(target, _)| target.node() == Some(gone.into()))
        .expect("the removed node was saved");
    assert_eq!(e.at, first_gone);
    assert_eq!(
        s.get(lp.cutoff()).unwrap(),
        Hz::new(300.0),
        "nothing landed"
    );

    saved.retain_live(&s.values()).unwrap();
    assert!(
        saved
            .iter()
            .all(|(target, _)| target.node() != Some(gone.into()))
    );
    s.set(&saved).unwrap();
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz::new(1_000.0));
}

/// **A policy is not saved**: the output policy is in no capture, and the tempo is.
#[test]
fn the_output_policy_is_not_saved_and_the_tempo_is() {
    let s = detached();
    let saved = s.saved_values().unwrap();

    assert!(saved.iter().all(|(t, _)| !t.is(s.policy::<OutputPolicy>())));
    assert!(
        saved
            .iter()
            .any(|(t, _)| t.is(s.transport_handle().tempo()))
    );
}

/// **`values_of` leaves out an assignment whose target is not saved**: the native
/// param stays, the output policy goes.
#[test]
fn values_of_skips_an_assignment_to_the_output_policy() {
    let mut s = detached();
    let lp = filter(&mut s);
    let items = [
        lp.cutoff().to(Hz::new(500.0)).erase(),
        s.policy::<OutputPolicy>()
            .to(OutputPolicy::default())
            .erase(),
    ];

    let values = s.values_of(items);

    assert_eq!(values.len(), 1);
    assert!(values.iter().all(|(t, _)| t.is(lp.cutoff())));
    s.set(&values).unwrap();
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz::new(500.0));
}

/// **`remove` takes one target out and leaves the capture it was cloned from**.
#[test]
fn removing_a_target_from_a_clone_leaves_the_original() {
    let mut s = detached();
    let lp = filter(&mut s);
    let saved = s.saved_values().unwrap();
    let mut edited = saved.clone();

    let (cutoff, _) = saved
        .iter()
        .find(|(t, _)| t.is(lp.cutoff()))
        .expect("the cutoff is saved");
    assert!(edited.remove(&cutoff).is_some());

    assert!(edited.remove(&cutoff).is_none());
    assert_eq!(edited.len() + 1, saved.len());
    s.set(lp.cutoff().to(Hz::new(2_000.0))).unwrap();
    s.set(&edited).unwrap();
    assert_eq!(
        s.get(lp.cutoff()).unwrap(),
        Hz::new(2_000.0),
        "the removed target is not restored"
    );
    s.set(&saved).unwrap();
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz::new(1_000.0));
}

/// **A capture is `Send + Sync` and reads from another thread**.
#[test]
fn a_capture_restores_from_the_values_handle_on_another_thread() {
    let mut s = detached();
    let lp = filter(&mut s);
    let values = s.values();
    let saved = values.saved_values().unwrap();
    s.set(lp.cutoff().to(Hz::new(300.0))).unwrap();

    std::thread::scope(|scope| {
        scope.spawn(|| values.set(&saved).unwrap()).join().unwrap();
    });

    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz::new(1_000.0));
}

/// **Every saved entry of a removed node is found by the node**: an app that drops a removed
/// node's entries from a saved map filters by `AnyTarget::node`, and the node's mode, mix and
/// setting entries are among those it finds, so none survives to land on a node that reuses the
/// key.
#[test]
fn a_removed_nodes_mode_mix_and_setting_are_found_by_its_node() {
    use tutti_buffers::ChannelTopology;
    use tutti_music::NoteDuration;
    use tutti_nodes::{DelayConfig, DelayLayout, DelayNode, DelaySync, MaxDelay, NoteTimes};
    use tutti_units::{Mix, Seconds};

    let mut s = detached();
    let layout = DelayLayout::builder(ChannelTopology::MONO, MaxDelay::of(Seconds::new(2.0)))
        .notes(NoteTimes::all(NoteDuration::QUARTER))
        .build()
        .expect("a layout");
    let delay = s
        .graph()
        .add(DelayNode::<f32>::from(DelayConfig::new(layout)));
    s.set((
        delay.mix().to(Mix::new(0.5)),
        delay.sync().to(DelaySync::Synced),
    ))
    .unwrap();
    let mut saved = s.saved_values().unwrap();
    s.remove(delay).unwrap();

    let ours: Vec<_> = saved
        .iter()
        .map(|(target, _)| target)
        .filter(|target| target.node() == Some(delay.into()))
        .collect();
    assert!(ours.iter().any(|t| t.is(delay.mix())), "the mix is found");
    assert!(ours.iter().any(|t| t.is(delay.mode())), "the mode is found");
    assert!(
        ours.iter().any(|t| t.is(delay.sync())),
        "the setting is found"
    );
    for target in &ours {
        saved.remove(target);
    }
    assert!(
        saved
            .iter()
            .all(|(t, _)| !t.is(delay.mix()) && !t.is(delay.mode()) && !t.is(delay.sync())),
        "no entry of the removed node survives its filter"
    );
}
