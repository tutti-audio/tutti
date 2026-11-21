#![doc = include_str!("../README.md")]
//! ## What this is
//!
//! The musical definitions every other tutti crate shares rather than
//! duplicating, over the unit newtypes of `tutti-units`. Use it directly when
//! you need only the vocabulary; otherwise reach it through the `tutti`
//! umbrella. What is in it:
//!
//! - **Notes**: `Note` and `PitchClass`, twelve-tone equal temperament with a
//!   fallible conversion to and from a MIDI number, and `Adsr`, the four stages
//!   of an envelope.
//! - **Durations**: `NoteDuration`, a note value with a plain, dotted or triplet
//!   `NoteModifier`, converted to beats.
//! - **Meter**: `TimeSignature`, the `MeterMap` timeline of changes, and the
//!   `Meter` trait that turns a `Beat` into a bar and a beat within it. Pure
//!   musical math, so it layers *over* a transport rather than living inside one.
//! - **The frame clock**: `FrameClock`, the playhead as an integer frame count,
//!   the `LoopRange` it wraps at, and the `TimelineSegment` that is the one
//!   conversion between a frame and a beat.
//!
//! **The root is the API.** Every name is re-exported there, so
//! `tutti_music::Note` resolves directly: import from the root, the modules are
//! private.
//!
//! ## What this crate does not own
//!
//! Anything that renders, decodes, shares state between threads or opens a
//! device. It names only `tutti-units` among the workspace crates (its outside
//! dependencies are `libm` and `thiserror`, plus `serde` behind its feature),
//! is `no_std` with `alloc`, and is `#![forbid(unsafe_code)]`. The tempo map, the
//! transport and the engine that read these types are other crates'.
//!
//! ## Example: a bar position
//!
//! ```rust
//! use tutti_music::{BeatsPerBar, Meter, TimeSignature};
//! use tutti_units::{BarNumber, Beat};
//!
//! // In 4/4, beat 8.0 of the timeline opens the third bar.
//! let pos = TimeSignature::of(4, 4).bar_at(Beat::new(8.0));
//! assert_eq!(pos.bar, BarNumber::new(3));
//! assert_eq!(pos.beat, BeatsPerBar::of(1));
//! ```
//!
//! ## Features
//!
//! Off by default (`default = []`).
//!
//! - `serde`: `Serialize`/`Deserialize` on the document- and wire-carried types:
//!   `Note` and the meter vocabulary, and `tutti-units`' `serde` with them. Not
//!   `LoopRange` or `Adsr`.
#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;

// Private: everything public in these is re-exported at the root below, so the
// module path would be a second name for a type that already has one.
mod adsr;
mod duration;
mod frame_clock;
mod meter;
mod note;
mod timeline;

pub use adsr::Adsr;
pub use duration::{NoteDuration, NoteModifier};
pub use note::{NotOnMidiScale, Note, NoteNumberOutOfRange, PitchClass};

// The frame is the source of truth for time: the one frame↔beat conversion,
// and the one rule for which frame a beat lands on.
pub use frame_clock::{
    ClockPass, ClockPassEnd, FrameClock, LoopRange, LoopRangeError, SegmentOrigin,
};
pub use timeline::{
    FRAME_TOLERANCE, TimelineSegment, beats_per_sample, first_frame_at_or_after, frames_per_beat,
    snap_to_whole_frame,
};

pub use meter::{
    BarPosition, BeatsPerBar, InvalidBeatsPerBar, InvalidNoteValue, MAX_BEATS_PER_BAR,
    MAX_NOTE_VALUE, MIN_NOTE_VALUE, Meter, MeterChange, MeterMap, NoteValue, TimeSignature,
};
