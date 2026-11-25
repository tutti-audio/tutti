//! Frame-count arithmetic for one offline render.
//!
//! Derived once from the job, and drives both how many frames the graph must
//! produce and how many leading frames the sink drops.

use crate::{Job, TailPolicy};
use tutti_units::Samples;

/// Fixed scheduling parameters for one render.
///
/// Every field is [`Samples`] — an unassigned frame count, which is what the type
/// exists for (its own doc names the compensation-delay path). Note that
/// `Samples` has no `Sub`: use `remaining_after` / `align_to`, which is why the
/// driver reads the way it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RenderPlan {
    /// The most frames the graph is pulled for, including latency slack when
    /// trimming: the programme and the tail's bound.
    pub total: Samples,
    /// The most frames the sink keeps: the requested span plus the tail's
    /// bound.
    pub output_length: Samples,
    /// The programme's length at the graph's input side, latency not
    /// counted: the frames pulled before a reported tail is first asked for,
    /// so every node's figure is measured from the instant the declared tail
    /// is.
    pub programme: Samples,
    /// What is pulled past `programme`.
    pub tail: TailPolicy,
    /// Leading frames the sink drops.
    pub latency: Samples,
}

impl RenderPlan {
    /// The plan of a job. Pure: arithmetic over three FRAME counts, and no
    /// graph in sight.
    ///
    /// The tail extends what the sink KEEPS, not just what the graph produces.
    /// Two gates truncate independently (`total` bounds the pull loop and
    /// `output_length` caps the kept frames), so extending only the first
    /// would render the decay and then discard it. The job's counts are
    /// both: the kept span plus the tail's bound, and that plus the trimmed
    /// head. A tail that follows the graph ends the render sooner
    /// ([`drive`](super::drive)), never later.
    pub(crate) fn of(job: &Job) -> Self {
        Self {
            total: job.max_total(),
            output_length: job.max_output_length(),
            programme: job.length(),
            tail: job.tail(),
            latency: job.latency(),
        }
    }

    /// The plan of `frames` already rendered: no head to trim, no tail to
    /// find.
    pub(crate) fn of_frames(frames: Samples) -> Self {
        Self {
            total: frames,
            output_length: frames,
            programme: frames,
            tail: TailPolicy::Frames(Samples::ZERO),
            latency: Samples::ZERO,
        }
    }
}

#[cfg(test)]
mod tests;
