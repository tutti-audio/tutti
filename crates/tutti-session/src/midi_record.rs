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
//! room for, or drops, is counted in the output's
//! [`Losses::lost`](tutti_units::Losses) ([`take_lost`]), never dropped
//! silently.
//!
//! **What is kept**, each in the form it was sent:
//!
//! - the complete System Exclusive messages (SysEx7 and SysEx8), in order,
//!   a message with the same packets kept once at its latest place, at most
//!   [`MAX_SYSEX_MESSAGES`] of at most [`MAX_SYSEX_PACKETS`] packets in all.
//!   SysEx is kept as state, not as history: a repeated message moves to its
//!   latest place, so a sequence whose repeats matter (a toggle, a counter) is
//!   not reproduced;
//! - the last Flex Data setup message (tempo, time signature, metronome, key
//!   signature, chord name) per Group and address;
//! - per Group and Channel of each protocol, the last of each:
//!   - MIDI 1.0 bank select (CC 0 and CC 32) as it applies: a bank takes effect
//!     at the next Program Change (MIDI 1.0 Detailed Specification, Bank
//!     Select), so the bank in force at the last Program Change is kept with
//!     it and a bank sent since is kept apart, pending;
//!   - the program (MIDI 2.0: the Program Change with its bank; a Program
//!     Change whose Bank Valid bit is clear keeps the receiver's bank, M2-104
//!     section 7.4.9, so the last valid bank is carried into it);
//!   - each Control Change value, except Portamento Control (CC 84), the
//!     High-Resolution Velocity Prefix (CC 88) and Data Increment and Decrement
//!     (CC 96, 97), which are events, not values;
//!   - each MIDI 2.0 Registered and Assignable Controller, and MIDI 1.0's
//!     parameters: the CC 101/100 (RPN) or 99/98 (NRPN) select as sent with the
//!     CC 6 and CC 38 data entries sent under it, and the select in force (the
//!     last pair sent, or the Null pair of a Reset All Controllers);
//!   - the pitch bend and the channel pressure;
//! - the keys down, in a [`Held`] (the one fact of which keys are down), and
//!   each one's Note On in MIDI 2.0 form (velocity and attribute), a MIDI 1.0
//!   Note On being kept as `Voice2::from` makes it (M2-104 Appendix D.3, with
//!   M2-115's min-center-max scaling). A key
//!   is caught up in the protocol it was started in.
//!
//! **Order of a catch-up, and why.** SysEx and Flex setup first: a device
//! reset must come before the state it would erase. Then, one Channel after
//! another: the bank in force and the program (a controller's meaning can
//! depend on the patch), the bank sent since, the controllers, the parameters,
//! the select in force (last, so the joiner is left on the stream's
//! selection, whatever order the parameters replay in), the pitch bend and the
//! pressure. The held notes come last, so each sounds with its channel's
//! patch, controllers and bend already in place.
//!
//! **Mode messages.** A Control Change of All Sound Off (120), All Notes Off
//! (123) or a channel mode (124 to 127: Omni Off, Omni On, Mono, Poly) ends the
//! channel's keys; a channel mode's value is also kept as a controller. A
//! Reset All Controllers (121) forgets exactly what RP-015 resets: modulation
//! (CC 1), expression (CC 11), the pedals (CC 64 to 67), pitch bend, channel
//! pressure and the parameter select (which becomes the Null pair). Every other
//! controller, the program and the parameter values stay.
//!
//! **Device reset.** A Universal Non-Real Time General MIDI System On, System
//! Off or GM2 System On (`7E dev 09 01|02|03`) erases the channel state of the
//! UMP Group it was sent in (a Group is one cable of a device, M2-104 section
//! 2.1.2, so it resets the device on that cable): the record forgets that
//! Group's channel state, parameter selects and held keys, and keeps the
//! message itself in SysEx order. Manufacturer resets (GS, XG) and a reset
//! sent as SysEx8 are not recognised.
//!
//! **Limits.** A note kept sounding only by a pedal (its Note Off sent) is not
//! caught up. Polyphonic pressure, the MIDI 2.0 per-note messages and relative
//! controllers are not kept.
//!
//! [`observe`]: Record::observe
//! [`catch_up`]: Record::catch_up
//! [`take_lost`]: Record::take_lost

use std::collections::{BTreeMap, VecDeque};

use tutti_ump::{
    Address, Attribute, CCNumber, Data128, Flex, FlexAddress, GroupChannelIndex, Held, Message,
    MidiGroup, NoteKey, NoteNumber, NoteVelocity, Protocol, SysexStatus, U7,
    UNIVERSAL_NON_REAL_TIME, Ump, Voice1, Voice2,
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
pub(crate) const MAX_PENDING: usize = 64;

/// How many values of channel state a record keeps: 8 192 is every Control
/// Change of four Groups of sixteen Channels (4 x 16 x 128), and a stream of one
/// protocol on a few Groups is far below it. A value beyond it is dropped and
/// counted.
pub(crate) const MAX_STATE_ENTRIES: usize = 8192;

/// A table of Note Ons covers one lane (a protocol, a Group, a Channel is
/// sixteen of them per Group): 128 keys.
const KEYS_PER_CHANNEL: usize = 128;

/// The channels of a Group: a note table covers those of one protocol's Group.
const CHANNELS: usize = tutti_ump::MidiChannel::COUNT as usize;

/// Two protocols of sixteen Groups.
const NOTE_TABLES: usize = 32;

/// The Universal Non-Real Time sub-id 1 of General MIDI (MIDI 1.0 Detailed
/// Specification, General MIDI System Messages) and its sub-id 2: System On
/// (`01`), System Off (`02`) and GM2 System On (`03`).
const GENERAL_MIDI: u8 = 0x09;
const GENERAL_MIDI_RESETS: std::ops::RangeInclusive<u8> = 0x01..=0x03;

/// The channel mode messages (Omni Off, Omni On, Mono On, Poly On): they end a
/// channel's notes as All Notes Off does.
const CHANNEL_MODES: std::ops::RangeInclusive<u8> = 124..=127;

/// Data Increment and Decrement, CC 96 and 97: events, not values.
const DATA_INCREMENT: CCNumber = CCNumber::of(96);
const DATA_DECREMENT: CCNumber = CCNumber::of(97);

/// Which half of a pair of MIDI 1.0 controllers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Half {
    Msb,
    Lsb,
}

/// Which parameter number a MIDI 1.0 CC 99/98 or 101/100 pair selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Selected {
    Rpn,
    Nrpn,
}

/// A parameter number: the kind, the bank (MSB) and the index (LSB).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Param {
    kind: Selected,
    msb: u8,
    lsb: u8,
}

/// What a slot holds a value of, in the order a catch-up sends them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum What {
    /// MIDI 1.0 bank select as the last Program Change took it.
    Bank(Half),
    Program,
    /// MIDI 1.0 bank select sent since the last Program Change.
    PendingBank(Half),
    Controller(CCNumber),
    Parameter(Param),
    /// The MIDI 1.0 parameter select in force.
    Selection,
    Bend,
    Pressure,
}

/// Where a value of a channel's state is kept: the lane (protocol, Group,
/// Channel) first, so a catch-up goes channel by channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Slot {
    lane: GroupChannelIndex,
    what: What,
}

/// A MIDI 1.0 channel's parameter select: the kind and the two halves as sent.
#[derive(Clone, Copy, Debug)]
struct Select {
    kind: Selected,
    msb: Option<(u8, Ump)>,
    lsb: Option<(u8, Ump)>,
}

/// A parameter's messages as sent: its select, and the data entries sent under
/// it.
#[derive(Clone, Copy, Debug)]
struct Parameter {
    select: [Ump; 2],
    data_msb: Option<Ump>,
    data_lsb: Option<Ump>,
}

/// What a slot holds.
#[derive(Clone, Copy, Debug)]
enum Entry {
    One(Ump),
    Selection(Select),
    Parameter(Parameter),
}

impl Entry {
    /// The messages of the entry, in the order they are replayed.
    fn push_to(&self, out: &mut Vec<Ump>) {
        match *self {
            Self::One(ump) => out.push(ump),
            Self::Selection(select) => {
                out.extend(select.msb.map(|(_, ump)| ump));
                out.extend(select.lsb.map(|(_, ump)| ump));
            }
            Self::Parameter(p) => {
                out.extend(p.select);
                out.extend(p.data_msb);
                out.extend(p.data_lsb);
            }
        }
    }
}

/// Where a Flex setup message is addressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FlexAt {
    Channel(usize),
    Group,
}

/// Which Flex setup message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FlexKind {
    Tempo,
    TimeSignature,
    Metronome,
    KeySignature,
    ChordName,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct FlexKey {
    group: usize,
    at: FlexAt,
    kind: FlexKind,
}

/// Which SysEx stream a packet belongs to: a Group has one SysEx7 stream and
/// SysEx8 streams by id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    Seven,
    Eight(u8),
}

/// A Note On as MIDI 2.0 carries it: what a catch-up of the key needs beyond the
/// key itself.
type NoteOn = (NoteVelocity, Attribute);

/// What one output has said. See the [module](self).
pub(crate) struct Record {
    /// The keys down; the note-ons are in `notes`, and "down" is only here.
    held: Held,
    /// A protocol's Group's Note Ons by channel and key, allocated at the first
    /// Note On there and never grown.
    notes: [Option<Box<[Option<NoteOn>]>>; NOTE_TABLES],
    state: BTreeMap<Slot, Entry>,
    /// Flex setup by Group, address and kind.
    flex: BTreeMap<FlexKey, Ump>,
    /// Complete SysEx messages, oldest first.
    sysex: VecDeque<Vec<Ump>>,
    /// SysEx messages being assembled, by Group and stream.
    pending: Vec<((usize, Stream), Vec<Ump>)>,
    lost: u64,
}

impl Default for Record {
    fn default() -> Self {
        Self {
            held: Held::new(),
            notes: std::array::from_fn(|_| None),
            state: BTreeMap::new(),
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
    /// What was dropped since the last call: a value, a SysEx message or a Note
    /// On table that did not fit, or a SysEx message that never completed.
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
                self.sysex_packet((group.index(), Stream::Seven), ump, packet.status());
            }
            Message::Data128(group, Data128::Sysex8(packet)) => {
                let key = (group.index(), Stream::Eight(packet.stream_id().get()));
                self.sysex_packet(key, ump, packet.status());
            }
            Message::Data128(group, Data128::Sysex8Abort { stream_id }) => {
                let key = (group.index(), Stream::Eight(stream_id.get()));
                self.lost += u64::from(self.take_pending(key).is_some());
            }
            Message::Flex(group, flex) => self.flex_setup(group, ump, flex),
            _ => {}
        }
    }

    fn voice1(&mut self, ump: Ump, at: Address, voice: Voice1) {
        let Some(lane) = GroupChannelIndex::at(Protocol::Midi1, at) else {
            return;
        };
        match voice {
            Voice1::NoteOn { note, velocity } if velocity.get() > 0 => {
                self.held.observe(ump);
                if let Voice2::NoteOn {
                    velocity,
                    attribute,
                    ..
                } = Voice2::from(voice)
                {
                    self.note_on(lane, note, (velocity, attribute));
                }
            }
            Voice1::NoteOn { .. } | Voice1::NoteOff { .. } => self.held.observe(ump),
            Voice1::PolyPressure { .. } => {}
            Voice1::ControlChange { controller, value } => match controller {
                CCNumber::BANK_SELECT => self.put_one(lane, What::PendingBank(Half::Msb), ump),
                CCNumber::BANK_SELECT_LSB => self.put_one(lane, What::PendingBank(Half::Lsb), ump),
                CCNumber::RPN_MSB | CCNumber::RPN_LSB | CCNumber::NRPN_MSB | CCNumber::NRPN_LSB => {
                    self.select(lane, ump, controller, value.get());
                }
                CCNumber::DATA_ENTRY | CCNumber::DATA_ENTRY_LSB => {
                    self.data_entry(lane, ump, controller);
                }
                _ => self.controller(lane, ump, controller),
            },
            Voice1::ProgramChange(_) => {
                // The bank sent since the last Program Change is in force now.
                for half in [Half::Msb, Half::Lsb] {
                    let pending = Slot {
                        lane,
                        what: What::PendingBank(half),
                    };
                    if let Some(bank) = self.state.remove(&pending) {
                        let slot = Slot {
                            lane,
                            what: What::Bank(half),
                        };
                        self.state.insert(slot, bank);
                    }
                }
                self.put_one(lane, What::Program, ump);
            }
            Voice1::ChannelPressure(_) => self.put_one(lane, What::Pressure, ump),
            Voice1::PitchBend(_) => self.put_one(lane, What::Bend, ump),
        }
    }

    fn voice2(&mut self, ump: Ump, at: Address, voice: Voice2) {
        let Some(lane) = GroupChannelIndex::at(Protocol::Midi2, at) else {
            return;
        };
        match voice {
            Voice2::NoteOn {
                note,
                velocity,
                attribute,
            } => {
                self.held.observe(ump);
                self.note_on(lane, note, (velocity, attribute));
            }
            Voice2::NoteOff { .. } => self.held.observe(ump),
            Voice2::ControlChange { controller, .. } => {
                if !controller.is_ignored_in_midi2() {
                    self.controller(lane, ump, controller);
                }
            }
            Voice2::ProgramChange { program, bank } => {
                // A Program Change without a valid bank keeps the receiver's.
                let ump = match bank {
                    Some(_) => ump,
                    None => self
                        .bank_of(lane)
                        .map(|bank| {
                            let voice = Voice2::ProgramChange {
                                program,
                                bank: Some(bank),
                            };
                            Ump::from(Message::midi2(at, voice))
                        })
                        .unwrap_or(ump),
                };
                self.put_one(lane, What::Program, ump);
            }
            Voice2::ChannelPressure(_) => self.put_one(lane, What::Pressure, ump),
            Voice2::PitchBend(_) => self.put_one(lane, What::Bend, ump),
            Voice2::Registered { bank, index, .. } => {
                let param = Param {
                    kind: Selected::Rpn,
                    msb: bank.get(),
                    lsb: index.get(),
                };
                self.put_one(lane, What::Parameter(param), ump);
            }
            Voice2::Assignable { bank, index, .. } => {
                let param = Param {
                    kind: Selected::Nrpn,
                    msb: bank.get(),
                    lsb: index.get(),
                };
                self.put_one(lane, What::Parameter(param), ump);
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

    /// The last valid bank of a MIDI 2.0 lane, from the Program Change kept.
    fn bank_of(&self, lane: GroupChannelIndex) -> Option<tutti_ump::Bank> {
        let Some(Entry::One(ump)) = self.state.get(&Slot {
            lane,
            what: What::Program,
        }) else {
            return None;
        };
        match ump.decode() {
            Ok(Message::Midi2(_, Voice2::ProgramChange { bank, .. })) => bank,
            _ => None,
        }
    }

    /// A Control Change that is not a bank select, a parameter select or a data
    /// entry: a mode message, or a value.
    fn controller(&mut self, lane: GroupChannelIndex, ump: Ump, controller: CCNumber) {
        if controller == CCNumber::RESET_ALL {
            self.reset_controllers(lane);
            return;
        }
        if controller == CCNumber::ALL_SOUND_OFF
            || controller == CCNumber::ALL_NOTES_OFF
            || CHANNEL_MODES.contains(&controller.get())
        {
            self.end_keys(lane);
        }
        let event = [
            CCNumber::ALL_SOUND_OFF,
            CCNumber::ALL_NOTES_OFF,
            CCNumber::PORTAMENTO_CONTROL,
            CCNumber::VELOCITY_PREFIX,
            DATA_INCREMENT,
            DATA_DECREMENT,
        ];
        if !event.contains(&controller) {
            self.put_one(lane, What::Controller(controller), ump);
        }
    }

    /// CC 101/100 (RPN) or 99/98 (NRPN): the select, whose kind the last one
    /// sent decides.
    fn select(&mut self, lane: GroupChannelIndex, ump: Ump, controller: CCNumber, value: u8) {
        let (kind, half) = match controller {
            CCNumber::RPN_MSB => (Selected::Rpn, Half::Msb),
            CCNumber::RPN_LSB => (Selected::Rpn, Half::Lsb),
            CCNumber::NRPN_MSB => (Selected::Nrpn, Half::Msb),
            _ => (Selected::Nrpn, Half::Lsb),
        };
        let mut select = match self.state.get(&selection(lane)) {
            Some(Entry::Selection(select)) if select.kind == kind => *select,
            _ => Select {
                kind,
                msb: None,
                lsb: None,
            },
        };
        match half {
            Half::Msb => select.msb = Some((value, ump)),
            Half::Lsb => select.lsb = Some((value, ump)),
        }
        self.put(selection(lane), Entry::Selection(select));
    }

    /// CC 6 or CC 38 under a complete select: the parameter's data as sent. The
    /// Null parameter (127/127) deselects and holds no value.
    fn data_entry(&mut self, lane: GroupChannelIndex, ump: Ump, controller: CCNumber) {
        let Some(&Entry::Selection(Select {
            kind,
            msb: Some((msb, msb_ump)),
            lsb: Some((lsb, lsb_ump)),
        })) = self.state.get(&selection(lane))
        else {
            return;
        };
        if (msb, lsb) == (U7::MAX.get(), U7::MAX.get()) {
            return;
        }
        let slot = Slot {
            lane,
            what: What::Parameter(Param { kind, msb, lsb }),
        };
        let mut parameter = Parameter {
            select: [msb_ump, lsb_ump],
            data_msb: None,
            data_lsb: None,
        };
        if let Some(&Entry::Parameter(before)) = self.state.get(&slot) {
            parameter.data_msb = before.data_msb;
            parameter.data_lsb = before.data_lsb;
        }
        if controller == CCNumber::DATA_ENTRY {
            parameter.data_msb = Some(ump);
        } else {
            parameter.data_lsb = Some(ump);
        }
        self.put(slot, Entry::Parameter(parameter));
    }

    /// Reset All Controllers (RP-015, Response to Reset All Controllers) resets
    /// Modulation (CC 1), Expression (CC 11), the pedals (CC 64 to 67), the
    /// RPN/NRPN select, pitch bend, channel pressure and poly pressure, and not
    /// the program, the bank select, volume, pan, the effects and sound
    /// controllers or the parameters' values. The record forgets those it holds
    /// of that list (poly pressure is not kept) and puts the lane's select on
    /// the Null pair, so a data entry after it is stored under no parameter.
    /// MIDI 1.0 and MIDI 2.0 take this branch alike.
    fn reset_controllers(&mut self, lane: GroupChannelIndex) {
        let pedals = CCNumber::SUSTAIN.get()..=CCNumber::SOFT_PEDAL.get();
        self.state.retain(|slot, _| {
            slot.lane != lane
                || match slot.what {
                    What::Controller(c) => {
                        c != CCNumber::MOD_WHEEL
                            && c != CCNumber::EXPRESSION
                            && !pedals.contains(&c.get())
                    }
                    What::Bend | What::Pressure => false,
                    _ => true,
                }
        });
        if self.state.contains_key(&selection(lane)) {
            let null = |controller| {
                let voice = Voice1::control_change(controller, U7::MAX);
                Ump::from(Message::midi1(lane.address(), voice))
            };
            let select = Select {
                kind: Selected::Rpn,
                msb: Some((U7::MAX.get(), null(CCNumber::RPN_MSB))),
                lsb: Some((U7::MAX.get(), null(CCNumber::RPN_LSB))),
            };
            self.put(selection(lane), Entry::Selection(select));
        }
    }

    /// Ends the keys of `lane`'s channel.
    fn end_keys(&mut self, lane: GroupChannelIndex) {
        let (protocol, at) = (lane.protocol(), lane.address());
        for note in (0..=NoteNumber::MAX.get()).filter_map(NoteNumber::new) {
            if let Some(key) = NoteKey::new(protocol, at, note) {
                self.held.keys_mut().remove(key);
            }
        }
    }

    /// A device reset in `group`: the channel state, the selects and the keys of
    /// the Group's lanes are forgotten.
    fn reset_group(&mut self, group: MidiGroup) {
        self.state
            .retain(|slot, _| slot.lane.address().group != group);
        let keys: Vec<NoteKey> = self
            .held
            .keys()
            .iter()
            .filter(|key| key.address().group == group)
            .collect();
        for key in keys {
            self.held.keys_mut().remove(key);
        }
    }

    fn put_one(&mut self, lane: GroupChannelIndex, what: What, ump: Ump) {
        self.put(Slot { lane, what }, Entry::One(ump));
    }

    fn put(&mut self, slot: Slot, entry: Entry) {
        if !self.state.contains_key(&slot) && self.state.len() >= MAX_STATE_ENTRIES {
            self.lost += 1;
            return;
        }
        self.state.insert(slot, entry);
    }

    fn note_on(&mut self, lane: GroupChannelIndex, note: NoteNumber, on: NoteOn) {
        let table = &mut self.notes[lane.index() / CHANNELS];
        if table.is_none() {
            let mut cells = Vec::new();
            if cells
                .try_reserve_exact(CHANNELS * KEYS_PER_CHANNEL)
                .is_err()
            {
                self.lost += 1;
                return;
            }
            cells.resize(CHANNELS * KEYS_PER_CHANNEL, None);
            *table = Some(cells.into_boxed_slice());
        }
        if let Some(table) = table {
            table[cell(lane, note)] = Some(on);
        }
    }

    fn flex_setup(&mut self, group: MidiGroup, ump: Ump, flex: Flex) {
        let (at, kind) = match flex {
            Flex::SetTempo(_) => (FlexAt::Group, FlexKind::Tempo),
            Flex::SetTimeSignature(_) => (FlexAt::Group, FlexKind::TimeSignature),
            Flex::SetMetronome(_) => (FlexAt::Group, FlexKind::Metronome),
            Flex::SetKeySignature(address, _) => match flex_at(address) {
                Some(at) => (at, FlexKind::KeySignature),
                None => return,
            },
            Flex::SetChordName(address, _) => match flex_at(address) {
                Some(at) => (at, FlexKind::ChordName),
                None => return,
            },
            _ => return,
        };
        let key = FlexKey {
            group: group.index(),
            at,
            kind,
        };
        self.flex.insert(key, ump);
    }

    /// The run being assembled at `key`, taken out.
    fn take_pending(&mut self, key: (usize, Stream)) -> Option<Vec<Ump>> {
        let at = self.pending.iter().position(|(k, _)| *k == key)?;
        Some(self.pending.remove(at).1)
    }

    /// One packet of a SysEx message, taken in at its `status`; a complete
    /// message is kept. A run that is replaced, cut short, has no start or
    /// passes [`MAX_SYSEX_PACKETS`] is counted lost, once: a run past the bound
    /// stays pending empty, so its later packets are discarded uncounted.
    fn sysex_packet(&mut self, key: (usize, Stream), ump: Ump, status: SysexStatus) {
        match status {
            SysexStatus::Complete => {
                self.lost += u64::from(self.take_pending(key).is_some_and(|p| !p.is_empty()));
                self.keep(vec![ump]);
            }
            SysexStatus::Start => {
                self.lost += u64::from(self.take_pending(key).is_some_and(|p| !p.is_empty()));
                if self.pending.len() >= MAX_PENDING {
                    let (_, evicted) = self.pending.remove(0);
                    self.lost += u64::from(!evicted.is_empty());
                }
                self.pending.push((key, vec![ump]));
            }
            SysexStatus::Continue => match self.take_pending(key) {
                Some(packets) if packets.is_empty() => self.pending.push((key, packets)),
                Some(mut packets) if packets.len() < MAX_SYSEX_PACKETS => {
                    packets.push(ump);
                    self.pending.push((key, packets));
                }
                Some(_) => {
                    self.lost += 1;
                    self.pending.push((key, Vec::new()));
                }
                None => self.lost += 1,
            },
            SysexStatus::End => match self.take_pending(key) {
                Some(packets) if packets.is_empty() => {}
                Some(mut packets) => {
                    packets.push(ump);
                    self.keep(packets);
                }
                None => self.lost += 1,
            },
            _ => {}
        }
    }

    /// Keeps a complete message at the latest place, dropping the oldest
    /// beyond the bounds; a device reset also erases its Group's state.
    fn keep(&mut self, message: Vec<Ump>) {
        if message.len() > MAX_SYSEX_PACKETS {
            self.lost += 1;
            return;
        }
        if let Some(group) = device_reset(&message) {
            self.reset_group(group);
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
        for entry in self.state.values() {
            entry.push_to(&mut out);
        }
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
        let table = self.notes[key.lane().index() / CHANNELS].as_deref()?;
        let (velocity, attribute) = table[cell(key.lane(), key.note())]?;
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

/// The slot of a lane's parameter select.
fn selection(lane: GroupChannelIndex) -> Slot {
    Slot {
        lane,
        what: What::Selection,
    }
}

/// The cell of `note` of `lane` in its table.
fn cell(lane: GroupChannelIndex, note: NoteNumber) -> usize {
    (lane.index() % CHANNELS) * KEYS_PER_CHANNEL + usize::from(note.get())
}

/// Where a Flex setup message goes, or `None` for a reserved address.
fn flex_at(address: FlexAddress) -> Option<FlexAt> {
    match address {
        FlexAddress::Channel(channel) => Some(FlexAt::Channel(channel.index())),
        FlexAddress::Group => Some(FlexAt::Group),
        FlexAddress::Reserved(_) => None,
    }
}

/// The Group of a Universal Non-Real Time General MIDI System On, System Off or
/// GM2 System On (`7E dev 09 01|02|03`), when `message` is one.
fn device_reset(message: &[Ump]) -> Option<MidiGroup> {
    let mut bytes = Vec::new();
    let mut group = None;
    for ump in message {
        let Ok(Message::Sysex7(g, packet)) = ump.decode() else {
            return None;
        };
        group = Some(g);
        bytes.extend_from_slice(packet.data());
    }
    match bytes[..] {
        [UNIVERSAL_NON_REAL_TIME, _, GENERAL_MIDI, mode] if GENERAL_MIDI_RESETS.contains(&mode) => {
            group
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_ump::{Bank, ControlValue, FlexTempo, MidiChannel, Sysex7Packet, U7, U14};

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

    fn program1(program: u8) -> Ump {
        Ump::from(Message::midi1(at(), Voice1::ProgramChange(U7::of(program))))
    }

    fn program2(program: u8, bank: Option<Bank>) -> Ump {
        Ump::from(Message::midi2(
            at(),
            Voice2::ProgramChange {
                program: U7::of(program),
                bank,
            },
        ))
    }

    fn bend1(value: u16) -> Ump {
        Ump::from(Message::midi1(at(), Voice1::PitchBend(U14::of(value))))
    }

    fn pressure1(value: u8) -> Ump {
        Ump::from(Message::midi1(at(), Voice1::ChannelPressure(U7::of(value))))
    }

    fn on1(note: u8, velocity: u8) -> Ump {
        Ump::from(Message::midi1(
            at(),
            Voice1::note_on(NoteNumber::of(note), U7::of(velocity)),
        ))
    }

    fn on2(note: u8, velocity: u16) -> Ump {
        Ump::from(Message::midi2(
            at(),
            Voice2::note_on(NoteNumber::of(note), NoteVelocity::new(velocity)),
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

    /// A SysEx8 Start (M2-104 section 7.8: status 1, the stream id counted in the
    /// byte count) of `stream`, one data byte.
    fn sysex8_start(stream: u8) -> Ump {
        Ump([
            0x5000_0000 | 1 << 20 | 2 << 16 | u32::from(stream) << 8 | 0x55,
            0,
            0,
            0,
        ])
    }

    /// The GM System On a device resets on (`7E 7F 09 01`).
    fn gm_on() -> Ump {
        sysex(SysexStatus::Complete, &[0x7E, 0x7F, 0x09, 0x01])
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

    /// M2-104 Appendix D.3 with M2-115 section 3: a 7-bit velocity reaches 16
    /// bits by min-center-max
    /// scaling, 64 at the center 0x8000, 127 at the top, and a velocity of 1
    /// at 1 << 9.
    #[test]
    fn a_midi1_note_on_is_kept_at_its_midi2_velocity() {
        let mut record = Record::default();
        let lane = GroupChannelIndex::at(Protocol::Midi1, at()).unwrap();
        for (v, want) in [(64u8, 0x8000u16), (127, 0xFFFF), (1, 0x0200)] {
            record.observe(on1(60, v));
            let table = record.notes[lane.index() / CHANNELS].as_deref().unwrap();
            assert_eq!(
                table[cell(lane, NoteNumber::of(60))],
                Some((NoteVelocity::new(want), Attribute::None)),
                "7-bit {v}"
            );
        }
    }

    /// A key sent in MIDI 1.0 is caught up in MIDI 1.0 (velocity 64 is 64 again),
    /// one sent in MIDI 2.0 keeps its 16-bit velocity and attribute.
    #[test]
    fn a_held_key_is_caught_up_in_the_protocol_it_started_in() {
        let on = Ump::from(Message::midi2(
            Address::first(MidiChannel::of(1)),
            Voice2::NoteOn {
                note: NoteNumber::of(61),
                velocity: NoteVelocity::new(0x1234),
                attribute: Attribute::Manufacturer(0xBEEF),
            },
        ));
        let record = observed(&[on1(60, 64), on]);
        assert_eq!(record.catch_up(), [on1(60, 64), on]);
    }

    /// A MIDI 1.0 key and a MIDI 2.0 key on one channel and note are two keys,
    /// each with its own note-on.
    #[test]
    fn the_two_protocols_keep_their_keys_apart() {
        let record = observed(&[on1(60, 64), on2(60, 0x1234)]);
        assert_eq!(record.catch_up(), [on1(60, 64), on2(60, 0x1234)]);
    }

    /// A key that ended is not caught up, a Note On at velocity 0 included.
    #[test]
    fn a_key_that_ended_is_not_caught_up() {
        let record = observed(&[on1(60, 90), off1(60), on1(61, 90), on1(62, 90), on1(63, 0)]);
        assert_eq!(record.catch_up(), [on1(61, 90), on1(62, 90)]);
    }

    /// All Sound Off, All Notes Off and the channel modes (Omni Off, Omni On,
    /// Mono, Poly: CC 120, 123 to 127) end the channel's keys; a channel mode's
    /// value is kept as a controller.
    #[test]
    fn mode_messages_end_the_channels_keys() {
        for mode in [120, 123] {
            let record = observed(&[on1(61, 90), cc1(mode, 0)]);
            assert_eq!(record.catch_up(), [], "CC {mode}");
        }
        for mode in 124..=127 {
            let record = observed(&[on1(61, 90), cc1(mode, 1)]);
            assert_eq!(record.catch_up(), [cc1(mode, 1)], "CC {mode}");
        }
    }

    /// Portamento Control, the Velocity Prefix and Data Increment and Decrement
    /// are events, not values.
    #[test]
    fn controllers_that_are_events_are_not_kept() {
        let record = observed(&[cc1(84, 1), cc1(88, 2), cc1(96, 0), cc1(97, 0), cc1(7, 9)]);
        assert_eq!(record.catch_up(), [cc1(7, 9)]);
    }

    /// RP-015: Reset All Controllers resets modulation, expression, the pedals,
    /// bend, pressure and the parameter select, and leaves the program, volume,
    /// pan and the other controllers as they were.
    #[test]
    fn reset_all_controllers_forgets_exactly_what_rp015_resets() {
        let record = observed(&[
            program1(5),
            cc1(7, 90),
            cc1(10, 30),
            cc1(1, 64),
            cc1(11, 40),
            cc1(64, 127),
            bend1(0x3000),
            pressure1(9),
            cc1(121, 0),
        ]);
        assert_eq!(record.catch_up(), [program1(5), cc1(7, 90), cc1(10, 30)]);
        let record = observed(&[cc2(7, 5), cc2(1, 9), cc2(121, 0)]);
        assert_eq!(record.catch_up(), [cc2(7, 5)], "MIDI 2.0 alike");
    }

    /// After a reset the parameter select is the Null pair: a data entry with no
    /// new select is stored under no parameter, a parameter value before it
    /// stays, and the joiner is left on Null.
    #[test]
    fn a_data_entry_after_a_reset_has_no_select() {
        let record = observed(&[cc1(101, 0), cc1(100, 0), cc1(6, 2), cc1(121, 0), cc1(6, 9)]);
        assert_eq!(
            record.catch_up(),
            [
                cc1(101, 0),
                cc1(100, 0),
                cc1(6, 2),
                cc1(101, 127),
                cc1(100, 127)
            ]
        );
    }

    /// The order: SysEx, then per channel the bank and program, controllers,
    /// bend and pressure, then the held keys.
    #[test]
    fn a_catch_up_sends_setup_then_channel_state_then_keys() {
        let volume_change = sysex(SysexStatus::Complete, &[0x7F, 0x7F, 0x04, 0x01, 0, 0x7F]);
        let record = observed(&[
            on1(60, 90),
            pressure1(9),
            bend1(0x3000),
            cc1(32, 2),
            cc1(0, 1),
            program1(5),
            cc1(7, 90),
            volume_change,
        ]);
        assert_eq!(
            record.catch_up(),
            [
                volume_change,
                cc1(0, 1),
                cc1(32, 2),
                program1(5),
                cc1(7, 90),
                bend1(0x3000),
                pressure1(9),
                on1(60, 90),
            ]
        );
    }

    /// MIDI 1.0 Detailed Specification, Bank Select: a bank takes effect at the
    /// next Program Change. The bank in force at the program replays before it,
    /// the bank sent since replays after it.
    #[test]
    fn a_bank_sent_after_the_program_replays_after_it() {
        let record = observed(&[cc1(0, 0), program1(5), cc1(0, 1)]);
        assert_eq!(record.catch_up(), [cc1(0, 0), program1(5), cc1(0, 1)]);
        let record = observed(&[cc1(0, 0), program1(5), cc1(0, 1), program1(6)]);
        assert_eq!(record.catch_up(), [cc1(0, 1), program1(6)]);
    }

    /// M2-104 section 7.4.9: a Program Change with its Bank Valid bit clear keeps
    /// the receiver's bank, so the last valid bank is carried into the program
    /// replayed; with none ever valid there is none.
    #[test]
    fn a_midi2_program_change_without_a_bank_keeps_the_last_valid_one() {
        let bank = Bank {
            msb: U7::of(1),
            lsb: U7::of(2),
        };
        let with_bank = program2(4, Some(bank));
        assert_eq!(observed(&[with_bank]).catch_up(), [with_bank]);
        assert_eq!(
            observed(&[with_bank, program2(9, None)]).catch_up(),
            [program2(9, Some(bank))]
        );
        assert_eq!(
            observed(&[program2(9, None)]).catch_up(),
            [program2(9, None)]
        );
    }

    /// A Universal Non-Real Time General MIDI System On erases the channel state
    /// of its Group and is kept itself; another Group's state stays.
    #[test]
    fn a_general_midi_reset_erases_its_groups_channel_state() {
        let record = observed(&[program1(5), cc1(7, 90), on1(60, 90), gm_on()]);
        assert_eq!(record.catch_up(), [gm_on()]);
        for mode in [0x02, 0x03] {
            let off = sysex(SysexStatus::Complete, &[0x7E, 0x7F, 0x09, mode]);
            assert_eq!(
                observed(&[program1(5), off]).catch_up(),
                [off],
                "mode {mode}"
            );
        }
        let other = Ump::from(Message::midi1(
            Address::new(MidiGroup::of(1), CH),
            Voice1::ProgramChange(U7::of(7)),
        ));
        let record = observed(&[other, program1(5), gm_on(), program1(6)]);
        assert_eq!(record.catch_up(), [gm_on(), program1(6), other]);
    }

    /// MIDI 1.0 RPN: the select, then the data entry MSB and LSB, as sent; the
    /// last data under a select replaces the one before; the Null RPN (127/127)
    /// holds no value and is what the joiner ends on.
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
            [
                cc1(101, 0),
                cc1(100, 0),
                cc1(6, 12),
                cc1(38, 10),
                cc1(101, 127),
                cc1(100, 127)
            ]
        );
    }

    /// An NRPN select replaces an RPN one; the joiner ends on the select in force.
    #[test]
    fn an_nrpn_select_replaces_an_rpn_one() {
        let record = observed(&[cc1(101, 0), cc1(6, 1), cc1(99, 3), cc1(98, 4), cc1(6, 5)]);
        assert_eq!(
            record.catch_up(),
            [cc1(99, 3), cc1(98, 4), cc1(6, 5), cc1(99, 3), cc1(98, 4)]
        );
    }

    /// The joiner ends on the stream's selection, not on the last parameter it
    /// replayed: RPN 1 then RPN 0 leaves RPN 0 selected (A), a stream on Null
    /// leaves Null (B), a select with no data is replayed (C).
    #[test]
    fn the_joiner_is_left_on_the_streams_current_select() {
        let a = observed(&[
            cc1(101, 0),
            cc1(100, 1),
            cc1(6, 1),
            cc1(101, 0),
            cc1(100, 0),
            cc1(6, 2),
        ]);
        assert_eq!(
            a.catch_up(),
            [
                cc1(101, 0),
                cc1(100, 0),
                cc1(6, 2),
                cc1(101, 0),
                cc1(100, 1),
                cc1(6, 1),
                cc1(101, 0),
                cc1(100, 0),
            ]
        );
        let b = observed(&[
            cc1(101, 0),
            cc1(100, 1),
            cc1(6, 1),
            cc1(101, 127),
            cc1(100, 127),
        ]);
        assert_eq!(
            b.catch_up(),
            [
                cc1(101, 0),
                cc1(100, 1),
                cc1(6, 1),
                cc1(101, 127),
                cc1(100, 127)
            ]
        );
        let c = observed(&[cc1(99, 4), cc1(98, 5)]);
        assert_eq!(c.catch_up(), [cc1(99, 4), cc1(98, 5)]);
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
        let mut record = observed(&events);
        assert_eq!(record.catch_up(), [b, a[0], a[1]]);
        assert_eq!(record.take_lost(), 0);
    }

    /// A run replaced by a new start, and a continuation or end with no start,
    /// are counted lost.
    #[test]
    fn a_sysex_run_that_never_completes_is_counted() {
        let mut record = observed(&[
            sysex(SysexStatus::Start, &[1]),
            sysex(SysexStatus::Start, &[2]),
            sysex(SysexStatus::Continue, &[3]),
            sysex(SysexStatus::End, &[4]),
        ]);
        assert_eq!(record.take_lost(), 1, "the replaced run");
        record.observe(sysex(SysexStatus::End, &[5]));
        record.observe(sysex(SysexStatus::Continue, &[6]));
        assert_eq!(record.take_lost(), 2, "no start");
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

    /// The packets of the kept messages are bounded, and a run past the bound
    /// is dropped and counted.
    #[test]
    fn sysex_packets_are_bounded() {
        let long = |tag: u8| -> Vec<Ump> {
            let mut run = vec![sysex(SysexStatus::Start, &[tag])];
            run.extend((0..1998).map(|_| sysex(SysexStatus::Continue, &[tag])));
            run.push(sysex(SysexStatus::End, &[tag]));
            run
        };
        let mut record = Record::default();
        for tag in 1..=3 {
            long(tag).into_iter().for_each(|u| record.observe(u));
        }
        assert_eq!(record.catch_up().len(), 2 * 2000, "two of 2000 packets fit");
        assert_eq!(record.take_lost(), 1, "the oldest went");

        let mut run = vec![sysex(SysexStatus::Start, &[1])];
        run.extend((0..MAX_SYSEX_PACKETS + 10).map(|_| sysex(SysexStatus::Continue, &[1])));
        run.push(sysex(SysexStatus::End, &[1]));
        let mut record = observed(&run);
        assert_eq!(
            record.take_lost(),
            1,
            "a run longer than the bound, counted once"
        );
        assert_eq!(record.catch_up(), [], "and not kept");
    }

    /// Past the number of runs assembled at once the oldest is dropped, counted.
    #[test]
    fn pending_sysex_runs_are_bounded() {
        let starts: Vec<Ump> = (0..=MAX_PENDING as u8).map(sysex8_start).collect();
        let mut record = observed(&starts);
        assert_eq!(record.pending.len(), MAX_PENDING);
        assert_eq!(record.take_lost(), 1);
    }

    /// Past the values a record keeps, a new one is dropped and counted, and a
    /// value already kept still updates.
    #[test]
    fn channel_state_is_bounded() {
        let mut record = Record::default();
        for group in MidiGroup::all() {
            for channel in MidiChannel::all() {
                for controller in 1..=127 {
                    let address = Address::new(group, channel);
                    record.observe(Ump::from(Message::midi1(
                        address,
                        Voice1::control_change(CCNumber::of(controller), U7::of(1)),
                    )));
                }
            }
        }
        assert_eq!(record.state.len(), MAX_STATE_ENTRIES);
        assert!(record.take_lost() > 0);
        record.observe(cc1(7, 2));
        assert_eq!(record.take_lost(), 0, "a kept value updates");
        assert!(record.catch_up().contains(&cc1(7, 2)));
    }

    /// Flex setup is kept per Group and address, the last one of each.
    #[test]
    fn flex_setup_is_kept_per_group() {
        let tempo = |group: u8, n: u32| {
            Ump::from(Message::Flex(
                MidiGroup::of(group),
                Flex::SetTempo(FlexTempo::new(n)),
            ))
        };
        let record = observed(&[tempo(0, 1), tempo(1, 5), tempo(0, 2)]);
        assert_eq!(record.catch_up(), [tempo(0, 2), tempo(1, 5)]);
    }
}
