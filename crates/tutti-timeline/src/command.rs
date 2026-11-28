//! The vocabulary of a motion change: what a request is, how it fades, what follows it.

use tutti_units::Beat;

/// How a motion change reaches the output.
///
/// The transport's only real fade decision: ramp the gain to zero first, or
/// switch on the next buffer. A parameter rather than a variant per verb, so
/// adding a new motion verb does not double the event list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FadeOut {
    /// Fade the audio out over the declick window. What a user-facing
    /// button should almost always be.
    #[default]
    Declick,
    /// Take effect on the next buffer. Clicks unless output is already silent.
    Immediate,
}

/// What the transport should be doing once a locate completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AfterLocate {
    /// Stay (or become) stopped at the target.
    #[default]
    Stop,
    /// Play from the target.
    Play,
    /// Keep whatever the transport was doing — a locate while rolling keeps
    /// rolling, a locate while stopped stays stopped.
    Keep,
}

/// A requested transport transition.
///
/// Every variant is a genuine state-machine input — the FSM may reject it
/// (`Play` while already rolling does nothing) or defer it (a `Declick` stop
/// while rolling fades out first). Settings changes are deliberately *not*
/// here; see [`TransportSettings`](crate::TransportSettings).
///
/// `fade` and `then` are orthogonal: `fade` is about the output gain, `then`
/// about the motion after landing. Neither constrains the other, which is what
/// makes them parameters rather than more variants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MotionCommand {
    /// Roll from the current position. A no-op while already rolling.
    Play,
    /// Stop at the current position.
    Stop {
        /// Whether to ramp the output down first.
        fade: FadeOut,
    },
    /// Jump to `beat`.
    Locate {
        /// The absolute target position.
        beat: Beat,
        /// Whether to ramp the output down before jumping.
        fade: FadeOut,
        /// What the transport should be doing once the jump lands.
        then: AfterLocate,
    },
    /// Begin scrubbing forward.
    FastForward,
    /// Begin scrubbing backward.
    Rewind,
    /// Leave fast-forward/rewind, returning to the previous motion.
    EndScrub,
}

/// What a [`MotionCommand`] does to whether the transport rolls
/// ([`MotionCommand::roll`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Roll {
    /// The transport rolls once the command applies: a play, or a locate that
    /// then plays.
    Starts,
    /// It does not: a stop, or a locate that then stops.
    Stops,
    /// The command leaves it as it was: a locate that keeps, or a scrub.
    Keeps,
}

impl MotionCommand {
    /// Whether the transport rolls once this command applies.
    #[must_use]
    pub const fn roll(self) -> Roll {
        match self {
            Self::Play
            | Self::Locate {
                then: AfterLocate::Play,
                ..
            } => Roll::Starts,
            Self::Stop { .. }
            | Self::Locate {
                then: AfterLocate::Stop,
                ..
            } => Roll::Stops,
            Self::Locate {
                then: AfterLocate::Keep,
                ..
            }
            | Self::FastForward
            | Self::Rewind
            | Self::EndScrub => Roll::Keeps,
        }
    }

    /// Returns a stop with a fade-out, the default stop.
    pub const fn stop() -> Self {
        Self::Stop {
            fade: FadeOut::Declick,
        }
    }

    /// Returns a stop that takes effect on the next buffer, unfaded. For
    /// teardown, and for tests that assert the unfaded path.
    pub const fn stop_unfaded() -> Self {
        Self::Stop {
            fade: FadeOut::Immediate,
        }
    }

    /// Returns a jump to `beat` that keeps the current motion — the
    /// scrub-bar seek.
    pub const fn locate(beat: Beat) -> Self {
        Self::Locate {
            beat,
            fade: FadeOut::Declick,
            then: AfterLocate::Keep,
        }
    }

    /// Returns a jump to `beat` that then rolls from there.
    pub const fn play_from(beat: Beat) -> Self {
        Self::Locate {
            beat,
            fade: FadeOut::Declick,
            then: AfterLocate::Play,
        }
    }

    /// Returns a stop that fades out, then returns to `beat` when the fade
    /// completes.
    ///
    /// The Stop button. Sending a stop and a locate as two events applies both
    /// in one callback, so the seek lands immediately and the fade ramps down
    /// audio rendered from the *new* position — protecting nothing.
    pub const fn stop_then_locate(beat: Beat) -> Self {
        Self::Locate {
            beat,
            fade: FadeOut::Declick,
            then: AfterLocate::Stop,
        }
    }
}
