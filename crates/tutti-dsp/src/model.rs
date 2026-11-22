//! The neutral model module: [`System`], what every processor answers, and the class
//! of one that is linear and time-invariant.
//!
//! A [`System`] is the mathematical object a processor realizes: its
//! [`latency`](System::latency) and its [`tail`](System::tail), a [`DecayTime`] measured to a
//! floor ([`TAIL_FLOOR`] by default). [`Lti`] is the class of a linear time-invariant
//! one, with the queries its mathematics has ([`frequency_response`](Lti::frequency_response),
//! [`spectral_radius`](Lti::spectral_radius), [`l1_norm`](Lti::l1_norm),
//! [`hinf_norm`](Lti::hinf_norm)).
//!
//! A class of system is a trait over structure, never one data type. [`Lti`] is the one
//! class with a trait; a system of another kind (a nonlinear one, one that varies in
//! time) answers through its own queries and is not made to implement [`Lti`].
//! Modulation is not a class: a control that moves is a trajectory of the controls,
//! written by an automation lane or a control source, and the system it moves is the same one.
//!
//! [`MaxDecay`] is the longest decay a reverb is built to.

mod lti;
mod max_decay;
mod system;

pub use lti::Lti;
pub(crate) use lti::{peak_magnitude, single_output};
pub use max_decay::{MaxDecay, MaxDecayError};
pub use system::{DecayTime, System, TAIL_FLOOR};
