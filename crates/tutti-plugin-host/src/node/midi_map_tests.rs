//! The plugin node's controller map, through the real node over the fake
//! plugin: a mapped controller moves its parameter by the analytic figure and
//! is taken out of the plugin's MIDI, an unmapped message is not.

use std::sync::Arc;

use tutti_fixtures::{main_thread, midi};
use tutti_graph::contract::Direct;
use tutti_graph::{Event, Node, Offset};
use tutti_plugin_types::fake::{Call, FakeLoaded, FakePlugin};
use tutti_plugin_types::{
    Control, Loaded, MaxBlock, MidiAssignment, MidiMapApplied, MidiSource, Prepare, Request,
};
use tutti_tempo::TempoMap;
use tutti_timeline::TempoReader;
use tutti_ump::{CCNumber, MidiChannel, U7};
use tutti_units::{Bpm, ParamId, SampleRate, Samples};

use super::client::PluginNode;
use super::instance::Instance;
use crate::{MidiMapSlot, client_over};
use tutti_rt::Mutex;

const SR: SampleRate = SampleRate::SR_48K;
const BLOCK: usize = 64;
const LEVEL: ParamId = ParamId::new(1);
const OTHER: ParamId = ParamId::new(2);

fn tempo() -> TempoReader {
    TempoReader::fixed(Arc::new(
        TempoMap::constant(Bpm::of(120.0)).expect("a tempo"),
    ))
}

fn cc74(param: ParamId) -> MidiAssignment {
    MidiAssignment {
        channel: Some(MidiChannel::of(0)),
        source: MidiSource::ControlChange(U7::of(74)),
        param,
    }
}

/// A node over a fake declaring parameters 1 and 2 and the controller table
/// `declared`, built as an out-of-process node is: over an activated plugin, never
/// pumped.
fn rig(
    declared: &[MidiAssignment],
) -> (
    FakePlugin,
    Direct<PluginNode<FakeLoaded>>,
    Arc<Mutex<Instance<FakeLoaded>>>,
    MidiMapSlot,
) {
    rig_answering(declared, MidiMapApplied::Host)
}

/// [`rig`], the plugin answering `applied` to the app's entries.
fn rig_answering(
    declared: &[MidiAssignment],
    applied: MidiMapApplied,
) -> (
    FakePlugin,
    Direct<PluginNode<FakeLoaded>>,
    Arc<Mutex<Instance<FakeLoaded>>>,
    MidiMapSlot,
) {
    let (fake, loaded) = FakePlugin::new();
    fake.declare(&[(LEVEL, false), (OTHER, false)]);
    fake.declare_midi_map(declared);
    fake.apply_midi_map(applied);
    let main = main_thread();
    let wiring = loaded.wiring(main);
    let descriptor = loaded.info().clone();
    let prepare = Prepare::new(SR, MaxBlock::new(Samples::new(BLOCK)).expect("a block"));
    let Ok((processor, controller)) = loaded.activate(main, prepare) else {
        panic!("the fake activates");
    };
    let client = client_over::<FakeLoaded, f32>(
        processor,
        controller,
        prepare,
        descriptor,
        &wiring,
        [LEVEL, OTHER],
        0,
        None,
    )
    .expect("the fake is narrow");
    let inner = Arc::clone(&client.inner);
    let slot = client.midi_map();
    let unit = Direct::new(client.bind(tempo()), SR, BLOCK);
    (fake, unit, inner, slot)
}

/// The node's event input fed `events`, one block rendered.
fn block(unit: &mut Direct<PluginNode<FakeLoaded>>, events: &[(usize, tutti_ump::Ump)]) {
    let port = unit.node.shape().event_input(0).expect("an event input");
    let events: Vec<Event> = events
        .iter()
        .map(|&(frame, ump)| Event::midi(Offset::of(Samples::new(frame), Samples::new(BLOCK)), ump))
        .collect();
    unit.events(port, &events);
    unit.block();
}

fn params(fake: &FakePlugin) -> Vec<(ParamId, f64)> {
    fake.calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::BlockParam(id, value) => Some((id, value.get())),
            _ => None,
        })
        .collect()
}

fn midi_seen(fake: &FakePlugin) -> usize {
    fake.calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::BlockMidi(n) => Some(n),
            _ => None,
        })
        .sum()
}

fn cc(channel: u8, controller: CCNumber, word: u32) -> tutti_ump::Ump {
    midi::cc(MidiChannel::of(channel), controller, word)
}

/// **A controller the plugin declared moves its parameter by the controller's
/// value over the 32-bit full scale, and the plugin is not also sent it as MIDI**,
/// from the first block of a node that was never pumped (an export's fork);
/// a controller and a note nothing maps reach the plugin as MIDI.
#[test]
fn a_declared_controller_moves_its_parameter_and_leaves_the_plugins_midi() {
    let (fake, mut unit, _inner, _slot) = rig(&[cc74(LEVEL)]);
    let word = 0x4000_0000_u32;
    block(
        &mut unit,
        &[
            (5, cc(0, CCNumber::BRIGHTNESS, word)),
            (6, cc(0, CCNumber::MOD_WHEEL, word)),
            (7, midi::note_on(MidiChannel::of(0), 60, 100)),
        ],
    );
    assert_eq!(
        params(&fake),
        [(LEVEL, f64::from(word) / f64::from(u32::MAX))]
    );
    assert_eq!(midi_seen(&fake), 2, "the mod wheel and the note remain");
}

/// **An app entry from the handle's path replaces the declared one for its
/// channel and message and the node follows from its next block**, and an entry
/// answered by the plugin (an AU) is not translated by the node.
#[test]
fn the_apps_entry_replaces_the_declared_one_unless_the_plugin_applies_it() {
    let (fake, mut unit, _inner, slot) = rig(&[cc74(LEVEL)]);
    slot.hand_app(&[cc74(OTHER)], |_| MidiMapApplied::Host);
    let word = u32::MAX;
    block(&mut unit, &[(0, cc(0, CCNumber::BRIGHTNESS, word))]);
    assert_eq!(params(&fake), [(OTHER, 1.0)]);

    fake.clear_calls();
    slot.hand_app(&[cc74(OTHER)], |_| MidiMapApplied::Plugin);
    block(&mut unit, &[(0, cc(0, CCNumber::BRIGHTNESS, word))]);
    assert_eq!(
        params(&fake),
        [(LEVEL, 1.0)],
        "the app's entry is the plugin's, the declared one is still the host's"
    );
    assert_eq!(midi_seen(&fake), 0);
}

/// **A plugin whose wiring says it applies its own declared table has it left to
/// itself.**
#[test]
fn a_plugin_that_applies_its_own_table_is_not_translated() {
    let (fake, mut unit, _inner, _slot) = rig_answering(&[cc74(LEVEL)], MidiMapApplied::Plugin);
    block(&mut unit, &[(0, cc(0, CCNumber::BRIGHTNESS, u32::MAX))]);
    assert!(params(&fake).is_empty());
    assert_eq!(midi_seen(&fake), 1);
}

/// **A table the plugin re-declares (`kMidiCCAssignmentChanged`) is the node's
/// from the next tick.**
#[test]
fn a_redeclared_table_is_the_nodes_after_the_tick() {
    let (fake, mut unit, inner, _slot) = rig(&[cc74(LEVEL)]);
    fake.declare_midi_map(&[cc74(OTHER)]);
    assert!(Request::MidiCcAssignmentChanged.raise(fake.request_flags()));
    let changes = inner.lock().pump(main_thread());
    assert!(
        matches!(
            changes.reports.as_slice(),
            [tutti_plugin_types::PluginInvalidation::MidiMap]
        ),
        "the handle's subscribers are told once: {:?}",
        changes.reports
    );
    block(&mut unit, &[(0, cc(0, CCNumber::BRIGHTNESS, u32::MAX))]);
    assert_eq!(params(&fake), [(OTHER, 1.0)]);
}

/// **The entries are handed to the plugin that is asked, and its answer is the one
/// published**: through the plugin's own `Control`, as a fork does for its own
/// instance, a plugin answering `Plugin` (an AU) is handed the entries and the
/// node leaves them out of its table.
#[test]
fn the_entries_are_handed_to_the_plugin_and_its_answer_is_published() {
    let (fake, mut unit, inner, slot) = rig_answering(&[], MidiMapApplied::Plugin);
    let main = main_thread();
    let app = [cc74(OTHER)];
    let answer = {
        let mut instance = inner.lock();
        let control = instance.control().expect("alive");
        slot.hand_app(&app, |app| control.set_app_midi_map(main, app))
    };
    assert_eq!(answer, MidiMapApplied::Plugin);
    assert_eq!(
        fake.app_midi_maps(),
        [app.to_vec()],
        "the plugin was handed them"
    );
    block(&mut unit, &[(0, cc(0, CCNumber::BRIGHTNESS, u32::MAX))]);
    assert!(
        params(&fake).is_empty(),
        "the unit applies them, not the node"
    );
    assert_eq!(midi_seen(&fake), 1);
}
