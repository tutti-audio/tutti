//! The offline analyses: each reads a whole buffer, or a curve another
//! analysis made, and returns its result in one call.

pub(crate) mod align;
pub(crate) mod chord;
pub(crate) mod features;
pub(crate) mod key;
pub(crate) mod mfcc;
pub(crate) mod notes;
pub(crate) mod pyin;
pub(crate) mod tempo;
pub(crate) mod transform;
pub(crate) mod xcorr;
pub(crate) mod yin;
