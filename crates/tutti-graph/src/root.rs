//! The graph root's placement on the world outside it: a width is not a
//! topology, so how a root of known or bare channels reaches a device or a
//! file of which only the width is known lives here, once.

use tutti_buffers::{Fold, FoldEnd, FoldError};
use tutti_types::{ChannelLayout, ChannelTopology, Speaker};

/// Why the graph root cannot be widened or placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RootError {
    /// A graph root's channels were declared at one width and the root is, or
    /// was asked to be, another: the declaration owns the root's width.
    #[error("the root is declared {declared:?} wide and cannot be {requested:?}")]
    Width {
        /// The width the root was declared with.
        declared: ChannelLayout,
        /// The width the root has, or was asked to widen to.
        requested: ChannelLayout,
    },
    /// The root's channels cannot be placed.
    #[error(transparent)]
    Fold(#[from] FoldError),
}

/// Builds the fold from `src` into a destination of which only the width is
/// known: a device, a file, a node's outputs.
///
/// The destination is taken to be the engine's own order for its width
/// ([`ChannelTopology::assumed`]): mono, stereo, quad, 5.1, 7.1 and 7.1.4. A
/// mono source lands on the centre of a 6, 8 or 12-wide one and the front pair
/// of a 2 or 4-wide one. The source is placed by speaker, except where a
/// speaker cannot be named:
///
/// - a source with an [`Unknown`](Speaker::Unknown) channel (a second LFE, a
///   wide or high speaker) is [`FoldError::Unnamed`], whatever the
///   destination: nothing can be said about where it goes;
/// - a source whose channels are all [`Unassigned`](Speaker::Unassigned) is
///   independent buses, so channel `i` stays channel `i` ([`Fold::by_index`]):
///   a wider destination is zero-filled and a narrower one is an error, never
///   a matrix. A caller that reads a bare count in the engine's order says so
///   with [`ChannelTopology::assumed`] before it calls;
/// - a destination width with no order (3, 5, 7, 9 to 11, 13 and up) names no
///   speaker, so channels go by index there too, and a narrowing is
///   [`FoldError::Unplaced`], never a truncation;
/// - otherwise, [`Fold::between`].
///
/// # Errors
///
/// As [`Fold::between`] and [`Fold::by_index`], and [`FoldError::Unnamed`] as
/// above.
pub fn fold_to_width(src: &ChannelTopology, dst: ChannelLayout) -> Result<Fold, FoldError> {
    if let Some(channel) = src
        .positions()
        .iter()
        .position(|p| matches!(p, Speaker::Unknown(_)))
    {
        return Err(FoldError::Unnamed {
            end: FoldEnd::Source,
            channel,
        });
    }
    let to = ChannelTopology::assumed(dst);
    if src.is_unassigned() || to.is_unassigned() {
        Fold::by_index(src.layout(), dst)
    } else {
        Fold::between(src, &to)
    }
}
