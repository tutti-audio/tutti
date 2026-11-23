//! The sample-accuracy contract suite's harness: for
//! a node and a way to excite it, run it through every path the graph can
//! put it on and assert that an excitation at frame `F` produces its
//! response at exactly frame `F + arrival + latency`.
//!
//! Behind the `contract` feature (off by default): it is test support, for
//! this crate's suite and for node crates that add rows for their own nodes.
//! Enable it from a dev-dependency.
//!
//! # A row
//!
//! A [`Row`] is a node constructor, an [`Excite`] (an event on an event
//! port, an audio impulse on an audio port, or an impulse on a modulated
//! param), a [`Detect`] (the first
//! sample above a threshold, or an exact expected response) and the output
//! channel to watch. [`Row::check`] runs one [`Path`]; the
//! [`contract_tests!`](crate::contract_tests) macro writes one `#[test]` per
//! path, so each path is a separate case that fails on its own.
//!
//! ```
//! use tutti_graph::contract::{Detect, Excite, Path, ImpulseSource, Row};
//! use tutti_graph::EventKind;
//! use tutti_fixtures::midi;
//! use tutti_ump::{MidiChannel, NoteVelocity};
//! use tutti_units::{Latency, Samples};
//!
//! let row = Row::new(
//!     "pulse, 37 frames late",
//!     || Box::new(ImpulseSource::new(Latency::new(Samples::new(37)))),
//!     Excite::Event {
//!         port: 0,
//!         kind: EventKind::Midi(midi::note_on(MidiChannel::FIRST, 60, 0xC800)),
//!     },
//!     Detect::Exact(vec![1.0]),
//! );
//! row.check(Path::BehindPdc);
//! ```
//!
//! # What "exactly" means
//!
//! The expected frame is computed, never measured: `F` is where the
//! excitation was put on the timeline, `arrival` is the node's compiled
//! arrival latency (asserted against what the path built, so a harness bug
//! cannot quietly turn a PDC path into a direct one), and `latency` is the
//! node's **declared** [`Shape::latency`](crate::Shape::latency). A node whose DSP delays by more or
//! less than it declares fails every path. An event excitation is held to
//! the node's declared [`Shape::event_resolution`](crate::Shape::event_resolution): `Sample` is the exact frame,
//! `Frames(n)` any frame within `n - 1` of it **in either direction** (no
//! grid origin is assumed, so a node chunking on its own cursor honours it),
//! `Block` any frame within the block the event arrives in. A node finer
//! than it declares passes. An audio impulse is always exact.
//!
//! For an [`Detect::Exact`] row the whole render is checked: exact silence
//! except each expected response, so a duplicated or a dropped delivery
//! fails as surely as a mistimed one.
//!
//! Every excitation is swept over [`OFFSETS`], so the offset inside its block
//! is 0, 1, either side of a 64-frame boundary (where a node that renders in
//! fixed pieces would seam, as the sampler's clip readers did before they
//! sized their scratch from `Prepare::max_block`), the middle
//! and the last frame — and, behind PDC, both where the excitation starts
//! and where the node sees it.
//!
//! # What is not here
//!
//! A node fed out of band — through a MIDI mailbox (a keyboard's queue on a
//! synth's own port) — has no row for that path: the harness can only time
//! what the graph delivers, and a mailbox's events are neither
//! PDC-compensated nor stamped against the graph's blocks. The instruments
//! take MIDI on an event port too, and carry rows for it in their crates
//! (`tutti-polysynth`, `tutti-soundfont`), each with the constant lead its
//! note's DSP starts with ([`Row::with_lead`]).
//!
//! # The fork's snapshot
//!
//! [`IsolateRow`] checks the other promise a node crate makes here:
//! that a [`ParamNode`](crate::ParamNode)'s fork, `fork_fresh`, severs every live control it
//! reads (every cell a control writes, addressed by its `ParamSet` or not),
//! so a fork renders the controls as they were at fork time. See
//! `src/contract/snapshot.rs`.

mod drive;
mod nodes;
mod rig;
mod snapshot;
mod suite;

pub use drive::{
    Driven, HAND_EVENT_CAPACITY, MAX_CHECKED_TAIL, assert_declared_tail, assert_param_fork, drive,
    drive_input, drive_input_in, drive_input_params, prepared,
};
pub use nodes::{Emitter, ImpulseSource, Latent, Lookahead, ParamEcho, ParamPulse, Sum};
pub use rig::{BlockRig, Direct, RIG_MAX_CHANNELS};
pub use snapshot::{IsolateRow, SNAPSHOT_FRAMES};
pub use suite::{Detect, Excite, MAX_BLOCK, OFFSETS, Path, Row, SAMPLE_RATE, SIBLING_LATENCY};
