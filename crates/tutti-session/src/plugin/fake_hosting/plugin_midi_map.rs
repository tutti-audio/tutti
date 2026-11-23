//! The app's controller map of a hosted plugin as a value: `Plugin::midi_map`
//! names the target, a set is checked at the call, held and delivered to the
//! plugin's control on the main thread's update, and a get reads it back. The
//! plugin is `tutti_plugin_types::fake`'s, which records the entries it is
//! handed.

use super::plugin_params::Fake;
use super::{await_tasks, host};
use std::sync::Arc;

use crate::{
    DetachedSession, MidiMap, PluginParamRefusal, Session, SetRefusal, ValueError, ValueErrorKind,
};
use tutti_graph::{ForkByClone, NodeKey};
use tutti_nodes::testing::Const;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_types::fake::FakePlugin;
use tutti_plugin_types::{MidiAssignment, MidiSource};
use tutti_ump::{MidiChannel, U7};
use tutti_units::{ParamId, SampleRate};

const CUTOFF: ParamId = ParamId::new(1);
const LEVEL: ParamId = ParamId::new(2);
const ABSENT: ParamId = ParamId::new(99);

fn cc(number: u8) -> MidiSource {
    MidiSource::ControlChange(U7::of(number))
}

fn entry(channel: Option<MidiChannel>, source: MidiSource, param: ParamId) -> MidiAssignment {
    MidiAssignment {
        channel,
        source,
        param,
    }
}

/// A detached session with a fake plugin that lists `CUTOFF` and `LEVEL` among
/// its parameters (`plugin_params`' backend), read.
fn hosting() -> (DetachedSession, NodeKey, FakePlugin) {
    let (fake, loaded) = FakePlugin::new();
    let control: Arc<dyn tutti_plugin_host::MainThreadControl> = Arc::new(loaded);
    let handle = PluginHandle::from_backend(
        Fake::new(),
        Some(control),
        tutti_plugin_types::PluginDescriptor::new("", tutti_plugin_types::PluginClass::Unknown),
        Default::default(),
        Default::default(),
    );
    let mut s = Session::builder()
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    let node = s.graph().add(ForkByClone(Const::mono(0.0)));
    host(
        &mut s,
        node,
        handle,
        tutti_plugin_types::PluginRef::file("fake.clap"),
    );
    s.commit();
    let node = node.erase();
    update(&mut s);
    await_tasks(&s);
    update(&mut s);
    (s, node, fake)
}

fn update(s: &mut DetachedSession) {
    let _ = s.update_on(tutti_fixtures::main_thread());
}

fn app_map() -> Vec<MidiAssignment> {
    vec![
        entry(Some(MidiChannel::of(2)), cc(21), LEVEL),
        entry(None, MidiSource::PitchBend, CUTOFF),
    ]
}

fn refused(error: ValueError<&'static str>) -> SetRefusal {
    match error.kind {
        ValueErrorKind::Refused(refusal) => refusal,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// **A set reaches the plugin's control once, with exactly the app's entries,
/// on the main thread's update, and a get reads it back.** Nothing is sent
/// before the update and a second update has nothing owed.
#[test]
fn a_set_midi_map_reaches_the_plugins_control_once_and_reads_back() {
    let (mut s, node, fake) = hosting();
    let target = s.plugin_view(node).unwrap().midi_map();
    assert_eq!(s.get(target).unwrap(), MidiMap::default());

    s.set(target.to(MidiMap::new(app_map()))).unwrap();
    assert!(
        fake.app_midi_maps().is_empty(),
        "held until the main thread"
    );
    assert_eq!(s.get(target).unwrap().assignments(), app_map());

    update(&mut s);
    update(&mut s);
    assert_eq!(fake.app_midi_maps(), [app_map()]);
}

/// **A map naming a parameter the plugin does not list is refused at the call
/// with that id, and nothing reaches the control.**
#[test]
fn a_map_naming_an_unlisted_parameter_is_refused_with_its_id() {
    let (mut s, node, fake) = hosting();
    let target = s.plugin_view(node).unwrap().midi_map();
    let map = MidiMap::new([entry(None, cc(1), LEVEL), entry(None, cc(2), ABSENT)]);
    let error = s.set(target.to(map)).unwrap_err();
    assert_eq!(
        refused(error),
        SetRefusal::Plugin(PluginParamRefusal::UnknownParam(ABSENT))
    );
    update(&mut s);
    assert!(fake.app_midi_maps().is_empty());
    assert_eq!(s.get(target).unwrap(), MidiMap::default());
}

/// **Two app entries for one channel and source are refused at the call, and
/// nothing reaches the control**; the same source on another channel, or on
/// any channel, is a different key.
#[test]
fn a_duplicate_channel_and_source_is_refused() {
    let (mut s, node, fake) = hosting();
    let target = s.plugin_view(node).unwrap().midi_map();
    let channel = Some(MidiChannel::of(3));
    let twice = MidiMap::new([entry(channel, cc(7), LEVEL), entry(channel, cc(7), CUTOFF)]);
    let error = s.set(target.to(twice)).unwrap_err();
    assert_eq!(
        refused(error),
        SetRefusal::Plugin(PluginParamRefusal::DuplicateMidiEntry {
            channel,
            message: cc(7)
        })
    );
    update(&mut s);
    assert!(fake.app_midi_maps().is_empty());

    let distinct = MidiMap::new([
        entry(channel, cc(7), LEVEL),
        entry(None, cc(7), CUTOFF),
        entry(Some(MidiChannel::of(4)), cc(7), CUTOFF),
    ]);
    s.set(target.to(distinct)).unwrap();
}

/// **The map is a saved value: `saved_values()` holds it and `set(&saved)`
/// after a change puts it back and delivers it again.**
#[test]
fn the_midi_map_is_saved_and_restored_and_delivered_again() {
    let (mut s, node, fake) = hosting();
    let target = s.plugin_view(node).unwrap().midi_map();
    s.set(target.to(MidiMap::new(app_map()))).unwrap();
    update(&mut s);
    let saved = s.saved_values().unwrap();
    assert!(
        saved.iter().any(|(held, _)| held.is(target)),
        "the saved values hold the map"
    );

    s.set(target.to(MidiMap::default())).unwrap();
    update(&mut s);
    assert_eq!(fake.app_midi_maps(), [app_map(), vec![]]);

    s.set(&saved).unwrap();
    assert_eq!(s.get(target).unwrap().assignments(), app_map());
    update(&mut s);
    assert_eq!(fake.app_midi_maps(), [app_map(), vec![], app_map()]);
}

/// **A watcher sees the set**, with the map as its new base.
#[test]
fn a_watcher_sees_a_set_midi_map() {
    let (s, node, _fake) = hosting();
    let target = s.plugin_view(node).unwrap().midi_map();
    let mut watcher = s.watch(target).unwrap();
    assert!(watcher.changes().unwrap().is_none());
    s.set(target.to(MidiMap::new(app_map()))).unwrap();
    let change = watcher.changes().unwrap().expect("the set is a change");
    assert_eq!(change.value.assignments(), app_map());
}

/// **`effective` lets an app entry replace the declared entry on the same
/// channel and source, and passes the other declared entries**: declared
/// survivors first in their order, then the app's.
#[test]
fn effective_replaces_a_declared_entry_per_channel_and_source() {
    let ch = Some(MidiChannel::of(0));
    let declared = [
        entry(ch, cc(74), CUTOFF),
        entry(ch, cc(7), LEVEL),
        entry(None, cc(74), CUTOFF),
    ];
    let app = MidiMap::new([entry(ch, cc(74), LEVEL), entry(ch, cc(21), CUTOFF)]);
    let effective: Vec<_> = app.effective(&declared).collect();
    assert_eq!(
        effective,
        [
            entry(ch, cc(7), LEVEL),
            entry(None, cc(74), CUTOFF),
            entry(ch, cc(74), LEVEL),
            entry(ch, cc(21), CUTOFF),
        ]
    );
}

/// **A removed plugin has no MIDI map target**: the value went with the
/// plugin's slot.
#[test]
fn a_removed_plugin_has_no_midi_map_target() {
    let (mut s, node, _fake) = hosting();
    let target = s.plugin_view(node).unwrap().midi_map();
    s.graph().remove(node).expect("the node is removed");
    assert!(s.get(target).is_err());
    assert!(s.set(target.to(MidiMap::default())).is_err());
}

/// **A map's wire form is its entries, in order, and a list that names a channel
/// and source twice is not a map.**
#[cfg(feature = "serde")]
#[test]
fn a_midi_map_round_trips_its_entries_and_refuses_a_duplicate_on_the_wire() {
    let map = MidiMap::new(app_map());
    let wire = serde_json::to_string(&map).unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<MidiAssignment>>(&wire).unwrap(),
        app_map(),
        "the wire form is the entry list"
    );
    assert_eq!(serde_json::from_str::<MidiMap>(&wire).unwrap(), map);

    let twice = serde_json::to_string(&[app_map()[0], app_map()[0]]).unwrap();
    assert!(serde_json::from_str::<MidiMap>(&twice).is_err());
}
