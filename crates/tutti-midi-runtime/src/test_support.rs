//! What the unit tests share.

use tutti_graph::{Offset, UmpAt};
use tutti_ump::Ump;
use tutti_units::Samples;

/// `ump` at frame `offset`, built against a block long enough to hold any offset a
/// test names.
pub(crate) fn at(offset: usize, ump: Ump) -> UmpAt {
    (Offset::of(Samples(offset), Samples(1 << 20)), ump)
}
