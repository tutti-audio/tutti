//! [`EnvelopeFollowerNode`]: an audio input's level, sent to control lanes
//! on the play-clock grid.

use tutti_buffers::ChannelLayout;
use tutti_buffers::Float;
use tutti_graph::{Cost, Io};
use tutti_modulation::{LevelFollower, SignalKind, SignalSource, SourceId};
use tutti_units::Ballistics;

use super::{SourceNode, SourcePorts};

/// An **envelope follower**: the level of its audio input, followed by a
/// one-pole attack and release ([`LevelFollower`]) and sent raw in `[0, 1]`
/// as linear pieces on the play-clock grid, one step late, addressed
/// `ParamAddr::Id(source.get())` for every control lane that reads `source`.
///
/// - **The level** of a frame is its **peak over channels**, `max |x_c|`,
///   clamped to `[0, 1]` (a NaN sample reads 0). Peak, not RMS: a frame's
///   RMS over channels is not a level in time but a width-dependent
///   average, so a hard-panned kick would read `1/√2` of itself in stereo
///   and all of itself in mono; the peak reads the same at any width, as a
///   compressor's linked sidechain does.
/// - **Silent, it sends nothing**, and **it resets at play start**: on the
///   frame the play clock restarts the envelope is 0, and it sends a 0 there
///   (if its last piece ended elsewhere). The rest is [`SourceNode`]'s.
///
/// Insert it `ForkByClone(node)`: it shares nothing with a clone, and a
/// fork's (the node as inserted, reset) starts at 0, as a live play start
/// leaves it.
pub type EnvelopeFollowerNode = SourceNode<LevelFollower>;

/// What an [`EnvelopeFollowerNode`] is made from: the width of the signal it follows, the
/// source id it sends as and the attack and release of its level.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EnvelopeFollowerConfig {
    input: ChannelLayout,
    source: SourceId,
    ballistics: Ballistics,
}

impl EnvelopeFollowerConfig {
    /// A follower of an `input`-wide signal, sending as `source`, with the attack and
    /// release of `ballistics`.
    #[must_use]
    pub const fn new(input: ChannelLayout, source: SourceId, ballistics: Ballistics) -> Self {
        Self {
            input,
            source,
            ballistics,
        }
    }

    /// The config `decl` describes (what `tutti_modulation::compile` gives each follower),
    /// following an `input`-wide signal: `None` when `decl` is not a follower or its times
    /// are not valid ones (`compile` refuses those).
    #[must_use]
    pub fn from_decl(input: ChannelLayout, decl: &SignalSource) -> Option<Self> {
        match decl.kind {
            SignalKind::Follower { attack, release } => {
                Some(Self::new(input, decl.id, Ballistics::new(attack, release)?))
            }
            SignalKind::NoteEnvelope { .. } => None,
        }
    }
}

/// The follower of `config`; its rate comes at `prepare`.
impl From<EnvelopeFollowerConfig> for EnvelopeFollowerNode {
    fn from(config: EnvelopeFollowerConfig) -> Self {
        Self::with_source(
            config.source,
            LevelFollower::new(config.ballistics),
            config.input,
        )
    }
}

impl SourcePorts for LevelFollower {
    /// A frame's peak over its channels and a one-pole step, with a piece
    /// every 32 frames, as the dynamics processors' detectors cost.
    const COST: Cost = Cost::Moderate;
    const MIDI_IN: bool = false;

    fn hear<F: Float>(&mut self, io: &Io<'_, F>, from: usize, to: usize) {
        let channels = io.input_count();
        for i in from..to {
            let mut level = 0.0f64;
            for c in 0..channels {
                // `max` ignores a NaN operand: a NaN sample reads 0.
                level = level.max(io.input(c)[i].abs().convert_to::<f64>());
            }
            self.follow(level);
        }
    }
}
