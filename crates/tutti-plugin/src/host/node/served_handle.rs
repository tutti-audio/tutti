//! The backend of a served plugin's [`PluginHandle`](tutti_plugin_host::PluginHandle).

use tutti_plugin_host::LiveView;
use tutti_plugin_ipc::{Ipc, IpcController};
use tutti_plugin_types::{
    Answer, AnyThread, AutomationMode, AutomationModeError, BypassWrite, Health, OwnBypass,
    ParamWriteError, ParameterInfo, PipelineCounts, PluginFailure, RateRefusal, RenderMode,
    StateError, StateUse, TerminationReason,
};
use tutti_units::{Normalized, ParamId};

/// What a handle over a served plugin reaches it through: the bridge's own
/// [`AnyThread`], except that a render mode is announced through the plugin's
/// lifecycle, which records the want and the tell, so the plugin's answer
/// settles it and is never read as a mode nobody asked for.
pub(super) struct ServedHandle {
    served: IpcController,
    live: LiveView<Ipc>,
}

impl ServedHandle {
    pub(super) fn new(served: IpcController, live: LiveView<Ipc>) -> Self {
        Self { served, live }
    }
}

impl AnyThread for ServedHandle {
    fn parameter_descriptors(&self) -> Answer<Option<Vec<ParameterInfo>>> {
        self.served.parameter_descriptors()
    }

    fn parameter_value(&self, id: ParamId) -> Answer<Option<Normalized>> {
        self.served.parameter_value(id)
    }

    fn set_parameter_value(&self, id: ParamId, value: Normalized) -> Result<(), ParamWriteError> {
        self.served.set_parameter_value(id, value)
    }

    fn parameter_plain(&self, id: ParamId, value: Normalized) -> Option<f64> {
        self.served.parameter_plain(id, value)
    }

    fn parameter_normalized(&self, id: ParamId, plain: f64) -> Answer<Option<Normalized>> {
        self.served.parameter_normalized(id, plain)
    }

    fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String> {
        self.served.parameter_text(id, value)
    }

    fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized> {
        self.served.parameter_value_from_text(id, text)
    }

    fn save_state(&self, state_use: StateUse) -> Result<Vec<u8>, StateError> {
        self.served.save_state(state_use)
    }

    fn set_render_mode(&self, mode: RenderMode) -> bool {
        self.live
            .announce_render_mode(mode, |mode| AnyThread::set_render_mode(&self.served, mode))
    }

    fn set_bypassed(&self, site: OwnBypass, bypassed: bool) -> BypassWrite {
        self.served.set_bypassed(site, bypassed)
    }

    fn bypassed(&self, site: OwnBypass) -> Option<bool> {
        self.served.bypassed(site)
    }

    fn set_automation_mode(&self, mode: AutomationMode) -> Result<(), AutomationModeError> {
        self.served.set_automation_mode(mode)
    }
}

impl Health for ServedHandle {
    fn is_crashed(&self) -> bool {
        Health::is_crashed(&self.served)
    }

    fn crash_cause(&self) -> Option<PluginFailure> {
        self.served.crash_cause()
    }

    fn rate_refusal(&self) -> Option<RateRefusal> {
        self.served.rate_refusal()
    }

    fn block_counts(&self) -> PipelineCounts {
        self.served.block_counts()
    }

    fn refused_edits(&self) -> u64 {
        self.served.refused_edits()
    }

    fn terminate(&self, reason: TerminationReason) {
        self.served.terminate(reason);
    }
}
