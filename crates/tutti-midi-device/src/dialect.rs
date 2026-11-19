//! What a peer's Function Block speaks, derived from what the peer declares.
//!
//! A [`Dialect`] is the one value a sender reads to know what to send to a
//! block: the channel voice protocol the Endpoint has active (M2-104 §7.1.6.3)
//! and, for MIDI 1.0, whether the block restricts its bandwidth (§7.1.8); whether
//! JR Timestamps are in force (§7.1.6.3); and whether the MPE Profile is enabled on
//! channels of the block (M2-120 §3.1), with its zones. [`Dialect::of`] derives it from an [`Endpoint`] and
//! the [`ProfileTable`] of the block's MIDI-CI peer; it reads, it sends
//! nothing.
//!
//! The MPE Channel Response (M2-120 §3.6.3) and Optional Features (§3.7) are not
//! in a Dialect: they come from a Profile Specific Data notification and a Profile
//! Details inquiry reply, which the Endpoint and the [`ProfileTable`] do not carry
//! (a host reads the features from those).
//!
//! The MPE Profile is enabled by a Set Profile On sent to the Manager Channel
//! with a number of Channels, so a zone is a Manager Channel and a count
//! ([`MpeZoneSpan`]); the channels are those of the Group the MIDI-CI messages
//! travel on, the first Group of the block. Several zones are several enabled
//! Profiles on channels that do not overlap (M2-120 §3.4).

use tutti_ump::{
    BlockDirection, FunctionBlockId, Midi1Port, MidiChannel, MpeChannels, MpeOverlap, MpeZoneSet,
    MpeZoneSpan, Protocol,
};

use crate::endpoint::Endpoint;
use tutti_midi_ci::{CiDeviceId, ProfileChannels, ProfileId, ProfileState, ProfileTable};

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
/// active stream configuration (M2-104 §7.1.6.3).
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
/// active stream configuration (M2-104 §7.1.6.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JrTimestamps {
    /// The Endpoint does not receive them.
    Off,
    /// The Endpoint receives them: the sender prepends them.
    Prepended,
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
    #[error(transparent)]
    MpeOverlap(#[from] MpeOverlap),
}

impl Dialect {
    /// The dialect of Function Block `block` of `endpoint`, with `table` the
    /// Profiles the block's MIDI-CI peer has reported.
    ///
    /// `endpoint` is the peer's view of itself, the Endpoint its Stream
    /// Configuration Notifications describe, so its `rx_jr` is what the peer
    /// receives: the direction this device sends.
    ///
    /// # Errors
    ///
    /// [`NoDialect`] for a block the Endpoint does not have, or has inactive or
    /// transmit-only; for a reserved active Protocol; and for an MPE Profile
    /// whose report is not a zone. One bad MPE report refuses the whole
    /// Dialect: the Profiles may not overlap (M2-120 §3.4), so a report that
    /// breaks that or the zone layout means the peer's channel state is not
    /// what the table says, and no zone of it is safe to send MPE to.
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
            BlockDirection::Input | BlockDirection::Bidirectional
        ) {
            return Err(NoDialect::NotReceiving(block));
        }
        let voice = match endpoint.active().protocol {
            Protocol::Midi1 => ChannelVoice::Midi1 {
                rate: match declared.midi1() {
                    Midi1Port::Restricted => Midi1Rate::Restricted,
                    Midi1Port::Unrestricted | Midi1Port::No | Midi1Port::Reserved(_) => {
                        Midi1Rate::Unrestricted
                    }
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

/// The zones of the MPE Profiles `table` has enabled, strictly: a Profile that
/// makes no zone is an error. A caller that must know the peer is
/// understood (building a [`Dialect`]) wants this reading; a caller that
/// follows what a peer enabled and skips the rest wants
/// [`ProfileTable::mpe_zones`](tutti_midi_ci::ProfileTable::mpe_zones).
fn expression(table: &ProfileTable) -> Result<Expression, NoDialect> {
    let mut zones: Option<MpeZoneSet> = None;
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
        zones = Some(match zones {
            None => MpeZoneSet::single(zone),
            Some(held) => held.with(zone)?,
        });
    }
    Ok(zones.map_or(Expression::Channelized, Expression::Mpe))
}

#[cfg(test)]
mod tests;
