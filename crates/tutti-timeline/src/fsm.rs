//! The transport state machine: which motion a command actually produces.
//!
//! [`MotionState`] is the published vocabulary; `TransportFsm` is the private
//! machine that decides transitions, and `TransitionResult` is what it hands
//! back for the engine's `MotionAudio` to publish.

use super::motion::{AfterLocate, FadeOut, MotionCommand};
use tutti_units::Beat;

/// What the transport is doing.
///
/// The four settled states are what a UI shows; the two `Declick*` states are
/// transient — the transport has already stopped or jumped, and the engine's
/// gain is still ramping; the engine settles them as the command lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum MotionState {
    /// Not moving. The default, and the safe reading of a corrupt mirror byte.
    #[default]
    Stopped,
    /// Playing forward at the session tempo.
    Rolling,
    /// Scrubbing forward.
    FastForward,
    /// Scrubbing backward.
    Rewind,
    /// Stopping, with the audio fading out.
    DeclickToStop,
    /// Jumping to a parked target, with the audio fading out.
    DeclickToLocate,
}

/// Encode for the published `AtomicU8` mirror. `#[repr(u8)]` makes the cast
/// exact, and the discriminants are the enum's own — not a parallel table that
/// could drift from it.
impl From<MotionState> for u8 {
    #[inline]
    fn from(state: MotionState) -> u8 {
        state as u8
    }
}

/// Decode from the published mirror.
///
/// Total rather than fallible: the only writer is
/// the engine's `MotionAudio`, which encodes
/// via the `From` above, so an out-of-range byte cannot occur through the
/// public API. A torn or corrupt read degrades to `Stopped` — the safe
/// interpretation for a transport — rather than panicking on the audio thread.
impl From<u8> for MotionState {
    #[inline]
    fn from(val: u8) -> Self {
        match val {
            1 => Self::Rolling,
            2 => Self::FastForward,
            3 => Self::Rewind,
            4 => Self::DeclickToStop,
            5 => Self::DeclickToLocate,
            _ => Self::Stopped,
        }
    }
}

impl core::fmt::Display for MotionState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Stopped => "stopped",
            Self::Rolling => "rolling",
            Self::FastForward => "fast-forward",
            Self::Rewind => "rewind",
            Self::DeclickToStop => "stopping",
            Self::DeclickToLocate => "seeking",
        })
    }
}

/// Where a locate lands, and what a scrub returns to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Settled {
    Stopped,
    Rolling,
}

impl From<Settled> for MotionState {
    #[inline]
    fn from(settled: Settled) -> Self {
        match settled {
            Settled::Stopped => Self::Stopped,
            Settled::Rolling => Self::Rolling,
        }
    }
}

/// Which way a scrub moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Forward,
    Backward,
}

/// What a fade performs when it reaches zero.
///
/// Decided at transition time and held in [`TransportFsm::Fading`], so
/// completion is a lookup rather than a re-derivation. Loop state is absent:
/// the machine holds none.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum DeclickOutcome {
    /// Stop at the position the fade ended on.
    Stop,
    /// Jump to `pos`, landing in `landing`.
    Locate { pos: Beat, landing: Settled },
}

impl DeclickOutcome {
    /// The published motion while a fade into `self` is in flight.
    #[inline]
    fn fading_motion(self) -> MotionState {
        match self {
            Self::Stop => MotionState::DeclickToStop,
            Self::Locate { .. } => MotionState::DeclickToLocate,
        }
    }

    /// Where the transport rests once the fade completes.
    #[inline]
    pub(crate) fn landing(self) -> Settled {
        match self {
            Self::Stop => Settled::Stopped,
            Self::Locate { landing, .. } => landing,
        }
    }
}

/// What a transition changed.
///
/// Self-describing: `MotionAudio::publish` applies it without reading the
/// machine back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum TransitionResult {
    /// Settled or scrubbing now, in this motion. Cancels any fade.
    Moved(MotionState),
    /// Jump now, landing in `landing`.
    Located { pos: Beat, landing: Settled },
    /// Fade out, then rest as the outcome says.
    FadeStarted(DeclickOutcome),
    /// A fade in flight changed what it completes into. Its ramp keeps
    /// counting: a fresh fade mid-fade would step the gain back to full and
    /// click.
    Retargeted(DeclickOutcome),
}

impl TransitionResult {
    /// The motion this result publishes.
    #[inline]
    pub(crate) fn motion(self) -> MotionState {
        match self {
            Self::Moved(motion) => motion,
            Self::Located { landing, .. } => landing.into(),
            Self::FadeStarted(outcome) | Self::Retargeted(outcome) => outcome.fading_motion(),
        }
    }
}

/// The transport state machine. The engine owns it, as a plain field of its
/// `MotionAudio`, which publishes its results; [`MotionState`] is its
/// projection.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) enum TransportFsm {
    #[default]
    Stopped,
    Rolling,
    /// Scrubbing; `EndScrub` returns to `anchor`.
    Scrub {
        direction: Direction,
        anchor: Settled,
    },
    /// Audio still sounds while the gain ramps down; the ramp ends in `into`.
    Fading {
        into: DeclickOutcome,
    },
}

impl From<Settled> for TransportFsm {
    #[inline]
    fn from(settled: Settled) -> Self {
        match settled {
            Settled::Stopped => Self::Stopped,
            Settled::Rolling => Self::Rolling,
        }
    }
}

impl TransportFsm {
    /// The published projection of the machine.
    pub fn motion(&self) -> MotionState {
        match *self {
            Self::Stopped => MotionState::Stopped,
            Self::Rolling => MotionState::Rolling,
            Self::Scrub {
                direction: Direction::Forward,
                ..
            } => MotionState::FastForward,
            Self::Scrub {
                direction: Direction::Backward,
                ..
            } => MotionState::Rewind,
            Self::Fading { into } => into.fading_motion(),
        }
    }

    /// The fade reached zero: rest where it was heading, and return its
    /// outcome. `None` when no fade is in flight, so a second call after the
    /// same fade changes nothing.
    pub fn complete_declick(&mut self) -> Option<DeclickOutcome> {
        let Self::Fading { into } = *self else {
            return None;
        };
        *self = into.landing().into();
        Some(into)
    }

    /// What `EndScrub` returns to if a scrub starts now.
    ///
    /// Scrubbing while already scrubbing keeps the pre-scrub anchor, so
    /// fast-forward → rewind → end-scrub returns to what was playing rather
    /// than to fast-forward. A fade interrupted by a scrub is abandoned, and
    /// the scrub returns to a stop.
    #[inline]
    fn scrub_anchor(&self) -> Settled {
        match *self {
            Self::Rolling => Settled::Rolling,
            Self::Scrub { anchor, .. } => anchor,
            Self::Stopped | Self::Fading { .. } => Settled::Stopped,
        }
    }

    /// Enter `next` immediately, abandoning any fade.
    #[inline]
    fn moved(&mut self, next: Self) -> Option<TransitionResult> {
        *self = next;
        Some(TransitionResult::Moved(next.motion()))
    }

    /// Applies `command`, returning what changed or `None` if the machine
    /// refused it.
    pub fn transition(&mut self, command: MotionCommand) -> Option<TransitionResult> {
        match command {
            MotionCommand::Play => match self {
                Self::Rolling => None,
                // A Play during a fade cancels it, including a pending jump.
                _ => self.moved(Self::Rolling),
            },

            MotionCommand::Stop {
                fade: FadeOut::Immediate,
            } => match self {
                Self::Stopped => None,
                _ => self.moved(Self::Stopped),
            },

            MotionCommand::Stop {
                fade: FadeOut::Declick,
            } => match *self {
                Self::Rolling => {
                    *self = Self::Fading {
                        into: DeclickOutcome::Stop,
                    };
                    Some(TransitionResult::FadeStarted(DeclickOutcome::Stop))
                }
                // Scrubbing is not material worth fading, and fading it would
                // just extend the scrub.
                Self::Scrub { .. } => self.moved(Self::Stopped),
                // Fading to a locate, now asked to stop: keep the ramp, change
                // only where it lands.
                Self::Fading {
                    into: DeclickOutcome::Locate { .. },
                } => {
                    *self = Self::Fading {
                        into: DeclickOutcome::Stop,
                    };
                    Some(TransitionResult::Retargeted(DeclickOutcome::Stop))
                }
                // Already stopped, or already fading to a stop: let the ramp
                // finish.
                Self::Stopped
                | Self::Fading {
                    into: DeclickOutcome::Stop,
                } => None,
            },

            MotionCommand::Locate {
                beat: pos,
                fade,
                then,
            } => {
                // `Keep` during a fade lands stopped; from a scrub it lands
                // rolling.
                let landing = match then {
                    AfterLocate::Play => Settled::Rolling,
                    AfterLocate::Stop => Settled::Stopped,
                    AfterLocate::Keep => match self {
                        Self::Rolling | Self::Scrub { .. } => Settled::Rolling,
                        Self::Stopped | Self::Fading { .. } => Settled::Stopped,
                    },
                };
                // A stopped transport has nothing to fade out of: the jump is
                // immediate. A fade in flight is audio still sounding, so a
                // locate during one fades rather than cutting it.
                if fade == FadeOut::Immediate || *self == Self::Stopped {
                    *self = landing.into();
                    return Some(TransitionResult::Located { pos, landing });
                }
                let into = DeclickOutcome::Locate { pos, landing };
                let was_fading = matches!(self, Self::Fading { .. });
                *self = Self::Fading { into };
                Some(if was_fading {
                    TransitionResult::Retargeted(into)
                } else {
                    TransitionResult::FadeStarted(into)
                })
            }

            MotionCommand::FastForward => self.moved(Self::Scrub {
                direction: Direction::Forward,
                anchor: self.scrub_anchor(),
            }),

            MotionCommand::Rewind => self.moved(Self::Scrub {
                direction: Direction::Backward,
                anchor: self.scrub_anchor(),
            }),

            MotionCommand::EndScrub => match *self {
                Self::Scrub { anchor, .. } => self.moved(anchor.into()),
                // Not scrubbing: nothing to end, and the motion stays.
                _ => None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moved(motion: MotionState) -> Option<TransitionResult> {
        Some(TransitionResult::Moved(motion))
    }

    #[test]
    fn play_and_stop_move_and_a_second_play_is_refused() {
        let mut fsm = TransportFsm::Stopped;
        assert_eq!(
            fsm.transition(MotionCommand::Play),
            moved(MotionState::Rolling)
        );
        assert_eq!(
            fsm.transition(MotionCommand::stop_unfaded()),
            moved(MotionState::Stopped)
        );
        fsm.transition(MotionCommand::Play);
        assert_eq!(fsm.transition(MotionCommand::Play), None);
    }

    #[test]
    fn a_locate_from_a_stop_lands_at_once_in_its_landing() {
        let mut fsm = TransportFsm::Stopped;
        assert!(matches!(
            fsm.transition(MotionCommand::locate(Beat::new(8.0))),
            Some(TransitionResult::Located {
                landing: Settled::Stopped,
                ..
            })
        ));
        assert!(matches!(
            fsm.transition(MotionCommand::play_from(Beat::new(4.0))),
            Some(TransitionResult::Located {
                landing: Settled::Rolling,
                ..
            })
        ));
        assert_eq!(fsm, TransportFsm::Rolling);
    }

    #[test]
    fn a_scrub_ends_in_the_motion_it_started_from() {
        let mut fsm = TransportFsm::Rolling;
        assert_eq!(
            fsm.transition(MotionCommand::FastForward),
            moved(MotionState::FastForward)
        );
        assert_eq!(
            fsm.transition(MotionCommand::EndScrub),
            moved(MotionState::Rolling)
        );
        assert_eq!(
            fsm.transition(MotionCommand::Rewind),
            moved(MotionState::Rewind)
        );
        assert_eq!(
            fsm.transition(MotionCommand::EndScrub),
            moved(MotionState::Rolling)
        );
    }

    #[test]
    fn a_declicked_stop_or_locate_while_rolling_starts_a_fade() {
        let mut fsm = TransportFsm::Rolling;
        assert_eq!(
            fsm.transition(MotionCommand::stop()),
            Some(TransitionResult::FadeStarted(DeclickOutcome::Stop))
        );

        let mut fsm = TransportFsm::Rolling;
        assert!(matches!(
            fsm.transition(MotionCommand::locate(Beat::new(4.0))),
            Some(TransitionResult::FadeStarted(DeclickOutcome::Locate {
                landing: Settled::Rolling,
                ..
            }))
        ));
    }

    /// The result a transition publishes is the projection of the state it
    /// left the machine in, over every (state, command) pair: a `Located`
    /// landing and a fade's outcome project to the motion the machine holds.
    #[test]
    fn every_accepted_transition_publishes_the_state_it_entered() {
        let commands = [
            MotionCommand::Play,
            MotionCommand::stop(),
            MotionCommand::stop_unfaded(),
            MotionCommand::locate(Beat::new(16.0)),
            MotionCommand::play_from(Beat::new(16.0)),
            MotionCommand::stop_then_locate(Beat::new(0.0)),
            MotionCommand::Locate {
                beat: Beat::new(2.0),
                fade: FadeOut::Immediate,
                then: AfterLocate::Keep,
            },
            MotionCommand::FastForward,
            MotionCommand::Rewind,
            MotionCommand::EndScrub,
        ];
        let states = [
            TransportFsm::Stopped,
            TransportFsm::Rolling,
            TransportFsm::Scrub {
                direction: Direction::Forward,
                anchor: Settled::Rolling,
            },
            TransportFsm::Scrub {
                direction: Direction::Backward,
                anchor: Settled::Stopped,
            },
            TransportFsm::Fading {
                into: DeclickOutcome::Stop,
            },
            TransportFsm::Fading {
                into: DeclickOutcome::Locate {
                    pos: Beat::new(1.0),
                    landing: Settled::Rolling,
                },
            },
        ];

        for start in states {
            for command in commands {
                let mut fsm = start;
                if let Some(result) = fsm.transition(command) {
                    assert_eq!(
                        fsm.motion(),
                        result.motion(),
                        "{command:?} from {start:?} published {result:?} but the machine is {fsm:?}"
                    );
                }
            }
        }
    }

    /// Changing scrub direction keeps the pre-scrub anchor: `EndScrub`
    /// returns to what was playing, not to fast-forward.
    #[test]
    fn scrub_direction_change_keeps_the_pre_scrub_anchor() {
        let mut fsm = TransportFsm::Rolling;
        fsm.transition(MotionCommand::FastForward);
        fsm.transition(MotionCommand::Rewind);
        assert_eq!(
            fsm.transition(MotionCommand::EndScrub),
            moved(MotionState::Rolling)
        );
    }

    /// A fade a scrub interrupts is abandoned: ending the scrub rests
    /// stopped, and there is no fade left to complete.
    #[test]
    fn a_scrub_abandons_the_fade_it_interrupts() {
        let mut fsm = TransportFsm::Rolling;
        fsm.transition(MotionCommand::stop());
        fsm.transition(MotionCommand::FastForward);
        assert_eq!(
            fsm.transition(MotionCommand::EndScrub),
            moved(MotionState::Stopped)
        );
        assert_eq!(fsm.complete_declick(), None);
    }

    /// `EndScrub` when not scrubbing changes nothing.
    #[test]
    fn end_scrub_while_not_scrubbing_is_a_no_op() {
        let mut fsm = TransportFsm::Rolling;
        assert_eq!(fsm.transition(MotionCommand::EndScrub), None);
        assert_eq!(fsm, TransportFsm::Rolling, "playback continues");
    }

    /// An immediate stop during a locate fade drops the jump: completing a
    /// fade afterwards finds none.
    #[test]
    fn stopping_discards_a_pending_locate() {
        let mut fsm = TransportFsm::Rolling;
        fsm.transition(MotionCommand::stop_then_locate(Beat::new(8.0)));
        fsm.transition(MotionCommand::stop_unfaded());
        assert_eq!(fsm.complete_declick(), None);
        assert_eq!(fsm, TransportFsm::Stopped);
    }

    /// A fade that changes its mind is retargeted, never started again.
    #[test]
    fn retargeting_a_fade_does_not_restart_the_ramp() {
        let mut fsm = TransportFsm::Rolling;
        fsm.transition(MotionCommand::stop_then_locate(Beat::new(0.0)));
        assert_eq!(
            fsm.transition(MotionCommand::stop()),
            Some(TransitionResult::Retargeted(DeclickOutcome::Stop))
        );

        let mut fsm = TransportFsm::Rolling;
        fsm.transition(MotionCommand::stop());
        assert!(matches!(
            fsm.transition(MotionCommand::locate(Beat::new(12.0))),
            Some(TransitionResult::Retargeted(DeclickOutcome::Locate {
                landing: Settled::Stopped,
                ..
            }))
        ));
    }

    /// A second declicked stop during a stop fade lets the ramp finish.
    #[test]
    fn stop_during_a_stop_fade_is_a_no_op() {
        let mut fsm = TransportFsm::Rolling;
        fsm.transition(MotionCommand::stop());
        assert_eq!(fsm.transition(MotionCommand::stop()), None);
        assert_eq!(fsm.motion(), MotionState::DeclickToStop);
    }

    #[test]
    fn motion_state_u8_roundtrip_is_exact() {
        for state in [
            MotionState::Stopped,
            MotionState::Rolling,
            MotionState::FastForward,
            MotionState::Rewind,
            MotionState::DeclickToStop,
            MotionState::DeclickToLocate,
        ] {
            let byte: u8 = state.into();
            assert_eq!(
                MotionState::from(byte),
                state,
                "{state} did not survive the atomic round-trip"
            );
        }
    }

    #[test]
    fn motion_state_decodes_unknown_bytes_as_stopped() {
        // Not reachable through the public API — the only writer encodes via
        // `From` — but a transport must degrade to "not moving", never panic.
        assert_eq!(MotionState::from(6), MotionState::Stopped);
        assert_eq!(MotionState::from(u8::MAX), MotionState::Stopped);
    }
}
