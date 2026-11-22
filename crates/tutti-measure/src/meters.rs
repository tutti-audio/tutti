//! The real-time readings: what a UI meter or a stream folds as it runs. Most
//! are a config that is a [`Measure`](crate::Measure), whose meter folds chunks
//! into a reading, with a one-shot entry point over a whole buffer; `stereo` is
//! the correlation reading and the smoother a stereo meter applies to it.

pub(crate) mod chroma;
pub(crate) mod loudness;
pub(crate) mod onset;
pub(crate) mod peaks;
pub(crate) mod silence;
pub(crate) mod spectrum;
pub(crate) mod stereo;
pub(crate) mod transient;
pub(crate) mod vectorscope;
