use tutti_units::{Normalized, SampleSpan, Samples};

/// How far a move of `length` frames has got: the one counter a glide steps.
///
/// `length` is a fractional frame count (a glide of 1.5 frames arrives on the
/// second frame). The move has arrived once `done` reaches `length`;
/// [`get`](Self::get) is then 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameProgress {
    done: SampleSpan,
    length: SampleSpan,
}

impl FrameProgress {
    /// A move of `length`, not started. A length of no frames is one frame long.
    #[must_use]
    pub fn new(length: SampleSpan) -> Self {
        Self {
            done: SampleSpan::ZERO,
            length: if length > SampleSpan::ZERO {
                length
            } else {
                SampleSpan::from_samples(Samples::new(1))
            },
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
    pub fn advance(&mut self, frames: Samples) -> Samples {
        let remaining = Samples::new(self.length.saturating_sub(self.done).get().ceil() as usize);
        let taken = frames.min(remaining);
        self.done += SampleSpan::from_samples(taken);
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fractional_length_arrives_on_the_frame_that_passes_it() {
        let mut p = FrameProgress::new(SampleSpan::clamped(2.5));
        assert_eq!(p.advance(Samples::new(2)), Samples::new(2));
        assert!(!p.has_arrived());
        assert_eq!(p.advance(Samples::new(5)), Samples::new(1));
        assert!(p.has_arrived());
        assert_eq!(p.get().get(), 1.0);
    }

    #[test]
    fn progress_is_the_share_of_the_length_stepped() {
        let mut p = FrameProgress::new(SampleSpan::clamped(8.0));
        assert_eq!(p.get().get(), 0.0);
        p.advance(Samples::new(2));
        assert_eq!(p.get().get(), 0.25);
    }

    #[test]
    fn a_length_of_no_frames_is_one_frame() {
        let mut p = FrameProgress::new(SampleSpan::ZERO);
        assert_eq!(p.advance(Samples::new(3)), Samples::new(1));
        assert!(p.has_arrived());
    }
}
