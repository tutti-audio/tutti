//! What one render is, validated once: [`Job`].
//!
//! A job is the four figures every render is sized from (its rate, the
//! audible length, the latency trimmed off the head and the tail kept past the
//! end), in frames and nothing else: no seconds, no beats, no tempo. Whoever
//! knows the project's time converts at its own seam and hands a count; the
//! constructor then refuses what no render can be, so the code after it never
//! re-checks.

use tutti_units::{SampleRate, Samples};

/// What a render does when the graph it renders dropped events
/// ([`Job::event_loss`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum EventLossPolicy {
    /// The render stops at the first block that dropped an event with
    /// [`RenderError::EventsDropped`](crate::RenderError::EventsDropped), and a file it
    /// wrote is removed. [`write_buffers`](crate::write_buffers) applies it to
    /// the planes' own count.
    #[default]
    Fail,
    /// The render succeeds and says how many events were dropped
    /// ([`Rendered::dropped_events`](crate::Rendered::dropped_events),
    /// [`Written::dropped_events`](crate::Written::dropped_events)): for a caller that
    /// checks the figure itself.
    Allow,
}

/// The most frames one render may span, latency and tail included:
/// `u32::MAX`, which is 6.2 hours at 192 kHz.
///
/// A bound rather than a measure of memory: a render held in memory can fail
/// to allocate well under it ([`RenderError::RenderTooLarge`](crate::RenderError::RenderTooLarge)),
/// and one written to a file is under the WAV format's own size limit long
/// before it.
pub const MAX_FRAMES: Samples = Samples::new(u32::MAX as usize);

/// What a render keeps past the programme's end: a figure fixed before it
/// starts, or what the graph reports when the programme has ended.
///
/// Either way the render is sized and planned for [`bound`](Self::bound): the
/// file's frame limit is checked against it, buffers are reserved for it and
/// progress reports it until the programme ends. A render that ends sooner
/// holds the frames it rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailPolicy {
    /// Exactly this many frames past the end.
    Frames(Samples),
    /// The tail the graph's nodes owe once the last frame of the programme
    /// has rendered ([`tutti_graph::Executor::reported_tail`]), spent under
    /// `ceiling`: a node owes what is left of its report when nothing upstream
    /// still owes, and its declared ceiling after an upstream that does. A graph
    /// that never decays, or whose nodes decline to say, is spent as
    /// [`GraphTail::resolve`](tutti_graph::GraphTail::resolve) does.
    Reported {
        /// The most frames kept past the end.
        ceiling: Samples,
    },
}

impl TailPolicy {
    /// The most frames this keeps past the programme's end.
    #[must_use]
    pub const fn bound(self) -> Samples {
        match self {
            Self::Frames(frames) | Self::Reported { ceiling: frames } => frames,
        }
    }
}

/// The frames a render covers and the rate it is rendered at ([`Job::new`]).
///
/// Every field is private; the getters are the reading. A `Job` that exists is
/// one a render can be sized from: the rate is finite and positive, the
/// audible length is not zero and the whole span fits in [`MAX_FRAMES`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Job {
    rate: SampleRate,
    length: Samples,
    latency: Samples,
    tail: TailPolicy,
    on_event_loss: EventLossPolicy,
}

impl Job {
    /// A job of `length` audible frames at `rate`, which drops `latency`
    /// leading frames and keeps `tail` frames past the end.
    ///
    /// `latency` is trimmed from the head, so the graph is pulled for it as
    /// well and the file is `latency` shorter than what the graph produced;
    /// `tail` is kept, so the file is the tail longer than the audible span:
    /// at most [`TailPolicy::bound`], and that many exactly for
    /// [`TailPolicy::Frames`].
    /// The event-loss policy is [`EventLossPolicy::Fail`]; see
    /// [`event_loss`](Self::event_loss).
    ///
    /// # Errors
    ///
    /// - [`JobError::ZeroLength`]: `length` is no frames.
    /// - [`JobError::TooLong`]: `latency + length +` the tail's bound overflows,
    ///   or is more than [`MAX_FRAMES`].
    pub fn new(
        rate: SampleRate,
        length: Samples,
        latency: Samples,
        tail: TailPolicy,
    ) -> Result<Self, JobError> {
        if length == Samples::new(0) {
            return Err(JobError::ZeroLength);
        }
        let total = latency
            .get()
            .checked_add(length.get())
            .and_then(|n| n.checked_add(tail.bound().get()));
        match total.map(Samples::new) {
            Some(n) if n <= MAX_FRAMES => {}
            total => return Err(JobError::TooLong { total }),
        }
        Ok(Self {
            rate,
            length,
            latency,
            tail,
            on_event_loss: EventLossPolicy::default(),
        })
    }

    /// This job with another answer to a graph that dropped events.
    #[must_use]
    pub fn event_loss(self, on_event_loss: EventLossPolicy) -> Self {
        Self {
            on_event_loss,
            ..self
        }
    }

    /// The rate the graph is rendered at.
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// The audible span, in frames. Never zero.
    pub fn length(&self) -> Samples {
        self.length
    }

    /// Leading frames dropped from the render.
    pub fn latency(&self) -> Samples {
        self.latency
    }

    /// What is kept past the audible span.
    pub fn tail(&self) -> TailPolicy {
        self.tail
    }

    /// What the render does when the graph dropped an event.
    pub(crate) fn on_event_loss(&self) -> EventLossPolicy {
        self.on_event_loss
    }

    /// The most frames the render keeps: the audible span plus the tail's
    /// bound.
    pub fn max_output_length(&self) -> Samples {
        self.length + self.tail.bound()
    }

    /// The most frames the graph is pulled for: what is kept at most plus the
    /// trimmed head.
    pub fn max_total(&self) -> Samples {
        self.latency + self.length + self.tail.bound()
    }
}

/// Why [`Job::new`] refused.
#[derive(thiserror::Error, Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum JobError {
    /// The audible span is no frames.
    #[error("the render's length is zero frames")]
    ZeroLength,
    /// Latency, length and tail together overflow a frame count or pass
    /// [`MAX_FRAMES`].
    #[error("the render spans {total:?} frames, more than the {MAX_FRAMES:?} a render may")]
    TooLong {
        /// The sum, or `None` where it overflowed a frame count.
        total: Option<Samples>,
    },
}

#[cfg(test)]
mod tests;
