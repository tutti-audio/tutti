//! The audio path's block pipeline: the chunk ceiling both ends of the process
//! edge size to, and what the path counts of the blocks and chunks it sent.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use tutti_rt::{Counter, Tally};

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
    /// Of the abandoned blocks, those whose reply it collected afterwards:
    /// the plugin answered, late.
    pub late: u64,
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
    skipped: OnceLock<Tally<u64>>,
    /// Commands the audio side could not hand to the bridge thread (a full
    /// command queue, an empty payload pool, a parameter buffer with no room
    /// to return to), counted by the bridge (set by
    /// [`attach_commands_refused`](Self::attach_commands_refused)).
    commands_refused: OnceLock<Tally<u64>>,
    /// Audio responses the bridge could not queue for the audio thread,
    /// counted by the bridge (set by
    /// [`attach_responses_lost`](Self::attach_responses_lost)).
    responses_lost: OnceLock<Counter<u64>>,
    /// Parameter buffers the audio thread leaked rather than free, counted by
    /// the payload pool (set by [`attach_leaked_buffers`](Self::attach_leaked_buffers)).
    leaked_buffers: OnceLock<Tally<u64>>,
    /// Transfers between the audio and editor instances that were refused,
    /// counted by the bridge (set by
    /// [`attach_mirror_transfers_refused`](Self::attach_mirror_transfers_refused)).
    mirror_transfers_refused: OnceLock<Tally<u64>>,
    /// Requests the editor instance made that were dropped, counted by the
    /// bridge (set by
    /// [`attach_editor_requests_dropped`](Self::attach_editor_requests_dropped)).
    editor_requests_dropped: OnceLock<Tally<u64>>,
    /// Host-side tasks of the server (bridge lanes, the exit task) that did not
    /// stop in time and were left running (set by
    /// [`attach_detached`](Self::attach_detached)).
    detached: OnceLock<Tally<u64>>,
    /// The server's own cumulative loss counts, as it last told them
    /// ([`note_server_losses`](Self::note_server_losses)).
    server_commands_refused: AtomicU64,
    server_edits_refused: AtomicU64,
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
            .field("commands_refused", &self.commands_refused())
            .field("responses_lost", &self.responses_lost())
            .field("leaked_buffers", &self.leaked_buffers())
            .field("mirror_transfers_refused", &self.mirror_transfers_refused())
            .field("editor_requests_dropped", &self.editor_requests_dropped())
            .field("detached", &self.detached())
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
    pub fn attach_skipped(&self, skipped: Tally<u64>) {
        let _ = self.skipped.set(skipped);
    }

    /// Reads the bridge's count of refused commands (the node's bind).
    pub fn attach_commands_refused(&self, refused: Tally<u64>) {
        let _ = self.commands_refused.set(refused);
    }

    /// See `PluginControls::refused_commands`.
    pub fn commands_refused(&self) -> u64 {
        self.commands_refused.get().map_or(0, Tally::get)
    }

    /// Reads the bridge's count of lost audio responses (the node's bind).
    pub fn attach_responses_lost(&self, lost: Counter<u64>) {
        let _ = self.responses_lost.set(lost);
    }

    /// See `PluginControls::lost_responses`.
    pub fn responses_lost(&self) -> u64 {
        self.responses_lost.get().map_or(0, Counter::get)
    }

    /// Reads the payload pool's count of leaked buffers (the node's bind).
    pub fn attach_leaked_buffers(&self, leaked: Tally<u64>) {
        let _ = self.leaked_buffers.set(leaked);
    }

    /// See `PluginControls::leaked_buffers`.
    pub fn leaked_buffers(&self) -> u64 {
        self.leaked_buffers.get().map_or(0, Tally::get)
    }

    /// Reads the editor mirror's count of refused transfers (the node's bind).
    pub fn attach_mirror_transfers_refused(&self, refused: Tally<u64>) {
        let _ = self.mirror_transfers_refused.set(refused);
    }

    /// See `PluginControls::mirror_transfers_refused`.
    pub fn mirror_transfers_refused(&self) -> u64 {
        self.mirror_transfers_refused.get().map_or(0, Tally::get)
    }

    /// Reads the count of editor-instance requests dropped (the node's bind).
    pub fn attach_editor_requests_dropped(&self, dropped: Tally<u64>) {
        let _ = self.editor_requests_dropped.set(dropped);
    }

    /// See `PluginControls::editor_requests_dropped`.
    pub fn editor_requests_dropped(&self) -> u64 {
        self.editor_requests_dropped.get().map_or(0, Tally::get)
    }

    /// Reads the server watch's count of detached tasks (the node's bind).
    pub fn attach_detached(&self, detached: Tally<u64>) {
        let _ = self.detached.set(detached);
    }

    /// See `PluginControls::detached_tasks`.
    pub fn detached(&self) -> u64 {
        self.detached.get().map_or(0, Tally::get)
    }

    /// Takes the server's cumulative counts, as it last told them.
    pub fn note_server_losses(&self, commands_refused: u64, edits_refused: u64) {
        self.server_commands_refused
            .store(commands_refused, Ordering::Relaxed);
        self.server_edits_refused
            .store(edits_refused, Ordering::Relaxed);
    }

    /// See `PluginControls::server_commands_refused`.
    pub fn server_commands_refused(&self) -> u64 {
        self.server_commands_refused.load(Ordering::Relaxed)
    }

    /// See `PluginControls::server_edits_refused`.
    pub fn server_edits_refused(&self) -> u64 {
        self.server_edits_refused.load(Ordering::Relaxed)
    }

    /// See `PluginControls::skipped_chunks`.
    pub fn skipped(&self) -> u64 {
        self.skipped.get().map_or(0, Tally::get)
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
