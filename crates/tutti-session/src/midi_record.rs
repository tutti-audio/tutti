//! What an output has said: the state its stream set, kept so a listener that
//! joins later is caught up alone.
//!
//! A [`Record`] folds in every event routed to one output ([`observe`]) and
//! hands back, on demand, the messages that bring a receiver that heard none of
//! them to the same state ([`catch_up`]). A device that returns and a network
//! peer that joins are such receivers: they join with no graph edit, so
//! nothing upstream of the output knows to send the setup again, and a source
//! that is not a clip (a live input forwarded to the output) has no setup to
//! send.
//!
//! **Which thread writes it.** The session's control thread, from
//! [`Phase::MidiOut`](crate::Phase::MidiOut)'s drain of the output's tap
//! (`route_drain`), the one place every event of an output passes, whether its
//! device is open, away or a host's sink. The audio thread never reaches it, so
//! it allocates (note-on tables, map nodes, SysEx messages). What it has no
//! room for is counted in the output's [`Losses::lost`](tutti_units::Losses)
//! ([`take_lost`]), never dropped silently.
//!
//! **What is kept**, each in the form it was sent:
//!
//! - the complete System Exclusive messages (SysEx7 and SysEx8), in order,
//!   a message with the same packets kept once at its latest place, at most
//!   [`MAX_SYSEX_MESSAGES`] of at most [`MAX_SYSEX_PACKETS`] packets in all;
//! - the last Flex Data setup message (tempo, time signature, metronome, key
//!   signature, chord name) per Group and address;
//! - per Group and Channel of each protocol: the last Program Change with its
//!   bank (MIDI 2.0: the Bank Valid bits; MIDI 1.0: the last CC 0 and CC 32),
//!   the last value of each Control Change, of each Registered and Assignable
//!   Controller (MIDI 1.0: the CC 101/100 or 99/98 select as sent, with the CC 6
//!   and CC 38 data entries sent under it), the pitch bend and the channel
//!   pressure;
//! - the keys down, in a [`Held`] (the one fact of which keys are down), and
//!   each one's Note On in MIDI 2.0 form (velocity and attribute), a MIDI 1.0
//!   Note On being kept as `Voice2::from` makes it (M2-104 Appendix D.3). A key
//!   is caught up in the protocol it was started in.
//!
//! **Order of a catch-up, and why.** SysEx and Flex setup first: a device
//! reset or a mode message must come before the state it would erase. Then,
//! one Channel after another, the bank and program (a controller's meaning can
//! depend on the patch), the controllers, the registered and assignable
//! parameters (their selects then data, as sent), the pitch bend and the
//! pressure. The held notes come last, so each sounds with its channel's patch,
//! controllers and bend already in place.
//!
//! A Control Change 120 or 123 (All Sound Off, All Notes Off) ends the
//! channel's keys, and a 121 (Reset All Controllers) forgets exactly what
//! RP-015 resets: modulation (CC 1), expression (CC 11), the pedals (CC 64 to
//! 67), pitch bend, channel pressure and the parameter select. Every other
//! controller, the program and the parameter values stay: a catch-up never
//! sounds what the stream ended, and never drops what it left set.
//!
//! [`observe`]: Record::observe
//! [`catch_up`]: Record::catch_up
//! [`take_lost`]: Record::take_lost

use std::collections::{BTreeMap, VecDeque};

use tutti_ump::{
    Address, Attribute, CCNumber, Data128, Flex, FlexAddress, GroupChannelIndex, Held, Message,
    MidiGroup, NoteKey, NoteNumber, NoteVelocity, Protocol, SysexStatus, Ump, Voice1, Voice2,
};

/// How many complete SysEx messages a record keeps. A device setup is a few
/// dozen messages (a GM, GS or XG reset and its parameter blocks are 10 to 200
/// bytes each, about two to thirty packets), so 128 holds several setups of
/// distinct devices; the oldest is dropped, and counted, beyond it.
pub(crate) const MAX_SYSEX_MESSAGES: usize = 128;

/// How many SysEx packets the kept messages hold in all: 4 096 packets are 64
/// KiB of UMP, which holds a few bulk dumps (a patch dump is a few hundred bytes
/// to some KiB) beside the setup. A message longer than this alone is dropped
/// and counted.
pub(crate) const MAX_SYSEX_PACKETS: usize = 4096;

/// How many messages are being assembled at once: a SysEx7 stream is one a
/// Group, a SysEx8 stream one a Group and stream id, and a sender uses a few.
const MAX_PENDING: usize = 64;

/// How many controller, program, parameter, bend and pressure values a record
/// keeps: 8 192 is every Control Change of two Groups of sixteen Channels, and a
/// stream of one protocol on a few Groups is far below it.
pub(crate) const MAX_STATE_ENTRIES: usize = 8192;

/// A table of Note Ons covers one Group: 16 channels of 128 keys.
const NOTES_PER_GROUP: usize = 16 * 128;

/// The part of a channel's state a message belongs to, in the order a catch-up
/// sends them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Part {
    /// Bank select (MIDI 1.0) and the program.
    Program,
    Controller,
    /// A registered or assignable parameter.
    Parameter,
    Bend,
    Pressure,
}

/// Where a value of a channel's state is kept: the lane (protocol, Group,
/// Channel) first, so a catch-up goes channel by channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Slot {
    lane: u16,
    part: Part,
    sub: u16,
}

/// The messages of one slot, as sent: one, or a parameter's select and data.
#[derive(Clone, Copy, Debug)]
struct Said([Option<Ump>; 4]);

impl Said {
    const fn one(ump: Ump) -> Self {
        Self([Some(ump), None, None, None])
    }
}

/// Which parameter number a MIDI 1.0 CC 99/98 or 101/100 pair selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Selected {
    Rpn,
    Nrpn,
}

/// A MIDI 1.0 channel's parameter select: the kind and the two halves as sent.
#[derive(Clone, Copy, Debug)]
struct Select {
    kind: Selected,
    msb: Option<(u8, Ump)>,
    lsb: Option<(u8, Ump)>,
}

/// A Note On as MIDI 2.0 carries it: what a catch-up of the key needs beyond the
/// key itself.
type NoteOn = (NoteVelocity, Attribute);

/// What one output has said. See the [module](self).
pub(crate) struct Record {
    /// The keys down; the note-ons are in `notes`, and "down" is only here.
    held: Held,
    /// A Group's Note Ons by channel and key, allocated at the Group's first
    /// Note On and never grown.
    notes: [Option<Box<[Option<NoteOn>]>>; 16],
    state: BTreeMap<Slot, Said>,
    select: BTreeMap<u16, Select>,
    /// Flex setup by (Group, address, kind).
    flex: BTreeMap<(u8, u8, u8), Ump>,
    /// Complete SysEx messages, oldest first.
    sysex: VecDeque<Vec<Ump>>,
    /// SysEx messages being assembled, by (Group, stream; 256 for SysEx7).
    pending: Vec<((u8, u16), Vec<Ump>)>,
    lost: u64,
}

impl Default for Record {
    fn default() -> Self {
        Self {
            held: Held::new(),
            notes: std::array::from_fn(|_| None),
            state: BTreeMap::new(),
            select: BTreeMap::new(),
            flex: BTreeMap::new(),
            sysex: VecDeque::new(),
            pending: Vec::new(),
            lost: 0,
        }
    }
}

impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Record")
            .field("held", &self.held)
            .field("values", &self.state.len())
            .field("sysex", &self.sysex.len())
            .finish_non_exhaustive()
    }
}

impl Record {
    /// What was refused for room since the last call: a value, a SysEx message
    /// or a Note On table that did not fit.
    pub(crate) fn take_lost(&mut self) -> u64 {
        std::mem::take(&mut self.lost)
    }

    /// Folds in one event the output is sent.
    pub(crate) fn observe(&mut self, ump: Ump) {
        let Ok(message) = ump.decode() else {
            return;
        };
        match message {
            Message::Midi1(at, voice) => self.voice1(ump, at, voice),
            Message::Midi2(at, voice) => self.voice2(ump, at, voice),
            Message::Sysex7(group, packet) => {
                self.sysex_packet((group.index() as u8, 256), ump, packet.status());
            }
            Message::Data128(group, Data128::Sysex8(packet)) => {
                let key = (group.index() as u8, u16::from(packet.stream_id().get()));
                self.sysex_packet(key, ump, packet.status());
            }
            Message::Data128(group, Data128::Sysex8Abort { stream_id }) => {
                let key = (group.index() as u8, u16::from(stream_id.get()));
                self.pending.retain(|(k, _)| *k != key);
            }
            Message::Flex(group, flex) => self.flex_setup(group, ump, flex),
            _ => {}
        }
    }

    fn voice1(&mut self, ump: Ump, at: Address, voice: Voice1) {
        let Some(lane) = GroupChannelIndex::at(Protocol::Midi1, at) else {
            return;
        };
        let lane = lane.index() as u16;
        match voice {
            Voice1::NoteOn { note, velocity } if velocity.get() > 0 => {
                self.held.observe(ump);
                if let Voice2::NoteOn {
                    velocity,
                    attribute,
                    ..
                } = Voice2::from(voice)
                {
                    self.note_on(at, note, (velocity, attribute));
                }
            }
            Voice1::NoteOn { .. } | Voice1::NoteOff { .. } => self.held.observe(ump),
            Voice1::PolyPressure { .. } => {}
            Voice1::ControlChange { controller, value } => {
                self.cc1(lane, at, ump, controller, value.get());
            }
            Voice1::ProgramChange(_) => self.put(lane, Part::Program, 2, Said::one(ump)),
            Voice1::ChannelPressure(_) => self.put(lane, Part::Pressure, 0, Said::one(ump)),
            Voice1::PitchBend(_) => self.put(lane, Part::Bend, 0, Said::one(ump)),
        }
    }

    fn voice2(&mut self, ump: Ump, at: Address, voice: Voice2) {
        let Some(lane) = GroupChannelIndex::at(Protocol::Midi2, at) else {
            return;
        };
        let lane = lane.index() as u16;
        match voice {
            Voice2::NoteOn {
                note,
                velocity,
                attribute,
            } => {
                self.held.observe(ump);
                self.note_on(at, note, (velocity, attribute));
            }
            Voice2::NoteOff { .. } => self.held.observe(ump),
            Voice2::ControlChange { controller, .. } => {
                if is_mode(controller) {
                    self.mode(lane, at, Protocol::Midi2, controller);
                } else if !controller.is_ignored_in_midi2() {
                    self.put(
                        lane,
                        Part::Controller,
                        u16::from(controller.get()),
                        Said::one(ump),
                    );
                }
            }
            Voice2::ProgramChange { .. } => self.put(lane, Part::Program, 2, Said::one(ump)),
            Voice2::ChannelPressure(_) => self.put(lane, Part::Pressure, 0, Said::one(ump)),
            Voice2::PitchBend(_) => self.put(lane, Part::Bend, 0, Said::one(ump)),
            Voice2::Registered { bank, index, .. } => {
                let sub = u16::from(bank.get()) << 7 | u16::from(index.get());
                self.put(lane, Part::Parameter, sub, Said::one(ump));
            }
            Voice2::Assignable { bank, index, .. } => {
                let sub = 1 << 14 | u16::from(bank.get()) << 7 | u16::from(index.get());
                self.put(lane, Part::Parameter, sub, Said::one(ump));
            }
            Voice2::RegisteredPerNote { .. }
            | Voice2::AssignablePerNote { .. }
            | Voice2::RelativeRegistered { .. }
            | Voice2::RelativeAssignable { .. }
            | Voice2::PerNotePitchBend { .. }
            | Voice2::PolyPressure { .. }
            | Voice2::PerNoteManagement { .. } => {}
        }
    }

    /// A MIDI 1.0 Control Change: a bank select half, a parameter select or data
    /// entry, a mode message, or a controller value.
    fn cc1(&mut self, lane: u16, at: Address, ump: Ump, controller: CCNumber, value: u8) {
        match controller.get() {
            0 => self.put(lane, Part::Program, 0, Said::one(ump)),
            32 => self.put(lane, Part::Program, 1, Said::one(ump)),
            98..=101 => self.select(lane, ump, controller, value),
            6 | 38 => self.data_entry(lane, ump, controller),
            _ if is_mode(controller) => self.mode(lane, at, Protocol::Midi1, controller),
            n => self.put(lane, Part::Controller, u16::from(n), Said::one(ump)),
        }
    }

    /// CC 101/100 (RPN) or 99/98 (NRPN): the select, whose kind the last one
    /// sent decides.
    fn select(&mut self, lane: u16, ump: Ump, controller: CCNumber, value: u8) {
        let (kind, is_msb) = match controller {
            CCNumber::RPN_MSB => (Selected::Rpn, true),
            CCNumber::RPN_LSB => (Selected::Rpn, false),
            CCNumber::NRPN_MSB => (Selected::Nrpn, true),
            _ => (Selected::Nrpn, false),
        };
        let select = self.select.entry(lane).or_insert(Select {
            kind,
            msb: None,
            lsb: None,
        });
        if select.kind != kind {
            *select = Select {
                kind,
                msb: None,
                lsb: None,
            };
        }
        if is_msb {
            select.msb = Some((value, ump));
        } else {
            select.lsb = Some((value, ump));
        }
    }

    /// CC 6 or CC 38 under a complete select: the parameter's data as sent. The
    /// Null parameter (127/127) deselects and holds no value.
    fn data_entry(&mut self, lane: u16, ump: Ump, controller: CCNumber) {
        let Some(&Select {
            kind,
            msb: Some((msb, msb_ump)),
            lsb: Some((lsb, lsb_ump)),
        }) = self.select.get(&lane)
        else {
            return;
        };
        if (msb, lsb) == (127, 127) {
            return;
        }
        let kind_bit = u16::from(kind == Selected::Nrpn) << 14;
        let sub = kind_bit | u16::from(msb) << 7 | u16::from(lsb);
        let mut said = Said([Some(msb_ump), Some(lsb_ump), None, None]);
        if let Some(before) = self.state.get(&Slot {
            lane,
            part: Part::Parameter,
            sub,
        }) {
            said.0[2] = before.0[2];
            said.0[3] = before.0[3];
        }
        said.0[if controller == CCNumber::DATA_ENTRY {
            2
        } else {
            3
        }] = Some(ump);
        self.put(lane, Part::Parameter, sub, said);
    }

    /// All Sound Off, Reset All Controllers or All Notes Off.
    ///
    /// Reset All Controllers (RP-015, Response to Reset All Controllers) resets
    /// Modulation (CC 1), Expression (CC 11), the pedals (CC 64 to 67), the
    /// RPN/NRPN select, pitch bend, channel pressure and poly pressure, and not
    /// the program, the bank select, volume, pan, the effects and sound
    /// controllers or the parameters' values. The record forgets those it holds
    /// of that list (poly pressure is not kept) and the lane's select, so a data
    /// entry after it is stored under no parameter. MIDI 1.0 and MIDI 2.0 take
    /// this branch alike.
    fn mode(&mut self, lane: u16, at: Address, protocol: Protocol, controller: CCNumber) {
        if controller == CCNumber::RESET_ALL {
            self.state.retain(|slot, _| {
                slot.lane != lane
                    || match slot.part {
                        Part::Controller => !matches!(slot.sub, 1 | 11 | 64..=67),
                        Part::Bend | Part::Pressure => false,
                        Part::Program | Part::Parameter => true,
                    }
            });
            self.select.remove(&lane);
            return;
        }
        for n in 0..=127 {
            let key = NoteNumber::new(n).and_then(|note| NoteKey::new(protocol, at, note));
            if let Some(key) = key {
                self.held.keys_mut().remove(key);
            }
        }
    }

    fn put(&mut self, lane: u16, part: Part, sub: u16, said: Said) {
        let slot = Slot { lane, part, sub };
        if !self.state.contains_key(&slot) && self.state.len() >= MAX_STATE_ENTRIES {
            self.lost += 1;
            return;
        }
        self.state.insert(slot, said);
    }

    fn note_on(&mut self, at: Address, note: NoteNumber, on: NoteOn) {
        let table = &mut self.notes[at.group.index()];
        if table.is_none() {
            let mut cells = Vec::new();
            if cells.try_reserve_exact(NOTES_PER_GROUP).is_err() {
                self.lost += 1;
                return;
            }
            cells.resize(NOTES_PER_GROUP, None);
            *table = Some(cells.into_boxed_slice());
        }
        if let Some(table) = table {
            table[at.channel.index() * 128 + usize::from(note.get())] = Some(on);
        }
    }

    fn flex_setup(&mut self, group: MidiGroup, ump: Ump, flex: Flex) {
        let (address, kind) = match flex {
            Flex::SetTempo(_) => (16, 0),
            Flex::SetTimeSignature(_) => (16, 1),
            Flex::SetMetronome(_) => (16, 2),
            Flex::SetKeySignature(address, _) => (flex_address(address), 3),
            Flex::SetChordName(address, _) => (flex_address(address), 4),
            _ => return,
        };
        if address <= 16 {
            self.flex.insert((group.index() as u8, address, kind), ump);
        }
    }

    /// One packet of a SysEx message, taken in at its `status`; a complete
    /// message is kept.
    fn sysex_packet(&mut self, key: (u8, u16), ump: Ump, status: SysexStatus) {
        match status {
            SysexStatus::Complete => {
                self.pending.retain(|(k, _)| *k != key);
                self.keep(vec![ump]);
            }
            SysexStatus::Start => {
                self.pending.retain(|(k, _)| *k != key);
                if self.pending.len() >= MAX_PENDING {
                    self.pending.remove(0);
                    self.lost += 1;
                }
                self.pending.push((key, vec![ump]));
            }
            SysexStatus::Continue => {
                if let Some(at) = self.pending.iter().position(|(k, _)| *k == key) {
                    if self.pending[at].1.len() >= MAX_SYSEX_PACKETS {
                        self.pending.remove(at);
                        self.lost += 1;
                    } else {
                        self.pending[at].1.push(ump);
                    }
                }
            }
            SysexStatus::End => {
                if let Some(at) = self.pending.iter().position(|(k, _)| *k == key) {
                    let (_, mut packets) = self.pending.remove(at);
                    packets.push(ump);
                    self.keep(packets);
                }
            }
            _ => {}
        }
    }

    /// Keeps a complete message at the latest place, dropping the oldest
    /// beyond the bounds.
    fn keep(&mut self, message: Vec<Ump>) {
        if message.len() > MAX_SYSEX_PACKETS {
            self.lost += 1;
            return;
        }
        if let Some(same) = self.sysex.iter().position(|m| *m == message) {
            self.sysex.remove(same);
        }
        self.sysex.push_back(message);
        let mut packets: usize = self.sysex.iter().map(Vec::len).sum();
        while self.sysex.len() > MAX_SYSEX_MESSAGES || packets > MAX_SYSEX_PACKETS {
            let Some(oldest) = self.sysex.pop_front() else {
                break;
            };
            packets -= oldest.len();
            self.lost += 1;
        }
    }

    /// The messages that bring a receiver that heard nothing to the state this
    /// record holds, in the order the [module](self) gives.
    pub(crate) fn catch_up(&self) -> Vec<Ump> {
        let mut out: Vec<Ump> = self.sysex.iter().flatten().copied().collect();
        out.extend(self.flex.values());
        out.extend(self.state.values().flat_map(|said| said.0.iter().flatten()));
        for key in self.held.keys().iter() {
            if let Some(ump) = self.note_on_of(key) {
                out.push(ump);
            }
        }
        out
    }

    /// `key`'s Note On, in the protocol the key was started in.
    fn note_on_of(&self, key: NoteKey) -> Option<Ump> {
        let at = key.address();
        let table = self.notes[at.group.index()].as_deref()?;
        let (velocity, attribute) =
            table[at.channel.index() * 128 + usize::from(key.note().get())]?;
        let voice = Voice2::NoteOn {
            note: key.note(),
            velocity,
            attribute,
        };
        match key.protocol() {
            Protocol::Midi2 => Some(Ump::from(Message::midi2(at, voice))),
            Protocol::Midi1 => Voice1::try_from(voice)
                .ok()
                .map(|voice| Ump::from(Message::midi1(at, voice))),
            _ => None,
        }
    }
}

/// Whether `controller` is a mode message the record acts on, and holds no
/// value of.
fn is_mode(controller: CCNumber) -> bool {
    matches!(controller.get(), 120 | 121 | 123)
}

/// A Flex address as a table byte: the channel, 16 for the Group, 17 reserved.
fn flex_address(address: FlexAddress) -> u8 {
    match address {
        FlexAddress::Channel(channel) => channel.index() as u8,
        FlexAddress::Group => 16,
        FlexAddress::Reserved(_) => 17,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_ump::{BendValue, ControlValue, MidiChannel, Sysex7Packet, U7};

    const CH: MidiChannel = MidiChannel::FIRST;

    fn at() -> Address {
        Address::first(CH)
    }

    fn cc1(controller: u8, value: u8) -> Ump {
        Ump::from(Message::midi1(
            at(),
            Voice1::control_change(CCNumber::new(controller).unwrap(), U7::of(value)),
        ))
    }

    fn cc2(controller: u8, value: u32) -> Ump {
        Ump::from(Message::midi2(
            at(),
            Voice2::ControlChange {
                controller: CCNumber::new(controller).unwrap(),
                value: ControlValue::new(value),
            },
        ))
    }

    fn on1(note: u8, velocity: u8) -> Ump {
        Ump::from(Message::midi1(
            at(),
            Voice1::note_on(NoteNumber::of(note), U7::of(velocity)),
        ))
    }

    fn off1(note: u8) -> Ump {
        Ump::from(Message::midi1(
            at(),
            Voice1::note_off(NoteNumber::of(note), U7::of(0)),
        ))
    }

    fn sysex(status: SysexStatus, bytes: &[u8]) -> Ump {
        Ump::from(Message::Sysex7(
            MidiGroup::FIRST,
            Sysex7Packet::new(status, bytes).unwrap(),
        ))
    }

    fn observed(events: &[Ump]) -> Record {
        let mut record = Record::default();
        for &ump in events {
            record.observe(ump);
        }
        record
    }

    /// A controller keeps its last value, and nothing else of its history.
    #[test]
    fn a_controller_is_caught_up_to_its_last_value_only() {
        let record = observed(&[cc2(7, 100), cc2(10, 5), cc2(7, 200)]);
        assert_eq!(record.catch_up(), [cc2(7, 200), cc2(10, 5)]);
    }

    /// M2-115 section 3: a 7-bit velocity reaches 16 bits by min-center-max
    /// scaling, 64 at the center 0x8000, 127 at the top, and a velocity of 1
    /// at 1 << 9.
    #[test]
    fn a_midi1_note_on_is_kept_at_its_midi2_velocity() {
        let mut record = Record::default();
        for (v, want) in [(64u8, 0x8000u16), (127, 0xFFFF), (1, 0x0200)] {
            record.observe(on1(60, v));
            let kept = record.notes[0].as_deref().unwrap()[60];
            assert_eq!(
                kept,
                Some((NoteVelocity::new(want), Attribute::None)),
                "7-bit {v}"
            );
        }
    }

    /// A key sent in MIDI 1.0 is caught up in MIDI 1.0 (velocity 64 is 64 again),
    /// one sent in MIDI 2.0 keeps its 16-bit velocity and attribute.
    #[test]
    fn a_held_key_is_caught_up_in_the_protocol_it_started_in() {
        let on2 = Ump::from(Message::midi2(
            Address::first(MidiChannel::of(1)),
            Voice2::NoteOn {
                note: NoteNumber::of(61),
                velocity: NoteVelocity::new(0x1234),
                attribute: Attribute::Manufacturer(0xBEEF),
            },
        ));
        let record = observed(&[on1(60, 64), on2]);
        assert_eq!(record.catch_up(), [on1(60, 64), on2]);
    }

    /// A key that ended is not caught up, a Note On at velocity 0 included, and
    /// All Notes Off ends the channel's keys.
    #[test]
    fn a_key_that_ended_is_not_caught_up() {
        let record = observed(&[on1(60, 90), off1(60), on1(61, 90), on1(62, 90), on1(63, 0)]);
        assert_eq!(record.catch_up(), [on1(61, 90), on1(62, 90)]);
        let record = observed(&[on1(61, 90), on1(62, 90), cc1(123, 0)]);
        assert_eq!(record.catch_up(), []);
    }

    /// RP-015: Reset All Controllers resets modulation, expression, the pedals,
    /// bend, pressure and the parameter select, and leaves the program, volume,
    /// pan and the other controllers as they were.
    #[test]
    fn reset_all_controllers_forgets_exactly_what_rp015_resets() {
        let program = Ump::from(Message::midi1(at(), Voice1::ProgramChange(U7::of(5))));
        let bend = Ump::from(Message::midi1(
            at(),
            Voice1::PitchBend(tutti_ump::U14::of(0x3000)),
        ));
        let pressure = Ump::from(Message::midi1(at(), Voice1::ChannelPressure(U7::of(9))));
        let record = observed(&[
            program,
            cc1(7, 90),
            cc1(10, 30),
            cc1(1, 64),
            cc1(11, 40),
            cc1(64, 127),
            bend,
            pressure,
            cc1(121, 0),
        ]);
        assert_eq!(record.catch_up(), [program, cc1(7, 90), cc1(10, 30)]);
        let record = observed(&[cc2(7, 5), cc2(1, 9), cc2(121, 0)]);
        assert_eq!(record.catch_up(), [cc2(7, 5)], "MIDI 2.0 alike");
    }

    /// After a reset the parameter select is null: a data entry with no new
    /// select is stored under no parameter, and a parameter value before it
    /// stays.
    #[test]
    fn a_data_entry_after_a_reset_has_no_select() {
        let record = observed(&[cc1(101, 0), cc1(100, 0), cc1(6, 2), cc1(121, 0), cc1(6, 9)]);
        assert_eq!(record.catch_up(), [cc1(101, 0), cc1(100, 0), cc1(6, 2)]);
    }

    /// The order: SysEx, then per channel the bank and program, controllers, bend
    /// and pressure, then the held keys.
    #[test]
    fn a_catch_up_sends_setup_then_channel_state_then_keys() {
        let program = Ump::from(Message::midi1(at(), Voice1::ProgramChange(U7::of(5))));
        let pressure = Ump::from(Message::midi1(at(), Voice1::ChannelPressure(U7::of(9))));
        let bend = Ump::from(Message::midi1(
            at(),
            Voice1::PitchBend(tutti_ump::U14::of(0x3000)),
        ));
        let reset = sysex(SysexStatus::Complete, &[0x7E, 0x7F, 0x09, 0x01]);
        let record = observed(&[
            on1(60, 90),
            pressure,
            bend,
            program,
            cc1(7, 90),
            cc1(32, 2),
            cc1(0, 1),
            reset,
        ]);
        assert_eq!(
            record.catch_up(),
            [
                reset,
                cc1(0, 1),
                cc1(32, 2),
                program,
                cc1(7, 90),
                bend,
                pressure,
                on1(60, 90)
            ]
        );
    }

    /// A MIDI 2.0 Program Change is kept with its bank.
    #[test]
    fn a_midi2_program_change_keeps_its_bank() {
        let with_bank = Ump::from(Message::midi2(
            at(),
            Voice2::ProgramChange {
                program: U7::of(4),
                bank: Some(tutti_ump::Bank {
                    msb: U7::of(1),
                    lsb: U7::of(2),
                }),
            },
        ));
        let plain = Ump::from(Message::midi2(
            at(),
            Voice2::ProgramChange {
                program: U7::of(9),
                bank: None,
            },
        ));
        assert_eq!(observed(&[with_bank]).catch_up(), [with_bank]);
        assert_eq!(observed(&[with_bank, plain]).catch_up(), [plain]);
    }

    /// MIDI 1.0 RPN: the select, then the data entry MSB and LSB, as sent; the
    /// last data under a select replaces the one before, and the Null RPN
    /// (127/127) holds nothing.
    #[test]
    fn a_midi1_rpn_is_caught_up_as_its_select_and_data() {
        let record = observed(&[
            cc1(101, 0),
            cc1(100, 0),
            cc1(6, 2),
            cc1(38, 10),
            cc1(6, 12),
            cc1(101, 127),
            cc1(100, 127),
            cc1(6, 99),
        ]);
        assert_eq!(
            record.catch_up(),
            [cc1(101, 0), cc1(100, 0), cc1(6, 12), cc1(38, 10)]
        );
    }

    /// A data entry with no complete select, or an NRPN after an RPN select, is
    /// not a value of the RPN.
    #[test]
    fn an_nrpn_select_replaces_an_rpn_one() {
        let record = observed(&[cc1(101, 0), cc1(6, 1), cc1(99, 3), cc1(98, 4), cc1(6, 5)]);
        assert_eq!(record.catch_up(), [cc1(99, 3), cc1(98, 4), cc1(6, 5)]);
    }

    /// MIDI 2.0 Registered and Assignable Controllers are kept per address.
    #[test]
    fn midi2_parameters_keep_their_last_value_per_address() {
        let reg = |index: u8, value: u32| {
            Ump::from(Message::midi2(
                at(),
                Voice2::Registered {
                    bank: U7::of(0),
                    index: U7::of(index),
                    value: ControlValue::new(value),
                },
            ))
        };
        let nrpn = Ump::from(Message::midi2(
            at(),
            Voice2::Assignable {
                bank: U7::of(0),
                index: U7::of(0),
                value: ControlValue::new(3),
            },
        ));
        let record = observed(&[reg(0, 1), nrpn, reg(0, 2), reg(1, 7)]);
        assert_eq!(record.catch_up(), [reg(0, 2), reg(1, 7), nrpn]);
    }

    /// A SysEx message of several packets is kept whole, in order, once at its
    /// latest place; a message cut short is not kept.
    #[test]
    fn a_sysex_message_is_kept_once_at_its_latest_place() {
        let a = [
            sysex(SysexStatus::Start, &[1, 2, 3, 4, 5, 6]),
            sysex(SysexStatus::End, &[7]),
        ];
        let b = sysex(SysexStatus::Complete, &[9]);
        let cut = sysex(SysexStatus::Start, &[8, 8]);
        let mut events = a.to_vec();
        events.extend([b, a[0], a[1], cut]);
        let record = observed(&events);
        assert_eq!(record.catch_up(), [b, a[0], a[1]]);
    }

    /// The oldest SysEx message goes past the bound, and is counted.
    #[test]
    fn sysex_past_the_bound_drops_the_oldest_and_counts_it() {
        let mut record = Record::default();
        let message = |n: usize| sysex(SysexStatus::Complete, &[(n % 100) as u8, (n / 100) as u8]);
        for n in 0..MAX_SYSEX_MESSAGES + 3 {
            record.observe(message(n));
        }
        let kept = record.catch_up();
        assert_eq!(kept.len(), MAX_SYSEX_MESSAGES);
        assert_eq!(kept[0], message(3));
        assert_eq!(record.take_lost(), 3);
        assert_eq!(record.take_lost(), 0);
    }

    /// Flex setup is kept per Group and address, the last one.
    #[test]
    fn flex_setup_is_kept_per_address() {
        let tempo = |n: u32| {
            Ump::from(Message::Flex(
                MidiGroup::FIRST,
                Flex::SetTempo(tutti_ump::FlexTempo::new(n)),
            ))
        };
        let record = observed(&[tempo(1), tempo(2)]);
        assert_eq!(record.catch_up(), [tempo(2)]);
    }

    /// A pitch bend is kept in the form sent.
    #[test]
    fn a_midi2_bend_is_kept() {
        let bend = Ump::from(Message::midi2(at(), Voice2::PitchBend(BendValue::CENTER)));
        assert_eq!(observed(&[bend]).catch_up(), [bend]);
    }
}
