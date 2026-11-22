use tutti_units::Normalized;

/// How far a move of `length` frames has got: the one counter a glide steps.
///
/// `length` is a frame count that may be fractional (a glide of 1.5 frames
/// arrives on the second frame). The move has arrived once `done` reaches
/// `length`; [`get`](Self::get) is then 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameProgress {
    done: f64,
    length: f64,
}

impl FrameProgress {
    /// A move of `length` frames, not started. A length that is not above 0 is
    /// one frame long.
    #[must_use]
    pub fn new(length: f64) -> Self {
        Self {
            done: 0.0,
            length: if length > 0.0 { length } else { 1.0 },
        }
    }

    /// The share of the move done, in `[0, 1]`.
    #[inline]
    #[must_use]
    pub fn get(self) -> Normalized {
        Normalized::clamped(self.done / self.length)
    }

    /// Whether every frame of the move has been stepped.
    #[inline]
    #[must_use]
    pub fn has_arrived(self) -> bool {
        self.done >= self.length
    }

    /// Steps up to `frames` frames, stopping at arrival, and returns how many
    /// were taken: fewer than `frames` when the move arrived on the way.
    pub fn advance(&mut self, frames: usize) -> usize {
        let remaining = (self.length - self.done).max(0.0).ceil();
        let taken = frames.min(remaining as usize);
        self.done += taken as f64;
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fractional_length_arrives_on_the_frame_that_passes_it() {
        let mut p = FrameProgress::new(2.5);
        assert_eq!(p.advance(2), 2);
        assert!(!p.has_arrived());
        assert_eq!(p.advance(5), 1);
        assert!(p.has_arrived());
        assert_eq!(p.get().get(), 1.0);
    }

    #[test]
    fn progress_is_the_share_of_the_length_stepped() {
        let mut p = FrameProgress::new(8.0);
        assert_eq!(p.get().get(), 0.0);
        p.advance(2);
        assert_eq!(p.get().get(), 0.25);
    }

    #[test]
    fn a_length_of_no_frames_is_one_frame() {
        let mut p = FrameProgress::new(0.0);
        assert_eq!(p.advance(3), 1);
        assert!(p.has_arrived());
    }
}
