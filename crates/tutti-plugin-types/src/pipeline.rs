//! The audio path's block pipeline: the chunk ceiling both ends of the process
//! edge size to, and what the path counts of the blocks and chunks it sent.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use tutti_rt::{Counter, Tally, TallyWriter, tally};
use tutti_units::{Calls, Chunks};

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

/// What the audio path has made of the chunks it sent a plugin, ever
/// (`Health::block_counts`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PipelineCounts {
    /// Chunks it gave up waiting for.
    pub abandoned: Chunks,
    /// Of the abandoned chunks, those whose reply it collected afterwards:
    /// the plugin answered, late.
    pub late: Chunks,
    /// Replies it took, on time or owed.
    pub replies: Chunks,
}

/// What a plugin's server lost of its own work, cumulative over the server's
/// life, as it last told the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ServerLosses {
    /// Commands the server could not give its render thread.
    pub commands_refused: u64,
    /// One-way edits the plugin refused.
    pub edits_refused: u64,
    /// Callbacks the plugin made from a thread its format forbids, which the
    /// server refused.
    pub refused_off_thread_calls: Calls,
    /// Tasks of the server's (the render lane, the load's beat) that did not
    /// stop within their patience and were left running.
    pub detached_tasks: u64,
    /// Render lanes of the server's that ended without handing their state
    /// back: they panicked, or were dropped under the executor's shutdown.
    pub lanes_lost: u64,
}

/// What became of the chunks a served plugin's pipeline did not have answered
/// in time, cumulative since the plugin was bound
/// (`PluginControls::chunk_outcomes` in `tutti-plugin-host`). Zero for a
/// plugin that runs in this process, which has no pipeline to be late.
///
/// In a reading taken while the pipeline is quiet,
/// `late + refused + unanswered + skipped <= missed`, not equal: a chunk a
/// reset dropped, and one whose channel the shared-memory slab lacks, is
/// missed and none of the four; and a missed chunk's fate is learned a few
/// chunks after it played, so a reading can hold missed chunks whose fate is
/// not known yet. A reading taken while the pipeline runs loads each count
/// on its own, so it can catch one count ahead of another.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChunkOutcomes {
    /// Chunks the server had not answered by the time the node played them,
    /// each played as silence (a dropout). A crashed plugin's silence is not
    /// counted. An offline fork waits for every chunk, so this stays zero
    /// unless it gave up.
    pub missed: Chunks,
    /// Of the missed chunks, those the server published after the node had
    /// played the silence: a late answer (an overloaded plugin or machine:
    /// raise the device buffer size), not a lost one. Seen in the shared ring
    /// or in the server's reply.
    pub late: Chunks,
    /// Of the missed chunks, those the server refused: it did not publish the
    /// audio, and said so in its reply. Its reasons are the chunk's input slot
    /// recycled before it ran, a block that did not fit, an output channel not
    /// written, and a refused sample rate.
    pub refused: Chunks,
    /// Of the missed chunks, those neither late, refused nor skipped: the
    /// server never said what became of them within the last few chunks. Not
    /// zero is a loss no other count explains.
    pub unanswered: Chunks,
    /// Chunks the bridge dropped because the plugin fell more than one chunk
    /// behind (the shared ring holds one chunk behind the newest): never sent,
    /// so each played as silence and was never answered. Raise the device
    /// buffer size if it grows.
    pub skipped: Chunks,
}

/// What the host side of a served plugin lost of its own work, cumulative
/// since the plugin was bound (`PluginControls::bridge_losses` in
/// `tutti-plugin-host`). Zero for a plugin that runs in this process. The
/// server's own losses are [`ServerLosses`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BridgeLosses {
    /// Commands the audio side could not hand to the bridge: a full command
    /// queue refused a reset or rate command, or a block found no payload or
    /// no room for its parameter buffer, so the block was refused and played
    /// as silence.
    pub commands_refused: u64,
    /// Audio responses (a block's MIDI-out and status) the bridge could not
    /// queue for the audio thread because its queue was full: lost.
    pub responses_lost: u64,
    /// Parameter-change buffers the audio thread leaked rather than free on
    /// its own thread: a broken sizing bound, apart from a full command queue.
    /// Raw: a buffer is not a block, chunk or call, and no unit counts buffers.
    pub leaked_buffers: u64,
    /// Transfers between the plugin and its editor instance that were refused:
    /// a parameter write the mirror's full queue dropped (the plugin still got
    /// it; only the open editor's display missed it), an edit made in the
    /// editor that the plugin did not take, and a state that could not be read
    /// or loaded either way (an editor opened at the plugin's defaults among
    /// them).
    pub mirror_transfers_refused: u64,
    /// Requests the plugin's editor instance made that were dropped: every
    /// [`Request`](crate::Request) but a transport request (a restart, an I/O
    /// change, a parameter rescan, a latency or tail change, a reload, a
    /// callback, a MIDI-CC or note-expression change, a timer or a file
    /// descriptor), which the plugin's audio instance cannot be told.
    pub editor_requests_dropped: u64,
    /// Tasks of the server on the host side (the bridge's lanes, the process's
    /// exit watch) that did not stop in time and were left running.
    /// Raw: a task is not a block, chunk or call, and no unit counts tasks.
    pub detached_tasks: u64,
}

/// What the pipeline reports to its controls
/// ([`outcomes`](Self::outcomes), [`bridge_losses`](Self::bridge_losses),
/// [`server_losses`](Self::server_losses) and
/// [`in_flight_answered`](Self::in_flight_answered), read by
/// `PluginControls` in `tutti-plugin-host`):
/// written on the audio thread with relaxed atomics, read anywhere.
///
/// Here, beside [`PipelineCounts`], because the batcher in `tutti-plugin-ipc`
/// writes it and `tutti-plugin-host`'s controls read it, and neither crate may
/// depend on the other.
pub struct ChunkCounters {
    /// The chunk submitted and not collected yet; 0 for none (sequences
    /// start at 1).
    pub in_flight: AtomicU64,
    /// Chunks the server had not answered when the node collected them
    /// (played as silence), the bridge alive.
    missed: TallyWriter<Chunks>,
    /// Of the chunks counted in [`missed`](Self::missed), those the server
    /// published after all: seen in the shared ring, or said published in the
    /// server's reply, which can come after the chunk's ring slot was reused.
    late: TallyWriter<Chunks>,
    /// Of the chunks counted in [`missed`](Self::missed), those the server
    /// refused: it did not publish the audio, and said so in its reply. Its
    /// reasons are the chunk's input slot recycled before it ran, a block that
    /// did not fit, an output channel not written, and a refused rate.
    refused: TallyWriter<Chunks>,
    /// Of the chunks counted in [`missed`](Self::missed), those whose fate
    /// (published, refused or skipped) was not learned within the last few
    /// chunks: the server never said what became of them. A loss no other
    /// counter explains. Every missed chunk is late, refused, skipped (counted
    /// by the bridge) or unanswered; a chunk a reset dropped, and one whose channel the
    /// slab lacks, is none.
    unanswered: TallyWriter<Chunks>,
    /// Chunks the bridge dropped because the plugin fell more than one chunk
    /// behind, counted by the bridge thread (set by [`attach_skipped`](Self::attach_skipped)).
    skipped: OnceLock<Tally<Chunks>>,
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
    /// The server's own cumulative loss counts, as it last told them, over
    /// the one server life these counters belong to (a respawned server gets
    /// a new connection and new counters). The only writer is
    /// [`note_server_losses`](Self::note_server_losses), on the one thread that
    /// hears the server: its read-then-add is sound only with that one writer.
    server_refused_commands: TallyWriter<u64>,
    server_refused_edits: TallyWriter<u64>,
    server_refused_off_thread_calls: TallyWriter<u64>,
    server_detached_tasks: TallyWriter<u64>,
    server_lanes_lost: TallyWriter<u64>,
    /// Whether the chunk of a sequence has been answered, set when the node is
    /// bound (the shared-memory slab's reply flag, which this file does not
    /// name).
    answered: OnceLock<Box<dyn Fn(u64) -> bool + Send + Sync>>,
}

impl std::fmt::Debug for ChunkCounters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChunkCounters")
            .field("in_flight", &self.in_flight)
            .field("outcomes", &self.outcomes())
            .field("bridge_losses", &self.bridge_losses())
            .field("server_losses", &self.server_losses())
            .finish_non_exhaustive()
    }
}

impl Default for ChunkCounters {
    fn default() -> Self {
        let cell = || tally().writer();
        Self {
            in_flight: AtomicU64::new(0),
            missed: cell(),
            late: cell(),
            refused: cell(),
            unanswered: cell(),
            skipped: OnceLock::new(),
            commands_refused: OnceLock::new(),
            responses_lost: OnceLock::new(),
            leaked_buffers: OnceLock::new(),
            mirror_transfers_refused: OnceLock::new(),
            editor_requests_dropped: OnceLock::new(),
            detached: OnceLock::new(),
            server_refused_commands: tally().writer(),
            server_refused_edits: tally().writer(),
            server_refused_off_thread_calls: tally().writer(),
            server_detached_tasks: tally().writer(),
            server_lanes_lost: tally().writer(),
            answered: OnceLock::new(),
        }
    }
}

impl ChunkCounters {
    /// Counts `n` chunks the server had not answered when the node collected
    /// them.
    pub fn note_missed(&self, n: Chunks) {
        self.missed.add(n);
    }

    /// Counts one missed chunk the server published after all.
    pub fn note_late(&self) {
        self.late.add(Chunks::new(1));
    }

    /// Counts one missed chunk the server refused.
    pub fn note_refused(&self) {
        self.refused.add(Chunks::new(1));
    }

    /// Counts one missed chunk whose fate was never learned.
    pub fn note_unanswered(&self) {
        self.unanswered.add(Chunks::new(1));
    }

    /// Records how to tell that a chunk is answered (the node's bind).
    pub fn attach(&self, answered: impl Fn(u64) -> bool + Send + Sync + 'static) {
        let _ = self.answered.set(Box::new(answered));
    }

    /// Reads the bridge thread's count of skipped chunks (the node's bind).
    pub fn attach_skipped(&self, skipped: Tally<Chunks>) {
        let _ = self.skipped.set(skipped);
    }

    /// Reads the bridge's count of refused commands (the node's bind).
    pub fn attach_commands_refused(&self, refused: Tally<u64>) {
        let _ = self.commands_refused.set(refused);
    }

    /// Reads the bridge's count of lost audio responses (the node's bind).
    pub fn attach_responses_lost(&self, lost: Counter<u64>) {
        let _ = self.responses_lost.set(lost);
    }

    /// Reads the payload pool's count of leaked buffers (the node's bind).
    pub fn attach_leaked_buffers(&self, leaked: Tally<u64>) {
        let _ = self.leaked_buffers.set(leaked);
    }

    /// Reads the editor mirror's count of refused transfers (the node's bind).
    pub fn attach_mirror_transfers_refused(&self, refused: Tally<u64>) {
        let _ = self.mirror_transfers_refused.set(refused);
    }

    /// Reads the count of editor-instance requests dropped (the node's bind).
    pub fn attach_editor_requests_dropped(&self, dropped: Tally<u64>) {
        let _ = self.editor_requests_dropped.set(dropped);
    }

    /// Reads the server watch's count of detached tasks (the node's bind).
    pub fn attach_detached(&self, detached: Tally<u64>) {
        let _ = self.detached.set(detached);
    }

    /// Takes the server's cumulative counts, as it last told them: each count
    /// rises to the figure told and never falls below what it has counted. One
    /// thread tells them (the one that hears the server).
    pub fn note_server_losses(&self, losses: ServerLosses) {
        fn rise_to(count: &TallyWriter<u64>, told: u64) {
            count.add(told.saturating_sub(count.get()));
        }
        rise_to(
            &self.server_refused_off_thread_calls,
            losses.refused_off_thread_calls.get(),
        );
        rise_to(&self.server_refused_commands, losses.commands_refused);
        rise_to(&self.server_refused_edits, losses.edits_refused);
        rise_to(&self.server_detached_tasks, losses.detached_tasks);
        rise_to(&self.server_lanes_lost, losses.lanes_lost);
    }

    /// What became of the missed chunks, as the counts stand now. Each count
    /// is one relaxed load: a reading taken while the pipeline runs can catch
    /// one count ahead of another.
    pub fn outcomes(&self) -> ChunkOutcomes {
        ChunkOutcomes {
            missed: self.missed.get(),
            late: self.late.get(),
            refused: self.refused.get(),
            unanswered: self.unanswered.get(),
            skipped: self.skipped.get().map_or(Chunks::ZERO, Tally::get),
        }
    }

    /// What the host side lost of its own work, as the counts stand now;
    /// zero for a count whose producer was never attached.
    pub fn bridge_losses(&self) -> BridgeLosses {
        let tally = |cell: &OnceLock<Tally<u64>>| cell.get().map_or(0, Tally::get);
        BridgeLosses {
            commands_refused: tally(&self.commands_refused),
            responses_lost: self.responses_lost.get().map_or(0, Counter::get),
            leaked_buffers: tally(&self.leaked_buffers),
            mirror_transfers_refused: tally(&self.mirror_transfers_refused),
            editor_requests_dropped: tally(&self.editor_requests_dropped),
            detached_tasks: tally(&self.detached),
        }
    }

    /// The server's own cumulative losses, as it last told them
    /// ([`note_server_losses`](Self::note_server_losses)); zero before it
    /// told any.
    pub fn server_losses(&self) -> ServerLosses {
        ServerLosses {
            commands_refused: self.server_refused_commands.get(),
            edits_refused: self.server_refused_edits.get(),
            refused_off_thread_calls: Calls::new(self.server_refused_off_thread_calls.get()),
            detached_tasks: self.server_detached_tasks.get(),
            lanes_lost: self.server_lanes_lost.get(),
        }
    }

    /// See `PluginControls::chunk_in_flight_answered`.
    pub fn in_flight_answered(&self) -> bool {
        let seq = self.in_flight.load(Ordering::Acquire);
        seq == 0 || self.answered.get().is_none_or(|answered| answered(seq))
    }
}
