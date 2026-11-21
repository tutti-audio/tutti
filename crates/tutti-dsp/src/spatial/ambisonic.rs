//! Ambisonics in the AmbiX convention: ACN channel order, SN3D scaling or N3D, orders 0
//! to 3.
//!
//! A [`Harmonics`] is the gains a plane wave takes into the channels, and an
//! [`AmbisonicEncoder`] plays a mono source through them. A [`Rotation`] turns the whole
//! sound field about the listener (yaw, pitch, roll) and an [`AmbisonicRotator`] glides
//! between two of them. An [`AmbisonicDecoder`] feeds a speaker array, by the sampling
//! rule or by AllRAD over the array's VBAP.
//!
//! The order and the channel count are values (`AmbisonicOrder`): the channels of order
//! `n` are the `(n + 1)^2` first.

mod decoder;
mod harmonics;
mod rotation;

pub use decoder::AmbisonicDecoder;
pub use harmonics::{AmbisonicEncoder, HarmonicNormalization, Harmonics};
pub use rotation::{AmbisonicRotator, Rotation};

#[cfg(test)]
mod tests;
