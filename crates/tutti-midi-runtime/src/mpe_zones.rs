//! The MPE zones a peer's block has enabled, as [`MpeZones`] (M2-120 §3.1, §3.4).
//!
//! [`Dialect`] says which channels of a block form each zone, in
//! `tutti-midi-device`'s terms; [`MpeZones`] is `tutti-midi-protocol`'s value
//! a sender or a receiver of MPE runs on. The two live here because neither
//! crate names the other.

use tutti_midi_device::{Dialect, Expression};
use tutti_midi_protocol::{MpeZoneConfig, MpeZones, PitchBendSensitivity};

/// The MPE zones `dialect` enables, each at the bend range the Profile starts
/// at (M2-120 §3.5), or `None` for a block with no MPE Profile enabled.
#[must_use]
pub fn mpe_zones(dialect: &Dialect) -> Option<MpeZones> {
    let Expression::Mpe(zones) = dialect.expression() else {
        return None;
    };
    let mut configs = zones.as_slice().iter().map(|zone| {
        MpeZoneConfig::profile(
            zone.manager(),
            zone.channels().get(),
            PitchBendSensitivity::MPE_PROFILE_DEFAULT,
        )
    });
    // A span the device crate built is a zone the protocol crate accepts, and
    // its zones do not overlap, so each step succeeds.
    let first = configs.next()??;
    configs.try_fold(MpeZones::single(first), |set, zone| set.with(zone?))
}

#[cfg(test)]
mod tests;
