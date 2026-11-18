//! [`Policy`]: a setting a session holds, and [`OutputPolicy`], where the mix
//! lands.
//!
//! The settings are read and changed through [`Session::policy`]; a change is
//! checked when it is set, staged, and applied by the next
//! [`Session::update`] (in [`Phase::Commit`](crate::Phase::Commit), before the
//! commit), live, with no restart.

use tutti_buffers::{Float, FoldLevels};
use tutti_engine::{OutputLevels, OutputMap, Routing, RoutingError};
use tutti_types::{ChannelLayout, ChannelTopology};

use crate::graph::Graph;
use crate::{Clock, Session};

/// A session setting reached through a typed handle.
pub trait Policy {
    /// What reads and changes the setting, borrowing the session.
    type Handle<'a, F: Float>;

    /// The handle on `session`'s setting.
    fn handle<K: crate::AppKey, C: Clock>(
        session: &mut Session<K, C>,
    ) -> Self::Handle<'_, C::Float>;
}

/// Where the mix lands: the format the root is heard in, the root's channels to
/// the device's, and each device output's gain.
///
/// The root, a bounce and a tap are never changed by it: they read the root.
/// What is heard is the device, and the heard tap
/// ([`Session::open_heard_tap`]).
///
/// The default routes the root to the device by speaker, as a session with no
/// policy does.
///
/// ```
/// use tutti_engine::{OutputMap, OutputStrip};
/// use tutti_session::OutputPolicy;
/// use tutti_types::{Bed, ChannelLayout};
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
    /// The format the root is heard in (a 7.1.4 mix checked as 5.1, stereo or
    /// mono); `None` is the root's own.
    pub monitor: Option<ChannelTopology>,
    /// The downmix levels of the fold into [`monitor`](Self::monitor), and of
    /// that fold alone: with no monitor the root reaches the device by speaker
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
    /// The routing has no place on the root and the device: the monitor fold,
    /// the map's width or outputs.
    #[error(transparent)]
    Routing(#[from] RoutingError),
}

impl Policy for OutputPolicy {
    type Handle<'a, F: Float> = OutputPolicyHandle<'a, F>;

    fn handle<K: crate::AppKey, C: Clock>(
        session: &mut Session<K, C>,
    ) -> OutputPolicyHandle<'_, C::Float> {
        OutputPolicyHandle {
            graph: &session.graph,
            policy: &mut session.output_policy,
            staged: &mut session.output_policy_staged,
        }
    }
}

/// Reads and changes a session's [`OutputPolicy`].
pub struct OutputPolicyHandle<'a, F: Float = f32> {
    graph: &'a Graph<F>,
    policy: &'a mut OutputPolicy,
    staged: &'a mut bool,
}

impl<F: Float> std::fmt::Debug for OutputPolicyHandle<'_, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputPolicyHandle")
            .field("policy", &*self.policy)
            .finish()
    }
}

impl<F: Float> OutputPolicyHandle<'_, F> {
    /// The policy in force, or the one set and not yet applied.
    #[must_use]
    pub fn get(&self) -> &OutputPolicy {
        self.policy
    }

    /// Sets the policy. It is checked against the device and the root now, and
    /// applied by the next [`Session::update`], live: the outputs ramp to their
    /// gains over the declick length, and a changed monitor or map folds from
    /// the next block on, crossfading from the old routing to the new over the
    /// declick length (a change made while a root change crossfades, or while
    /// the last routing change still fades, cuts instead).
    ///
    /// # Errors
    ///
    /// [`OutputPolicyError`] naming what has no place; the policy in force is
    /// kept.
    pub fn set(&mut self, policy: OutputPolicy) -> Result<(), OutputPolicyError> {
        let (device, placed) = self
            .graph
            .placed_device()
            .ok_or(OutputPolicyError::Detached)?;
        if policy.outputs.len() > usize::from(device.count()) {
            return Err(OutputPolicyError::StripPast {
                output: policy.outputs.len() - 1,
                device,
            });
        }
        // A level that is not a gain is refused even where no monitor uses it.
        policy.levels.check().map_err(RoutingError::Fold)?;
        let root = self.graph.root_in_force();
        policy.routing().fold::<F>(&root, &placed)?;
        *self.policy = policy;
        *self.staged = true;
        Ok(())
    }
}

impl<K: crate::AppKey, C: Clock> Session<K, C> {
    /// The setting `P`, to read and change: `session.policy::<OutputPolicy>()`.
    pub fn policy<P: Policy>(&mut self) -> P::Handle<'_, C::Float> {
        P::handle(self)
    }

    /// Applies a staged policy to the engine, in [`Phase::Commit`](crate::Phase::Commit).
    pub(crate) fn apply_output_policy(&mut self) {
        if !std::mem::take(&mut self.output_policy_staged) {
            return;
        }
        if let Some(control) = self.graph.output_control() {
            control.set_routing(self.output_policy.routing());
            control.set_levels(&self.output_policy.outputs);
        }
    }
}
