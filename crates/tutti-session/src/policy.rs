//! [`Policy`]: a setting a session holds, and [`OutputPolicy`], where the mix
//! lands.
//!
//! The settings are targets of [`Session::set`] and [`Session::get`], named by
//! [`Session::policy`]; a write is checked when it is set, staged, and applied
//! by the next [`Session::update`] (in [`Phase::Commit`](crate::Phase::Commit),
//! before the commit), live, with no restart.

use std::sync::Arc;

use tutti_buffers::FoldLevels;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_engine::{OutputLevels, OutputMap, Routing, RoutingError};
use tutti_rt::Mutex;

use crate::value::{Cell, SetRefusal, Target};
use crate::{Clock, Session};

/// A session setting, written and read as a target: `session.policy::<P>()`.
pub trait Policy: Clone + PartialEq + Send + Sync + 'static {
    /// The target of the setting.
    type Target: Target<Value = Self>;

    /// The setting's target.
    fn target() -> Self::Target;
}

/// Where the mix lands: the format the main output is heard in, the main output's channels to
/// the device's, and each device output's gain.
///
/// The main output, a bounce and a tap are never changed by it: they read the main output.
/// What is heard is the device, and the heard tap
/// ([`Session::open_heard_tap`]).
///
/// The default routes the main output to the device by speaker, as a session with no
/// policy does.
///
/// ```
/// use tutti_engine::{OutputMap, OutputStrip};
/// use tutti_session::OutputPolicy;
/// use tutti_buffers::{Bed, ChannelLayout};
///
/// // Hear a 5.1 mix as stereo, on outputs 3 and 4 of the interface.
/// let mut policy = OutputPolicy {
///     monitor: Some(Bed::Stereo.topology()),
///     map: OutputMap::channels([2, 3]).unwrap(),
///     ..OutputPolicy::default()
/// };
/// policy
///     .outputs
///     .set_strip(2, OutputStrip { mute: true, ..OutputStrip::default() })
///     .unwrap();
/// assert!(policy.outputs.strip(2).mute);
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OutputPolicy {
    /// The device channel each heard channel goes to; the identity (by
    /// speaker) by default.
    pub map: OutputMap,
    /// The format the main output is heard in (a 7.1.4 mix checked as 5.1, stereo or
    /// mono); `None` is the main output's own.
    pub monitor: Option<ChannelTopology>,
    /// The downmix levels of the fold into [`monitor`](Self::monitor), and of
    /// that fold alone: with no monitor the main output reaches the device by speaker
    /// at the default levels (BS.775), whatever these say.
    pub levels: FoldLevels,
    /// A gain, mute and solo per device output, and the dim over all.
    pub outputs: OutputLevels,
}

impl OutputPolicy {
    /// The routing this policy asks of the engine's folds.
    #[must_use]
    pub fn routing(&self) -> Routing {
        Routing {
            monitor: self.monitor.clone(),
            map: self.map.clone(),
            levels: self.levels,
        }
    }
}

/// Why an [`OutputPolicy`] was refused when it was set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OutputPolicyError {
    /// The session has no engine (a detached one): nothing is folded to a
    /// device, so there is nothing to route.
    #[error("the session has no output device to route to")]
    Detached,
    /// An output has a strip and the device has no such output.
    #[error("output {output} has a strip and the device has {device:?}")]
    StripPast {
        /// The output.
        output: usize,
        /// The device's width.
        device: ChannelLayout,
    },
    /// The routing has no place on the main output and the device: the monitor fold,
    /// the map's width or outputs.
    #[error(transparent)]
    Routing(#[from] RoutingError),
}

impl Policy for OutputPolicy {
    type Target = crate::OutputPolicyTarget;

    fn target() -> crate::OutputPolicyTarget {
        crate::OutputPolicyTarget::new()
    }
}

/// What the device and the main output are, as the graph last placed them: what an
/// [`OutputPolicy`] is checked against. The graph publishes it at each
/// change; it is `None` with no engine.
#[derive(Clone, Debug)]
pub(crate) struct OutputFacts {
    /// The device's width.
    pub device: ChannelLayout,
    /// What the device is, for a fold; `None` past
    /// [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS), where its channels are
    /// unassigned.
    pub placed: Option<ChannelTopology>,
    /// The main output the next commit renders.
    pub main_output: ChannelTopology,
}

/// [`OutputFacts`] behind a lock the value lock does not cover: the graph
/// writes it, a policy write reads it.
pub(crate) type FactsCell = Arc<Mutex<Option<OutputFacts>>>;

/// The cell of the session's output policy, checking against `facts`.
pub(crate) fn output_cell(facts: FactsCell) -> Arc<Cell<OutputPolicy>> {
    Cell::new(OutputPolicy::default(), move |policy| {
        let facts = facts.lock();
        let facts = facts.as_ref().ok_or(OutputPolicyError::Detached)?;
        if policy.outputs.len() > usize::from(facts.device.count()) {
            return Err(OutputPolicyError::StripPast {
                output: policy.outputs.len() - 1,
                device: facts.device,
            }
            .into());
        }
        // A level that is not a gain is refused even where no monitor uses it.
        policy
            .levels
            .check()
            .map_err(RoutingError::Fold)
            .map_err(OutputPolicyError::from)?;
        // A check over the routing's shape against the main output and the device, not a
        // render: whether it has a place does not depend on the sample format.
        policy
            .routing()
            .fold::<f32>(&facts.main_output, facts.device, facts.placed.as_ref())
            .map_err(OutputPolicyError::from)?;
        Ok::<(), SetRefusal>(())
    })
}

impl<K: crate::AppKey, C: Clock> Session<K, C> {
    /// The setting `P`, as a target: `session.set(session.policy::<OutputPolicy>().to(p))`.
    #[must_use]
    pub fn policy<P: Policy>(&self) -> P::Target {
        P::target()
    }

    /// Applies a staged policy to the engine, in [`Phase::Commit`](crate::Phase::Commit).
    pub(crate) fn apply_output_policy(&mut self, out: &mut Vec<crate::SessionEvent>) {
        let Some(policy) = self.output.policy.take_staged() else {
            return;
        };
        if let Some(control) = self.graph.output_control() {
            if let Err(refused) = control.set_routing(policy.routing()) {
                out.push(crate::SessionEvent::OutputRoutingRefused(refused));
            }
            control.set_levels(&policy.outputs);
        }
    }
}
