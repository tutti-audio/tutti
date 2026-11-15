//! What a peer's Function Block speaks, derived from what the peer declares.
//!
//! A [`Dialect`] is the one value a sender reads to know what to send to a
//! block: the channel voice protocol the Endpoint has active (M2-104 §7.1.6)
//! and, for MIDI 1.0, whether the block restricts its bandwidth (§7.1.8); whether
//! JR Timestamps are in force (§7.1.6); and whether the MPE Profile is enabled on
//! channels of the block (M2-120 §3.1), with its zones. [`Dialect::of`] derives it from an [`Endpoint`] and
//! the [`ProfileTable`] of the block's MIDI-CI peer; it reads, it sends
//! nothing.
//!
//! The MPE Channel Response (M2-120 §3.6.3) and Optional Features (§3.7) are not
//! in a Dialect: they come from a Profile Specific Data notification and a Profile
//! Details inquiry reply, which the Endpoint and the [`ProfileTable`] do not carry
//! (`tutti-midi-runtime` reads the features).
//!
//! The MPE Profile is enabled by a Set Profile On sent to the Manager Channel
//! with a number of Channels, so a zone is a Manager Channel and a count
//! ([`MpeZoneSpan`]); the channels are those of the Group the MIDI-CI messages
//! travel on, the first Group of the block. Several zones are several enabled
//! Profiles on channels that do not overlap (M2-120 §3.4).

use tutti_ump::{Direction, FunctionBlockId, Midi1Port, MidiChannel, Protocol};

use crate::ci::{CiDeviceId, ProfileChannels, ProfileId, ProfileState, ProfileTable};
use crate::endpoint::Endpoint;

/// How fast a block takes MIDI 1.0 messages (M2-104 §7.1.8, the MIDI 1.0 field
/// of Function Block Info).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Midi1Rate {
    /// No limit declared: the block is an unrestricted MIDI 1.0 port, or not a
    /// MIDI 1.0 port at all and so declares none.
    Unrestricted,
    /// The block is a MIDI 1.0 port whose bandwidth is restricted to 31.25
    /// kb/s, the rate of a MIDI 1.0 cable.
    Restricted,
}

/// The channel voice messages a block is sent: the Protocol of the Endpoint's
/// active stream configuration (M2-104 §7.1.6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChannelVoice {
    /// MIDI 1.0 channel voice messages, at the block's rate.
    Midi1 {
        /// How fast the block takes them.
        rate: Midi1Rate,
    },
    /// MIDI 2.0 channel voice messages.
    Midi2,
}

/// Whether what is sent carries JR Timestamps: the receive direction of the
/// active stream configuration (M2-104 §7.1.6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JrTimestamps {
    /// The Endpoint does not receive them.
    Off,
    /// The Endpoint receives them: the sender prepends them.
    Prepended,
}

tutti_types::ranged! {
    /// The number of Channels of an MPE zone: its Manager Channel and one to
    /// fifteen Member Channels, so `2..=16` (M2-120 §2.1).
    #[derive(Eq, Hash, PartialOrd, Ord)]
    MpeChannels(u8), NotMpeChannels, "a count of MPE channels (2 to 16)",
    |raw| raw >= 2 && raw <= 16
}

/// The channels of one MPE zone: the Manager Channel, which is the lowest, and
/// the Member Channels above it (M2-120 §2.1, §3.1 steps 3 and 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MpeZoneSpan {
    manager: MidiChannel,
    channels: MpeChannels,
}

impl MpeZoneSpan {
    /// The zone of `channels` channels from `manager` up, or `None` when it
    /// would run past channel 16.
    #[must_use]
    pub fn new(manager: MidiChannel, channels: MpeChannels) -> Option<Self> {
        (u16::from(manager.get()) + u16::from(channels.get()) <= 16)
            .then_some(Self { manager, channels })
    }

    /// The Manager Channel.
    #[must_use]
    pub const fn manager(self) -> MidiChannel {
        self.manager
    }

    /// How many channels the zone has, the Manager Channel included.
    #[must_use]
    pub const fn channels(self) -> MpeChannels {
        self.channels
    }

    /// The Member Channels, in order.
    pub fn members(self) -> impl Iterator<Item = MidiChannel> {
        let first = self.manager.get() + 1;
        (first..self.manager.get() + self.channels.get()).filter_map(MidiChannel::new)
    }

    /// The first channel above the zone, `16` for one that ends on channel 16.
    const fn end(self) -> u8 {
        self.manager.get() + self.channels.get()
    }
}

/// The MPE zones enabled on a block: at least one, ordered by Manager Channel,
/// no two sharing a channel (M2-120 §3.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MpeZoneSet(Vec<MpeZoneSpan>);

impl MpeZoneSet {
    /// The zones, lowest Manager Channel first.
    #[must_use]
    pub fn as_slice(&self) -> &[MpeZoneSpan] {
        &self.0
    }

    /// The zone that has `channel`, as its Manager or as a Member.
    #[must_use]
    pub fn zone_of(&self, channel: MidiChannel) -> Option<MpeZoneSpan> {
        self.0
            .iter()
            .copied()
            .find(|z| channel.get() >= z.manager.get() && channel.get() < z.end())
    }
}

/// How a block's channels are used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expression {
    /// No MPE Profile is enabled: channels are channels.
    Channelized,
    /// The MPE Profile is enabled on these zones; the channels outside them are
    /// channels.
    Mpe(MpeZoneSet),
}

/// What to send to one Function Block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dialect {
    block: FunctionBlockId,
    voice: ChannelVoice,
    jr: JrTimestamps,
    expression: Expression,
}

/// Why a block has no dialect to send in, or why what the peer declares for
/// it cannot be a dialect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NoDialect {
    /// The Endpoint has no Function Block with that number.
    #[error("the Endpoint has no Function Block {}", .0.get())]
    UnknownBlock(FunctionBlockId),
    /// The block is declared inactive (M2-104 §7.1.8).
    #[error("Function Block {} is not active", .0.get())]
    Inactive(FunctionBlockId),
    /// The block only transmits.
    #[error("Function Block {} does not receive", .0.get())]
    NotReceiving(FunctionBlockId),
    /// The active Protocol is neither MIDI 1.0 nor MIDI 2.0: a reserved value.
    #[error("the active Protocol {0:?} carries no channel voice messages")]
    UnspokenProtocol(Protocol),
    /// The MPE Profile is enabled on the Group or the Function Block as a
    /// whole: it is a Profile of channels (M2-120 §3.1).
    #[error("the MPE Profile is enabled on {0:?}, not on a channel")]
    MpeNotOnChannel(CiDeviceId),
    /// The MPE Profile is enabled on a channel, but its number of channels is
    /// not known: only a Reply to Profile Inquiry has arrived for it.
    #[error("the number of channels of the MPE Profile on channel {} is not known", .0.get())]
    MpeChannelsUnknown(MidiChannel),
    /// The MPE Profile is enabled with a number of channels no zone has, or
    /// one that runs past channel 16.
    #[error("{} MPE channels from channel {} is not a zone", .channels.get(), .manager.get())]
    MpeNotAZone {
        /// The Manager Channel.
        manager: MidiChannel,
        /// The count the peer reported.
        channels: ProfileChannels,
    },
    /// Two enabled MPE Profiles share a channel (M2-120 §3.4).
    #[error("the MPE zones of channels {} and {} overlap", .first.get(), .second.get())]
    MpeOverlap {
        /// The Manager Channel of the lower zone.
        first: MidiChannel,
        /// The Manager Channel of the zone that begins inside it.
        second: MidiChannel,
    },
}

impl Dialect {
    /// The dialect of Function Block `block` of `endpoint`, with `table` the
    /// Profiles the block's MIDI-CI peer has reported.
    ///
    /// # Errors
    ///
    /// [`NoDialect`] for a block the Endpoint does not have, or has inactive or
    /// transmit-only; for a reserved active Protocol; and for an MPE Profile
    /// whose report is not a zone.
    pub fn of(
        endpoint: &Endpoint,
        block: FunctionBlockId,
        table: &ProfileTable,
    ) -> Result<Self, NoDialect> {
        let declared = endpoint
            .function_block(block)
            .ok_or(NoDialect::UnknownBlock(block))?;
        if !declared.active() {
            return Err(NoDialect::Inactive(block));
        }
        if !matches!(
            declared.direction(),
            Direction::Input | Direction::Bidirectional
        ) {
            return Err(NoDialect::NotReceiving(block));
        }
        let voice = match endpoint.active().protocol {
            Protocol::Midi1 => ChannelVoice::Midi1 {
                rate: match declared.midi1() {
                    Midi1Port::Restricted => Midi1Rate::Restricted,
                    _ => Midi1Rate::Unrestricted,
                },
            },
            Protocol::Midi2 => ChannelVoice::Midi2,
            other => return Err(NoDialect::UnspokenProtocol(other)),
        };
        Ok(Self {
            block,
            voice,
            jr: if endpoint.active().rx_jr {
                JrTimestamps::Prepended
            } else {
                JrTimestamps::Off
            },
            expression: expression(table)?,
        })
    }

    /// The Function Block.
    #[must_use]
    pub const fn block(&self) -> FunctionBlockId {
        self.block
    }

    /// The channel voice messages to send.
    #[must_use]
    pub const fn voice(&self) -> ChannelVoice {
        self.voice
    }

    /// Whether to prepend JR Timestamps.
    #[must_use]
    pub const fn jr_timestamps(&self) -> JrTimestamps {
        self.jr
    }

    /// How the channels are used.
    #[must_use]
    pub const fn expression(&self) -> &Expression {
        &self.expression
    }
}

/// The zones of the MPE Profiles `table` has enabled.
fn expression(table: &ProfileTable) -> Result<Expression, NoDialect> {
    let mut zones = Vec::new();
    for address in table.addresses() {
        let Some(entry) = table.get(address, ProfileId::MPE) else {
            continue;
        };
        if entry.state() != ProfileState::Enabled {
            continue;
        }
        let CiDeviceId::Channel(manager) = address else {
            return Err(NoDialect::MpeNotOnChannel(address));
        };
        let reported = entry
            .channels()
            .ok_or(NoDialect::MpeChannelsUnknown(manager))?;
        let zone = u8::try_from(reported.get())
            .ok()
            .and_then(MpeChannels::new)
            .and_then(|channels| MpeZoneSpan::new(manager, channels))
            .ok_or(NoDialect::MpeNotAZone {
                manager,
                channels: reported,
            })?;
        zones.push(zone);
    }
    // The table lists channels in order, so the zones are in order of Manager.
    for pair in zones.windows(2) {
        if pair[1].manager.get() < pair[0].end() {
            return Err(NoDialect::MpeOverlap {
                first: pair[0].manager,
                second: pair[1].manager,
            });
        }
    }
    Ok(if zones.is_empty() {
        Expression::Channelized
    } else {
        Expression::Mpe(MpeZoneSet(zones))
    })
}

#[cfg(test)]
mod tests;
