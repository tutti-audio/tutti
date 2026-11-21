//! A hosted plugin's parameters as views:
//! [`PluginParams`], [`PluginParam`] and [`ParamGesture`].
//!
//! Every one of them borrows the session through [`Plugin`](crate::Plugin), so
//! none can outlive what it names. The descriptors a [`PluginParams`] shows are the
//! session's own, read at load and read again when the plugin announces a
//! session's own, read at load and read again when the plugin announces a
//! rescan ([`PluginChange::ParamsRescanned`](crate::PluginChange::ParamsRescanned),
//! [`ParamTitlesChanged`](crate::PluginChange::ParamTitlesChanged)); the read
//! is handed to the plugin host and answered in [`Session::update`](crate::Session::update), which
//! takes `&mut Session`. So **a handle cannot be stale**: holding one across
//! an update is a borrow error, and what a host keeps across updates is the
//! `ParamId`, resolved again with [`PluginParams::get`] (which refuses an id the
//! plugin stopped declaring). No generation counter is checked, because there
//! is no state to check it against.
//!
//! The kept id resolved again after an update:
//!
//! ```
//! # use tutti_session::Session;
//! # use tutti_graph::NodeKey;
//! # use tutti_units::ParamId;
//! # fn rescan(session: &mut Session, node: NodeKey) {
//! let id = ParamId::new(0);
//! let before = session.plugin_view(node).unwrap().params().get(id).unwrap().normalized();
//! let _events = session.update();
//! let after = session.plugin_view(node).unwrap().params().get(id).map(|p| p.normalized());
//! # let _ = (before, after);
//! # }
//! ```
//!
//! A handle held across the update is the stale one, and does not compile:
//!
//! ```compile_fail
//! # use tutti_session::Session;
//! # use tutti_graph::NodeKey;
//! # use tutti_units::ParamId;
//! # fn rescan(session: &mut Session, node: NodeKey) {
//! let level = session.plugin_view(node).unwrap().params().get(ParamId::new(0)).unwrap();
//! let _events = session.update();
//! let _ = level.normalized();
//! # }
//! ```
//!
//! None of the calls takes the main thread: a value reaches the plugin through
//! its handle's token-free calls (a parameter set goes to the plugin's edit ring
//! from any thread), so the views belong on the read-only `Plugin<'_>`, which
//! `Session::plugin_view` lends. A call that asks the plugin something
//! (`text`, `plain`) may be a round trip to the
//! plugin, and for an in-process plugin an answer from its main thread, so a
//! host does not make one per frame per parameter.
//!
//! What the host sets reads back at once (`normalized` answers the value just
//! set) and is not reported as a [`PluginChange`](crate::PluginChange) unless it
//! is made inside a [`ParamGesture`]: a gesture is what a host that records
//! automation touches a parameter with, so the session reports the host's own
//! begin, changes and end in the one stream with the plugin's, in order.
//!
//! The gesture is the one owned handle ([`ParamGesture`]): a drag spans frames,
//! each of which calls `update` under `&mut Session`, so it cannot borrow. A
//! shared gate stands in for the borrow and refuses a `set` once the plugin is
//! gone.
//!
//! The gesture is the host's own. No hosted format has a call that tells a plugin
//! the host holds a parameter (CLAP's and VST3's gesture events go from the
//! plugin to the host, VST2 has none, AU's are notifications to listeners), so a
//! plugin hears only the values.

use tutti_graph::NodeKey;

use tutti_plugin_host::{ParamReporter, PluginHandle, PluginSignal};
use tutti_plugin_types::{ParamEdit, ParameterInfo, UnknownParam};
use tutti_rt::LockExt;
use tutti_units::Normalized;
use tutti_units::ParamId;

use crate::plugin::{Gate, HostedPlugin, Signals, queue};
use crate::{PluginParamRefusal, PluginParamTarget, PluginUnit, UnitError};

/// The lane knobs of a plugin's driven parameters: where a set of one goes,
/// whether the host made it or the plugin reported it. The modulation apply
/// replaces it (`Graph::refresh_driven`) after every lane it built or dropped,
/// so within one update a parameter without a lane, or whose lane was removed,
/// is absent and a set of it goes to the plugin.
#[derive(Clone, Debug, Default)]
pub(crate) struct Driven {
    knobs: std::sync::Arc<std::sync::Mutex<Knobs>>,
}

/// The lanes with a knob, by the parameter they drive.
pub(crate) type Knobs = std::collections::BTreeMap<ParamId, tutti_nodes::automation::LaneControls>;

impl Driven {
    /// Writes `value` into parameter `id`'s lane knob. `false` when no lane
    /// with a knob drives it.
    pub(crate) fn write(&self, id: ParamId, value: Normalized) -> bool {
        self.knobs
            .lock_recover()
            .get(&id)
            .is_some_and(|lane| lane.store(value.get()))
    }

    /// Replaces the driven parameters with `knobs`; nothing is touched while
    /// there were none and there are none.
    pub(crate) fn replace(&self, knobs: Knobs) {
        let mut held = self.knobs.lock_recover();
        if held.is_empty() && knobs.is_empty() {
            return;
        }
        *held = knobs;
    }

    /// Reports each driven parameter's knob into `mirror`: a read of the
    /// plugin lands the value it plays (the knob plus the lane's routes), and
    /// the mirror holds the knob.
    pub(crate) fn report_knobs(&self, mirror: &tutti_plugin_host::PluginParamValues) {
        for (&id, lane) in self.knobs.lock_recover().iter() {
            if let Some(knob) = lane.value() {
                mirror.report(id, Normalized::clamped(knob));
            }
        }
    }
}

/// A hosted plugin's parameters, lent by [`Plugin::params`](crate::Plugin::params).
#[derive(Debug, Clone, Copy)]
pub struct PluginParams<'a> {
    node: NodeKey,
    hosted: &'a HostedPlugin,
}

impl<'a> PluginParams<'a> {
    pub(crate) fn new(node: NodeKey, hosted: &'a HostedPlugin) -> Self {
        Self { node, hosted }
    }

    /// Whether the plugin's parameters have been read. A plugin loaded through
    /// the session is read at load; one the host inserted itself
    /// ([`Graph::add_plugin`](crate::Graph::add_plugin)) is read by the
    /// plugin host and answered in [`Session::update`](crate::Session::update).
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.hosted.mirror.is_ready()
    }

    /// What the plugin declares, in the order it lists it. Empty until the
    /// parameters are [read](Self::is_ready).
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &'a ParameterInfo> + use<'a> {
        self.hosted.mirror.descriptors().iter()
    }

    /// How many parameters the plugin declares.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hosted.mirror.descriptors().len()
    }

    /// Whether the plugin declares none, or they are not read yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Resolves parameter `id` once. The [`PluginParam`] reads its value from the
    /// session's mirror.
    ///
    /// # Errors
    ///
    /// [`PluginParamRefusal::NotRead`] before the parameters are read;
    /// [`PluginParamRefusal::UnknownParam`] for an id the plugin does not declare.
    pub fn get(&self, id: ParamId) -> Result<PluginParam<'a>, PluginParamRefusal> {
        if !self.is_ready() {
            return Err(PluginParamRefusal::NotRead);
        }
        let position = self
            .hosted
            .mirror
            .position(id)
            .ok_or(PluginParamRefusal::UnknownParam(id))?;
        Ok(PluginParam {
            params: *self,
            position,
        })
    }

    /// The plugin's node.
    #[must_use]
    pub fn node(&self) -> NodeKey {
        self.node
    }
}

/// One parameter of a hosted plugin, resolved by [`PluginParams::get`].
///
/// `Copy`, and good for as long as the borrow of the session it came from: see
/// the module docs for why that is as long as the parameter list can stay
/// as it is.
#[derive(Debug, Clone, Copy)]
pub struct PluginParam<'a> {
    params: PluginParams<'a>,
    /// Where the parameter is in the mirror's list: valid while the read that
    /// made it is the mirror's, which the borrow guarantees.
    position: usize,
}

impl<'a> PluginParam<'a> {
    /// What the plugin declares about the parameter.
    #[must_use]
    pub fn info(&self) -> &'a ParameterInfo {
        &self.params.hosted.mirror.descriptors()[self.position]
    }

    /// The parameter's id.
    #[must_use]
    pub fn id(&self) -> ParamId {
        self.info().id()
    }

    /// The value now: what the plugin last reported or the host last set, else
    /// what was read. From the session's mirror, so it asks the plugin nothing.
    ///
    /// A parameter a modulation lane drives reads its base, the lane's knob;
    /// the value the plugin plays is that plus the lane's routes. The mirror
    /// is written with the knob: by a set of the parameter, and again when a
    /// read of the plugin lands (which finds the played value). Nothing reports
    /// the played value into it: a plugin's report of a value the host just
    /// gave it is not an edit (`PluginParamValues`).
    #[must_use]
    pub fn normalized(&self) -> Normalized {
        self.params.hosted.mirror.value_at(self.position)
    }

    /// The target of this parameter: what `session.set` writes and a
    /// modulation route targets, the one `key.param(id)` gives.
    #[must_use]
    pub fn target(&self) -> crate::PluginParamTarget {
        crate::Key::<crate::AnyPlugin>::new(self.params.node).param(self.id())
    }

    /// The target of this parameter in the tutti unit `U`: what `session.set`
    /// writes in `U`, through the parameter's declared map and unit.
    ///
    /// The declaration is checked here, and read again at every write, so a
    /// rescan that removes the unit or the map refuses later writes with the
    /// same [`UnitError`] (as [`SetRefusal::Plugin`](crate::SetRefusal::Plugin)).
    ///
    /// # Errors
    ///
    /// [`UnitError::NoUnit`] for a parameter with no declared unit or no map the
    /// host computes (every VST3 parameter, a VST2 parameter without an integer
    /// range), [`UnitError::UnitMismatch`] for a unit `U` does not read
    /// (`Db` on a parameter in hertz) and [`UnitError::RangeUnfit`] for a
    /// declared range `U` cannot hold all of.
    pub fn unit<U: PluginUnit>(&self) -> Result<PluginParamTarget<U>, UnitError> {
        PluginParamTarget::declared(self.params.node, self.info())
    }

    /// The write policy every format answers
    /// ([`ParamWriteError`](tutti_plugin_types::ParamWriteError)), on what the
    /// mirror holds of this parameter's declaration.
    fn writable(&self) -> Result<(), PluginParamRefusal> {
        use tutti_plugin_types::{ParamAccess, ParamWriteError};
        // The mirror holds a declaration, so the policy can only refuse as
        // read-only; any other refusal is reported as an unknown parameter.
        ParamWriteError::check(self.id(), Some(ParamAccess::of(self.info()))).map_err(|refused| {
            match refused {
                ParamWriteError::ReadOnly(id) => PluginParamRefusal::ReadOnly(id),
                other => PluginParamRefusal::UnknownParam(other.id()),
            }
        })
    }

    /// The plugin's own text for the value now ("-6.0 dB"); `None` when the
    /// plugin (or its format) has none.
    #[must_use]
    pub fn text(&self) -> Option<String> {
        self.params
            .hosted
            .handle
            .parameter_text(self.id(), self.normalized())
    }

    /// The value now in the plugin's own units, through the format's own map;
    /// `None` where the format has none.
    ///
    /// A bare `f64`, on purpose: a plugin's plain value has no tutti unit unless
    /// the plugin declares one, and [`PluginParam::unit`] is the typed door for that.
    #[must_use]
    pub fn plain(&self) -> Option<f64> {
        self.params
            .hosted
            .handle
            .parameter_plain(self.id(), self.normalized())
    }

    /// Takes hold of the parameter: the session reports the host's gesture
    /// (a [`PluginChange::GestureBegan`](crate::PluginChange::GestureBegan) now,
    /// each [`set`](ParamGesture::set) as a
    /// [`ParamChanged`](crate::PluginChange::ParamChanged), and a
    /// [`GestureEnded`](crate::PluginChange::GestureEnded) when the guard
    /// drops, a drop in the middle of a drag included).
    ///
    /// # Errors
    ///
    /// [`PluginParamRefusal::ReadOnly`]: a gesture on a parameter that cannot be
    /// set is refused before it begins, so the guard's `set` cannot fail.
    pub fn edit(&self) -> Result<ParamGesture, PluginParamRefusal> {
        self.writable()?;
        let hosted = self.params.hosted;
        hosted.hear(ParamEdit::Begin(self.id()));
        Ok(ParamGesture {
            node: self.params.node,
            id: self.id(),
            gate: hosted.gate.clone(),
            driven: hosted.driven.clone(),
            handle: hosted.handle.clone(),
            notices: std::sync::Arc::clone(&hosted.notices),
            reporter: hosted.mirror.reporter(),
        })
    }
}

/// A host's hold on a parameter: made by [`PluginParam::edit`], which begins the
/// gesture, and ended when it drops.
///
/// **It is the one handle that is owned, with no borrow of the session.** A drag
/// is a mouse down, many frames each calling [`Session::update`](crate::Session::update)
/// (which takes `&mut Session`), and a mouse up, so the guard has to live across
/// them; the other handles stay borrowed because a stale one is then a compile
/// error. What stands in for the borrow is a shared gate the session ends when
/// the plugin is removed, replaced (a crossfade, a reload) or stops declaring the
/// parameter (a rescan landed): [`set`](Self::set) then refuses with
/// [`PluginParamRefusal::UnknownParam`] and nothing is sent, and the drop sends nothing.
/// The notices go through the plugin's own queue, so the begin, the changes and
/// the end are reported in the order they were made, with the plugin's.
///
/// There is no way to end it but to drop it, so a drag that is cut short (an
/// early return, a panic unwinding) still lets go.
#[must_use = "the gesture ends when this drops: keep it for as long as the hand is down"]
#[derive(Debug)]
pub struct ParamGesture {
    node: NodeKey,
    id: ParamId,
    gate: Gate,
    driven: Driven,
    handle: PluginHandle,
    notices: Signals,
    reporter: ParamReporter,
}

impl ParamGesture {
    /// The plugin's node.
    #[must_use]
    pub fn node(&self) -> NodeKey {
        self.node
    }

    /// The parameter held.
    #[must_use]
    pub fn id(&self) -> ParamId {
        self.id
    }

    /// Moves the parameter to `value`, inside the gesture: the plugin takes the
    /// value, and the session reports a
    /// [`ParamChanged`](crate::PluginChange::ParamChanged) for it.
    ///
    /// # Errors
    ///
    /// [`PluginParamRefusal::UnknownParam`] once the plugin was removed or replaced, or no
    /// longer declares the parameter, including when the write finds it
    /// undeclared.
    pub fn set(&self, value: Normalized) -> Result<(), PluginParamRefusal> {
        if !self.gate.holds(self.id) {
            return Err(PluginParamRefusal::UnknownParam(self.id));
        }
        if !self.driven.write(self.id, value) {
            self.handle
                .send_parameter(self.id, value)
                .map_err(|UnknownParam(id)| PluginParamRefusal::UnknownParam(id))?;
        }
        self.reporter.report(self.id, value);
        queue(
            &self.notices,
            PluginSignal::Param(ParamEdit::change(self.id, value)),
        );
        Ok(())
    }
}

impl Drop for ParamGesture {
    fn drop(&mut self) {
        if self.gate.holds(self.id) {
            queue(&self.notices, PluginSignal::Param(ParamEdit::End(self.id)));
        }
    }
}

impl HostedPlugin {
    /// Queues an edit the host made as if the plugin had reported it, for the
    /// next `Phase::Plugins`'s event, in the order it was made.
    pub(crate) fn hear(&self, edit: ParamEdit) {
        crate::plugin::queue(&self.notices, PluginSignal::Param(edit));
    }
}
