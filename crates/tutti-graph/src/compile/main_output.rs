//! The graph's main output's placement on the world outside it: a width is not a
//! topology, so how a main output of known or bare channels reaches a device or a
//! file of which only the width is known lives here, once.

use tutti_buffers::FoldError;

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
    /// The main output a graph starts with: the empty one, until a declaration.
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

/// Why the graph's main output cannot be placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MainOutputError {
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
