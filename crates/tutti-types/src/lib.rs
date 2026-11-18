#![doc = include_str!("../README.md")]
//! The cells shared by every engine crate: ids and the atomic parameter cell,
//! over the unit newtypes of `tutti-units`.
//!
//! ## What this is
//!
//! The common definitions every other Tutti crate shares rather than duplicating.
//! Use it directly when you need only the vocabulary; otherwise reach it through
//! the `tutti` umbrella, which re-exports it as `tutti::types`. What is in it:
//!
//! - **`value`**: what a parameter is and how it is carried: the atomic
//!   `Param<U>` cell that holds a unit and `AtomicSampleRate`.
//! - **Ids**: `Mint`, a lock-free counter of unique non-zero numbers from 1, and
//!   `id!`, the macro that writes an id type minted from one.
//!
//! The unit newtypes themselves (`Hz`, `Seconds`, `Db`, `Beat`, `Samples`, ...)
//! and `ranged!` are `tutti-units`'; the musical vocabulary (`Note`,
//! `TimeSignature`, `FrameClock`, ...) is `tutti-music`'s; the channel family
//! (`ChannelLayout`, `ChannelTopology`, `Speaker`, `MaxBlock`, ...) is
//! `tutti-buffers`'; name those crates for them.
//!
//! **The root is the API.** Every name is re-exported there, so
//! `tutti_types::Param` resolves directly: import from the root, the
//! modules are private. The graph as a value (`Topology`), the latency pass and
//! the tail fold are `tutti-graph`'s.
//!
//! ## What this crate does not own
//!
//! Anything that renders, decodes, or opens a device. It is engine-free: it names
//! only `tutti-units` among the workspace crates (its outside dependencies are
//! `atomic_float` and `thiserror`, plus `serde` behind its feature),
//! `no_std` with `alloc`, and `#![forbid(unsafe_code)]`. The lock-free and `unsafe`
//! runtime primitives, the graph runtime, the engine, the device and the file
//! codecs are other crates'. The exception is `Mint`, a bare atomic counter:
//! it has no protocol to model, like `Param` and `AtomicSampleRate`, so it
//! needs no loom model and sits with the vocabulary.
//!
//! ## Features
//!
//! Off by default (`default = []`).
//!
//! - `serde`: `tutti-units`' `serde`, for the unit newtypes. Not `Param`.
#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;

// `id!` is `#[macro_export]`ed: the one way any crate writes an id, used as
// `tutti_types::id!`.
mod id;
mod mint;

mod value;

// The cell a `Param` shares (`Param::as_atomic`), named here so a crate that
// addresses params without their unit types need not depend on `atomic_float`
// itself.
pub use atomic_float::{AtomicF32, AtomicF64};

// The counter ids and file-name suffixes are numbered from.
pub use mint::Mint;

// Value vocabulary.
pub use value::{AtomicSampleRate, Param, Param64, ParamCell, ParkedSampleRate};
