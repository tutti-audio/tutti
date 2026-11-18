#![doc = include_str!("../README.md")]
//! The shapes and cells shared by every engine crate: channel layouts, ids and
//! the atomic parameter cell, over the unit newtypes of `tutti-units`.
//!
//! ## What this is
//!
//! The common definitions every other Tutti crate shares rather than duplicating.
//! Use it directly when you need only the vocabulary; otherwise reach it through
//! the `tutti` umbrella, which re-exports it as `tutti::types`. What is in it:
//!
//! - **`value`**: what a parameter is and how it is carried: the atomic
//!   `Param<U>` cell that holds a unit, `AtomicSampleRate` and `MaxBlock`.
//! - **Channels**: `ChannelLayout`, a `u16` channel count with named constants
//!   (`MONO`, `STEREO`, ...): the one answer to "how many channels?" that every
//!   subsystem shares instead of a bare integer. `ChannelTopology` + `Speaker`
//!   answer the *different* question of which speaker each channel feeds; distinct
//!   questions, so distinct types.
//! - **Ids**: `Mint`, a lock-free counter of unique non-zero numbers from 1, and
//!   `id!`, the macro that writes an id type minted from one.
//!
//! The unit newtypes themselves (`Hz`, `Seconds`, `Db`, `Beat`, `Samples`, ...)
//! and `ranged!` are `tutti-units`'; the musical vocabulary (`Note`,
//! `TimeSignature`, `FrameClock`, ...) is `tutti-music`'s; name those crates for
//! them.
//!
//! **The root is the API.** Every name is re-exported there, so
//! `tutti_types::ChannelLayout` resolves directly: import from the root, the
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
//! ## Example: a layout
//!
//! ```rust
//! use tutti_types::ChannelLayout;
//!
//! assert_eq!(ChannelLayout::STEREO.count(), 2);
//! ```
//!
//! ## Features
//!
//! Off by default (`default = []`).
//!
//! - `serde`: `Serialize`/`Deserialize` on the wire-carried types:
//!   `ChannelLayout`, `ChannelTopology` and `MaxBlock`, and `tutti-units`' `serde`
//!   with them. Not `Param`.
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

// Private: everything public in these is re-exported at the root below, so the
// module path would be a second name for a type that already has one.
mod ambisonic;
mod bed;
mod bus;
mod channels;
mod topology;

// The cell a `Param` shares (`Param::as_atomic`), named here so a crate that
// addresses params without their unit types need not depend on `atomic_float`
// itself.
pub use atomic_float::{AtomicF32, AtomicF64};

// The counter ids and file-name suffixes are numbered from.
pub use mint::Mint;

// Value vocabulary.
pub use value::{
    AtomicSampleRate, InvalidMaxBlock, MaxBlock, Param, Param64, ParamCell, ParamRaw,
    ParkedSampleRate,
};

// Channel layout — how many channels.
pub use bus::{BusTooWide, BusTopology};
pub use channels::{
    ChannelIndex, ChannelIndexOutOfRange, ChannelLayout, LayoutError, MAX_CHANNELS,
};

// Channel topology — which speaker each channel feeds. Distinct from the count
// above, and additive to it: a width-only caller keeps using `ChannelLayout`.
pub use ambisonic::{
    AmbisonicFormat, AmbisonicNormalization, AmbisonicOrder, AmbisonicOrderOutOfRange,
    AmbisonicOrdering,
};
pub use bed::Bed;
pub use topology::{ChannelTopology, Placement, Speaker, Speakers, TopologyError};
