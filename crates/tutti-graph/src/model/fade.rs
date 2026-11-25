//! Replacing a running node's unit with a crossfade: the [`Fade`] an
//! [`Editor::replace_or_swap`](crate::Editor::replace_or_swap) asks for, and the
//! [`FadeLaw`] it follows.
//!
//! The rules the executor implements:
//!
//! - **Both units run on the same inputs** for the fade's `duration` frames,
//!   and the node's output is `outgoing * g_out + incoming * g_in`, sample by
//!   sample, with the gains `(g_out, g_in)` of [`FadeLaw::gains`]. Frame `k` of
//!   the fade (counted from the first frame of the block the commit lands on)
//!   takes `gains(k, duration)`, for `k` in `0..duration`; from frame
//!   `duration` on, the incoming unit is the node's output alone. So the fade
//!   is exactly `duration` frames long, and neither end is a step.
//! - **Events go to the incoming unit only.** The outgoing unit is handed no
//!   events and its event outputs are discarded: it only finishes what it was
//!   already sounding, the way a DAW swaps an instrument. A note it holds is
//!   faded out with it, not released.
//! - **The shapes must agree** in everything the plan was compiled from:
//!   ports, latency, in-place acceptance, event resolution and event
//!   capacity (the plan sizes the port's buffers from it). Only the tail
//!   may differ (a fading node is never skipped). A latency change is
//!   refused rather than re-aligned: the running plan's PDC is compiled for
//!   one latency, and both units must be aligned to it. Swap a unit whose
//!   latency differs with a plain [`Editor::insert`](crate::Editor::insert).
//! - **A replace while a fade runs at that key is queued**: the running fade
//!   finishes, and a fade from
//!   its incoming unit to the newest one starts at the next block. Only two
//!   units ever run at once, and no swap is ever a step. A third replace
//!   while one waits supersedes the waiting unit, which never ran.
//! - **The outgoing unit retires on the control thread**, and a fade holds
//!   no commit. The commit that starts a fade comes back when applied, like
//!   any other; the crossfade comes back on its own, on the fade-return
//!   ring, when it ends (or is cut or superseded), carrying the outgoing
//!   unit, and `Editor::collect` frees it and reports its key. A long fade
//!   therefore blocks no later commit or re-prepare, and a unit removed in
//!   the same commit comes back after one block. At most `FADE_CAPACITY`
//!   crossfades are in flight; each takes its slot when its commit is sent.
//! - **A hard edit cuts a fade.** Removing the key, replacing it without a
//!   fade, or a re-prepare retires every unit at the key but the newest (a
//!   re-prepare renders silence while its units are out, so there is nothing
//!   to fade).

use tutti_curve::FadeLaw;
use tutti_units::{SampleRate, Samples, Seconds};

/// How [`Editor::replace_or_swap`](crate::Editor::replace_or_swap) swaps a unit: over
/// `duration` frames, along `law`. See [`Editor::replace_or_swap`](crate::Editor::replace_or_swap)
/// for the rules.
///
/// A zero `duration` is a plain swap: the new unit takes over at the block
/// the commit lands on, as with [`Editor::insert`](crate::Editor::insert).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fade {
    /// The fade's length, in frames at the rate the graph runs at.
    pub duration: Samples,
    /// Its gain law: [`FadeLaw::EqualAmplitude`] for a unit in phase with the
    /// one it replaces (a re-declared node), [`FadeLaw::EqualPower`] for a
    /// different source.
    pub law: FadeLaw,
}

impl Fade {
    /// A fade of `duration` frames along [`FadeLaw::EqualAmplitude`].
    #[must_use]
    pub const fn new(duration: Samples) -> Self {
        Self {
            duration,
            law: FadeLaw::EqualAmplitude,
        }
    }

    /// The gain law the fade runs along.
    #[must_use]
    pub const fn law(mut self, law: FadeLaw) -> Self {
        self.law = law;
        self
    }

    /// A fade of `duration` at `rate`, rounded to the nearest frame. The
    /// graph's rate is [`Editor::prepare`](crate::Editor::prepare)'s.
    pub fn seconds(duration: Seconds, rate: SampleRate) -> Self {
        Self::new(duration.to_samples(rate))
    }

    /// Whether this swaps at once.
    pub(crate) fn is_cut(&self) -> bool {
        self.duration.is_zero()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_round_to_the_nearest_frame() {
        let f = Fade::seconds(Seconds::new(0.005), SampleRate::SR_48K);
        assert_eq!(f.duration, Samples::new(240));
        let f = Fade::seconds(Seconds::new(1.5 / 1000.0), SampleRate::of(1000.0));
        assert_eq!(f.duration, Samples::new(2));
    }
}
