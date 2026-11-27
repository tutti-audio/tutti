//! The contract every format's writer keeps.

use crate::error::Result;
use tutti_buffers::{Interleaved, Sample};
use tutti_units::{SampleRate, Samples, Seconds};

/// The audio a WAV or AIFF writer takes between two back-patches of its sizes.
const REFRESH_PERIOD: Seconds = Seconds::new(2.0);

/// Counts the frames written since a file's sizes were last back-patched, in
/// frames at the file's rate and never in wall-clock time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Refresh {
    period: Samples,
    since: Samples,
}

impl Refresh {
    /// A counter that comes due after [`REFRESH_PERIOD`] of audio at `rate`.
    pub(crate) fn new(rate: SampleRate) -> Self {
        Self {
            period: REFRESH_PERIOD.to_samples(rate),
            since: Samples::ZERO,
        }
    }

    /// Counts `frames` more; true, and the count restarts, when a period of
    /// audio has gone by since the last refresh.
    pub(crate) fn add(&mut self, frames: Samples) -> bool {
        self.since += frames;
        let due = self.since >= self.period;
        if due {
            self.since = Samples::ZERO;
        }
        due
    }
}

/// A `FormatWriter` writes blocks of interleaved samples into its sink as one
/// file of its format.
///
/// The writer is made by its own `create`, which writes the header. It takes the
/// blocks it is given, at the rate and width its spec states: it does not pull,
/// resample or dither, and nothing here stops a caller from handing it blocks
/// that were not (the pipeline in front of it is the caller's, and
/// `tutti-export` has one path through its own). [`write`](Self::write) is
/// fallible, so a full disk in the middle of a take is an error at the block that
/// hit it; [`finish`](Self::finish) takes the writer by value, so a file is
/// closed exactly once, and it closes what was written even after a failed
/// `write`, handing the sink back. A writer dropped before `finish` leaves the
/// sink as it is (WAV also back-patches its sizes, best effort, so the take
/// stays readable). A WAV or AIFF writer also back-patches its sizes and
/// flushes its sink every 2 s of audio written, so a process that dies without
/// unwinding leaves a file that describes the frames up to the last refresh;
/// a power loss can still lose what the OS had not written.
///
/// The sink bounds belong to each implementation, not to the trait: a format
/// that needs more of its sink asks for more on its own `impl`.
///
/// [`Encoder`](crate::Encoder) is the one type that holds any of them.
pub(crate) trait FormatWriter: Sized {
    /// The sink the file is written to, handed back by [`finish`](Self::finish).
    type Sink;

    /// Lowers the number of frames the file may hold to `frames`, for a format
    /// whose container has a size limit. A format without one holds any count
    /// and ignores this.
    fn limit_frames(&mut self, _frames: u64) {}

    /// Writes one block of any [`Sample`] format, converting each value to the
    /// file's width by the rule in [`Sample`]; a value already in the file's
    /// format is unchanged. An empty block writes nothing.
    ///
    /// # Errors
    ///
    /// The sink's failure, or the codec's refusal of the block. After an error
    /// the file is not a complete take.
    fn write<S: Sample>(&mut self, block: Interleaved<'_, S>) -> Result<()>;

    /// Closes the file: back-patches its sizes and flushes the sink, which is
    /// returned positioned at the end of the file.
    ///
    /// # Errors
    ///
    /// The sink's failure, or the codec's.
    fn finish(self) -> Result<Self::Sink>;
}
