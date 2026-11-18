//! Value vocabulary: what a parameter is and how it is carried, over the unit
//! newtypes of `tutti_units`.
//!
//! - [`param`]: [`Param`], a lock-free atomic cell holding a `Unit` value,
//!   shareable with the audio thread (load / store / handle), and
//!   [`Param64`], its `f64` neighbour.
//! - [`atomic_rate`]: [`AtomicSampleRate`] and [`ParkedSampleRate`], a
//!   `SampleRate` shared with another thread as one atomic word.
//! - [`frame_clock`]: [`FrameClock`], the playhead as a frame count, and
//!   the [`LoopRange`] it wraps at.
//! - [`timeline`]: [`TimelineSegment`], the one frame↔beat conversion, and
//!   [`first_frame_at_or_after`], the one rule for the frame a beat lands on.
//! - [`note`], [`adsr`]: [`Note`] and [`Adsr`], the pitch and envelope
//!   vocabulary.
//! - [`max_block`]: [`MaxBlock`], the largest block a node is asked to render.

pub mod adsr;
pub mod atomic_rate;
pub mod frame_clock;
pub mod max_block;
pub mod note;
pub mod param;
pub mod timeline;

pub use adsr::Adsr;
pub use atomic_rate::{AtomicSampleRate, ParkedSampleRate};
pub use frame_clock::{FrameClock, LoopRange, LoopRangeError, Pass, PassEnd, SegmentOrigin};
pub use max_block::{InvalidMaxBlock, MaxBlock};
pub use note::{NotOnMidiScale, Note, NoteNumberOutOfRange, PitchClass};
pub use param::{Param, Param64, ParamCell, ParamRaw};
pub use timeline::{
    FRAME_TOLERANCE, TimelineSegment, beats_per_sample, first_frame_at_or_after, frames_per_beat,
    snap_to_whole_frame,
};
