//! A plugin's saved state and its restore over a fake control: what the
//! plugin was sent, and what [`Session::apply`] refuses by name.

use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_graph::{ForkByClone, NodeKey};
use tutti_nodes::testing::Const;
use tutti_plugin_host::{PluginControls, PluginHandle};
use tutti_plugin_types::fake::{Call, FakePlugin};
use tutti_plugin_types::{PluginRef, PluginTail, StateUse};
use tutti_units::{SampleRate, Samples};

use super::{Inert, host};
use crate::plugin::{PluginApplyError, PluginSignal};
use crate::{AnyPlugin, DetachedSession, Key, PeerState, Session};

fn handle() -> (PluginHandle, FakePlugin) {
    let (fake, loaded) = FakePlugin::new();
    let control: Arc<dyn tutti_plugin_host::MainThreadControl> = Arc::new(loaded);
    let handle = PluginHandle::from_backend(
        Arc::new(Inert::default()),
        Some(control),
        Default::default(),
        Default::default(),
        Default::default(),
    );
    (handle, fake)
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
    let (node, _) = s.graph().add(ForkByClone(Const::mono(0.0)));
    let key = host(&mut s, node, handle, plugin());
    s.commit();
    (s, key, fake)
}

/// How many refreshes of the parameter values wait at `node`.
fn refreshes(s: &DetachedSession, node: Key<AnyPlugin>) -> usize {
    let node: NodeKey = node.erase();
    let hosted = s.graph.plugins.get(&node).expect("hosted");
    let waiting = hosted.notices.lock().unwrap();
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

    let (replacement, _fake) = handle();
    let (controls, _) =
        PluginControls::new(Samples::new(0), PluginTail::default(), SampleRate::SR_48K);
    let outgoing = s
        .graph
        .plugins
        .get_mut(&node.erase())
        .unwrap()
        .replace_hosted(replacement, (*controls).clone(), plugin());
    drop(outgoing);
    let before = refreshes(&s, node);

    assert_eq!(s.apply(restored).unwrap_err(), PluginApplyError::Stale);
    assert_eq!(
        refreshes(&s, node),
        before,
        "a refused apply changes nothing"
    );
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
