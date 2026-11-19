//! The audio widths a plugin accepts, and the one a request settles on.

use tutti_buffers::FloatWidth;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// The float widths a plugin processes audio in: 32-bit always, 64-bit only
/// when the plugin declares it.
///
/// [`Loaded::widths`](crate::Loaded::widths) reports it before activation, so
/// a caller that hosts at one width reads whether the plugin takes it, and
/// [`negotiate`](Self::negotiate) is the one rule that turns a request into
/// the width an activation runs at.
///
/// ```
/// use tutti_buffers::FloatWidth;
/// use tutti_plugin_types::Widths;
///
/// assert_eq!(Widths::BOTH.negotiate(FloatWidth::F64), FloatWidth::F64);
/// assert_eq!(Widths::F32.negotiate(FloatWidth::F64), FloatWidth::F32);
/// assert!(Widths::F32.accepts(FloatWidth::F32));
/// assert!(!Widths::F32.accepts(FloatWidth::F64));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Widths {
    f64: bool,
}

impl Widths {
    /// A plugin that processes 32-bit audio only.
    pub const F32: Self = Self { f64: false };
    /// A plugin that processes 32-bit and 64-bit audio.
    pub const BOTH: Self = Self { f64: true };

    /// The widths of a plugin that does (`true`) or does not process 64-bit
    /// audio.
    #[must_use]
    pub const fn new(supports_f64: bool) -> Self {
        Self { f64: supports_f64 }
    }

    /// Whether the plugin processes audio in `width`.
    #[must_use]
    pub const fn accepts(self, width: FloatWidth) -> bool {
        match width {
            FloatWidth::F32 => true,
            FloatWidth::F64 => self.f64,
        }
    }

    /// The width an activation runs at when `requested` is asked of this
    /// plugin: the request when the plugin accepts it, else 32-bit, the width
    /// every format processes. A plugin's capability alone never raises a
    /// 32-bit request.
    #[must_use]
    pub const fn negotiate(self, requested: FloatWidth) -> FloatWidth {
        match requested {
            FloatWidth::F64 if self.f64 => FloatWidth::F64,
            FloatWidth::F64 | FloatWidth::F32 => FloatWidth::F32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole table: 64-bit only when asked for and accepted; a plugin's
    /// capability alone never raises a 32-bit request.
    #[test]
    fn negotiation_table() {
        use FloatWidth::{F32, F64};
        for (widths, requested, want) in [
            (Widths::F32, F32, F32),
            (Widths::BOTH, F32, F32),
            (Widths::F32, F64, F32),
            (Widths::BOTH, F64, F64),
        ] {
            assert_eq!(
                widths.negotiate(requested),
                want,
                "{widths:?} {requested:?}"
            );
            assert!(
                widths.accepts(want),
                "{widths:?} accepts what it settles on"
            );
        }
    }
}
