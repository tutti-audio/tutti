//! The MPE zones a peer's block has enabled, as [`ProfileZones`] (M2-120 §3.1, §3.4).
//!
//! [`Dialect`] says which channels of a block form each zone, in
//! `tutti-midi-device`'s terms; [`ProfileZones`] is `tutti-midi-protocol`'s value
//! a sender or a receiver of MPE runs on. The two live here because neither
//! crate names the other.

use tutti_midi_device::{Dialect, Expression};
use tutti_midi_protocol::{PitchBendSensitivity, ProfileZones};

/// The MPE zones `dialect` enables, each at the bend range the Profile starts
/// at (M2-120 §3.5), or `None` for a block with no MPE Profile enabled: the
/// only reason there are none.
#[must_use]
pub fn mpe_zones(dialect: &Dialect) -> Option<ProfileZones> {
    match dialect.expression() {
        Expression::Channelized => None,
        Expression::Mpe(zones) => Some(ProfileZones::new(
            zones,
            PitchBendSensitivity::MPE_PROFILE_DEFAULT,
        )),
    }
}

#[cfg(test)]
mod tests;
