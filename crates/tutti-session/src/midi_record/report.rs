//! The messages of a MIDI Message Report (M2-101 section 9): the state a
//! [`Record`] holds, filtered by what was asked and put in the order the report
//! owes, which is not a catch-up's.
//!
//! **What is reported.** The kinds of the three bitmaps the record keeps: Pitch Bend,
//! Control Change, RPN, NRPN, Program Change, Channel Pressure and Notes. It keeps no
//! System message, no Poly Pressure and no per-note controller, so a Reply never sets
//! those bits (section 9.6: no bit for what is not sent); in particular the complete set
//! of eight MTC quarter frames (section 9.7.2) is not held, so its bit stays clear.
//!
//! **Order** (section 9.7.1). Groups ascending, in a Function Block's order; in a Group the
//! Channels ascending, one Channel complete before the next; in a Channel the bitmap's
//! order: Pitch Bend, Control Change, RPN, NRPN, Program Change, Channel Pressure, Notes.
//! The channel state's kind is [`ChannelState`], the one classification of what sets what;
//! the bit it reports under is one match ([`bit`]) and the order within a kind a second
//! sort key.
//!
//! **Rules of the kinds** (section 9.7.2).
//! - Control Change: a number 0 to 31 is followed by its LSB, 32 to 63; the rest in
//!   number order, then the mode in force (Local Control, then Omni, then Mono or Poly,
//!   each the last of its pair sent, Mono On with the channel count it was sent with).
//!   MIDI 1.0 Bank Select, RPN, NRPN and Data Entry numbers are held as what they are
//!   used as and are not Control Changes here.
//! - RPN and NRPN: the parameters as sent, and on a MIDI 1.0 lane the Null Function
//!   Value after them, when any was reported.
//! - Program Change: on a MIDI 1.0 lane the bank in force (MSB, LSB) first, then the
//!   program; a MIDI 2.0 Program Change carries its bank.
//! - Notes: each key down as the Note On it was sent with (velocity and attribute), then,
//!   once everything else is reported, a Note Off for each of them. The record keeps a
//!   Note On in MIDI 2.0 form and the report hands it on so; the device's output converts
//!   to the Protocol in use, so a MIDI 1.0 device is sent a MIDI 1.0 Note On with its
//!   velocity (section 9.7.2) and a MIDI 2.0 device the attribute too. A report reports
//!   the notes that are active (0x7F as well as 0x01): the record of what was sent holds
//!   no inactive ones.
//!
//! A report leaves the device's bank select and RPN/NRPN select at what the report sent
//! last, which is not necessarily what the stream had selected.
//!
//! **How much** ([`DataControl`]). `Capabilities` reports nothing and declares what could
//! be; `Full` reports everything held; `NonDefault` leaves out the Control Changes at their
//! default value (see [`default_of`]) and keeps the rest, the keys down included.

use tutti_midi_ci::{ChannelControllers, DataControl, NoteData, ReportScope, SuggestedCc};
use tutti_ump::{
    Address, CCNumber, ChannelMode, ChannelState, ControlValue, GroupChannelIndex, Message,
    MidiChannel, MidiGroup, NoteVelocity, ParameterKind, Protocol, U7, Ump, Voice1, Voice2,
};

use super::{Entry, Half, Record, Slot, What};

/// What a report holds: the bitmaps it declares and the messages between its Reply and
/// its End.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Report {
    /// The kinds the Reply declares (section 9.6): those asked that the record can report.
    pub(crate) declared: ReportScope,
    /// The messages, in order.
    pub(crate) messages: Vec<Ump>,
}

/// The Channel Controller kinds the record can report.
const CONTROLLERS: ChannelControllers = ChannelControllers::all();

/// The Note Data kinds the record can report.
const NOTES: NoteData = NoteData::NOTES;

/// The bitmap bit a value of channel state is reported under.
fn bit(state: ChannelState) -> ChannelControllers {
    match state {
        ChannelState::Bend => ChannelControllers::PITCH_BEND,
        ChannelState::Controller(_) => ChannelControllers::CONTROL_CHANGE,
        ChannelState::Parameter(ParameterKind::Registered, ..) => {
            ChannelControllers::REGISTERED_CONTROLLER
        }
        ChannelState::Parameter(ParameterKind::Assignable, ..) => {
            ChannelControllers::ASSIGNABLE_CONTROLLER
        }
        ChannelState::Program => ChannelControllers::PROGRAM_CHANGE,
        ChannelState::Pressure => ChannelControllers::CHANNEL_PRESSURE,
    }
}

/// The value of a Control Change a `NonDefault` report leaves out (section 9.5.1): the
/// value Reset All Controllers sets it to under M2-113 where that is one, 100 for Volume
/// and 64 for Pan (the examples section 9.5.1 gives), and 0 for the rest.
fn default_of(controller: CCNumber) -> U7 {
    match controller {
        CCNumber::VOLUME => U7::of(100),
        CCNumber::PAN => U7::of(64),
        _ => match SuggestedCc::of(controller).reset() {
            tutti_midi_ci::CcReset::To(value) => value,
            _ => U7::of(0),
        },
    }
}

/// The order of Control Changes 0 to 63: a number and its LSB side by side.
fn controller_order(controller: CCNumber) -> (u8, u8) {
    let n = controller.get();
    if n < 64 { (n % 32, n / 32) } else { (n, 0) }
}

/// Whether the Control Change `ump` holds is at `default`.
fn at_default(ump: Ump, default: U7) -> bool {
    match ump.decode() {
        Ok(Message::Midi1(_, Voice1::ControlChange { value, .. })) => value == default,
        Ok(Message::Midi2(_, Voice2::ControlChange { value, .. })) => {
            value == ControlValue::from(default)
        }
        _ => false,
    }
}

impl Record {
    /// The report of `scope` over `groups` (ascending) at `control`: the Channel of the
    /// scope, or each Channel, of each Group.
    pub(crate) fn report(
        &self,
        control: DataControl,
        scope: &ReportScope,
        groups: impl IntoIterator<Item = MidiGroup>,
    ) -> Report {
        let supported =
            ReportScope::from_wire(scope.address(), 0, CONTROLLERS.bits(), NOTES.bits())
                .unwrap_or_else(|| ReportScope::none(scope.address()));
        let declared = supported.within(scope);
        let mut messages = Vec::new();
        if control == DataControl::Capabilities {
            return Report { declared, messages };
        }
        let channels: Vec<MidiChannel> = match scope {
            ReportScope::Channel { channel, .. } => vec![*channel],
            _ => MidiChannel::all().collect(),
        };
        let mut note_offs = Vec::new();
        for group in groups {
            for &channel in &channels {
                let at = Address { group, channel };
                for protocol in [Protocol::Midi1, Protocol::Midi2] {
                    if let Some(lane) = GroupChannelIndex::at(protocol, at) {
                        self.report_lane(lane, control, declared, &mut messages);
                    }
                }
                if declared.notes().contains(NoteData::NOTES) {
                    self.report_notes(at, &mut messages, &mut note_offs);
                }
            }
        }
        messages.extend(note_offs);
        Report { declared, messages }
    }

    /// One lane's Channel Controller messages, in the bitmap's order.
    fn report_lane(
        &self,
        lane: GroupChannelIndex,
        control: DataControl,
        declared: ReportScope,
        out: &mut Vec<Ump>,
    ) {
        let asked = declared.controllers();
        let first = Slot {
            lane,
            what: What::Bank(Half::Msb),
        };
        let mut held: Vec<(ChannelControllers, (u8, u8), &Entry, What)> = self
            .state
            .range(first..)
            .take_while(|(slot, _)| slot.lane == lane)
            .filter_map(|(slot, entry)| {
                let (kind, order) = match slot.what {
                    What::State(state) => (bit(state), state_order(state)),
                    // The modes follow the controllers, Local Control (122) first.
                    What::Mode(mode) => (ChannelControllers::CONTROL_CHANGE, mode_order(mode)),
                    What::Bank(_) | What::PendingBank(_) | What::Selection => return None,
                };
                asked
                    .intersects(kind)
                    .then_some((kind, order, entry, slot.what))
            })
            .collect();
        held.sort_by_key(|(kind, order, ..)| (kind.bits().trailing_zeros(), *order));
        let midi1 = lane.protocol() == Protocol::Midi1;
        let mut open: Option<ChannelControllers> = None;
        for (kind, _, entry, what) in held {
            if let (
                DataControl::NonDefault,
                What::State(ChannelState::Controller(c)),
                Entry::One(ump),
            ) = (control, what, entry)
                && at_default(*ump, default_of(c))
            {
                continue;
            }
            if open.is_some_and(|parameters| parameters != kind) {
                self.push_null(lane, open.take(), midi1, out);
            }
            if kind == ChannelControllers::PROGRAM_CHANGE && midi1 {
                self.push_bank(lane, out);
            }
            if PARAMETERS.contains(kind) {
                open = Some(kind);
            }
            entry.push_to(out);
        }
        self.push_null(lane, open, midi1, out);
    }

    /// The bank in force on a MIDI 1.0 lane: its MSB and LSB.
    fn push_bank(&self, lane: GroupChannelIndex, out: &mut Vec<Ump>) {
        for half in [Half::Msb, Half::Lsb] {
            let slot = Slot {
                lane,
                what: What::Bank(half),
            };
            if let Some(entry) = self.state.get(&slot) {
                entry.push_to(out);
            }
        }
    }

    /// The Null Function Value after the parameters of `kind` (RPN or NRPN), on a
    /// MIDI 1.0 lane; a MIDI 2.0 parameter is a message of its own.
    fn push_null(
        &self,
        lane: GroupChannelIndex,
        kind: Option<ChannelControllers>,
        midi1: bool,
        out: &mut Vec<Ump>,
    ) {
        let controllers = match kind {
            Some(ChannelControllers::REGISTERED_CONTROLLER) => {
                [CCNumber::RPN_MSB, CCNumber::RPN_LSB]
            }
            Some(_) => [CCNumber::NRPN_MSB, CCNumber::NRPN_LSB],
            None => return,
        };
        if midi1 {
            out.extend(controllers.map(|controller| {
                let voice = Voice1::control_change(controller, U7::MAX);
                Ump::from(Message::midi1(lane.address(), voice))
            }));
        }
    }

    /// The Note On of each key down on `at`'s channel, and a Note Off for each in
    /// `offs`.
    fn report_notes(&self, at: Address, out: &mut Vec<Ump>, offs: &mut Vec<Ump>) {
        for key in self.held.keys().iter().filter(|key| key.address() == at) {
            if let Some(on) = self.note_on_of(key) {
                out.push(on);
                let off = Voice2::note_off(key.note(), NoteVelocity::new(0));
                offs.push(Ump::from(Message::midi2(at, off)));
            }
        }
    }
}

/// The parameter kinds, whose messages are followed by a Null Function Value.
const PARAMETERS: ChannelControllers =
    ChannelControllers::REGISTERED_CONTROLLER.union(ChannelControllers::ASSIGNABLE_CONTROLLER);

/// The order within a kind: Control Changes by [`controller_order`], the rest as held.
fn state_order(state: ChannelState) -> (u8, u8) {
    match state {
        ChannelState::Controller(controller) => controller_order(controller),
        _ => (0, 0),
    }
}

/// The order of the modes among the Control Changes: after number 127.
fn mode_order(mode: ChannelMode) -> (u8, u8) {
    match mode {
        ChannelMode::LocalControl => (128, 0),
        ChannelMode::Omni => (129, 0),
        ChannelMode::MonoPoly => (130, 0),
    }
}

#[cfg(test)]
mod tests {
    use tutti_ump::{Bank, NoteNumber, U14};

    use super::*;

    fn addr(group: u8, channel: u8) -> Address {
        Address::new(MidiGroup::of(group), MidiChannel::of(channel))
    }

    fn say1(record: &mut Record, at: Address, voice: Voice1) {
        record.observe(Ump::from(Message::midi1(at, voice)));
    }

    fn say2(record: &mut Record, at: Address, voice: Voice2) {
        record.observe(Ump::from(Message::midi2(at, voice)));
    }

    fn cc1(record: &mut Record, at: Address, controller: u8, value: u8) {
        let controller = CCNumber::new(controller).unwrap();
        say1(
            record,
            at,
            Voice1::control_change(controller, U7::of(value)),
        );
    }

    /// A message as `g<group>c<channel>:<what>`, to read an order at a glance.
    fn tag(ump: Ump) -> String {
        let place = |at: Address| format!("g{}c{}", at.group.get(), at.channel.get());
        match ump.decode().expect("a message") {
            Message::Midi1(at, voice) => format!("{}:{}", place(at), tag1(voice)),
            Message::Midi2(at, voice) => format!("{}:{}", place(at), tag2(voice)),
            other => format!("{other:?}"),
        }
    }

    fn tag1(voice: Voice1) -> String {
        match voice {
            Voice1::ControlChange { controller, value } => {
                format!("cc{}={}", controller.get(), value.get())
            }
            Voice1::ProgramChange(program) => format!("pc{}", program.get()),
            Voice1::PitchBend(value) => format!("bend{}", value.get()),
            Voice1::ChannelPressure(value) => format!("press{}", value.get()),
            Voice1::NoteOn { note, velocity } => format!("on{}v{}", note.get(), velocity.get()),
            Voice1::NoteOff { note, .. } => format!("off{}", note.get()),
            other => format!("{other:?}"),
        }
    }

    fn tag2(voice: Voice2) -> String {
        match voice {
            Voice2::ControlChange { controller, value } => {
                format!("m2cc{}={:#x}", controller.get(), value.get())
            }
            Voice2::ProgramChange { program, bank } => format!(
                "m2pc{}{}",
                program.get(),
                bank.map_or(String::new(), |b| format!(
                    "b{}.{}",
                    b.msb.get(),
                    b.lsb.get()
                ))
            ),
            Voice2::NoteOn {
                note,
                velocity,
                attribute,
            } => format!("m2on{}v{:#x}{attribute:?}", note.get(), velocity.get()),
            Voice2::NoteOff { note, .. } => format!("m2off{}", note.get()),
            other => format!("{other:?}"),
        }
    }

    fn tags(report: &Report) -> Vec<String> {
        report.messages.iter().copied().map(tag).collect()
    }

    fn everything() -> ReportScope {
        ReportScope::Group {
            controllers: ChannelControllers::all(),
            notes: NoteData::all(),
        }
    }

    fn group_zero() -> [MidiGroup; 1] {
        [MidiGroup::of(0)]
    }

    fn full(record: &Record, scope: &ReportScope) -> Report {
        record.report(DataControl::Full, scope, group_zero())
    }

    /// Spec: m2-101/9.6#2, m2-101/9.6#4
    #[test]
    fn the_reply_declares_the_kinds_asked_that_the_record_can_report_and_no_other() {
        // §9.6: a bit is set for what will be sent and for no kind that was not requested;
        // the record keeps no System message and no Poly Pressure or per-note controller.
        let record = Record::default();
        let reply = full(&record, &everything()).declared;
        assert_eq!(reply.controllers(), ChannelControllers::all());
        assert_eq!(reply.notes(), NoteData::NOTES);
        let some = ReportScope::Group {
            controllers: ChannelControllers::PITCH_BEND,
            notes: NoteData::empty(),
        };
        assert_eq!(full(&record, &some).declared, some);
        let function_block = ReportScope::FunctionBlock {
            system: tutti_midi_ci::SystemMessages::all(),
            controllers: ChannelControllers::empty(),
            notes: NoteData::empty(),
        };
        assert!(full(&record, &function_block).declared.is_empty());
    }

    /// Spec: m2-101/9.7#1, m2-101/9.7.1#1, m2-101/9.7.1#2
    #[test]
    fn a_channel_is_reported_in_the_order_of_the_bitmap_with_the_current_values() {
        let mut record = Record::default();
        let at = addr(0, 0);
        say1(&mut record, at, Voice1::ChannelPressure(U7::of(9)));
        say1(&mut record, at, Voice1::ProgramChange(U7::of(5)));
        cc1(&mut record, at, 101, 0);
        cc1(&mut record, at, 100, 2);
        cc1(&mut record, at, 6, 12);
        cc1(&mut record, at, 99, 1);
        cc1(&mut record, at, 98, 3);
        cc1(&mut record, at, 6, 20);
        cc1(&mut record, at, 7, 30);
        cc1(&mut record, at, 7, 90);
        say1(&mut record, at, Voice1::PitchBend(U14::of(100)));
        say1(
            &mut record,
            at,
            Voice1::note_on(NoteNumber::of(60), U7::of(80)),
        );
        let report = full(&record, &everything());
        let names = tags(&report);
        let at_of = |what: &str| names.iter().position(|n| n.ends_with(what)).unwrap();
        // Pitch Bend, Control Change (the latest value only), RPN, NRPN, Program, Pressure, Notes.
        let order = ["bend100", "cc7=90", "cc101=0", "cc99=1", "pc5", "press9"];
        let places: Vec<usize> = order.iter().map(|n| at_of(n)).collect();
        assert!(places.windows(2).all(|w| w[0] < w[1]), "{names:?}");
        assert!(!names.iter().any(|n| n.ends_with("cc7=30")), "{names:?}");
        assert!(names[names.len() - 2].contains("m2on60"), "{names:?}");
    }

    /// Spec: m2-101/9.7.1#3, m2-101/9.7.1#4, m2-101/9.7.1#5, m2-101/9.7.1#6, m2-101/9.5#5
    #[test]
    fn channels_ascend_complete_one_after_another_and_groups_ascend() {
        let mut record = Record::default();
        for (group, channel) in [(1, 0), (0, 3), (0, 1), (1, 2)] {
            cc1(&mut record, addr(group, channel), 7, 50);
            say1(
                &mut record,
                addr(group, channel),
                Voice1::ProgramChange(U7::of(1)),
            );
        }
        let function_block = ReportScope::FunctionBlock {
            system: tutti_midi_ci::SystemMessages::empty(),
            controllers: ChannelControllers::all(),
            notes: NoteData::empty(),
        };
        let both = [MidiGroup::of(0), MidiGroup::of(1)];
        let report = record.report(DataControl::Full, &function_block, both);
        let places: Vec<String> = tags(&report)
            .iter()
            .map(|n| n.split(':').next().unwrap().to_owned())
            .collect();
        // Each Channel's controller and program together, Channels then Groups ascending.
        assert_eq!(
            places,
            [
                "g0c1", "g0c1", "g0c3", "g0c3", "g1c0", "g1c0", "g1c2", "g1c2"
            ]
        );
        // A Group's report names no other Group's Channels, and a Channel's no other's.
        let one_channel = ReportScope::Channel {
            channel: MidiChannel::of(3),
            controllers: ChannelControllers::all(),
            notes: NoteData::empty(),
        };
        let report = full(&record, &one_channel);
        assert_eq!(tags(&report), ["g0c3:cc7=50", "g0c3:pc1"]);
    }

    /// Spec: m2-101/9.7.2#2, m2-101/9.7.2#4
    #[test]
    fn control_changes_0_to_63_pair_each_msb_with_its_lsb_and_the_rest_ascend() {
        let mut record = Record::default();
        let at = addr(0, 0);
        for controller in [33, 1, 70, 2, 122, 65, 32 + 2] {
            cc1(&mut record, at, controller, 10);
        }
        let report = full(&record, &everything());
        let numbers: Vec<String> = tags(&report)
            .iter()
            .map(|n| {
                n.split(':')
                    .nth(1)
                    .unwrap()
                    .split('=')
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            numbers,
            ["cc1", "cc33", "cc2", "cc34", "cc65", "cc70", "cc122"]
        );
    }

    /// Spec: m2-101/9.7.2#5, m2-101/9.7.2#6
    #[test]
    fn only_the_modes_in_force_are_sent_and_mono_on_states_its_channel_count() {
        let mut record = Record::default();
        let at = addr(0, 0);
        cc1(&mut record, at, 124, 0);
        cc1(&mut record, at, 125, 0);
        cc1(&mut record, at, 127, 0);
        cc1(&mut record, at, 126, 4);
        let report = full(&record, &everything());
        assert_eq!(tags(&report), ["g0c0:cc125=0", "g0c0:cc126=4"]);
    }

    /// Spec: m2-101/9.7.2#8
    #[test]
    fn bank_select_and_parameter_numbers_are_not_reported_as_plain_control_changes() {
        // In MIDI 1.0 Protocol, CC 0, 6, 32, 38 and 98 to 101 in use as Bank Select, RPN
        // and NRPN are not Control Changes of the report.
        let mut record = Record::default();
        let at = addr(0, 0);
        for controller in [0, 32, 6, 38, 98, 99, 100, 101] {
            cc1(&mut record, at, controller, 1);
        }
        let controllers = ReportScope::Group {
            controllers: ChannelControllers::CONTROL_CHANGE,
            notes: NoteData::empty(),
        };
        assert_eq!(tags(&full(&record, &controllers)), Vec::<String>::new());
    }

    /// Spec: m2-101/9.7.2#9, m2-101/9.7.2#10
    #[test]
    fn the_null_function_value_follows_the_rpns_and_the_nrpns_in_midi_1() {
        let mut record = Record::default();
        let at = addr(0, 0);
        for controller in [101, 100, 6, 99, 98, 6] {
            cc1(
                &mut record,
                at,
                controller,
                if controller == 6 { 9 } else { 1 },
            );
        }
        let report = full(&record, &everything());
        assert_eq!(
            tags(&report),
            [
                "g0c0:cc101=1",
                "g0c0:cc100=1",
                "g0c0:cc6=9",
                "g0c0:cc101=127",
                "g0c0:cc100=127",
                "g0c0:cc99=1",
                "g0c0:cc98=1",
                "g0c0:cc6=9",
                "g0c0:cc99=127",
                "g0c0:cc98=127",
            ]
        );
        // No parameter reported, no Null.
        assert!(full(&Record::default(), &everything()).messages.is_empty());
    }

    /// Spec: m2-101/9.7.2#11, m2-101/9.7.2#12
    #[test]
    fn a_midi_1_program_follows_its_bank_in_force_or_stands_alone() {
        let mut record = Record::default();
        let at = addr(0, 0);
        say1(&mut record, at, Voice1::ProgramChange(U7::of(3)));
        let programs = ReportScope::Group {
            controllers: ChannelControllers::PROGRAM_CHANGE,
            notes: NoteData::empty(),
        };
        assert_eq!(tags(&full(&record, &programs)), ["g0c0:pc3"]);
        // A bank sent, then the program that applies it; one sent since is pending.
        cc1(&mut record, at, 0, 2);
        cc1(&mut record, at, 32, 7);
        say1(&mut record, at, Voice1::ProgramChange(U7::of(4)));
        cc1(&mut record, at, 0, 9);
        assert_eq!(
            tags(&full(&record, &programs)),
            ["g0c0:cc0=2", "g0c0:cc32=7", "g0c0:pc4"]
        );
    }

    /// Spec: m2-101/9.7.2#13
    #[test]
    fn a_midi_2_program_change_carries_its_bank() {
        let mut record = Record::default();
        let bank = Bank {
            msb: U7::of(1),
            lsb: U7::of(2),
        };
        say2(
            &mut record,
            addr(0, 0),
            Voice2::ProgramChange {
                program: U7::of(8),
                bank: Some(bank),
            },
        );
        assert_eq!(tags(&full(&record, &everything())), ["g0c0:m2pc8b1.2"]);
    }

    /// Spec: m2-101/9.7.2#15, m2-101/9.7.2#16
    #[test]
    fn each_key_down_is_a_note_on_as_sent_and_each_has_a_note_off_after_everything_else() {
        let mut record = Record::default();
        let (a, b) = (addr(0, 0), addr(0, 1));
        say1(
            &mut record,
            a,
            Voice1::note_on(NoteNumber::of(60), U7::of(80)),
        );
        say2(
            &mut record,
            b,
            Voice2::note_on(NoteNumber::of(62), NoteVelocity::new(0x1234)),
        );
        cc1(&mut record, b, 7, 50);
        let report = full(&record, &everything());
        let names = tags(&report);
        // The velocity a MIDI 1.0 Note On carried, as the Record keeps it (M2-104 D.3).
        let velocity = U7::of(80);
        let Voice2::NoteOn {
            velocity: kept,
            attribute,
            ..
        } = Voice2::from(Voice1::note_on(NoteNumber::of(60), velocity))
        else {
            panic!("a Note On");
        };
        assert_eq!(
            names[0],
            format!("g0c0:m2on60v{:#x}{attribute:?}", kept.get())
        );
        assert_eq!(names[1], "g0c1:cc7=50");
        assert!(names[2].starts_with("g0c1:m2on62v0x1234"), "{names:?}");
        // Offs for both, last, in the order of the Ons.
        assert_eq!(names[3..], ["g0c0:m2off60", "g0c1:m2off62"]);
        // Without the Notes kind there are no Ons and no Offs.
        let controllers = ReportScope::Group {
            controllers: ChannelControllers::all(),
            notes: NoteData::empty(),
        };
        assert_eq!(tags(&full(&record, &controllers)), ["g0c1:cc7=50"]);
    }

    #[test]
    fn a_full_report_of_notes_covers_the_keys_down_and_a_released_key_is_not_one() {
        let mut record = Record::default();
        let at = addr(0, 0);
        say1(
            &mut record,
            at,
            Voice1::note_on(NoteNumber::of(60), U7::of(80)),
        );
        say1(
            &mut record,
            at,
            Voice1::note_on(NoteNumber::of(61), U7::of(80)),
        );
        say1(
            &mut record,
            at,
            Voice1::note_off(NoteNumber::of(60), U7::of(0)),
        );
        let names = tags(&full(&record, &everything()));
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names[0].contains("m2on61") && names[1] == "g0c0:m2off61");
    }

    /// MIDI 2.0 Registered and Assignable Controllers are messages of their own: no Null
    /// Function Value follows them.
    #[test]
    fn a_midi_2_parameter_is_sent_whole_with_no_null_function_value() {
        let mut record = Record::default();
        say2(
            &mut record,
            addr(0, 0),
            Voice2::Registered {
                bank: U7::of(0),
                index: U7::of(2),
                value: ControlValue::new(0x1234_0000),
            },
        );
        let report = full(&record, &everything());
        assert_eq!(report.messages.len(), 1, "{:?}", tags(&report));
        assert!(matches!(
            report.messages[0].decode(),
            Ok(Message::Midi2(_, Voice2::Registered { .. }))
        ));
    }

    #[test]
    fn a_capabilities_report_declares_and_sends_no_data() {
        let mut record = Record::default();
        cc1(&mut record, addr(0, 0), 7, 90);
        let report = record.report(DataControl::Capabilities, &everything(), group_zero());
        assert!(report.messages.is_empty());
        assert_eq!(report.declared.controllers(), ChannelControllers::all());
    }

    #[test]
    fn a_non_default_report_leaves_out_controllers_at_their_default_and_keeps_the_rest() {
        // §9.5.1: 100 for Volume, 64 for Pan and Balance, 0 for most others.
        let mut record = Record::default();
        let at = addr(0, 0);
        for (controller, value) in [
            (7, 100),
            (10, 64),
            (8, 64),
            (1, 0),
            (11, 127),
            (74, 0),
            (91, 5),
        ] {
            cc1(&mut record, at, controller, value);
        }
        say1(
            &mut record,
            at,
            Voice1::note_on(NoteNumber::of(60), U7::of(80)),
        );
        let report = record.report(DataControl::NonDefault, &everything(), group_zero());
        let names = tags(&report);
        assert_eq!(names[0], "g0c0:cc91=5", "{names:?}");
        assert!(names[1].contains("m2on60") && names[2].contains("m2off60"));
        assert_eq!(names.len(), 3);
        // Full leaves nothing out.
        assert_eq!(
            tags(&full(&record, &everything())).len(),
            7 + 2,
            "seven controllers, an On and an Off"
        );
        // A value off its default is reported.
        cc1(&mut record, at, 7, 99);
        let report = record.report(DataControl::NonDefault, &everything(), group_zero());
        assert!(tags(&report).contains(&"g0c0:cc7=99".to_owned()));
    }
}
