//! What the editor holds of the main output's generation.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::{Committed, MainOutputGate};
use crate::exec::CommitSeq;
use crate::model::gain::EdgeFade;
use crate::{EdgeId, Fade, MainOutputBank, MainOutputId};
use tutti_units::Gain;

/// What the editor holds of the main output's generation: the host that folds
/// it, the fade a change takes, and the crossfade the plans sent last carry.
pub(super) struct MainOutput {
    /// The host that folds the main output into a device, once one is attached: a
    /// commit whose main output it has no folds for is refused.
    gate: Option<Arc<dyn MainOutputGate>>,
    /// How a commit that changes the main output's generation trades the old main output
    /// for the new ([`set_main_output_fade`](super::Editor::set_main_output_fade)); `None` cuts.
    fade: Option<Fade>,
    phase: Phase,
}

/// Whether the plan sent last carries an old main output's channels.
enum Phase {
    /// It carries none.
    Steady,
    /// It carries them, until a commit without them follows.
    Crossfading {
        /// The commit that began the crossfade, and whether the executor has
        /// rendered it through.
        bank: SentBank,
        /// The old main output the plan renders, kept for the commits that
        /// continue the crossfade.
        fading: Fading,
    },
}

/// A crossfade of an old main output's channels beside the main output's own.
#[derive(Clone, Copy, Debug)]
struct SentBank {
    /// The commit whose plan began the crossfade: a commit that continues it
    /// keeps this.
    began: CommitSeq,
    /// Whether the executor has rendered the crossfade through.
    ended: bool,
}

/// The old main output of a crossfade in progress, as the commit that began it
/// found it: what each commit while the crossfade runs compiles the bank from.
/// It shares the committed spec and shapes until the crossfade ends.
#[derive(Clone)]
pub(super) struct Fading {
    /// The old main output's spec and shapes.
    pub(super) was: Committed,
    /// The base of each gain lane of `was`'s spec when the crossfade began.
    pub(super) bases: BTreeMap<EdgeId, Gain>,
    /// The fade of each gain lane of `was`'s spec that ran one when the
    /// crossfade began.
    pub(super) fades: BTreeMap<EdgeId, EdgeFade>,
    /// The main output generation that fades in.
    pub(super) main_output: MainOutputId,
    /// The bank the plans carry.
    pub(super) bank: MainOutputBank,
}

impl MainOutput {
    pub(super) fn new() -> Self {
        Self {
            gate: None,
            fade: None,
            phase: Phase::Steady,
        }
    }

    /// Attaches the host that folds the main output; a second call replaces it.
    pub(super) fn attach(&mut self, gate: Arc<dyn MainOutputGate>) {
        self.gate = Some(gate);
    }

    /// Sets how a change of the main output's generation is faded; a cut is
    /// no fade.
    pub(super) fn fade_with(&mut self, fade: Option<Fade>) {
        self.fade = fade.filter(|f| !f.is_cut());
    }

    fn phase(&self) -> &Phase {
        &self.phase
    }

    /// Whether the host has folds for main output `id` (always, when no host
    /// is attached).
    pub(super) fn publishes(&self, id: MainOutputId) -> bool {
        self.gate.as_ref().is_none_or(|g| g.published(id))
    }

    /// Whether the plan sent last carries an old main output whose crossfade
    /// the executor has not been reported through.
    pub(super) fn fade_running(&self) -> bool {
        matches!(self.phase(), Phase::Crossfading { bank, .. } if !bank.ended)
    }

    /// Whether the crossfade the plan sent last carries has been rendered
    /// through: the next commit compiles the old main output away.
    pub(super) fn fade_ended(&self) -> bool {
        matches!(self.phase(), Phase::Crossfading { bank, .. } if bank.ended)
    }

    /// The executor has rendered the crossfade of commit `ended` and before
    /// through: returns whether that ended the one the plan sent last carries.
    pub(super) fn finish(&mut self, ended: CommitSeq) -> bool {
        match &mut self.phase {
            Phase::Crossfading { bank, .. } if !bank.ended && bank.began.0 <= ended.0 => {
                bank.ended = true;
                true
            }
            _ => false,
        }
    }

    /// The crossfade running, when a commit leaves its main output alone
    /// (`now` is the spec's main output generation, `current` the committed
    /// spec's). The same generation is the same declaration, so the width its
    /// bank sits past stands.
    pub(super) fn continuing(
        &self,
        now: MainOutputId,
        current: Option<MainOutputId>,
    ) -> Option<&Fading> {
        match self.phase() {
            Phase::Crossfading { fading, bank }
                if !bank.ended && fading.main_output == now && current == Some(now) =>
            {
                Some(fading)
            }
            _ => None,
        }
    }

    /// The fade and the committed main output a commit that changes the main
    /// output's generation to `now` trades away from; `None` when it leaves
    /// the generation alone, has no fade set, or follows no commit.
    pub(super) fn begins<'a>(
        &self,
        now: MainOutputId,
        committed: Option<&'a Committed>,
    ) -> Option<(Fade, &'a Committed)> {
        let was = committed.filter(|c| c.spec.main_output_id != now)?;
        self.fade.map(|fade| (fade, was))
    }

    /// A plan was sent as commit `seq`, carrying the crossfade `fading` or
    /// none. A bank that continues the crossfade the last plan carried keeps
    /// it: the executor keeps its progress, and it ends under the commit that
    /// began it.
    pub(super) fn sent(&mut self, fading: Option<Fading>, seq: CommitSeq) {
        self.phase = match (fading, &self.phase) {
            (None, _) => Phase::Steady,
            (Some(fading), Phase::Crossfading { bank, .. }) if fading.bank.continues => {
                Phase::Crossfading {
                    bank: *bank,
                    fading,
                }
            }
            (Some(fading), _) => Phase::Crossfading {
                bank: SentBank {
                    began: seq,
                    ended: false,
                },
                fading,
            },
        };
    }
}
