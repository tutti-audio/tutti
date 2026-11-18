#![doc = include_str!("../README.md")]
//! The shapes and cells shared by every engine crate: channel layouts, musical
//! time, notes and the atomic parameter cell, over the unit newtypes of
//! `tutti-units`.
//!
//! ## What this is
//!
//! The common definitions every other Tutti crate shares rather than duplicating.
//! Use it directly when you need only the vocabulary; otherwise reach it through
//! the `tutti` umbrella, which re-exports it as `tutti::types`. What is in it:
//!
//! - **`value`**: what a parameter is and how it is carried: the atomic
//!   `Param<U>` cell that holds a unit, `AtomicSampleRate`, `Note` and `Adsr`,
//!   and the frame clock (`FrameClock`, `LoopRange`, `TimelineSegment`) that
//!   places beats on frames.
//! - **Channels**: `ChannelLayout`, a `u16` channel count with named constants
//!   (`MONO`, `STEREO`, ...): the one answer to "how many channels?" that every
//!   subsystem shares instead of a bare integer. `ChannelTopology` + `Speaker`
//!   answer the *different* question of which speaker each channel feeds; distinct
//!   questions, so distinct types.
//! - **Ids**: `Mint`, a lock-free counter of unique non-zero numbers from 1, and
//!   `id!`, the macro that writes an id type minted from one.
//! - **`meter`**: musical meter: `TimeSignature`, the `MeterMap` timeline of
//!   changes, and the `Meter` trait that turns a `Beat` into a bar and beat. Pure
//!   musical math, so it layers *over* a transport rather than living inside one.
//!
//! The unit newtypes themselves (`Hz`, `Seconds`, `Db`, `Beat`, `Samples`, ...)
//! and `ranged!` are `tutti-units`'; name that crate for them.
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
//! `atomic_float`, `libm` and `thiserror`, plus `serde` behind its feature),
//! `no_std` with `alloc`, and `#![forbid(unsafe_code)]`. The lock-free and `unsafe`
//! runtime primitives, the graph runtime, the engine, the device and the file
//! codecs are other crates'. The exception is `Mint`, a bare atomic counter:
//! it has no protocol to model, like `Param` and `AtomicSampleRate`, so it
//! needs no loom model and sits with the vocabulary.
//!
//! ## Example: a layout and a bar position
//!
//! ```rust
//! use tutti_types::{BeatsPerBar, ChannelLayout, Meter, TimeSignature};
//! use tutti_units::{BarNumber, Beat};
//!
//! assert_eq!(ChannelLayout::STEREO.count(), 2);
//!
//! // In 4/4, beat 8.0 of the timeline opens the third bar.
//! let pos = TimeSignature::of(4, 4).bar_at(Beat(8.0));
//! assert_eq!(pos.bar, BarNumber(3));
//! assert_eq!(pos.beat, BeatsPerBar::of(1));
//! ```
//!
//! ## Features
//!
//! Off by default (`default = []`).
//!
//! - `serde`: `Serialize`/`Deserialize` on the wire- and document-carried types:
//!   `Note`, `ChannelLayout`, `ChannelTopology` and the meter vocabulary, and
//!   `tutti-units`' `serde` with them. Not `Param` or `LoopRange`.
// Catches a doc comment attached to the wrong item (say, an `impl Debug` right
// below a struct), which leaves the type undocumented in rustdoc while it looks
// documented in the source. Nothing else catches that.
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

mod meter;

// The cell a `Param` shares (`Param::as_atomic`), named here so a crate that
// addresses params without their unit types need not depend on `atomic_float`
// itself.
pub use atomic_float::{AtomicF32, AtomicF64};

// The counter ids and file-name suffixes are numbered from.
pub use mint::Mint;

// Value vocabulary.
pub use value::{
    Adsr, AtomicSampleRate, InvalidMaxBlock, MaxBlock, NotOnMidiScale, Note, NoteNumberOutOfRange,
    Param, Param64, ParamCell, ParamRaw, ParkedSampleRate, PitchClass,
};

// The frame is the source of truth for time: the one frame↔beat conversion,
// and the one rule for which frame a beat lands on.
pub use value::{
    FRAME_TOLERANCE, FrameClock, LoopRange, LoopRangeError, Pass, PassEnd, SegmentOrigin,
    TimelineSegment, beats_per_sample, first_frame_at_or_after, frames_per_beat,
    snap_to_whole_frame,
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

// Musical meter.
pub use meter::{
    BarPosition, BeatsPerBar, InvalidBeatsPerBar, InvalidNoteValue, MAX_BEATS_PER_BAR,
    MAX_NOTE_VALUE, MIN_NOTE_VALUE, Meter, MeterChange, MeterMap, NoteValue, TimeSignature,
};
