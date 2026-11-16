//! The audio path's block pipeline: the chunk ceiling both ends of the process
//! edge size to, and what the path counts of the blocks and chunks it sent.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// The largest chunk that crosses the process edge: the largest device
/// callback a live plugin keeps in step with, and so the ceiling on the
/// pipeline's declared latency (85 ms at 48 kHz).
///
/// The shared-memory slab is sized to this when it is laid out, at launch and
/// at each activation over other buses (`subprocess::launch`, the other
/// consumer of this constant; or to the host's smaller `max_buffer_size`),
/// before the node is prepared — so `tutti-plugin-ipc`'s batcher (`Batcher::prepare`)
/// can only narrow the chunk, never widen it, and the slab and the batcher
/// agree on the per-chunk ceiling by construction. A device calling back
/// with more frames than this gets a chunk of this many, which no longer
/// lines up with its callbacks (the batcher's module docs).
pub const MAX_CHUNK: usize = 4096;

/// What the audio path has made of the blocks it sent a plugin, ever
/// (`Health::block_counts`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockCounts {
    /// Blocks it gave up waiting for.
    pub abandoned: u64,
    /// Replies it took, on time or owed.
    pub replies: u64,
}

/// What the pipeline reports to its controls
/// (`PluginControls::missed_chunks`, `late_chunks`, `refused_chunks`, `skipped_chunks`, `unanswered_chunks` and `chunk_in_flight_answered`
/// in `tutti-plugin-host`):
/// written on the audio thread with relaxed atomics, read anywhere.
///
/// Here, beside [`BlockCounts`], because the batcher in `tutti-plugin-ipc`
/// writes it and `tutti-plugin-host`'s controls read it, and neither crate may
/// depend on the other.
#[derive(Default)]
pub struct ChunkCounters {
    /// The chunk submitted and not collected yet; 0 for none (sequences
    /// start at 1).
    pub in_flight: AtomicU64,
    /// Chunks the server had not answered when the node collected them
    /// (played as silence), the bridge alive.
    pub missed: AtomicU64,
    /// Of the chunks counted in [`missed`](Self::missed), those the server
    /// published after all: seen in the shared ring, or said published in the
    /// server's reply, which can come after the chunk's ring slot was reused.
    pub late: AtomicU64,
    /// Of the chunks counted in [`missed`](Self::missed), those the server
    /// refused: it did not publish the audio, and said so in its reply. Its
    /// reasons are the chunk's input slot recycled before it ran, a block that
    /// did not fit, an output channel not written, and a refused rate.
    pub refused: AtomicU64,
    /// Of the chunks counted in [`missed`](Self::missed), those whose fate
    /// (published, refused or skipped) was not learned within the last few
    /// chunks: the server never said what became of them. A loss no other
    /// counter explains. Every missed chunk is late, refused, skipped (counted
    /// by the bridge) or unanswered; a chunk a reset dropped, and one whose channel the
    /// slab lacks, is none.
    pub unanswered: AtomicU64,
    /// Chunks the bridge dropped because the plugin fell more than one chunk
    /// behind, counted by the bridge thread (set by [`attach_skipped`](Self::attach_skipped)).
    skipped: OnceLock<Arc<AtomicU64>>,
    /// Whether the chunk of a sequence has been answered, set when the node is
    /// bound (the shared-memory slab's reply flag, which this file does not
    /// name).
    answered: OnceLock<Box<dyn Fn(u64) -> bool + Send + Sync>>,
}

impl std::fmt::Debug for ChunkCounters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkCounters")
            .field("in_flight", &self.in_flight)
            .field("missed", &self.missed)
            .field("late", &self.late)
            .field("refused", &self.refused)
            .field("unanswered", &self.unanswered)
            .field("skipped", &self.skipped())
            .finish_non_exhaustive()
    }
}

impl ChunkCounters {
    /// Records how to tell that a chunk is answered (the node's bind).
    pub fn attach(&self, answered: impl Fn(u64) -> bool + Send + Sync + 'static) {
        let _ = self.answered.set(Box::new(answered));
    }

    /// See `PluginControls::missed_chunks`.
    pub fn missed(&self) -> u64 {
        self.missed.load(Ordering::Relaxed)
    }

    /// Reads the bridge thread's count of skipped chunks (the node's bind).
    pub fn attach_skipped(&self, skipped: Arc<AtomicU64>) {
        let _ = self.skipped.set(skipped);
    }

    /// See `PluginControls::skipped_chunks`.
    pub fn skipped(&self) -> u64 {
        self.skipped
            .get()
            .map_or(0, |skipped| skipped.load(Ordering::Relaxed))
    }

    /// See `PluginControls::late_chunks`.
    pub fn late(&self) -> u64 {
        self.late.load(Ordering::Relaxed)
    }

    /// See `PluginControls::refused_chunks`.
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }

    /// See `PluginControls::unanswered_chunks`.
    pub fn unanswered(&self) -> u64 {
        self.unanswered.load(Ordering::Relaxed)
    }

    /// See `PluginControls::chunk_in_flight_answered`.
    pub fn in_flight_answered(&self) -> bool {
        let seq = self.in_flight.load(Ordering::Acquire);
        seq == 0 || self.answered.get().is_none_or(|answered| answered(seq))
    }
}
