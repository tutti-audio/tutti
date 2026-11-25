//! [`RenderModeSupport`]: how a format takes a change of
//! [`RenderMode`](tutti_graph::RenderMode).
//!
//! The mode is the session's, carried by `tutti_graph::Prepare` and copied into
//! a plugin's [`Prepare`](crate::Prepare); the mapping onto a plugin ABI lives
//! at each format's edge. Only VST3 carries it on the block
//! (`ProcessData::processMode`), and even VST3 reaches `kOffline` only through
//! `setupProcessing`. CLAP's `clap_plugin_render.set` is `[main-thread]`
//! (`ext/render.h:33`), AU's `kAudioUnitProperty_OfflineRender` may size
//! buffers at `AudioUnitInitialize`, and VST2 answers it through a host
//! callback, so a plugin takes it beside the sample rate and block size.

/// How a plugin takes a change of [`RenderMode`](tutti_graph::RenderMode), fixed for a loaded plugin.
///
/// Every format has the concept, but they differ in when the plugin reads it,
/// and a plugin may lack it altogether. A host asks this once and acts on the
/// answer: a [`Live`](Self::Live) plugin is told, an
/// [`OnActivation`](Self::OnActivation) one is deactivated and activated again
/// in the new mode, and an [`Unsupported`](Self::Unsupported) one is left
/// running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RenderModeSupport {
    /// The plugin takes the mode while active: CLAP's `render.set`, or VST2's
    /// answer to `audioMasterGetCurrentProcessLevel`, which the host changes
    /// whenever it likes.
    Live,
    /// The plugin reads the mode only when it is activated: VST3's
    /// `setupProcessing`, AU's `AudioUnitInitialize`. A change is a
    /// deactivation and an activation in the new mode.
    OnActivation,
    /// The plugin has no render mode (a CLAP plugin without `clap.render`, an
    /// Audio Unit that refuses `kAudioUnitProperty_OfflineRender`) and renders
    /// the same either way. A change is recorded for the next activation and
    /// never reactivates the plugin.
    Unsupported,
}

/// What a plugin did with the render mode it was told
/// ([`Control::set_render_mode`](crate::Control::set_render_mode)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModeTold {
    /// The plugin runs in the mode.
    Took,
    /// The plugin has a render mode and did not take this one: it runs in the
    /// mode it had.
    Refused,
    /// The plugin has no render mode to tell. Asked one anyway, the lifecycle
    /// reads it as a refusal: the plugin did not take it.
    Unsupported,
    /// The plugin was told and answers later, through
    /// [`Control::take_answers`](crate::Control::take_answers).
    Asked,
}
