#![doc = include_str!("../README.md")]
//! Audio sample formats and the borrowed views over them.
//!
//! A [`Sample`] is a storage format: `i8`, `i16`, [`I24`], `i32`, `f32` or
//! `f64`. A [`Float`] is a `Sample` that arithmetic runs in (`f32` or `f64`),
//! with its math in `libm`. The two never merge: a format says what a buffer
//! holds, a float says what a node computes in.
//!
//! The views borrow a slice and allocate nothing; each is generic over the
//! format, defaulting to `f32`:
//!
//! - [`Mono`] / [`MonoMut`]: one channel;
//! - [`Interleaved`] / [`InterleavedMut`]: whole frames in one slice, carrying
//!   their [`ChannelLayout`];
//! - [`Planes`] / [`PlanesMut`]: one equal-length slice per channel, and
//!   [`StereoPlanes`], exactly two;
//! - [`InputOutput`]: one block's frame count with its input and output planes.
//!
//! [`Wave`] is the owned counterpart: a resident planar buffer of one format that
//! hands those views out and takes frames in.
//!
//! Construction checks once and returns a [`BufferError`] for a zero width, a
//! length that is not whole frames or a ragged plane; nothing truncates. Every
//! count is [`Samples`](tutti_units::Samples), in frames.
//!
//! [`Source`] and [`Sink`] are the edges: a stream pulled into, or written from,
//! an interleaved view of the implementor's format, on a clock the graph does
//! not keep. A [`Pull`] says whether an empty read is late, finished or broken.
//!
//! [`convert()`], [`convert_counting_clips`], [`convert_mono`] and
//! [`convert_planes`] change format, [`interleave`] and [`deinterleave`] change
//! layout and format in one pass; a mismatch is an error, never a short copy.
//! [`Fold`] turns one channel topology into another, built once on the control
//! side.
//!
//! ```
//! use tutti_buffers::{convert, deinterleave, Interleaved, InterleavedMut, PlanesMut};
//! use tutti_buffers::ChannelLayout;
//!
//! let pcm = [16384i16, -16384, 8192, 0]; // two stereo frames
//! let mut audio = [0.0f32; 4];
//! convert(
//!     Interleaved::new(&pcm, ChannelLayout::STEREO)?,
//!     InterleavedMut::new(&mut audio, ChannelLayout::STEREO)?,
//! )?;
//! assert_eq!(audio, [0.5, -0.5, 0.25, 0.0]);
//!
//! let (mut l, mut r) = ([0.0f32; 2], [0.0f32; 2]);
//! let mut planes = [&mut l[..], &mut r[..]];
//! deinterleave(
//!     Interleaved::new(&audio, ChannelLayout::STEREO)?,
//!     PlanesMut::new(&mut planes)?,
//! )?;
//! assert_eq!((l, r), ([0.5, 0.25], [-0.5, 0.0]));
//! # Ok::<(), tutti_buffers::BufferError>(())
//! ```
#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

mod ambisonic;
mod bed;
mod bounds;
mod channel_set;
mod channels;
mod convert;
mod edge;
mod error;
mod fold;
mod i24;
mod input_output;
mod interleaved;
mod max_block;
mod mono;
mod planes;
mod sample;
mod stereo;
mod topology;
mod wave;

pub use ambisonic::{
    AmbisonicFormat, AmbisonicNormalization, AmbisonicOrder, AmbisonicOrderOutOfRange,
    AmbisonicOrdering,
};
pub use bed::Bed;
pub use channel_set::ChannelSet;
pub use channels::{
    ChannelIndex, ChannelIndexOutOfRange, ChannelLayout, LayoutError, MAX_CHANNELS,
};
pub use convert::{
    convert, convert_counting_clips, convert_mono, convert_planes, deinterleave, interleave,
};
pub use edge::{Pull, Sink, SinkFailed, Source, Stop};
pub use error::BufferError;
pub use fold::{Fold, FoldEnd, FoldError, FoldLevel, FoldLevels, FoldSpec};
pub use i24::{I24, I24OutOfRange};
pub use input_output::{BlockMut, BlockSide, InputOutput};
pub use interleaved::{Frames, FramesMut, Interleaved, InterleavedMut};
pub use max_block::{InvalidMaxBlock, MaxBlock};
pub use mono::{Mono, MonoMut};
pub use planes::{PlaneIter, PlaneIterMut, Planes, PlanesMut};
pub use sample::{ByWidth, Float, FloatWidth, NotAFloat, OverFloat, Sample, SampleFormat};
pub use stereo::StereoPlanes;
pub use topology::{ChannelTopology, Placement, Speaker, Speakers, TopologyError};
pub use wave::Wave;

#[cfg(test)]
mod error_bounds {
    //! Each error type this crate returns is an `Error` a caller can box and move (`?` into
    //! `Box<dyn Error + Send + Sync>`); the bound is checked where the function
    //! is written, so a non-`Send` field fails the build.

    fn err<T: std::error::Error + Send + Sync + 'static>() {}

    #[allow(dead_code)]
    fn the_errors_are_boxable() {
        err::<crate::I24OutOfRange>();
    }
}
