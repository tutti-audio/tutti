//! A plugin's own bypass: where it lives ([`OwnBypass`]) and what a write of it
//! came to ([`BypassWrite`]).

use crate::{ParamFlags, ParamWriteError, ParameterInfo};
use tutti_units::Normalized;
use tutti_units::ParamId;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Where a plugin keeps a bypass of its own: only a bypass the host can read
/// back is claimed, so a write is never assumed applied. VST2's `effSetBypass`
/// has no read, so a VST2 plugin claims none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum OwnBypass {
    /// A parameter the plugin flags as its bypass (VST3 `kIsBypass`, CLAP
    /// `CLAP_PARAM_IS_BYPASS`): `1` bypassed, `0` processing. Readable.
    Parameter(ParamId),
    /// A property of the plugin that is not a parameter: AU's
    /// `kAudioUnitProperty_BypassEffect`. The format's host writes and reads it
    /// ([`Control::set_bypassed`](crate::Control::set_bypassed),
    /// [`Control::bypassed`](crate::Control::bypassed)), so it is claimed only
    /// where the host reaches the plugin on the main thread (not served).
    Property,
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
    /// The write was handed to the plugin. The plugin may still not apply it.
    Sent,
    /// The write did not reach the plugin, or the format refused it: a served
    /// plugin's command queue was full or its link gone, an in-process
    /// plugin's instance was busy or the main thread did not answer in time,
    /// or an AU refused the property.
    Lost,
    /// The plugin refused the write at the call: its bypass parameter is not
    /// declared, or is declared read-only
    /// ([`ParamWriteError::Undeclared`], [`ParamWriteError::ReadOnly`]). The
    /// plugin is at fault, not the link: writing it again changes nothing.
    Refused,
}

impl BypassWrite {
    /// What a write of the bypass parameter came to: handed over, refused by
    /// the write policy, or not made because the plugin was lost or did not
    /// answer.
    pub fn of_write(write: Result<(), ParamWriteError>) -> Self {
        match write {
            Ok(()) => Self::Sent,
            Err(ParamWriteError::Undeclared(_) | ParamWriteError::ReadOnly(_)) => Self::Refused,
            Err(ParamWriteError::Lost(_) | ParamWriteError::Unanswered(_)) => Self::Lost,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// **A write's outcome maps by cause: handed over is `Sent`, a refusal of
    /// the write policy is `Refused`, a lost or unanswering plugin is `Lost`.**
    #[test]
    fn of_write_maps_each_write_error_by_its_cause() {
        let id = ParamId::new(5);
        assert_eq!(BypassWrite::of_write(Ok(())), BypassWrite::Sent);
        assert_eq!(
            BypassWrite::of_write(Err(ParamWriteError::Undeclared(id))),
            BypassWrite::Refused
        );
        assert_eq!(
            BypassWrite::of_write(Err(ParamWriteError::ReadOnly(id))),
            BypassWrite::Refused
        );
        assert_eq!(
            BypassWrite::of_write(Err(ParamWriteError::Lost(id))),
            BypassWrite::Lost
        );
        assert_eq!(
            BypassWrite::of_write(Err(ParamWriteError::Unanswered(id))),
            BypassWrite::Lost
        );
    }
}
