//! The graph's main output's placement on the world outside it: a width is not a
//! topology, so how a main output of known or bare channels reaches a device or a
//! file of which only the width is known lives here, once.

use tutti_buffers::{ChannelLayout, ChannelSet, ChannelTopology, Speaker};
use tutti_buffers::{Float, Fold, FoldEnd, FoldError, FoldLevels};

/// The generation of the graph's main output: each commit that changes the main output's
/// declaration carries the next one.
///
/// The plan carries it ([`Plan::main_output_id`](crate::Plan::main_output_id)), so what
/// depends on the main output's topology (the engine's folds into a device) is looked
/// up by the plan being rendered and swaps with it, and no topology crosses to
/// the audio thread. The first main output, before any declaration, is
/// [`GENESIS`](Self::GENESIS).
///
/// ```
/// use tutti_graph::MainOutputId;
///
/// assert!(MainOutputId::GENESIS < MainOutputId::GENESIS.next());
/// assert_eq!(MainOutputId::GENESIS.next().get(), MainOutputId::GENESIS.get() + 1);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MainOutputId(u64);

impl MainOutputId {
    /// The main output a graph starts with: undeclared, the engine's order for its
    /// width.
    pub const GENESIS: Self = Self(0);

    /// The generation after this one.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }

    /// The generation as a number, for a display or a log.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for MainOutputId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The old main output's outputs a plan renders beside its own while a main output change
/// crossfades.
///
/// A plan that carries one renders `base` channels of the new main output (the main output's
/// width, [`Plan::main_outputs`](crate::Plan::main_outputs)) and, after
/// them, the old main output's channels: the same nodes, wired as the previous main output
/// wired them. The host folds each bank with its own main output's fold and sums them
/// under the [`fade`](Self::fade)'s gains, the old main output's as the outgoing side,
/// for the first [`Fade::duration`](crate::Fade::duration) frames rendered
/// since the commit that declared the new main output
/// ([`Executor::main_output_fade`](crate::Executor::main_output_fade) counts them). A later
/// plan that leaves the main output alone carries the same bank, and the count goes on
/// through it.
///
/// The old main output's channels align to the total latency of the plan the main output
/// change was made from, through every plan that continues the fade: when the
/// old and new totals differ, a source present in both main outputs is heard at two
/// delays during the fade and shifts once at its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MainOutputBank {
    /// The generation of the main output that fades out.
    pub old: MainOutputId,
    /// The index of the old main output's first channel among the plan's outputs:
    /// the new main output's width.
    pub base: usize,
    /// How the two main outputs trade places.
    pub fade: crate::Fade,
    /// This bank carries on the crossfade the plan before it began: the
    /// editor's one decision, which the compiler and the executor read
    /// instead of deciding again. It can only be set by this crate.
    pub(crate) continues: bool,
}

/// Why the graph's main output cannot be widened or placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MainOutputError {
    /// A graph's main output's channels were declared at one width and the main output is, or
    /// was asked to be, another: the declaration owns the main output's width.
    #[error("the graph's main output is declared {declared:?} wide and cannot be {requested:?}")]
    Width {
        /// The width the main output was declared with.
        declared: ChannelLayout,
        /// The width the main output has, or was asked to widen to.
        requested: ChannelLayout,
    },
    /// The main output's channels cannot be placed.
    #[error(transparent)]
    Fold(#[from] FoldError),
    /// A main output was published under an id that is not past every main output already
    /// published: ids are minted in order by the one graph that declares them.
    #[error("main output {id} is not past the last published main output {last}")]
    NotPast {
        /// The id that was published.
        id: MainOutputId,
        /// The newest id already published.
        last: MainOutputId,
    },
    /// Too many published main outputs are waiting for a plan to carry them: the
    /// stream is not rendering (or not fast enough), so the engine holds the
    /// main outputs in flight and takes no more until a plan of a later main output has
    /// rendered.
    #[error("{limit} main outputs are published and none has rendered; wait for a block")]
    TooManyPending {
        /// The most main outputs that may be pending.
        limit: usize,
    },
}

/// Builds the fold from `src` into a destination of which only the width is
/// known: a device, a file, a node's outputs.
///
/// The destination is taken to be the engine's own order for its width
/// ([`ChannelSet::default_for`]): mono, stereo, quad, 5.1, 7.1 and 7.1.4,
/// and unassigned channels past [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS). A
/// destination that states its placement is [`fold_to_channels`].
///
/// # Errors
///
/// As [`fold_to_channels`].
pub fn fold_to_width<F: Float>(
    src: &ChannelTopology,
    dst: ChannelLayout,
) -> Result<Fold<F>, FoldError> {
    fold_to_channels(src, &ChannelSet::default_for(dst), FoldLevels::default())
}

/// [`fold_to_topology_with`] into the channels `dst`: by speaker where it has a
/// topology, and by index into a destination wider than
/// [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS), which has none.
///
/// # Errors
///
/// As [`fold_to_topology_with`].
pub fn fold_to_channels<F: Float>(
    src: &ChannelTopology,
    dst: &ChannelSet,
    levels: FoldLevels,
) -> Result<Fold<F>, FoldError> {
    match dst.topology() {
        Some(topology) => fold_to_topology_with(src, topology, levels),
        None => {
            refuse_unnamed(src)?;
            Fold::by_index(src.width(), dst.width())
        }
    }
}

/// [`FoldError::Unnamed`] for a source with an [`Unknown`](Speaker::Unknown)
/// channel, whatever the destination.
fn refuse_unnamed(src: &ChannelTopology) -> Result<(), FoldError> {
    match src
        .positions()
        .and_then(|list| list.iter().position(|p| matches!(p, Speaker::Unknown(_))))
    {
        Some(channel) => Err(FoldError::Unnamed {
            end: FoldEnd::Source,
            channel,
        }),
        None => Ok(()),
    }
}

/// Builds the fold from `src` into `dst`. A mono source lands on the centre of
/// a 6, 8 or 12-wide destination and the front pair of a 2 or 4-wide one. The
/// source is placed by speaker, except where a speaker cannot be named:
///
/// - a source with an [`Unknown`](Speaker::Unknown) channel (a second LFE, a
///   wide or high speaker) is [`FoldError::Unnamed`], whatever the
///   destination: nothing can be said about where it goes;
/// - a source whose channels are all [`Unassigned`](Speaker::Unassigned) is
///   independent buses, so channel `i` stays channel `i` ([`Fold::by_index`]):
///   a wider destination is zero-filled and a narrower one is an error, never
///   a matrix. A caller that reads a bare count in the engine's order says so
///   with [`ChannelTopology::default_for`] before it calls;
/// - a destination with no named speaker (3, 5, 7, 9 to 11, 13 and up
///   unassigned) takes channels by index too, and a narrowing is
///   [`FoldError::Unplaced`], never a truncation;
/// - otherwise, [`Fold::between`].
///
/// # Errors
///
/// As [`Fold::between`] and [`Fold::by_index`], and [`FoldError::Unnamed`] as
/// above.
pub fn fold_to_topology<F: Float>(
    src: &ChannelTopology,
    dst: &ChannelTopology,
) -> Result<Fold<F>, FoldError> {
    fold_to_topology_with(src, dst, FoldLevels::default())
}

/// [`fold_to_topology`] with the downmix levels stated: where [`Fold::between`]
/// is the fold, `levels` are its gains ([`Fold::spec`]). Channels placed by
/// index have no levels.
///
/// # Errors
///
/// As [`fold_to_topology`].
pub fn fold_to_topology_with<F: Float>(
    src: &ChannelTopology,
    dst: &ChannelTopology,
    levels: FoldLevels,
) -> Result<Fold<F>, FoldError> {
    refuse_unnamed(src)?;
    // An ambisonic source is as unplaced as unassigned channels: by index.
    if src.is_unassigned() || src.ambisonic().is_some() || dst.is_unassigned() {
        Fold::by_index(src.width(), dst.width())
    } else {
        Fold::spec(src, dst).levels(levels).build()
    }
}
