#![doc = include_str!("../README.md")]
//! The easing laws of the engine, and the automation trajectory built on them.
//!
//! A [`Law`] is a function `g` of progress `t` in `[0, 1]` with `g(0) = 0` and
//! `g(1) = 1`: how a value moves between two endpoints. It is a closed set of
//! six, one `Copy` value a few words long, so a parameter segment on the audio
//! thread carries one without an `Arc<dyn _>`. The graph's parameter ports, the
//! modulation shaping and the automation lanes all evaluate the same `Law`.
//!
//! A [`FadeLaw`] is the gain pair of a fade between two signals, and
//! [`DECLICK`] its length in time: one law and one length for every fade of the
//! engine. A ramp, continuous interpolation chained block after block, is a
//! linear [`Piece`] ([`Piece::ramp_at`]).
//!
//! A [`Piece`] is a value moving from one endpoint to another along a `Law`, read at
//! a progress: the one formula every consumer evaluates.
//!
//! A [`FrameProgress`] is how far a move of some frames has got: what a glide steps.
//!
//! A [`Trajectory`] is the least a lane or a recorder needs of automation:
//! breakpoints in [`Beat`](tutti_units::Beat), each with a value in the unit
//! the trajectory automates and the `Law` of the piece it starts, read at a beat.
//!
//! ```
//! use tutti_curve::{Breakpoint, Exponent, Law, Piece, Trajectory};
//! use tutti_units::{Beat, Hz, Normalized};
//!
//! assert_eq!(Piece::new(0.0, 8.0).law(Law::PowerIn(Exponent::of(2.0))).value(None, Normalized::of(0.5)), 2.0);
//!
//! let mut lane = Trajectory::new();
//! lane.add_breakpoint(Breakpoint::new(Beat::new(0.0), Hz::new(0.0)).law(Law::PowerIn(Exponent::of(2.0))));
//! lane.add_breakpoint(Breakpoint::new(Beat::new(4.0), Hz::new(1.0)));
//! assert_eq!(lane.value_at(Beat::new(2.0)), Some(Hz::new(0.25)));
//! ```
#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod fade;
mod frame_progress;
mod law;
mod parameter;
mod piece;
mod trajectory;

pub use fade::{DECLICK, FadeLaw, declick_frames};
pub use frame_progress::FrameProgress;
pub use law::Law;
pub use parameter::{Curvature, CurvatureError, Exponent, ExponentError};
pub use piece::Piece;
pub use trajectory::{Breakpoint, Trajectory};
