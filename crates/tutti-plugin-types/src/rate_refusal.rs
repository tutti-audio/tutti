//! [`RateRefusal`]: a plugin refused the sample rate the host asked it to run at.

use thiserror::Error;
use tutti_units::SampleRate;

/// What a refused rate left the plugin as, and the plugin's own words for why.
///
/// A refusal is not "keep the old rate": the plugin renders silence until a
/// rate it takes, so the arm says what state it was left in. The `reason` text
/// is the format's own message, shown and never parsed.
#[derive(Error, Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum RateRefusalCause {
    /// The plugin has no configuration to change: it was never prepared.
    #[error("the plugin has no configuration to change")]
    Unconfigured,
    /// The change was refused and rolled back: the plugin runs at `running`.
    #[error("the plugin is left running at {running}: {reason}")]
    RolledBack {
        /// The rate the plugin runs at.
        running: SampleRate,
        /// The plugin's own message.
        reason: String,
    },
    /// The change was refused and the rollback failed too: the plugin is left
    /// deactivated.
    #[error("the plugin is left deactivated: {reason}")]
    Deactivated {
        /// The plugin's own message.
        reason: String,
    },
    /// The plugin did not answer the rate change within `within`; its late
    /// answer settles the state when it comes.
    #[error("the plugin did not answer the rate change within {within:?}")]
    NoAnswer {
        /// How long the host waited.
        within: std::time::Duration,
    },
}

/// A plugin refused the sample rate it was asked to run at.
#[derive(Error, Debug, Clone, PartialEq)]
#[error("the plugin refused sample rate {rate}: {cause}")]
pub struct RateRefusal {
    /// The rate that was refused.
    pub rate: SampleRate,
    /// What the refusal left the plugin as.
    #[source]
    pub cause: RateRefusalCause,
}

impl RateRefusal {
    /// The refusal of `rate` for `cause`.
    #[must_use]
    pub fn new(rate: SampleRate, cause: RateRefusalCause) -> Self {
        Self { rate, cause }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A cause from a newer peer this build does not know is a decode
    /// failure**, never a different cause: the wire's variant index is the
    /// arm.
    #[cfg(feature = "serde")]
    #[test]
    fn an_unknown_cause_from_a_newer_peer_fails_to_decode() {
        let unknown = [0x7f_u8];
        assert!(postcard::from_bytes::<RateRefusalCause>(&unknown).is_err());
    }
}
