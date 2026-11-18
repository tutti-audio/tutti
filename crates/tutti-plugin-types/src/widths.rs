//! The audio widths a plugin accepts, and the one a request settles on.

use tutti_buffers::SampleFormat;

/// The float widths a plugin processes audio in: 32-bit always, 64-bit only
/// when the plugin declares it.
///
/// [`Loaded::widths`](crate::Loaded::widths) reports it before activation, so
/// a caller that hosts at one width reads whether the plugin takes it, and
/// [`negotiate`](Self::negotiate) is the one rule that turns a request into
/// the width an activation runs at.
///
/// ```
/// use tutti_buffers::SampleFormat;
/// use tutti_plugin_types::Widths;
///
/// assert_eq!(Widths::BOTH.negotiate(SampleFormat::F64), SampleFormat::F64);
/// assert_eq!(Widths::F32.negotiate(SampleFormat::F64), SampleFormat::F32);
/// assert!(Widths::F32.accepts(SampleFormat::F32));
/// assert!(!Widths::F32.accepts(SampleFormat::F64));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    /// Whether the plugin processes audio in `format`. An integer format is
    /// not an audio width of any plugin.
    #[must_use]
    pub const fn accepts(self, format: SampleFormat) -> bool {
        match format {
            SampleFormat::F32 => true,
            SampleFormat::F64 => self.f64,
            _ => false,
        }
    }

    /// The format an activation runs at when `requested` is asked of this
    /// plugin: the request when it is a width the plugin accepts, else 32-bit,
    /// the width every format processes. A plugin's capability alone never
    /// raises a 32-bit request.
    #[must_use]
    pub const fn negotiate(self, requested: SampleFormat) -> SampleFormat {
        match requested {
            SampleFormat::F64 if self.f64 => SampleFormat::F64,
            _ => SampleFormat::F32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole table: 64-bit only when asked for and accepted; a plugin's
    /// capability alone never raises a 32-bit request; an integer format
    /// settles on 32-bit.
    #[test]
    fn negotiation_table() {
        use SampleFormat::{F32, F64, I16};
        for (widths, requested, want) in [
            (Widths::F32, F32, F32),
            (Widths::BOTH, F32, F32),
            (Widths::F32, F64, F32),
            (Widths::BOTH, F64, F64),
            (Widths::BOTH, I16, F32),
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
        assert!(!Widths::BOTH.accepts(I16));
    }
}
