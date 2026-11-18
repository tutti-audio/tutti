//! The HRIR byte format's words: little-endian `u32` counts and indices and `f32` positions and
//! samples. The sphere holds positions and samples as `f64`; a word widens on the way in and
//! narrows on the way out here.

use super::HrtfBinauralError;

/// A little-endian cursor that reports a short read as an error, never a
/// panic.
pub(super) struct Cursor<'a> {
    pub(super) bytes: &'a [u8],
    pub(super) at: usize,
}

impl Cursor<'_> {
    fn word(&mut self) -> Result<[u8; 4], HrtfBinauralError> {
        let word = self
            .bytes
            .get(self.at..self.at + 4)
            .and_then(|w| <[u8; 4]>::try_from(w).ok())
            .ok_or(HrtfBinauralError::Truncated {
                needed: self.at + 4,
                found: self.bytes.len(),
            })?;
        self.at += 4;
        Ok(word)
    }

    pub(super) fn u32(&mut self) -> Result<u32, HrtfBinauralError> {
        self.word().map(u32::from_le_bytes)
    }

    /// A sample or position word, widened.
    pub(super) fn float(&mut self) -> Result<f64, HrtfBinauralError> {
        self.word().map(|w| f64::from(f32::from_le_bytes(w)))
    }
}
/// Appends `x` as an `f32` word.
pub(super) fn put(bytes: &mut Vec<u8>, x: f64) {
    bytes.extend_from_slice(&(x as f32).to_le_bytes());
}
