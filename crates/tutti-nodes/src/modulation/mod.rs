//! Time-modulation effects: chorus, flanger and phaser.
//!
//! All three sweep something with an internal LFO and blend the result back
//! against the dry signal, but they split into two mechanisms.
//! [`ModDelayNode`] sweeps a **delay time** — chorus and flanger are the same
//! node with different [`Sweep`](tutti_dsp::Sweep)s, chorus sitting at a longer base
//! delay (10 ms) for thickening and flanger at a short one (1 ms) for the swept
//! comb notches. [`PhaserNode`] sweeps the centre of an **all-pass chain**
//! instead, so it notches by phase cancellation and uses no delay line at all.
//!
//! Both are width-generic, with per-channel phase offsets, and the LFO is state on the
//! stream's clock, so what is heard does not depend on how the stream is cut into blocks:
//! [`ModDelayNode`] and [`PhaserNode`] are each a [`KernelNode`](crate::KernelNode).
//!
//! Consequence worth knowing: chorus and flanger take their depth in
//! [`Seconds`](tutti_units::Seconds) of delay sweep, while the phaser's is a
//! unitless [`Bipolar`](tutti_units::Bipolar) fraction of its frequency range.

mod mod_delay;
mod phaser;
mod sweep_clock;

pub use mod_delay::{ModDelayConfig, ModDelayKernel, ModDelayNode, ModDelaySet};
pub use phaser::{PhaserConfig, PhaserKernel, PhaserNode, PhaserSet};
