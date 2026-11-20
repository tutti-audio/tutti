//! Where a take belongs on the graph's clock: the latencies it is placed by.
//!
//! One [`TakeLatency`] places an audio take and a MIDI tap alike: the frame the
//! inlet emitted, less the device's input and output latency and the inlet's
//! lead, plus the user's [`Trim`].

use tutti_units::{Frame, Samples};

/// A manual shift of where a take is placed, either way: a trim that can only
/// move later could not undo a device that reports too much latency.
///
/// ```
/// use tutti_session::Trim;
/// use tutti_units::Samples;
///
/// assert_eq!(Trim::default(), Trim::NONE);
/// assert_eq!(Trim::Earlier(Samples::new(64)).frames(), -64);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trim {
    /// Place the take this many frames later than the reported latencies say.
    Later(Samples),
    /// Place it this many frames earlier.
    Earlier(Samples),
}

impl Trim {
    /// No trim.
    pub const NONE: Trim = Trim::Later(Samples::ZERO);

    /// The trim as a signed frame count, positive for later.
    #[must_use]
    pub fn frames(self) -> i128 {
        match self {
            Self::Later(n) => n.get() as i128,
            Self::Earlier(n) => -(n.get() as i128),
        }
    }
}

impl Default for Trim {
    fn default() -> Self {
        Self::NONE
    }
}

/// The latency of a take's placement, each as the take found it: a reading, read
/// from `TakePlacement::latency` or `Recorded::latency` (feature `capture`), or
/// made by `TakeLatency::new` for a MIDI tap's frames (`Midi::tap`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct TakeLatency {
    /// The capture's delay: how long before a graph block a frame was sung.
    /// `None`: the input has not reported one.
    pub input: Option<Samples>,
    /// The playback's delay: how long after a block was rendered it was
    /// heard. `None`: the output has not reported one.
    pub output: Option<Samples>,
    /// The inlet's fixed lead (`OpenedInput::lead`).
    pub lead: Samples,
    /// The input's manual trim (`InputConfig::offset`).
    pub offset: Trim,
}

impl TakeLatency {
    /// The latency of a placement.
    #[must_use]
    pub fn new(
        input: Option<Samples>,
        output: Option<Samples>,
        lead: Samples,
        offset: Trim,
    ) -> Self {
        Self {
            input,
            output,
            lead,
            offset,
        }
    }

    /// Whether both latencies were reported. When they were not, the start is
    /// placed by the figures that were reported, and is that much later than it belongs.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.input.is_some() && self.output.is_some()
    }

    /// The start this latency puts a take whose first frame is graph frame
    /// `first` at: `first - (in + out + lead) + offset`, with an unreported
    /// latency left out.
    #[must_use]
    pub fn place(&self, first: Frame) -> Start {
        let behind = self.input.map_or(0, Samples::get) as i128
            + self.output.map_or(0, Samples::get) as i128
            + self.lead.get() as i128;
        let at = i128::from(first.get()) - behind + self.offset.frames();
        match u64::try_from(at) {
            Ok(frame) => Start::At(Frame::new(frame)),
            Err(_) => Start::BeforeOrigin(Samples::new(usize::try_from(-at).unwrap_or(usize::MAX))),
        }
    }
}

/// A frame on the graph's clock that a take's start can fall before: a take
/// armed in the first moments of a session, placed earlier than it began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// The frame.
    At(Frame),
    /// Before the clock's origin by this many frames.
    BeforeOrigin(Samples),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A start is `F - (input + output + lead) + offset`**, kept as before the
    /// origin rather than clamped: 1,000 - (240 + 480 + 7) = 273, and with a 10
    /// frame trim later 283; at 100 it is 627 before the origin. A latency not
    /// reported leaves `is_complete` false and is left out of the sum.
    #[test]
    fn a_start_is_the_documented_sum_and_before_the_origin_is_stated() {
        let latency = TakeLatency::new(
            Some(Samples::new(240)),
            Some(Samples::new(480)),
            Samples::new(7),
            Trim::NONE,
        );
        assert_eq!(latency.place(Frame::new(1_000)), Start::At(Frame::new(273)));
        assert_eq!(
            latency.place(Frame::new(100)),
            Start::BeforeOrigin(Samples::new(627))
        );
        assert!(latency.is_complete());
        let trimmed = TakeLatency {
            offset: Trim::Later(Samples::new(10)),
            ..latency
        };
        assert_eq!(trimmed.place(Frame::new(1_000)), Start::At(Frame::new(283)));
        let partial = TakeLatency::new(Some(Samples::new(240)), None, Samples::new(0), Trim::NONE);
        assert!(!partial.is_complete());
        assert_eq!(partial.place(Frame::new(1_000)), Start::At(Frame::new(760)));
        assert_eq!(
            TakeLatency::default().place(Frame::new(77)),
            Start::At(Frame::new(77)),
            "no compensation, no move"
        );
    }
}
