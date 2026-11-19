//! The meters: each config is a [`Measure`](crate::Measure) whose meter folds
//! chunks of a running stream into a reading, with a one-shot entry point over a
//! whole buffer.

pub(crate) mod chroma;
pub(crate) mod loudness;
pub(crate) mod onset;
pub(crate) mod peaks;
pub(crate) mod silence;
pub(crate) mod spectrum;
pub(crate) mod stereo;
pub(crate) mod vectorscope;
