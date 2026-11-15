//! [`MaxDecay`]: the longest decay a reverb is built to.

use tutti_types::Seconds;

/// The longest decay a reverb's control may ask for.
///
/// A value is finite, above zero and at most [`CAP`](Self::CAP): a network sized from
/// it becomes a reverb's allocations, so it is capped (a 60 s decay at
/// 192 kHz is already some 11.5 M frames of line to hold).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaxDecay(Seconds);

/// A refused longest decay: the raw it carries.
#[derive(Clone, Copy, Debug, PartialEq, thiserror::Error)]
#[error("{0:?} is not a longest decay: it must be above zero and at most {cap:?}", cap = MaxDecay::CAP)]
pub struct MaxDecayError(pub Seconds);

impl MaxDecay {
    /// The longest decay a control may ask for by default.
    pub const DEFAULT: MaxDecay = MaxDecay(Seconds::new(8.0));
    /// The most [`new`](Self::new) accepts.
    pub const CAP: Seconds = Seconds::new(60.0);

    /// A longest decay of `seconds`, or `None` when it is not finite, not
    /// above zero or above [`CAP`](Self::CAP). Never clamps.
    #[must_use]
    pub fn new(seconds: Seconds) -> Option<Self> {
        let s = seconds.get();
        (s.is_finite() && s > 0.0 && s <= Self::CAP.get()).then_some(Self(seconds))
    }

    /// The longest decay.
    #[must_use]
    pub fn get(self) -> Seconds {
        self.0
    }
}

impl TryFrom<Seconds> for MaxDecay {
    type Error = MaxDecayError;

    fn try_from(seconds: Seconds) -> Result<Self, MaxDecayError> {
        Self::new(seconds).ok_or(MaxDecayError(seconds))
    }
}

impl From<MaxDecay> for Seconds {
    fn from(decay: MaxDecay) -> Seconds {
        decay.0
    }
}

#[cfg(test)]
mod tests;
