//! MIDI Polyphonic Expression (MPE, RP-053): the zone layout of an MPE
//! instrument and the MPE Configuration Message that declares it.
//!
//! MPE spreads one instrument's notes across several MIDI 1.0 channels so each
//! note gets its own pitch bend, pressure and timbre, the expression MIDI 1.0
//! otherwise only has per *channel*. A [`Mpe11Zone`] is one MPE 1.1 zone,
//! [`Mpe11Zones`] the one or two an instrument occupies, and [`MpeMode`]
//! whether MPE is in play at all. The two bend ranges the channel roles carry
//! are [`PitchBendSensitivity`](crate::PitchBendSensitivity)s. Every type
//! refuses what RP-053 does at its constructor, and a persisted one is read
//! back through the same check.
//!
//! The MIDI-CI MPE Profile (M2-120) replaces the Configuration Message with a
//! zone per enabled Profile: a [`ProfileZone`] and [`ProfileZones`], with the
//! Profile's two Specific Data values ([`ChannelResponse`], [`MpeFeatures`]).
//! A zone of either kind hands out the [`MpeLayout`] of its channels.
//!
//! These are values: the state that drives a zone from incoming MIDI (which
//! member channel holds which note) belongs to the receiver.

mod expression;
mod profile;
mod zone;

pub use expression::MpeExpression;
pub use profile::{ChannelResponse, ControllerSupport, MpeFeatures, PitchBendSupport};
pub use zone::InvalidMpeZones;
#[cfg(feature = "serde")]
pub use zone::{InvalidMpeLayout, InvalidProfileZone};
pub use zone::{
    Mcm, MemberCount, Mpe11Zone, Mpe11Zones, MpeLayout, MpeMode, MpeZone, NotMemberCount,
    ProfileZone, ProfileZones,
};
