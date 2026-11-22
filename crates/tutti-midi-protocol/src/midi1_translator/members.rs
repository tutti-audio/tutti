//! MPE note allocation: the per-note messages of one Group as MIDI 1.0 channel
//! messages on the member channels of a zone (M2-104 D.4).

use alloc::boxed::Box;
use core::cmp::Reverse;

use tutti_ump::{
    Address, CCNumber, ExtremeValues, Midi1Bytes, MidiChannel, MidiGroup, U7, U14, Voice1, Voice2,
};

use crate::MpeZoneConfig;

/// One member channel's allocation state.
#[derive(Clone, Copy, Debug)]
struct Member {
    /// How many notes the channel plays.
    notes: u16,
    /// The tick the channel was last given a note: of the least loaded, the
    /// oldest takes the next note.
    started: u32,
    /// The tick the channel was last freed: the longest idle is reused first.
    released: u32,
    /// A bend, pressure or Control Change 74 was sent since the channel was
    /// idle, so the next note on an idle channel starts from a reset one.
    bent: bool,
    pressed: bool,
    slid: bool,
}

impl Member {
    const IDLE: Self = Self {
        notes: 0,
        started: 0,
        released: 0,
        bent: false,
        pressed: false,
        slid: false,
    };
}

/// How many note numbers a source channel has.
const NOTES: usize = 128;

/// One Group's sixteen member channels and the channel of each note, keyed by
/// the channel the note came from and its number.
#[derive(Clone, Debug)]
struct GroupMembers {
    channels: [Member; MidiChannel::COUNT as usize],
    owner: [[Option<MidiChannel>; NOTES]; MidiChannel::COUNT as usize],
}

impl GroupMembers {
    const IDLE: Self = Self {
        channels: [Member::IDLE; MidiChannel::COUNT as usize],
        owner: [[None; NOTES]; MidiChannel::COUNT as usize],
    };
}

/// The channels of every Group, one table each on the heap: built by
/// [`new`](Self::new) and emptied in place by [`clear`](Self::clear), so a
/// translator moves a pointer and the audio thread never allocates.
#[derive(Clone, Debug)]
pub(super) struct Members {
    groups: Box<[GroupMembers]>,
    tick: u32,
}

impl Members {
    pub(super) fn new() -> Self {
        Self {
            groups: (0..MidiGroup::COUNT).map(|_| GroupMembers::IDLE).collect(),
            tick: 0,
        }
    }

    /// Forgets every note, in place.
    pub(super) fn clear(&mut self) {
        self.groups.fill(GroupMembers::IDLE);
        self.tick = 0;
    }

    /// Sends `voice` as the MPE messages it stands for, and returns whether it
    /// was one this allocation handles: a note or a per-note message.
    ///
    /// A new note takes a channel that plays none, the one idle longest. With
    /// none idle it shares a channel (M2-120 section 3.6.1), the one that
    /// plays the fewest notes and of those the one given a note longest ago;
    /// the section leaves the choice to the sender.
    pub(super) fn route(
        &mut self,
        zone: &MpeZoneConfig,
        extremes: ExtremeValues,
        address: Address,
        voice: Voice2,
        out: &mut impl FnMut(Midi1Bytes),
    ) -> bool {
        let GroupMembers { channels, owner } = &mut self.groups[address.group.index()];
        let source = &mut owner[address.channel.index()];
        let mut send = |channel: MidiChannel, voice: Voice1| out(voice.bytes(channel));
        match voice {
            Voice2::NoteOn { note, .. } => {
                self.tick = self.tick.wrapping_add(1);
                let tick = self.tick;
                let held = source[usize::from(note.get())];
                if let Some(held) = held {
                    // The same key struck again ends its earlier note.
                    channels[held.index()].notes -= 1;
                    send(
                        held,
                        Voice1::NoteOff {
                            note,
                            velocity: U7::of(0),
                        },
                    );
                }
                let member = held.unwrap_or_else(|| {
                    let load = |c: &MidiChannel| channels[c.index()].notes;
                    let idle = zone
                        .member_channels()
                        .filter(|c| load(c) == 0)
                        .min_by_key(|c| Reverse(tick.wrapping_sub(channels[c.index()].released)));
                    idle.unwrap_or_else(|| {
                        let key = |c: &MidiChannel| {
                            (
                                load(c),
                                Reverse(tick.wrapping_sub(channels[c.index()].started)),
                            )
                        };
                        zone.member_channels().fold(zone.first_member(), |best, c| {
                            if key(&c) < key(&best) { c } else { best }
                        })
                    })
                });
                let slot = &mut channels[member.index()];
                // A channel that plays a note keeps its controllers: a reset
                // would move that note.
                if slot.notes == 0 {
                    if slot.bent {
                        send(member, Voice1::PitchBend(U14::of(0x2000)));
                    }
                    if slot.pressed {
                        send(member, Voice1::ChannelPressure(U7::of(0)));
                    }
                    // M1-100 A.4.3 (Relative): Control Change 74 at a Note On is
                    // 0x40, and its section 2.2 example sends 0x40 as the initial
                    // timbre; M2-120 section 5 asks for initial values and gives none.
                    if slot.slid {
                        send(
                            member,
                            Voice1::ControlChange {
                                controller: CCNumber::BRIGHTNESS,
                                value: U7::of(0x40),
                            },
                        );
                    }
                    slot.bent = false;
                    slot.pressed = false;
                    slot.slid = false;
                }
                slot.notes += 1;
                slot.started = tick;
                source[usize::from(note.get())] = Some(member);
                if let Ok(on) = Voice1::from_voice2(voice, extremes) {
                    send(member, on);
                }
            }
            Voice2::NoteOff { note, .. } => {
                if let Some(member) = source[usize::from(note.get())].take() {
                    let slot = &mut channels[member.index()];
                    slot.notes -= 1;
                    slot.released = self.tick;
                    if let Ok(off) = Voice1::from_voice2(voice, extremes) {
                        send(member, off);
                    }
                }
            }
            Voice2::PerNotePitchBend { note, value } => {
                if let Some(member) = source[usize::from(note.get())] {
                    channels[member.index()].bent = true;
                    if let Ok(bend) = Voice1::from_voice2(Voice2::PitchBend(value), extremes) {
                        send(member, bend);
                    }
                }
            }
            Voice2::PolyPressure { note, value } => {
                if let Some(member) = source[usize::from(note.get())] {
                    channels[member.index()].pressed = true;
                    if let Ok(pressure) =
                        Voice1::from_voice2(Voice2::ChannelPressure(value), extremes)
                    {
                        send(member, pressure);
                    }
                }
            }
            Voice2::AssignablePerNote {
                note,
                controller,
                value,
            } if controller == CCNumber::BRIGHTNESS.get() => {
                // Index 74 is the MPE convention for the third dimension of
                // control (M2-120 section 4.5), the one an MPE receiver
                // makes of Control Change 74 on a member channel.
                if let Some(member) = source[usize::from(note.get())] {
                    channels[member.index()].slid = true;
                    let slide = Voice2::ControlChange {
                        controller: CCNumber::BRIGHTNESS,
                        value,
                    };
                    if let Ok(slide) = Voice1::from_voice2(slide, extremes) {
                        send(member, slide);
                    }
                }
            }
            Voice2::RegisteredPerNote { .. }
            | Voice2::AssignablePerNote { .. }
            | Voice2::PerNoteManagement { .. } => {}
            _ => return false,
        }
        true
    }
}
