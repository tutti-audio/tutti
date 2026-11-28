//! A hosted plugin's track info as a value: `Plugin::track_info` names the
//! target, a set is held and delivered to the plugin's control on the main
//! thread's update, and a get reads back what was set. The plugin is
//! `tutti_plugin_types::fake`'s, which records the `TrackInfo` it is handed.

use super::{Inert, host};
use std::sync::Arc;

use crate::Session;
use tutti_buffers::ChannelLayout;
use tutti_graph::{ForkByClone, NodeKey};
use tutti_nodes::testing::Const;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::fake::FakePlugin;
use tutti_plugin_types::{Delivered, TrackInfo, TrackKind};
use tutti_units::SampleRate;

/// An offline session with a fake plugin at a node.
fn hosting() -> (Session, NodeKey, FakePlugin) {
    let (fake, loaded) = FakePlugin::new();
    let control: Arc<dyn tutti_plugin_host::MainThreadControl> = Arc::new(loaded);
    let handle = PluginHandle::from_backend(
        Arc::new(Inert::default()),
        Some(control),
        tutti_plugin_types::PluginDescriptor::new("", tutti_plugin_types::PluginClass::Unknown),
        Default::default(),
        Default::default(),
    );
    let mut s = Session::builder()
        .sample_rate(SampleRate::DEFAULT)
        .block_dates(crate::BlockDates::Frames)
        .build()
        .expect("an offline session builds");
    let node = s.graph().add(ForkByClone(Const::mono(0.0)));
    host(
        &mut s,
        node,
        handle,
        tutti_plugin_types::PluginRef::file("fake.clap"),
    );
    s.commit();
    (s, node.erase(), fake)
}

fn drums() -> TrackInfo {
    TrackInfo {
        name: Some("Drums".into()),
        audio: Some(ChannelLayout::STEREO),
        kind: TrackKind::Bus,
        ..TrackInfo::default()
    }
}

fn update(s: &mut Session) {
    let _ = s.update_on(tutti_fixtures::main_thread());
}

/// **A set reaches the plugin's control once, on the main thread's update, and
/// a get reads it back.** Nothing is sent before the update and a second update
/// has nothing owed.
#[test]
fn a_set_track_info_reaches_the_plugins_control_once_and_reads_back() {
    let (mut s, node, fake) = hosting();
    let target = s.plugin_view(node).unwrap().track_info();
    assert_eq!(s.get(target).unwrap(), TrackInfo::default());

    s.set(target.to(drums())).unwrap();
    assert_eq!(s.get(target).unwrap(), drums());
    assert!(fake.track_infos().is_empty(), "held until the main thread");

    update(&mut s);
    update(&mut s);
    assert_eq!(fake.track_infos(), [drums()]);
}

/// **A queue that was full is retried at the next update; a plugin that is
/// gone is not.** Neither refuses the set, and the value reads back.
#[test]
fn a_dropped_track_info_is_delivered_again_and_a_dead_plugin_is_told_once() {
    let (mut s, node, fake) = hosting();
    let target = s.plugin_view(node).unwrap().track_info();
    fake.answer_track_info(Delivered::Dropped);
    s.set(target.to(drums())).unwrap();
    update(&mut s);
    assert_eq!(fake.track_infos().len(), 1);
    fake.answer_track_info(Delivered::Yes);
    update(&mut s);
    update(&mut s);
    assert_eq!(
        fake.track_infos(),
        [drums(), drums()],
        "once more, then settled"
    );

    fake.answer_track_info(Delivered::PluginDead);
    s.set(target.to(TrackInfo::default())).unwrap();
    update(&mut s);
    update(&mut s);
    assert_eq!(fake.track_infos().len(), 3, "a dead plugin is told once");
    assert_eq!(s.get(target).unwrap(), TrackInfo::default());
}

/// **A removed plugin has no track info target**: the value went with the
/// plugin's slot.
#[test]
fn a_removed_plugin_has_no_track_info_target() {
    let (mut s, node, _fake) = hosting();
    let target = s.plugin_view(node).unwrap().track_info();
    s.set(target.to(drums())).unwrap();
    s.graph().remove(node).expect("the node is removed");
    assert!(s.get(target).is_err(), "the target went with its plugin");
}

/// **Track info is not saved**: it is the host's setup of the plugin's place,
/// like the automation mode, so a preset made of it holds nothing.
#[test]
fn track_info_is_not_a_saved_value() {
    let (s, node, _fake) = hosting();
    let target = s.plugin_view(node).unwrap().track_info();
    assert!(s.values_of([target.to(drums()).erase()]).is_empty());
}
