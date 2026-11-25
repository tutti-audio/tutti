#![doc = include_str!("../README.md")]
//!
//! ## What this is
//!
//! The structures the engine's audio callback touches, allocation-free on the
//! audio path (`tests/no_alloc/rt_no_alloc.rs` gates it), and the home of the
//! engine's lock-free and `unsafe` runtime structures. It names three tutti
//! crates, `tutti-units` (the units a [`Gauge`] and a [`Param`] hold, and [`RequestFlag`]'s range),
//! `tutti-buffers` (the `ChannelLayout` a [`PosRing`] is written in and the
//! `Sample` formats it is generic over) and `tutti-platform` (the `Instant` its
//! waits read; the clock, threads, priority and OS calls are that crate's);
//! its other dependencies are `arc-lend`, `atomic_float` and `bytemuck` (and `loom`
//! under `--cfg loom`).
//!
//! **Cells and collections**
//!
//! - [`AudioThreadCell`] — interior mutability through `&self` with a
//!   one-borrow-at-a-time contract checked in every build (behind an `Arc` or
//!   a COM object).
//! - [`RtVec`] — a capped collection through `&mut self` that refuses the
//!   write at the cap rather than growing, latching an overflow flag. An owner
//!   that shares one behind an `Arc` puts it in an [`AudioThreadCell`].
//!   [`CappedVec`] is the same with a cap chosen at construction, for a count
//!   known only at run time.
//! - [`AtomicCell`] / [`AtomicWord`] — a value in one atomic word, stored by its
//!   encoding (a fieldless enum by [`atomic_word!`], an integer, a pair of
//!   `u32`s), so a reader never pairs one write's field with another's.
//! - [`SeqWriter`] / [`SeqReader`] — a sequence lock for a `Pod` value one
//!   writer overwrites in place and any thread copies whole.
//! - [`seq_slots()`] — an array of those, one slot per index, that one reader
//!   drains by changed slot, with an epoch that makes earlier writes stale.
//! - [`level_cell()`] — a peak and an energy the audio thread folds each block
//!   into and one reader takes whole since its last take (a level meter's lane).
//! - [`gauge()`] / [`counter()`] — a reading a node writes and any thread reads:
//!   a level that moves both ways ([`Gauge`]) or a count that only grows
//!   ([`Counter`], one or several tallies, such as a [`ring()`]'s loss count),
//!   one load per word.
//! - [`tally()`] — a count with many writers ([`Tally`], [`TallyWriter`]):
//!   `Clone` writers that `fetch_add`, for owners whose writers overlap.
//!
//! - [`Param`] / [`Param64`] / [`ParamCell`] — a unit held in an atomic cell:
//!   shared and cloned by handle ([`Param`], addressed without its unit as a
//!   [`ParamCell`]) or inline ([`Param64`]); an [`AtomicCell`] carries a
//!   `SampleRate` the same way.
//! - [`Mint`] and [`id!`] — a lock-free counter of unique non-zero numbers from 1,
//!   and the macro that writes an id type minted from one.
//! - [`SplitMix64`] — the pseudo-random generator: a seeded stream of
//!   words, and the stateless mix of a counter.
//!
//! **Queues and signals**
//!
//! - [`ring()`] — a bounded single-producer single-consumer queue of any `Send`
//!   value, its full-ring policy ([`Overflow`]) chosen at construction and its
//!   losses counted by the ring; values can be borrowed in place and a run of
//!   them moves with one publish.
//! - [`PosRing`] — frames a writer thread places ahead of the audio thread,
//!   indexed by position (a disk stream's ring).
//! - [`AtomicRequestFlags`] — up to 32 kinds of request ([`RequestFlag`]) raised from
//!   any thread with one `fetch_or` and taken whole by any one thread.
//! - [`DirtySet`] — a sized set of dirty flags, an atomic bit per index, with
//!   no capacity to overflow.
//! - [`FaultLatch`] — a flag a producer raises when it stops for good, with
//!   its first [`Cause`], read before the ring so a failure never hides the
//!   frames pushed ahead of it.
//! - [`drain_clock()`] — counts the drains the audio side begins, so the
//!   control side can tell which of its writes a block has surely taken.
//! - [`handoff()`] — one value that moves between a control thread and the
//!   audio thread at a block boundary ([`Handoff`] lends and recalls, [`Rt`]
//!   trades).
//! - [`parker()`], [`Unparker`] and [`Parker`] — wake one parked thread from
//!   the audio thread (a futex wake only if the parker is parked), with no
//!   wake-up lost between the parker's check and its park.
//! - [`Bells`], [`Bell`] and [`Listener`] — carry an audio-thread ring to a
//!   future's `Waker` through one control-side tick (a thread where one can
//!   start, `Bells::tick` where not), the audio thread never touching the waker.
//! - [`RenderCount`] — the renders of one thing in flight, and a `quiescent`
//!   check so a thread that cleared what they read frees only when none can
//!   still read it.
//! - [`Publisher`] / [`Loan`] / [`Reader`] — the publish cell, re-exported from
//!   `arc-lend`: a control thread publishes non-scalar state, the audio thread
//!   only borrows it.
//!
//! **The parallel executor**
//!
//! - [`SplitMut`] / [`SplitRw`] — one slice borrowed by several threads, each
//!   element or chunk claimed before use, so a borrow that would alias is
//!   refused rather than made.
//! - [`TaskGraph`] — a DAG of tasks run once per block by any number of
//!   threads: self-resetting activation counters and a ready list.
//! - [`JobGate`] — lends a borrowed job to a pool's helper threads for one call
//!   and returns only when every helper has let go of it.
//! - [`BlockWaits`] — how long the audio thread and a helper each wait inside a
//!   parallel block before giving way.
//!
//! **Scope**
//!
//! - [`RtScope`] — a per-thread "inside a block" marker that a `Drop` impl
//!   checks ([`RtScope::check_outside`]), or a [`ControlDrop`] field does.
//! - [`Mutex`], [`RwLock`] and [`Condvar`] — the blocking face: each wait first asserts, in a
//!   debug build, that it is not inside an [`RtScope`], so a lock taken in a block
//!   fails the realtime tests the way an allocation does.
//!
//! The `rtsan` feature (Linux and macOS; `cargo xtask rtsan`) makes each [`RtScope`] also
//! hold LLVM's RealtimeSanitizer region, which sees what those checks cannot: a C `malloc`,
//! a pthread lock or file I/O inside a block, in a plugin or a dependency. It does nothing
//! unless `RTSAN_ENABLE` is set when the crate builds.
//!
//! **The root is the API.** Every item is re-exported at the root
//! (`tutti_rt::RtVec`); the modules are private.
//!
//! Each lock-free structure has a loom model in `tests/*_loom.rs` that runs the
//! shipped code:
//!
//! ```text
//! cargo test -p tutti-rt --release --test pos_ring_loom   # with the loom cfg flag set
//! ```
//!
//! ## Safety
//!
//! Outside the `-ffi` crates (the plugin formats, the OS MIDI APIs, the plugin
//! process edge) and `tutti-platform` (the OS calls), this crate holds all of the engine's `unsafe`, and the external `arc-lend`
//! crate holds the publish cell's;
//! every other library carries `#![forbid(unsafe_code)]`. Each site exists because the safe construct would
//! have cost something on the audio thread:
//!
//! - `AudioThreadCell` (`cell.rs`): an `UnsafeCell` with `unsafe impl Sync`, made
//!   true by one atomic word (a writer bit and a reader count) checked on every
//!   borrow in every build. A conflicting borrow panics, or its `try_` form
//!   returns `None`, instead of aliasing.
//! - `SplitMut`, `SplitRw` and `ClaimTable` (`split.rs`): one slice borrowed by
//!   several threads, each element or chunk claimed in an atomic table before a
//!   reference to it is made, so a second claim is refused rather than aliased.
//!   They are why `tutti-graph`'s executor can hand every node its buffers out of
//!   one arena under `#![forbid(unsafe_code)]`.
//! - `JobGate` (`job_gate.rs`): erases a borrowed job's lifetime for a pool's
//!   helpers; `run` neither returns nor unwinds until no helper is inside the job
//!   (a store-load pair with `SeqCst` fences).
//! - `ring()` (`ring/plain.rs`): packed `UnsafeCell<MaybeUninit<T>>` slots under a head and a
//!   tail index; the one writer writes a slot only after the reader's `Release` store of `tail`
//!   freed it, and the one reader reads it only after the writer's `Release` store of `head`
//!   published it.
//! - `Exclusive` (`exclusive.rs`): `unsafe impl Sync` for a `T: Send` that no `&self`
//!   method reaches, so a shared borrow grants nothing (std's `core::sync::Exclusive`).
//! - `handoff` (`handoff.rs`): one `MaybeUninit` slot moved between the two ends
//!   under a state word.
//! - `RtVec` (`vec.rs`): `N` inline `MaybeUninit` slots under a length, so
//!   [`RtVec::boxed`] can build a large one in its box with no stack copy; the
//!   slots `..len` are the initialised ones.
//!
//! `arc-lend`'s `Publisher` (an external crate) holds the rest: an `AtomicPtr` to an `Arc::into_raw`
//! pointer that a `Loan` dereferences under a hazard-slot protocol, so the audio
//! thread can never free a value (the reason it exists instead of `ArcSwap`).
//!
//! What checks it: a loom model per lock-free structure, run against the shipped
//! code (`tests/*_loom.rs` here, and arc-lend's own loom model for `Publisher`), and miri
//! over this crate's unit tests (arc-lend's CI runs its own), in CI's `loom` and
//! `miri` jobs (`cargo xtask loom`, `cargo xtask miri`). Miri executes no FFI, so a green run
//! says nothing about numerics. The plugin formats'
//! `unsafe` is answered structurally instead: a plugin that corrupts memory
//! takes down the out-of-process `plugin-server`, not the host.
#![deny(missing_docs)]
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_methods,
        reason = "a test starts the threads it races"
    )
)]

mod backoff;
mod capped_vec;
mod cell;
mod claim;
mod claim_lock;
mod dirty_set;
mod drain_clock;
mod exclusive;
mod fault_latch;
mod gauge;
mod handoff;
mod parker;
// `id!` is `#[macro_export]`ed: the one way any crate writes an id, used as
// `tutti_rt::id!`.
mod atomic_cell;
mod bells;
mod blocking;
mod id;
mod job_gate;
mod last_out;
mod level_cell;
mod mint;
mod param;
mod pos_ring;
mod reclaim;
mod render_count;
mod render_gate;
mod request_flags;
mod ring;
mod scope;
mod seq_cell;
mod seq_slots;
mod spin;
mod split;
mod splitmix;
mod sync;
mod task_graph;
mod vec;

pub use atomic_cell::{AtomicCell, AtomicWord};
#[cfg(loom)]
pub use bells::Ticker;
pub use bells::{Bell, Bells, BellsHandle, Listener};
pub use blocking::{Condvar, Mutex, RwLock};
pub use capped_vec::CappedVec;
pub use cell::{AudioThreadCell, BorrowGuard, BorrowRef};
pub use claim_lock::{Claim, ClaimGuard};
pub use dirty_set::{DirtyIndex, DirtySet, Taken};
pub use drain_clock::{DrainBeat, DrainMark, DrainWatch, drain_clock};
pub use exclusive::Exclusive;
pub use fault_latch::{Cause, Drained, FaultLatch};
pub use gauge::{
    Count, Counter, CounterWriter, Gauge, GaugeWriter, Reading, Tally, TallyWriter, Words, counter,
    gauge, tally,
};
pub use handoff::{Handback, Handoff, Rt, handoff};
pub use job_gate::JobGate;
pub use last_out::{LastOut, Party};
pub use level_cell::{LevelReader, LevelSpan, LevelWriter, level_cell};
pub use mint::Mint;
pub use param::{Param, Param64, ParamCell};
pub use parker::{Parker, Unparker, parker};
pub use pos_ring::{
    MAX_POS_RING_FRAMES, MAX_POS_RING_POSITION, PosClaim, PosFrame, PosReader, PosRing, PosWriter,
    RingFloat, RingSample, RingWindow,
};
pub use reclaim::Reclaim;
pub use render_count::{RenderCount, RenderGuard};
pub use render_gate::{GateGuard, GateState, RenderGate, Transition};
pub use request_flags::{AtomicRequestFlags, RequestFlag, RequestFlagOutOfRange, RequestFlags};
pub use ring::{Drain, Iter, Overflow, PushError, Returned, RingReader, RingWriter, ring};
pub use scope::{ControlDrop, RtScope, RtScopeGuard};
pub use seq_cell::{Attempt, Contended, SeqReader, SeqWriter};
pub use seq_slots::{
    EpochWatch, Generation, SeqSlotsReader, SeqSlotsWriter, SweptWatch, seq_slots,
};
pub use spin::BlockWaits;
pub use split::{ClaimConflict, ClaimTable, Claimed, ClaimedRun, Held, RwView, SplitMut, SplitRw};
pub use splitmix::SplitMix64;
pub use task_graph::{Participant, TaskGraph};
pub use vec::RtVec;

// The cell non-scalar state reaches the audio thread through: a control
// thread publishes, the audio thread only borrows (`Loan`), so it never frees.
// The external `arc-lend` crate, which names nothing of tutti's; re-exported here
// so the engine spells it `tutti_rt::Publisher`.
pub use arc_lend::{Loan, Publisher, Reader};

#[cfg(test)]
mod error_bounds {
    //! Each error type this crate returns is an `Error` a caller can box and move (`?` into
    //! `Box<dyn Error + Send + Sync>`); the bound is checked where the function
    //! is written, so a non-`Send` field fails the build.

    fn err<T: std::error::Error + Send + Sync + 'static>() {}

    #[allow(dead_code)]
    fn the_errors_are_boxable() {
        err::<crate::ClaimConflict>();
        err::<crate::Contended>();
        err::<crate::RequestFlagOutOfRange>();
    }
}
