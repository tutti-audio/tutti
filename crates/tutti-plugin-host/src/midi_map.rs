//! The host's controller map: the one place a MIDI message becomes a
//! parameter change.
//!
//! A plugin node turns each mapped Control Change, channel pressure or pitch
//! bend of its event input into a [`Normalized`] point on the parameter the
//! map names, at the message's frame, in the delivery that carries the node's
//! other parameter writes; the message is taken out of the MIDI the plugin
//! reads, so a plugin never applies a mapped controller twice. An unmapped
//! message passes through unchanged. VST3's host is required to do this
//! (`IMidiMapping`); CLAP and VST2 declare no table and map only what the app
//! entered.
//!
//! # What is translated
//!
//! The wiring says who applies the plugin's declared table
//! ([`PluginWiring::assignments_applied`]): the host for VST3 (and for the formats
//! that declare none), the unit for an AU, so the node translates it from its
//! first block with no call to the plugin. [`Control::set_app_midi_map`] answers
//! who applies the app's entries. The map is the declared table the host applies
//! and the app's entries the host applies, together, the app's replacing a
//! declared entry for the same channel and message. What the plugin applies
//! (an AU's unit applies both its tables) is left out, because a second
//! application would be audible.
//!
//! The table holds Control Changes 0 to 127, channel pressure and pitch bend,
//! the controllers VST3 can drive a parameter with. A program change or polyphonic
//! pressure has no such slot (VST3 defines those two numbers for its legacy
//! output event only, and a polyphonic pressure names a key as well): an entry
//! naming one is not translatable, is left out of the table, and is counted by
//! [`MidiMapSlot::untranslatable`]; the message itself reaches the plugin as
//! MIDI. Only UMP Group 0 maps, as the declared table is the plugin's answer
//! for its first event bus.
//!
//! # Values
//!
//! A message is read at MIDI 2.0 width (`tutti_midi_protocol::channel_voice`):
//! a MIDI 1.0 value is widened by the centre-preserving scaling of M2-115, so
//! 0 is 0.0, 64 is the centre and 127 is 1.0. A 32-bit control value is its
//! word over `0xFFFF_FFFF`; a pitch bend maps `-1..=1` onto `0..=1`, the
//! centre at 0.5.
//!
//! # Threads
//!
//! The control side compiles the map into a [`MidiTable`] and publishes it
//! through a [`Publisher`]; the node's [`MidiTranslator`] holds the one
//! [`Reader`], lent once per block (once per piece of a chunked block) and
//! never kept across blocks. [`MidiMapSlot::reclaim`] runs on the instance's
//! control tick. The audio side never allocates, locks or logs: a point that
//! does not fit is counted by the node's [`ParamDelivery`].
//!
//! [`Control::set_app_midi_map`]: tutti_plugin_types::Control::set_app_midi_map

use std::sync::Arc;

use tutti_midi_protocol::channel_voice;
use tutti_plugin_types::{
    MidiAssignment, MidiMapApplied, MidiSource, PluginWiring, effective_midi_map,
};
use tutti_rt::{Loan, Mutex, Publisher, Reader};
use tutti_ump::{Address, MidiChannel, MidiGroup, U7, Ump, Voice2};
use tutti_units::{Bipolar, Normalized, ParamId};

use crate::param_delivery::{ParamDelivery, chunk_offset};

/// The controllers a channel's row holds: Control Changes 0 to 127, channel
/// pressure and pitch bend.
const SOURCES: usize = 130;

/// The row of channel pressure.
const PRESSURE: usize = 128;

/// The row of pitch bend.
const PITCH_BEND: usize = 129;

/// Where `(channel, source)` is in a table, or `None` for a message the table
/// has no row for (a program change, polyphonic pressure). The one place the
/// table's layout is known.
fn index(channel: MidiChannel, source: MidiSource) -> Option<usize> {
    let row = match source {
        MidiSource::ControlChange(controller) => usize::from(controller.get()),
        MidiSource::ChannelPressure => PRESSURE,
        MidiSource::PitchBend => PITCH_BEND,
        _ => return None,
    };
    Some(channel.index() * SOURCES + row)
}

/// The compiled map: the parameter each channel and controller drives.
pub(crate) struct MidiTable {
    /// `index(channel, source)` to the parameter it drives.
    slots: Box<[Option<ParamId>]>,
    /// Whether no slot is mapped: the audio side skips decoding then.
    empty: bool,
}

impl MidiTable {
    /// A table that maps nothing. Allocates: control side.
    fn empty() -> Self {
        Self {
            slots: vec![None; MidiChannel::COUNT as usize * SOURCES].into(),
            empty: true,
        }
    }

    /// The table of [`effective_midi_map`]`(declared, app)`, and how many of its
    /// entries have no row in it ([`MidiMapSlot::untranslatable`]).
    ///
    /// **Channel precedence**: an entry naming a channel wins on that channel over
    /// an any-channel entry for the same message, so the any-channel entries are
    /// written first and the named ones over them; among entries of one kind the
    /// later one wins. Allocates: control side.
    fn effective(declared: &[MidiAssignment], app: &[MidiAssignment]) -> (Self, usize) {
        let mut table = Self::empty();
        let mut untranslatable = 0;
        for any_channel in [true, false] {
            for entry in effective_midi_map(declared, app)
                .filter(|entry| entry.channel.is_none() == any_channel)
            {
                let mut translatable = false;
                for channel in MidiChannel::all() {
                    if entry.channel.is_some_and(|only| only != channel) {
                        continue;
                    }
                    if let Some(i) = index(channel, entry.source) {
                        table.slots[i] = Some(entry.param);
                        table.empty = false;
                        translatable = true;
                    }
                }
                untranslatable += usize::from(!translatable);
            }
        }
        (table, untranslatable)
    }

    /// The parameter `source` on `channel` drives.
    fn lookup(&self, channel: MidiChannel, source: MidiSource) -> Option<ParamId> {
        self.slots[index(channel, source)?]
    }

    /// Takes `ump` into `params` when the map names its controller: a point at
    /// chunk frame `frame`, and `true` for the caller to leave the message out of
    /// the plugin's MIDI. A message nothing maps changes nothing and gives `false`.
    /// Allocation-free.
    pub(crate) fn take(&self, params: &mut ParamDelivery, frame: usize, ump: Ump) -> bool {
        if self.empty {
            return false;
        }
        let Some((Address { group, channel }, voice)) = channel_voice(ump) else {
            return false;
        };
        if group != MidiGroup::FIRST {
            return false;
        }
        let (source, value) = match voice {
            Voice2::ControlChange { controller, value } => (
                MidiSource::ControlChange(U7::masked(controller.get())),
                value.normalized(),
            ),
            Voice2::ChannelPressure(pressure) => {
                (MidiSource::ChannelPressure, pressure.normalized())
            }
            Voice2::PitchBend(bend) => (MidiSource::PitchBend, bend_as_unit(bend.bipolar())),
            _ => return false,
        };
        let Some(param) = self.lookup(channel, source) else {
            return false;
        };
        // A parameter the plugin does not list refuses the point, counted by
        // the delivery; the message was mapped all the same.
        params.point(param, chunk_offset(frame), value.get());
        true
    }
}

/// A bend on the parameter's `0..=1` scale: the bottom of the bend is 0, the
/// centre 0.5 and the top 1.
fn bend_as_unit(bend: Bipolar) -> Normalized {
    Normalized::clamped((bend.get() + 1.0) / 2.0)
}

/// What the control side keeps of the map.
struct State {
    /// The plugin's declared table.
    declared: Vec<MidiAssignment>,
    /// Who applies `declared` ([`PluginWiring::assignments_applied`]).
    declared_applied: MidiMapApplied,
    /// The app's entries, as last set.
    app: Vec<MidiAssignment>,
    /// Who applies `app`; `None` until the app sets some (the plugin is asked
    /// then, with them).
    app_applied: Option<MidiMapApplied>,
    /// Entries of the translated tables the table has no row for.
    untranslatable: usize,
}

struct Shared {
    state: Mutex<State>,
    cell: Publisher<MidiTable>,
}

/// The control side of a plugin node's controller map: what the plugin
/// declares and what the app entered, compiled and published to the node's
/// translator. Cheap to clone; every clone is the same map.
#[derive(Clone)]
pub struct MidiMapSlot {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for MidiMapSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiMapSlot").finish_non_exhaustive()
    }
}

impl MidiMapSlot {
    /// A slot over the table `wiring` declares, translated from the first block
    /// when the host applies it. Allocates.
    #[must_use]
    pub fn new(wiring: &PluginWiring) -> Self {
        let state = State {
            declared: wiring.assignments.clone(),
            declared_applied: wiring.assignments_applied,
            app: Vec::new(),
            app_applied: None,
            untranslatable: 0,
        };
        let (table, untranslatable) = Self::compile(&state);
        let mut state = state;
        state.untranslatable = untranslatable;
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(state),
                cell: Publisher::new(table),
            }),
        }
    }

    /// The node's end: the one reader of the published table. Control side,
    /// where the node is built.
    pub(crate) fn translator(&self) -> MidiTranslator {
        MidiTranslator {
            reader: self.shared.cell.reader(),
        }
    }

    /// Takes the plugin's re-read declared table and who applies it, and
    /// publishes. Control side.
    pub(crate) fn declare(&self, wiring: &PluginWiring) {
        let mut state = self.shared.state.lock();
        state.declared = wiring.assignments.clone();
        state.declared_applied = wiring.assignments_applied;
        self.publish(&mut state);
    }

    /// Hands the app's entries, replacing the last set, to the plugin through
    /// `ask`, which answers who applies them, and publishes the entries the host
    /// applies to the node. The one place the plugin's answer and the node's
    /// table are paired: the handle and each fork call it with their own
    /// plugin's ask, never another's answer. Returns the answer. Control side.
    /// Public for `tutti-plugin`'s fork of an out-of-process node.
    pub fn hand_app(
        &self,
        app: &[MidiAssignment],
        ask: impl FnOnce(&[MidiAssignment]) -> MidiMapApplied,
    ) -> MidiMapApplied {
        let applied = ask(app);
        let mut state = self.shared.state.lock();
        state.app = app.to_vec();
        state.app_applied = Some(applied);
        self.publish(&mut state);
        applied
    }

    /// The app's entries, as last set; `None` before the app set any. Control
    /// side. Public for `tutti-plugin`'s fork of an out-of-process node.
    #[must_use]
    pub fn app(&self) -> Option<Vec<MidiAssignment>> {
        let state = self.shared.state.lock();
        state.app_applied.map(|_| state.app.clone())
    }

    /// Entries of the declared table and the app's that no controller of the
    /// table carries (a program change, polyphonic pressure): not translated,
    /// the message reaches the plugin as MIDI. Zero while the map is empty.
    #[must_use]
    pub fn untranslatable(&self) -> usize {
        self.shared.state.lock().untranslatable
    }

    /// Lets the tables the node no longer reads go. The instance's control tick.
    pub(crate) fn reclaim(&self) {
        self.shared.cell.reclaim();
    }

    /// The table the entries the host applies make: the declared ones when the
    /// host applies them, the app's when the plugin answered `Host` to them.
    fn compile(state: &State) -> (MidiTable, usize) {
        let declared: &[MidiAssignment] = if state.declared_applied == MidiMapApplied::Host {
            &state.declared
        } else {
            &[]
        };
        let app: &[MidiAssignment] = if state.app_applied == Some(MidiMapApplied::Host) {
            &state.app
        } else {
            &[]
        };
        MidiTable::effective(declared, app)
    }

    fn publish(&self, state: &mut State) {
        let (table, untranslatable) = Self::compile(state);
        state.untranslatable = untranslatable;
        self.shared.cell.publish(table);
    }
}

/// The audio side of the map, held by the node: the reader of the published
/// table.
pub(crate) struct MidiTranslator {
    reader: Reader<MidiTable>,
}

impl MidiTranslator {
    /// The table as published now, for one block or one piece of it. Drop the
    /// loan before the call returns.
    pub(crate) fn lend(&mut self) -> Loan<'_, MidiTable> {
        self.reader.lend()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeliveryCounters;
    use tutti_plugin_types::fake::at_frame;
    use tutti_ump::{BendValue, CCNumber, ControlValue, U14};

    fn assign(channel: Option<u8>, source: MidiSource, param: u32) -> MidiAssignment {
        MidiAssignment {
            channel: channel.map(MidiChannel::of),
            source,
            param: ParamId::new(param),
        }
    }

    fn cc(n: u8) -> MidiSource {
        MidiSource::ControlChange(U7::of(n))
    }

    fn table(declared: &[MidiAssignment], app: &[MidiAssignment]) -> MidiTable {
        MidiTable::effective(declared, app).0
    }

    /// A delivery with parameters 1 and 2, closed, and its counters.
    fn delivery() -> (ParamDelivery, Arc<DeliveryCounters>) {
        let counters = Arc::new(DeliveryCounters::default());
        let mut d =
            ParamDelivery::closed([ParamId::new(1), ParamId::new(2)], Arc::clone(&counters));
        d.prepare(256);
        d.begin();
        (d, counters)
    }

    /// The `(offset, value)` points of parameter `id` in the chunk.
    fn points(d: &ParamDelivery, id: u32) -> Vec<(u32, f64)> {
        d.chunk()
            .queues
            .iter()
            .filter(|q| q.param_id == ParamId::new(id))
            .flat_map(|q| q.points.iter())
            .map(|p| (p.sample_offset.get(), p.value.get()))
            .collect()
    }

    fn ump_cc(channel: u8, controller: CCNumber, word: u32) -> Ump {
        tutti_fixtures::midi::cc(MidiChannel::of(channel), controller, word)
    }

    /// **The table's rows are channel by controller**: each of the 16 channels,
    /// the 128 Control Changes, pressure and bend has its own row, in range, and a
    /// program change or polyphonic pressure has none.
    #[test]
    fn index_gives_each_channel_and_controller_its_own_row() {
        let mut seen = std::collections::BTreeSet::new();
        for channel in MidiChannel::all() {
            let sources = (0..=127)
                .map(cc)
                .chain([MidiSource::ChannelPressure, MidiSource::PitchBend]);
            for source in sources {
                let i = index(channel, source).expect("a row");
                assert!(i < MidiChannel::COUNT as usize * SOURCES);
                assert!(seen.insert(i), "{channel:?} {source:?} shares a row");
            }
        }
        assert_eq!(seen.len(), 16 * 130);
        assert_eq!(index(MidiChannel::FIRST, MidiSource::ProgramChange), None);
        assert_eq!(index(MidiChannel::FIRST, MidiSource::PolyPressure), None);
    }

    /// **The app's entry replaces the declared one for the same channel and
    /// message and leaves the others**: an app entry for any channel covers all
    /// 16, a declared entry for another message stays.
    #[test]
    fn an_app_entry_wins_over_a_declared_one_and_a_named_channel_over_any() {
        let declared = [
            assign(Some(0), cc(74), 1),
            assign(Some(3), cc(74), 1),
            assign(Some(0), cc(1), 1),
        ];
        let app = [assign(Some(3), cc(74), 2)];
        let t = table(&declared, &app);
        assert_eq!(t.lookup(MidiChannel::of(0), cc(74)), Some(ParamId::new(1)));
        assert_eq!(t.lookup(MidiChannel::of(3), cc(74)), Some(ParamId::new(2)));
        assert_eq!(t.lookup(MidiChannel::of(0), cc(1)), Some(ParamId::new(1)));
        assert_eq!(t.lookup(MidiChannel::of(1), cc(74)), None);

        // An any-channel app entry covers the channels no entry names; a declared
        // entry naming a channel wins there (channel precedence).
        let omni = table(&declared, &[assign(None, cc(74), 2)]);
        assert_eq!(
            omni.lookup(MidiChannel::of(0), cc(74)),
            Some(ParamId::new(1))
        );
        assert_eq!(
            omni.lookup(MidiChannel::of(3), cc(74)),
            Some(ParamId::new(1))
        );
        assert_eq!(
            omni.lookup(MidiChannel::of(5), cc(74)),
            Some(ParamId::new(2))
        );
        assert_eq!(
            omni.lookup(MidiChannel::of(0), cc(1)),
            Some(ParamId::new(1))
        );
    }

    /// **A program change or polyphonic pressure entry is not translatable**:
    /// counted, absent from the table, and its message passes through.
    #[test]
    fn a_program_change_or_poly_pressure_entry_is_counted_and_not_mapped() {
        let entries = [
            assign(None, MidiSource::ProgramChange, 1),
            assign(Some(2), MidiSource::PolyPressure, 1),
            assign(Some(2), cc(7), 2),
        ];
        let (t, untranslatable) = MidiTable::effective(&entries, &[]);
        assert_eq!(untranslatable, 2);
        assert_eq!(t.lookup(MidiChannel::of(2), cc(7)), Some(ParamId::new(2)));
        let (mut d, _) = delivery();
        let program = tutti_fixtures::midi::program_change(MidiChannel::of(2), 5, None);
        assert!(!t.take(&mut d, 0, program));
    }

    /// **A MIDI 1.0 controller reads on the MIDI 2.0 scale of M2-115**: 0 is 0,
    /// full scale is 1.0 and 64 is the centre word over the full word, at the
    /// message's frame.
    #[test]
    fn a_midi1_controller_is_a_point_on_the_widened_scale_at_its_frame() {
        let t = table(&[assign(Some(0), cc(74), 1)], &[]);
        for (value, expected) in [
            (0, 0.0),
            (127, 1.0),
            (64, f64::from(0x8000_0000_u32) / f64::from(u32::MAX)),
        ] {
            let (mut d, _) = delivery();
            let ump = tutti_fixtures::midi::m1_cc(
                MidiChannel::of(0),
                CCNumber::BRIGHTNESS,
                U7::of(value),
            );
            assert!(t.take(&mut d, 17, ump));
            let got = points(&d, 1);
            assert_eq!(got.len(), 1, "{value}: {got:?}");
            assert_eq!(got[0].0, at_frame(17).get());
            assert!((got[0].1 - expected).abs() < 1e-12, "{value}: {got:?}");
        }
    }

    /// **A MIDI 2.0 controller keeps its 32-bit word**: a word one step above the
    /// centre is told apart from the centre.
    #[test]
    fn a_midi2_controller_keeps_its_full_width() {
        let t = table(&[assign(Some(0), cc(74), 1)], &[]);
        let word = 0x8000_0001_u32;
        let (mut d, _) = delivery();
        assert!(t.take(&mut d, 0, ump_cc(0, CCNumber::BRIGHTNESS, word)));
        let got = points(&d, 1);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, f64::from(word) / f64::from(u32::MAX));
        assert!(got[0].1 > f64::from(0x8000_0000_u32) / f64::from(u32::MAX));
    }

    /// **Channel pressure and pitch bend map too, the bend's centre at 0.5.**
    #[test]
    fn channel_pressure_and_pitch_bend_are_points_with_the_bend_centred_on_half() {
        let t = table(
            &[
                assign(Some(0), MidiSource::ChannelPressure, 1),
                assign(Some(4), MidiSource::PitchBend, 2),
            ],
            &[],
        );
        let (mut d, _) = delivery();
        let full = tutti_fixtures::midi::channel_pressure(
            MidiChannel::of(0),
            ControlValue::from(U7::of(127)).get(),
        );
        assert!(t.take(&mut d, 3, full));
        assert_eq!(points(&d, 1), [(at_frame(3).get(), 1.0)]);
        let centre = tutti_fixtures::midi::pitch_bend(
            MidiChannel::of(4),
            BendValue::from(U14::of(8192)).get(),
        );
        assert!(t.take(&mut d, 4, centre));
        let bend = points(&d, 2);
        assert_eq!(bend.len(), 1);
        assert!((bend[0].1 - 0.5).abs() < 1e-3, "{bend:?}");
    }

    /// **An unmapped message changes nothing and is left to the plugin**: a note,
    /// a controller no entry names, a mapped controller on another channel, and a
    /// mapped controller in another group.
    #[test]
    fn an_unmapped_message_passes_through_and_writes_no_point() {
        let t = table(&[assign(Some(0), cc(74), 1)], &[]);
        let (mut d, _) = delivery();
        let word = ControlValue::from(U7::of(100)).get();
        let note = tutti_fixtures::midi::note_on(MidiChannel::of(0), 60, 100);
        assert!(!t.take(&mut d, 0, note));
        assert!(!t.take(&mut d, 0, ump_cc(0, CCNumber::MOD_WHEEL, word)));
        assert!(!t.take(&mut d, 0, ump_cc(1, CCNumber::BRIGHTNESS, word)));
        assert!(d.chunk().queues.is_empty(), "no point was written");
    }

    /// **A mapped parameter the plugin does not list is counted, and the
    /// message is still taken.**
    #[test]
    fn a_point_the_delivery_refuses_is_counted() {
        let t = table(&[assign(Some(0), cc(74), 99)], &[]);
        let (mut d, counters) = delivery();
        let word = ControlValue::from(U7::of(100)).get();
        assert!(t.take(&mut d, 0, ump_cc(0, CCNumber::BRIGHTNESS, word)));
        assert_eq!(counters.snapshot().points_dropped, 1);
    }

    /// **The slot translates what the host applies**: the declared table from the
    /// first block, the app's entries over it, and neither where the plugin applies it.
    #[test]
    fn the_slot_translates_what_the_host_applies() {
        let wiring = |applied| PluginWiring {
            assignments: vec![assign(Some(0), cc(74), 1)],
            assignments_applied: applied,
            ..PluginWiring::default()
        };
        let slot = MidiMapSlot::new(&wiring(MidiMapApplied::Host));
        let mut translator = slot.translator();
        assert_eq!(
            translator.lend().lookup(MidiChannel::of(0), cc(74)),
            Some(ParamId::new(1)),
            "the declared table from the first block"
        );
        slot.hand_app(&[assign(Some(0), cc(74), 2)], |_| MidiMapApplied::Host);
        assert_eq!(
            translator.lend().lookup(MidiChannel::of(0), cc(74)),
            Some(ParamId::new(2))
        );
        slot.hand_app(&[], |_| MidiMapApplied::Host);
        assert_eq!(
            translator.lend().lookup(MidiChannel::of(0), cc(74)),
            Some(ParamId::new(1))
        );
        slot.hand_app(&[assign(Some(0), cc(1), 2)], |_| MidiMapApplied::Plugin);
        assert_eq!(
            translator.lend().lookup(MidiChannel::of(0), cc(1)),
            None,
            "an entry the plugin applies is not translated"
        );
        slot.declare(&wiring(MidiMapApplied::Plugin));
        assert!(translator.lend().empty, "an AU applies its own table");
        slot.reclaim();
    }
}
