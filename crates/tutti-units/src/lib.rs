#![doc = include_str!("../README.md")]
//! ## What this is
//!
//! The measurement vocabulary every other tutti crate shares rather than
//! duplicating: the `Unit` marker trait, the quantities themselves (`Hz`,
//! `Seconds`, `Db`, `Beat`, `Bpm`, `SampleRate`, ...), the bar counts `BarNumber` and `BarCount`, the integer frame count
//! `Samples` and its partners `Frame` and `At`, the `Latency` and `Tail` a node
//! reports, `Text`, a bounded UTF-8 string held inline, and `UnitParam`, the name of the quantity a parameter carries. Use
//! it directly when you need only the vocabulary (a crate that only needs
//! `Beat`); otherwise reach it through the `tutti` umbrella.
//!
//! **The root is the API.** Every name is exported there, so `tutti_units::Bpm`
//! and `tutti_units::Samples` resolve directly; the modules are private.
//! [`ranged!`] is the one way any crate writes a range-checked type of its own.
//!
//! ## What this crate does not own
//!
//! Anything that allocates, shares state between threads, renders, decodes or
//! opens a device. It is the **root leaf**: depending on no workspace crate (its
//! outside dependencies are `libm` and `thiserror`, plus `serde` behind its
//! feature), `no_std` with no `alloc`, no atomics, and `#![forbid(unsafe_code)]`.
//! Unit newtypes are mandatory across the engine, so their home has to be
//! reachable from everywhere: a crate that only needs `Beat` gets `Beat` and
//! nothing else.
//!
//! ## Example: the measurement vocabulary
//!
//! Cross a family boundary with the **named converter**, never with arithmetic
//! on the inner float.
//!
//! ```rust
//! use tutti_units::{Cents, Db, SampleRate, Samples, Seconds, Semitones};
//!
//! // dB → linear gain. `Db` is logarithmic, so this is a conversion, not a cast.
//! let trim = Db::new(-6.0);
//! assert!((trim.to_amplitude().get() - 0.501_187).abs() < 1e-5);
//!
//! // Cascaded gain stages ADD in dB: that operator is opted in because it
//! // means something. Multiplication is NOT: scaling the dB value squares the
//! // amplitude, so `trim * 2.0` would be a quarter of the signal, not half of
//! // it. `Db` has no `Mul`; a factor of two lives in the amplitude domain.
//! assert_eq!(trim + trim, Db::new(-12.0));
//! let quartered = Db::new(-12.0).to_amplitude().get();
//! let squared = trim.to_amplitude().get() * trim.to_amplitude().get();
//! assert!((quartered - squared).abs() < 1e-6);
//!
//! // Seconds → frames. The rounding is IN THE NAME because allocating a delay
//! // line and counting elapsed frames want different answers from one span.
//! let rate = SampleRate::SR_48K;
//! let block = Seconds::new(0.010_51); // 504.48 frames
//! assert_eq!(block.to_samples_floor(rate), Samples::new(504)); // frames elapsed
//! assert_eq!(block.to_samples_ceil(rate), Samples::new(505)); // frames to allocate
//!
//! // Pitch offsets convert too: `cents.get() / 100.0` would compile and return
//! // `Cents`, a value wrong by 100x whose type claims it is fine.
//! assert_eq!(Cents::new(1200.0).to_semitones(), Semitones::new(12.0));
//! ```
//!
//! ## Units are labels
//!
//! A unit is a name on a float, not a validated value: `Pan::new(5.0)` builds. The
//! range a type documents is what its `new_clamped` or wrap produces, and
//! `new_clamped` passes NaN through (see the `units` module docs). The types that
//! do validate are `SampleRate`, `Bpm`, `Depth`, `Bipolar`, `Metres`, `Speed`,
//! `FrequencyRatio` and `ModulationIndex`: `SampleRate::new` (finite and above zero)
//! returns `None` for a raw it cannot hold.
//!
//! ## Operators are opt-in, and every omission is deliberate
//!
//! A unit gets exactly the algebra it has. `Db * 2.0`, `Beat + Beat` and
//! `Azimuth: Ord` do not exist, because each would compile and mean the wrong
//! thing (a squared amplitude, two positions added with no origin, an ordering
//! on a circle). Each type's docs name what it omits and the named method that
//! replaces it, so the answer to a missing operator is always a method, never an
//! escape to raw `f32`.
//!
//! ## Features
//!
//! Off by default (`default = []`).
//!
//! - `serde`: `Serialize`/`Deserialize` on the unit newtypes, `Samples`,
//!   `Resyncs`, `Frame`, `Latency`, `Tail` and `Text`. Not `UnitParam`. A float
//!   unit serializes and deserializes refusing NaN and keeping an infinity.
#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

// `units` is declared first and `#[macro_use]`d so `unit!`, the macro that
// defines every quantity, is in scope for the modules below: `macro_rules!`
// are textually scoped, so a module declared *before* the one defining them
// cannot see them.
//
// The macro stays crate-private rather than `#[macro_export]`ed: a name as
// generic as `unit!` would sit permanently at the root of a published crate,
// un-renameable and un-feature-gateable.
#[macro_use]
mod units;

// `ranged!` is `#[macro_export]`ed: it is the one way any crate writes a
// range-checked type, used as `tutti_units::ranged!`.
mod maths;
mod normalized;
mod ranged;
mod real;

mod bars;
mod blocks;
mod calls;
mod chunks;
mod frame;
mod latency;
mod losses;
mod play;
mod resyncs;
mod samples;
mod slope;
mod tail;
mod text;
mod unit_param;

pub use bars::{BarCount, BarNumber};
pub use blocks::Blocks;
pub use calls::Calls;
pub use chunks::Chunks;
pub use frame::{At, Frame, Moment};
pub use latency::Latency;
pub use losses::Losses;
pub use normalized::{Normalized, NormalizedOutOfRange};
pub use play::PlayReading;
pub use real::Real;
pub use resyncs::Resyncs;
pub use samples::Samples;
pub use slope::slope;
pub use tail::Tail;
pub use text::{Text, TextError};
pub use unit_param::{ParamAddr, ParamId, ParamKey, UnitAddr, UnitParam, UnitParamOutOfRange};
pub use units::Quantity;
pub use units::{
    Amplitude, ArcDegrees, Azimuth, Balance, Ballistics, Beat, BeatDuration, BeatsPerCycle,
    BeatsPerCycleOutOfRange, Bipolar, BipolarOutOfRange, BitDepth, Bpm, BpmOutOfRange, Cents,
    CentsSpan, CentsSpanOutOfRange, CompressionRatio, Confidence, Correlation, Db, Depth,
    DepthOutOfRange, Direction, Drive, Elevation, Feedback, FramesPerBeat, FramesPerBeatOutOfRange,
    FrequencyRatio, FrequencyRatioOutOfRange, Gain, HoldFactor, Hz, Metres, MetresOutOfRange, Mix,
    ModulationIndex, ModulationIndexOutOfRange, Pan, Phase, PhaseIncrement, PhaseStep,
    PlaybackRate, PlaybackRateOutOfRange, Q, Radians, ReadPosition, ReadRate, Resonance,
    RunningPhase, SamplePosition, SampleRate, SampleRateOutOfRange, SampleSpan,
    SampleSpanOutOfRange, Seconds, Semitones, Speed, SpeedOutOfRange, Spread, SrcRatio,
    StereoBalance, StereoWidth, StretchFactor, TimecodeFrames, Unit, Velocity,
};
