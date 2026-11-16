//! MPE note allocation: the per-note messages of one Group as MIDI 1.0 channel
//! messages on the member channels of a zone (M2-104 D.4).

use core::cmp::Reverse;

use tutti_ump::{
    Address, CCNumber, Midi1Bytes, MidiChannel, MidiGroup, NoteNumber, U7, U14, Voice1, Voice2,
};

use crate::{ExtremeValues, MpeZoneConfig, narrow_bend, narrow_value};

/// A note a member channel plays: the channel it was sent on and its number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Held {
    source: MidiChannel,
    note: NoteNumber,
}

/// One member channel's allocation state.
#[derive(Clone, Copy, Debug, Default)]
struct Member {
    held: Option<Held>,
    /// The tick the channel was last given a note: the oldest is stolen.
    started: u32,
    /// The tick the channel was last freed: the longest idle is reused first.
    released: u32,
    /// A bend or pressure was sent since the last Note On, so the next note
    /// starts from a reset one.
    bent: bool,
    pressed: bool,
}

/// The sixteen channels of every Group.
#[derive(Clone, Debug)]
pub(super) struct Members {
    channels: [[Member; MidiChannel::COUNT as usize]; MidiGroup::COUNT as usize],
    tick: u32,
}

impl Members {
    pub(super) const fn new() -> Self {
        Self {
            channels: [[Member {
                held: None,
                started: 0,
                released: 0,
                bent: false,
                pressed: false,
            }; MidiChannel::COUNT as usize]; MidiGroup::COUNT as usize],
            tick: 0,
        }
    }

    /// Sends `voice` as the MPE messages it stands for, and returns whether it
    /// was one this allocation handles: a note or a per-note message.
    pub(super) fn route(
        &mut self,
        zone: &MpeZoneConfig,
        extremes: ExtremeValues,
        address: Address,
        voice: Voice2,
        out: &mut impl FnMut(Midi1Bytes),
    ) -> bool {
        let members = &mut self.channels[address.group.index()];
        let source = address.channel;
        let find = |members: &[Member; 16], note: NoteNumber| {
            zone.member_channels().find(|c| {
                members[c.index()]
                    .held
                    .is_some_and(|h| h.source == source && h.note == note)
            })
        };
        let mut send = |channel: MidiChannel, voice: Voice1| out(voice.bytes(channel));
        match voice {
            Voice2::NoteOn { note, .. } => {
                self.tick = self.tick.wrapping_add(1);
                let tick = self.tick;
                let member = find(members, note).unwrap_or_else(|| {
                    let free = zone
                        .member_channels()
                        .filter(|c| members[c.index()].held.is_none())
                        .min_by_key(|c| Reverse(tick.wrapping_sub(members[c.index()].released)));
                    let chosen = free.unwrap_or_else(|| {
                        zone.member_channels()
                            .min_by_key(|c| Reverse(tick.wrapping_sub(members[c.index()].started)))
                            .unwrap_or(zone.master_channel())
                    });
                    // A member that still plays is stolen: its note ends first.
                    if let Some(old) = members[chosen.index()].held {
                        let off = Voice1::NoteOff {
                            note: old.note,
                            velocity: U7::of(0),
                        };
                        send(chosen, off);
                    }
                    chosen
                });
                let slot = &mut members[member.index()];
                if slot.bent {
                    send(member, Voice1::PitchBend(U14::of(0x2000)));
                }
                if slot.pressed {
                    send(member, Voice1::ChannelPressure(U7::of(0)));
                }
                *slot = Member {
                    held: Some(Held { source, note }),
                    started: tick,
                    bent: false,
                    pressed: false,
                    ..*slot
                };
                if let Ok(on) = extremes.narrow(voice) {
                    send(member, on);
                }
            }
            Voice2::NoteOff { note, .. } => {
                if let Some(member) = find(members, note) {
                    let slot = &mut members[member.index()];
                    slot.held = None;
                    slot.released = self.tick;
                    if let Ok(off) = extremes.narrow(voice) {
                        send(member, off);
                    }
                }
            }
            Voice2::PerNotePitchBend { note, value } => {
                if let Some(member) = find(members, note) {
                    members[member.index()].bent = true;
                    send(member, Voice1::PitchBend(narrow_bend(value)));
                }
            }
            Voice2::PolyPressure { note, value } => {
                if let Some(member) = find(members, note) {
                    members[member.index()].pressed = true;
                    send(member, Voice1::ChannelPressure(narrow_value(value)));
                }
            }
            Voice2::AssignablePerNote {
                note,
                controller,
                value,
            } if controller == CCNumber::BRIGHTNESS.get() => {
                if let Some(member) = find(members, note) {
                    send(
                        member,
                        Voice1::ControlChange {
                            controller: CCNumber::BRIGHTNESS,
                            value: narrow_value(value),
                        },
                    );
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
