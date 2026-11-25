//! A plugin's saved state and its restore over a fake control: what the
//! plugin was sent, and what [`Session::apply`] refuses by name.

use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_graph::{ForkByClone, NodeKey};
use tutti_nodes::testing::Const;
use tutti_plugin_host::{PluginControls, PluginHandle};
use tutti_plugin_types::fake::{Call, FakePlugin};
use tutti_plugin_types::{PluginRef, PluginTail, StateError, StateUse};
use tutti_units::{SampleRate, Samples};

use super::{Inert, Run as _, host};
use crate::plugin::{PluginApplyError, PluginSignal};
use crate::{AnyPlugin, DetachedSession, Key, PeerState, Session};

fn handle() -> (PluginHandle, FakePlugin) {
    let (fake, loaded) = FakePlugin::new();
    let control: Arc<dyn tutti_plugin_host::MainThreadControl> = Arc::new(loaded);
    (handle_over(control), fake)
}

fn handle_over(control: Arc<dyn tutti_plugin_host::MainThreadControl>) -> PluginHandle {
    PluginHandle::from_backend(
        Arc::new(Inert::default()),
        Some(control),
        tutti_plugin_types::PluginDescriptor::new("", tutti_plugin_types::PluginClass::Unknown),
        Default::default(),
        Default::default(),
    )
}

fn session() -> DetachedSession {
    Session::builder()
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds")
}

fn plugin() -> PluginRef {
    PluginRef::file("fake.clap")
}

/// A session with a fake plugin hosted at a node.
fn hosted() -> (DetachedSession, Key<AnyPlugin>, FakePlugin) {
    let mut s = session();
    let (handle, fake) = handle();
    let node = s.graph().add(ForkByClone(Const::mono(0.0)));
    let key = host(&mut s, node, handle, plugin());
    s.commit();
    (s, key, fake)
}

/// How many refreshes of the parameter values wait at `node`.
fn refreshes(s: &DetachedSession, node: Key<AnyPlugin>) -> usize {
    let node: NodeKey = node.erase();
    let hosted = s.graph.plugins.get(&node).expect("hosted");
    let waiting = hosted.notices.lock();
    waiting
        .iter()
        .filter(|signal| matches!(signal, PluginSignal::Refresh(_)))
        .count()
}

/// **A save and a restore carry the bytes both ways, and an applied restore
/// queues one read of the parameters.** The bytes of a state restored are the
/// bytes the plugin is sent, and a save reads what the plugin then holds.
#[test]
fn a_restored_state_reaches_the_plugin_and_is_applied_once() {
    let (mut s, node, fake) = hosted();
    let main = tutti_fixtures::main_thread();
    let state = PeerState::of_plugin(plugin(), vec![1, 2, 3, 4, 5]);

    let restored = s
        .restore_state(&node, state.clone())
        .unwrap()
        .run(main)
        .unwrap();
    assert!(fake.calls().contains(&Call::LoadState(5)));
    assert_eq!(refreshes(&s, node), 0, "nothing is queued before the apply");
    s.apply(restored).unwrap();
    assert_eq!(refreshes(&s, node), 1);

    let saved = s
        .save_state(&node, StateUse::Project)
        .unwrap()
        .run(main)
        .unwrap();
    assert_eq!(saved.bytes(), state.bytes());
}

/// **A state of another plugin is refused at the call and nothing is sent.**
#[test]
fn a_state_of_another_plugin_sends_nothing() {
    let (s, node, fake) = hosted();
    let foreign = PeerState::of_plugin(PluginRef::file("other.clap"), vec![9]);
    assert!(matches!(
        s.restore_state(&node, foreign),
        Err(crate::StateStartError::OtherPeer(_))
    ));
    assert!(!fake.calls().iter().any(|c| matches!(c, Call::LoadState(_))));
}

/// **An output applied to a removed node is `Gone`.**
#[test]
fn a_restore_applied_after_its_node_was_removed_is_gone() {
    let (mut s, node, _fake) = hosted();
    let main = tutti_fixtures::main_thread();
    let restored = s
        .restore_state(&node, PeerState::of_plugin(plugin(), vec![1]))
        .unwrap()
        .run(main)
        .unwrap();
    s.graph().remove(node).unwrap();
    assert_eq!(s.apply(restored).unwrap_err(), PluginApplyError::Gone);
}

/// Replaces the plugin at `node` with a new fake, as a crash recovery or a swap
/// does: the record takes another handle, so the outgoing instance is not the
/// node's.
fn replace(s: &mut DetachedSession, node: Key<AnyPlugin>) -> FakePlugin {
    let (replacement, fake) = handle();
    let (controls, _) =
        PluginControls::new(Samples::new(0), PluginTail::default(), SampleRate::SR_48K);
    let outgoing = s
        .graph
        .plugins
        .get_mut(&node.erase())
        .unwrap()
        .replace_hosted(replacement, (*controls).clone(), plugin());
    drop(outgoing);
    fake
}

/// **An output applied after the node's plugin was replaced is `Stale`, and
/// the new plugin is not asked to read its parameters.**
#[test]
fn a_restore_applied_after_the_plugin_was_replaced_is_stale() {
    let (mut s, node, _fake) = hosted();
    let main = tutti_fixtures::main_thread();
    let restored = s
        .restore_state(&node, PeerState::of_plugin(plugin(), vec![1]))
        .unwrap()
        .run(main)
        .unwrap();
    let _new = replace(&mut s, node);
    assert_eq!(refreshes(&s, node), 0);

    assert_eq!(s.apply(restored).unwrap_err(), PluginApplyError::Stale);
    assert_eq!(refreshes(&s, node), 0, "a refused apply queues nothing");
}

/// **A restore run after the plugin was replaced refuses `Lost` and sends the
/// outgoing instance nothing**, which is still alive until it is retired.
#[test]
fn a_restore_run_after_the_plugin_was_replaced_is_lost_and_sends_nothing() {
    let (mut s, node, outgoing) = hosted();
    let main = tutti_fixtures::main_thread();
    let restore = s
        .restore_state(&node, PeerState::of_plugin(plugin(), vec![1, 2]))
        .unwrap();
    let _new = replace(&mut s, node);
    assert!(matches!(restore.run(main), Err(StateError::Lost)));
    assert!(
        !outgoing
            .calls()
            .iter()
            .any(|call| matches!(call, Call::LoadState(_))),
        "the outgoing instance received no state"
    );
}

/// **A save run after the node was removed refuses `Lost` and asks the plugin
/// nothing.**
#[test]
fn a_save_run_after_the_node_was_removed_is_lost_and_asks_nothing() {
    let (mut s, node, fake) = hosted();
    let save = s.save_state(&node, StateUse::Project).unwrap();
    s.graph().remove(node).unwrap();
    assert!(matches!(
        save.run(tutti_fixtures::main_thread()),
        Err(StateError::Lost)
    ));
    assert!(!fake.calls().contains(&Call::SaveState));
}

/// **Two applies before the next phase queue one read of the parameters.**
#[test]
fn applies_before_a_phase_queue_one_read() {
    let (mut s, node, _fake) = hosted();
    let main = tutti_fixtures::main_thread();
    for bytes in [vec![1], vec![2]] {
        let restored = s
            .restore_state(&node, PeerState::of_plugin(plugin(), bytes))
            .unwrap()
            .run(main)
            .unwrap();
        s.apply(restored).unwrap();
    }
    assert_eq!(refreshes(&s, node), 1);
}

/// **An output of another session is `Stale` there, and changes nothing.**
#[test]
fn a_restore_of_another_session_is_stale() {
    let (one, node, _fake) = hosted();
    let (mut other, other_node, _other) = hosted();
    let main = tutti_fixtures::main_thread();
    let restored = one
        .restore_state(&node, PeerState::of_plugin(plugin(), vec![1]))
        .unwrap()
        .run(main)
        .unwrap();
    assert_eq!(other.apply(restored).unwrap_err(), PluginApplyError::Stale);
    assert_eq!(refreshes(&other, other_node), 0);
}

/// A control whose host writes a node is draining until `release`.
struct Draining {
    plugin: tutti_plugin_types::fake::FakeLoaded,
    draining: std::sync::atomic::AtomicBool,
}

impl tutti_plugin_host::MainThreadControl for Draining {
    fn lend(&self, f: &mut dyn FnMut(&mut dyn tutti_plugin_types::Control)) {
        f(&mut self.plugin.clone());
    }

    fn settle(
        &self,
        _main: tutti_plugin_types::MainThreadMarker,
        _pending: &mut tutti_plugin_host::PendingWrites,
    ) -> std::task::Poll<()> {
        if self.draining.load(std::sync::atomic::Ordering::Acquire) {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    }

    fn editor_idle(&self, _main: tutti_plugin_types::MainThreadMarker) {}
}

/// **A save is not asked of the plugin while a node drains the writes before
/// it, resolves once the drain ends, and stalls at its deadline when it does
/// not.** Nothing is waited for: each poll answers at the instant it is given.
#[test]
fn a_save_is_pending_while_a_node_drains_and_stalls_at_its_deadline() {
    use std::sync::atomic::Ordering;
    use std::task::Poll;

    let main = tutti_fixtures::main_thread();
    let saves = |fake: &FakePlugin| {
        fake.calls()
            .iter()
            .filter(|call| matches!(call, Call::SaveState))
            .count()
    };
    let (fake, loaded) = FakePlugin::new();
    let control = Arc::new(Draining {
        plugin: loaded,
        draining: true.into(),
    });
    let mut s = session();
    let node = s.graph().add(ForkByClone(Const::mono(0.0)));
    let key = host(&mut s, node, handle_over(control.clone()), plugin());
    s.commit();

    let now = tutti_platform::Instant::now();
    let crate::SaveState::InProcess(mut save) = s.save_state(&key, StateUse::Project).unwrap()
    else {
        panic!("a fake plugin runs in process");
    };
    assert_eq!(
        save.deadline(),
        None,
        "nothing is due before the first poll"
    );
    assert!(save.poll(main, now).is_pending());
    assert_eq!(
        saves(&fake),
        0,
        "the plugin is not asked while writes drain"
    );
    control.draining.store(false, Ordering::Release);
    assert!(matches!(save.poll(main, now), Poll::Ready(Ok(_))));
    assert_eq!(saves(&fake), 1);

    control.draining.store(true, Ordering::Release);
    let crate::SaveState::InProcess(mut stuck) = s.save_state(&key, StateUse::Project).unwrap()
    else {
        panic!("a fake plugin runs in process");
    };
    assert!(stuck.poll(main, now).is_pending());
    let due = stuck.deadline().expect("polled once");
    assert!(
        stuck
            .poll(main, due - std::time::Duration::from_millis(1))
            .is_pending()
    );
    assert!(matches!(
        stuck.poll(main, due),
        Poll::Ready(Err(StateError::Stalled { .. }))
    ));
    assert_eq!(saves(&fake), 1, "a stalled save asked nothing");
}
