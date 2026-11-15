//! A plugin's own bypass: where it lives ([`OwnBypass`]) and what a write of it
//! came to ([`BypassWrite`]).

use crate::{Normalized, ParamFlags, ParamId, ParameterInfo};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Where a plugin keeps a bypass of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum OwnBypass {
    /// A parameter the plugin flags as its bypass (VST3 `kIsBypass`, CLAP
    /// `CLAP_PARAM_IS_BYPASS`): `1` bypassed, `0` processing. Readable.
    Parameter(ParamId),
    /// Not a parameter: AU's `kAudioUnitProperty_BypassEffect`, VST2's
    /// `effSetBypass`. Written by the format's host, not read back.
    Unlisted,
}

/// What a write of the plugin's own bypass came to.
///
/// [`Sent`](Self::Sent) is not "applied": a plugin may refuse or lose a write
/// it was handed, and only reading the bypass back tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[must_use = "a lost write leaves the plugin in the other state"]
pub enum BypassWrite {
    /// The plugin has no bypass of its own; nothing was written.
    NoOwnBypass,
    /// The write was handed to the plugin.
    Sent,
    /// The write did not reach the plugin, or the plugin refused it: the
    /// queue was full, the plugin is gone, the main thread did not answer in
    /// time, or the format said no.
    Lost,
}

/// The parameter a plugin lists as its own bypass, if it lists one: the one
/// place that reads the [`BYPASS`](ParamFlags::BYPASS) flag.
#[must_use]
pub fn bypass_parameter(list: &[ParameterInfo]) -> Option<ParamId> {
    list.iter()
        .find(|p| p.flag(ParamFlags::BYPASS) == Some(true))
        .map(ParameterInfo::id)
}

/// A bypass parameter's value: `1` bypassed, `0` processing.
#[must_use]
pub fn bypass_value(bypassed: bool) -> Normalized {
    Normalized::clamped(if bypassed { 1.0 } else { 0.0 })
}

/// Whether a bypass parameter's value says bypassed.
#[must_use]
pub fn is_bypassed(value: Normalized) -> bool {
    value.get() >= 0.5
}
