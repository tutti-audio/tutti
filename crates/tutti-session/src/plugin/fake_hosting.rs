//! A hosted plugin's record over a fake backend (no `plugin-server`): the
//! plugin is a `PluginHandle::from_backend`, put in the record of a node the
//! test added, as the session's own load does with a loaded plugin.

use std::sync::Arc;

use tutti_graph::NodeKey;
use tutti_plugin_host::{PluginControls, PluginHandle};
use tutti_plugin_types::{ParameterInfo, PluginFailure, PluginTail};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::{SampleRate, Samples};

use crate::plugin::HostedPlugin;
use crate::{AnyPlugin, Clock, Key, Session};
use tutti_plugin_types::PluginRef;

mod editor;
mod losses;
mod plugin_health;
mod plugin_params;
mod plugin_state;

/// `handle` (the plugin `plugin`) hosted at `node`, which a test added to the
/// graph and which hosts nothing yet.
pub(super) fn host<K: crate::AppKey, C: Clock<Float = f32>>(
    session: &mut Session<K, C>,
    node: impl Into<NodeKey>,
    handle: PluginHandle,
    plugin: PluginRef,
) -> Key<AnyPlugin> {
    let node = node.into();
    let (controls, _) =
        PluginControls::new(Samples::new(0), PluginTail::default(), SampleRate::SR_48K);
    let previous = session
        .graph
        .plugins
        .insert(node, HostedPlugin::new(handle, plugin, (*controls).clone()));
    assert!(previous.is_none(), "{node:?} hosts one plugin");
    session.graph.provide_plugin(node);
    session.graph.write_own_bypass(node);
    Key::new(node)
}

/// [`host`] for a plugin served from a plugin server: its record drops where it
/// is retired.
pub(super) fn host_served<K: crate::AppKey, C: Clock<Float = f32>>(
    session: &mut Session<K, C>,
    node: impl Into<NodeKey>,
    handle: PluginHandle,
    plugin: PluginRef,
) -> Key<AnyPlugin> {
    let node = node.into();
    let (controls, _) =
        PluginControls::new(Samples::new(0), PluginTail::default(), SampleRate::SR_48K);
    let mut record = HostedPlugin::new(handle, plugin, (*controls).clone());
    record.placement = crate::plugin::Placement::Served;
    record.controls = Some(controls);
    let previous = session.graph.plugins.insert(node, record);
    assert!(previous.is_none(), "{node:?} hosts one plugin");
    session.graph.provide_plugin(node);
    session.graph.write_own_bypass(node);
    Key::new(node)
}

/// Waits until the session has no plugin task in flight ([`Session::plugin_tasks`]): the
/// ends the tasks sent are in the inbox for the next update. The guard only bounds a hang.
pub(super) fn await_tasks<K: crate::AppKey, C: Clock<Float = f32>>(session: &Session<K, C>) {
    let guard = tutti_platform::Instant::now() + std::time::Duration::from_secs(60);
    while session.plugin_tasks() > 0 {
        assert!(
            tutti_platform::Instant::now() < guard,
            "{} plugin task(s) never ended",
            session.plugin_tasks()
        );
        std::thread::yield_now();
    }
}

/// A backend that answers nothing: the editor route is the fake control's.
#[derive(Default)]
pub(super) struct Inert {
    /// The thread the backend dropped on: the last owner of the handle lets go there.
    pub(super) dropped_on: Arc<std::sync::Mutex<Option<std::thread::ThreadId>>>,
}

impl Drop for Inert {
    fn drop(&mut self) {
        *self.dropped_on.lock().unwrap() = Some(std::thread::current().id());
    }
}

impl tutti_plugin_types::AnyThread for Inert {
    fn parameter_descriptors(&self) -> tutti_plugin_types::Answer<Option<Vec<ParameterInfo>>> {
        tutti_plugin_types::answered(None)
    }
    fn parameter_value(&self, _id: ParamId) -> tutti_plugin_types::Answer<Option<Normalized>> {
        tutti_plugin_types::answered(None)
    }

    fn set_parameter_value(
        &self,
        _id: ParamId,
        _value: Normalized,
    ) -> Result<(), tutti_plugin_types::UnknownParam> {
        Ok(())
    }
    fn parameter_plain(&self, _id: ParamId, _value: Normalized) -> Option<f64> {
        None
    }
    fn parameter_normalized(
        &self,
        _id: ParamId,
        _plain: f64,
    ) -> tutti_plugin_types::Answer<Option<Normalized>> {
        tutti_plugin_types::answered(None)
    }
    fn parameter_text(&self, _id: ParamId, _value: Normalized) -> Option<String> {
        None
    }
    fn parameter_value_from_text(&self, _id: ParamId, _text: &str) -> Option<Normalized> {
        None
    }
    fn save_state(
        &self,
        _: tutti_plugin_types::StateUse,
    ) -> Result<Vec<u8>, tutti_plugin_types::StateError> {
        Err(tutti_plugin_types::StateError::NoStateRoute)
    }
    fn set_render_mode(&self, _mode: tutti_plugin_types::RenderMode) -> bool {
        false
    }
    fn set_automation_mode(
        &self,
        _mode: tutti_plugin_types::AutomationMode,
    ) -> Result<(), tutti_plugin_types::AutomationModeError> {
        Err(tutti_plugin_types::AutomationModeError::PluginDead)
    }
}

impl tutti_plugin_types::Health for Inert {
    fn is_crashed(&self) -> bool {
        false
    }
    fn crash_cause(&self) -> Option<PluginFailure> {
        None
    }
    fn rate_refusal(&self) -> Option<tutti_plugin_types::RateRefusal> {
        None
    }
    fn block_counts(&self) -> tutti_plugin_types::BlockCounts {
        tutti_plugin_types::BlockCounts::default()
    }
    fn terminate(&self, _reason: tutti_plugin_types::TerminationReason) {}
}

/// The work values of a plugin hosted in process, run on the main thread: the
/// fakes are in-process plugins, so the other arm is a test's mistake.
pub(super) trait Run {
    type Output;

    /// Runs the in-process work.
    ///
    /// # Panics
    ///
    /// When the work is for a plugin in a plugin server.
    fn run(self, main: tutti_plugin_types::MainThreadMarker) -> Self::Output;
}

impl Run for crate::SaveState {
    type Output = Result<crate::PeerState, tutti_plugin_types::StateError>;

    fn run(self, main: tutti_plugin_types::MainThreadMarker) -> Self::Output {
        match self {
            Self::InProcess(save) => save.run(main),
            Self::OutOfProcess(_) => panic!("a fake plugin runs in process"),
        }
    }
}

impl Run for crate::RestoreState {
    type Output = Result<crate::RestoredState, tutti_plugin_types::StateError>;

    fn run(self, main: tutti_plugin_types::MainThreadMarker) -> Self::Output {
        match self {
            Self::InProcess(restore) => restore.run(main),
            Self::OutOfProcess(_) => panic!("a fake plugin runs in process"),
        }
    }
}
