//! [`BouncePart`] without the `export` feature: no bounce runs, so the collect has nothing to
//! drop or watch. `bounce::BounceState` is this module's twin when export is built.

use tutti_platform::Instant;

/// No bounce is in flight.
#[derive(Debug, Default)]
pub(crate) struct BouncePart;

impl BouncePart {
    /// Nothing came back and nothing stalls.
    pub(crate) fn collect(&mut self, _: Instant) {}
}
