//! Post-`initialize()` VST3 state. Audio processing is **not** active here:
//! [`Loaded::activate`](tutti_plugin_types::Loaded::activate) splits the plugin
//! into a [`ProcessorAny`](crate::ProcessorAny), which renders, and a
//! [`Vst3Controller`](crate::Vst3Controller), which holds everything else.
//!
//! `Vst3Loaded` serves GUI-only hosting, offline parameter inspection and state
//! save/restore: the parameter tree, units and program lists, note expression
//! and keyswitch tables, `IMidiLearn`, XML representations, state, and the
//! editor. `process()` lives on the Processor only. The Controller `Deref`s to
//! the `Vst3Loaded` it holds, so all of this stays reachable while the plugin
//! renders.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use tutti_buffers::ChannelTopology;
use tutti_buffers::FloatWidth;
use tutti_plugin_types::{
    ParamAccess, ParamFlags, ParamSteps, ParameterInfo, PluginDescriptor, PluginParamRange,
};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_vst3_ffi::{
    BusDirection, CreateError, EditorError, MediaType, Plugin, PluginNotification,
};

use crate::error::{LoadCause, LoadStage, Result, Vst3Error};
use crate::types::{
    ContentScale, EditorCapabilities, PluginInfo, PrefetchSupport, Vst3KeyswitchInfo,
    Vst3NoteExpressionInfo, Vst3ParameterInfo, Vst3ProgramListInfo, Vst3UnitInfo, WindowHandle,
};
use tutti_plugin_types::Uid;

use super::instance::usable_topology;
use super::library::Library;
use super::midi_learn::MidiLearnConsumer;

mod buses;
mod editor;
mod lifecycle;

pub use lifecycle::class_id;
mod parameters;
pub(crate) use parameters::unit_groups;
mod state;

/// Plugin instance that has been `initialize()`'d and has usable parameter,
/// editor, and state surfaces, but is **not** processing audio.
///
/// [`Loaded::activate`](tutti_plugin_types::Loaded::activate) splits it into a
/// Processor and a Controller to render. For GUI-only hosting (no audio ever),
/// stay here: skip the `setActive(1) + setProcessing(1)` cost entirely.
pub struct Vst3Loaded {
    // Declaration order IS teardown order — Rust drops fields top-to-bottom.
    // Every COM object below is implemented *inside* the plugin DSO, so its
    // vtable lives in that module's text: releasing one after the DSO is gone
    // jumps through a dangling function pointer. `_library` therefore has to be
    // the LAST field, not the first. (Same rule `Library` documents for its
    // own fields.)
    pub(super) plugin: Plugin,
    pub(super) info: PluginInfo,
    /// IMidiLearn forwarding: armed off the main thread, fed captured CCs from
    /// the audio thread, drained in [`Self::poll_plugin_notifications`]. Built at load
    /// and outlives activate/deactivate cycles.
    pub(super) midi_learn: MidiLearnConsumer,
    /// What `rebuild` and `Reopen` load again.
    pub(super) origin: tutti_plugin_types::PluginRef,
    /// The collector of this plugin's owner, which a reload gives its new instance.
    pub(super) work: tutti_plugin_types::MainWorkHandle,
    /// The catalog identity, with the editor measured on first ask.
    pub(super) descriptor: OnceLock<PluginDescriptor>,
    /// Whether the plugin is activated, and at which width.
    pub(super) active: Activation,
    /// Writes a rejoin took from the halves that no block had delivered, for
    /// the next activation's first block.
    pub(super) carried: Vec<(ParamId, Normalized)>,
    /// The host's gestures toward the plugin's controller, drained on
    /// [`flush_edits`](tutti_plugin_types::Control::flush_edits); the same list
    /// for the plugin's life.
    pub(in crate::host) host_edits: Arc<tutti_plugin_types::HostEdits>,
    /// The writes the Processor refused when it took them (an undeclared or
    /// read-only parameter), over the plugin's life: every activation's
    /// Processor adds to it, and the Control reads it.
    pub(super) refused: tutti_rt::Tally<u64>,
    /// What the plugin declared when it was activated, and what each parameter
    /// declares of its writability, sorted by id: the table the Processor drops
    /// writes against. `None` while inactive. A write is answered from it while
    /// active, so `Ok` means the Processor will apply it; the plugin's live
    /// parameter info may have moved since (a `kParamTitlesChanged` restart), and
    /// the table is read again at the next activation.
    pub(super) declared: Option<Arc<[(ParamId, ParamAccess)]>>,
    /// What the `IMidiMapping` table declares, read when the table is rebuilt
    /// (at load, at every activation and when the plugin says it changed).
    pub(super) assignments: Vec<tutti_plugin_types::MidiAssignment>,
    /// How many notifications the handler had dropped at the last poll: a
    /// poll that finds more reports every parameter value stale.
    pub(super) dropped_seen: u64,
    /// Keeps the DSO loaded for the plugin's lifetime. **Must stay last** — see
    /// the teardown-order note at the top of this struct.
    pub(super) _library: Arc<Library>,
}

/// The plugin's activation, as the host last made it.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Activation {
    /// The sample format of the current activation; `None` while `setActive(1)`
    /// has not run or `setActive(0)` has since.
    pub(super) format: Option<FloatWidth>,
}

impl Activation {
    pub(super) fn is_active(self) -> bool {
        self.format.is_some()
    }

    pub(super) fn is_f64(self) -> bool {
        self.format == Some(FloatWidth::F64)
    }
}

/// How to load a plugin from a bundle.
///
/// The default loads the bundle's first audio class.
#[derive(Debug, Clone, Default)]
pub struct LoadOptions {
    class_id: Option<Uid>,
}

impl LoadOptions {
    /// The default: the bundle's first audio class.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads the audio class with this id from a bundle that exports several:
    /// the id a [`PluginId::Vst3`](tutti_plugin_types::PluginId) holds (and
    /// [`class_id`] gives over a [`Library::class_info`]). A display name is
    /// not an id.
    ///
    /// One VST3 bundle may export many plugins: that is the normal shape for a
    /// commercial suite, and `mda-vst3` exports 34 audio classes from one
    /// binary.
    #[must_use]
    pub fn class_id(mut self, id: Uid) -> Self {
        self.class_id = Some(id);
        self
    }
}

impl std::fmt::Debug for Vst3Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst3Loaded")
            .field("info", &self.info)
            .field("_library", &self._library)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod activation_tests {
    use super::Activation;
    use tutti_buffers::FloatWidth;

    /// Inactive is the absence of a format; only a 64-bit activation is f64.
    #[test]
    fn activation_is_its_format() {
        let off = Activation::default();
        assert!(!off.is_active() && !off.is_f64());
        let f32_ = Activation {
            format: Some(FloatWidth::F32),
        };
        assert!(f32_.is_active() && !f32_.is_f64());
        let f64_ = Activation {
            format: Some(FloatWidth::F64),
        };
        assert!(f64_.is_active() && f64_.is_f64());
    }
}
