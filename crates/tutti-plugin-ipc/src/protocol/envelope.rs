//! Wire-envelope enums — the host↔server message types.
//!
//! Each connection has its own types: [`AudioRequest`] and [`AudioReply`] on the audio
//! connection, [`ControlRequest`] and [`ControlFrame`] (a [`ControlReply`] or [`News`]) on the
//! control one, so a message of the other connection has no variant to be written as. All are
//! postcard-encoded, so **variant order is wire-significant**: a discriminant is a
//! varint over declaration order, and inserting mid-enum renumbers every later
//! variant. Append, and bump
//! [`PROTOCOL_VERSION`](super::PROTOCOL_VERSION).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::process::ProcessAudioData;
use super::shm::SlabLayout;
use dpi::Size;
use tutti_buffers::FloatWidth;
use tutti_plugin_types::MidiEventVec;
use tutti_plugin_types::Samples;
use tutti_plugin_types::{BlockStatus, ContentScale, PluginId, StateUse};
use tutti_plugin_types::{
    LoadedPlugin, ParameterInfo, PluginDescriptor, PluginFormat, PluginTail, Preset, PresetId,
};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::SampleRate;

/// Wire-deserialization fallback for [`ControlRequest::LoadPlugin::block_size`]
/// when a message arrives without the field. The operative value at runtime
/// comes from `config.max_buffer_size` (see `subprocess::launch`); this is
/// only a safety net.
pub(crate) const DEFAULT_BLOCK_SIZE: usize = 512;

/// The format a message that names none asks for: 32-bit, which every plugin processes.
pub(crate) fn default_format() -> FloatWidth {
    FloatWidth::F32
}

fn default_block_size() -> usize {
    DEFAULT_BLOCK_SIZE
}

/// A state staged for the next load: what [`ControlRequest::LoadPlugin`] declares
/// it is restoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedState {
    /// The staged state's length in bytes.
    pub len: u64,
    /// What the state is restored for.
    pub state_use: StateUse,
}

/// A request on the audio connection: a block, the parameters outside one, or a reset.
///
/// Variant order is wire-significant; see the module docs.
// `ProcessAudio` is inline, as `AudioReply::AudioProcessed` is: it is the
// message of every block, and a box would be an allocation at each end per
// block.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AudioRequest {
    /// Process one audio block. Audio rides the shared `AudioSlab`, in the ring
    /// slot `seq` selects; the payload carries the per-block side-band.
    ProcessAudio(ProcessAudioData),
    /// Deliver parameter changes outside a block, on the audio connection, in
    /// order with the blocks: the parameters the host wrote that no block has
    /// carried, as one point per parameter on the first frame. The server
    /// applies them through the format's own route for flushing parameters
    /// (CLAP `params.flush`, VST3 `process` with no samples, a direct set for
    /// VST2 and AU) between blocks, and answers
    /// [`ParametersApplied`](AudioReply::ParametersApplied).
    ApplyParameters {
        /// The changes, each parameter's newest value.
        param_changes: tutti_plugin_types::ParameterChanges,
    },
    /// Clear the plugin's internal state — tails, delay lines, voices.
    Reset,
}

/// A reply on the audio connection.
///
/// Variant order is wire-significant; see the module docs.
// `AudioProcessed` holds the inline `MidiEventVec` (~5 KB). A reply is decoded
// into one stack value and moved once to the lane thread's queue; boxing it
// would put a heap allocation on every block's reply.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AudioReply {
    /// Acknowledges a processed block. Audio output goes into the block's slot
    /// in the shared `AudioSlab`'s output ring, published there before this
    /// message is sent; the measured latency and any MIDI the plugin emitted
    /// travel here. (Parameter / note-expression output is not routed back.)
    /// `midi_out` is capped at `MIDI_STACK_CAPACITY` server-side; the host
    /// refuses a longer list at decode.
    ///
    /// **This message is not what makes the audio readable.** The slab carries a
    /// per-slot generation counter and answers that itself, so the host emits
    /// silence for an unpublished block even if this arrives for it. The echoed
    /// `seq` is for diagnostics and ordering.
    AudioProcessed {
        /// Wall-clock time the plugin spent in `process`, in microseconds.
        latency_us: u64,
        /// Echo of the request's [`ProcessAudioData::seq`].
        #[serde(default)]
        seq: u64,
        /// MIDI the plugin emitted during the block.
        #[serde(default, with = "super::wire::midi")]
        midi_out: MidiEventVec,
        /// What the plugin said of itself at the end of the block (CLAP's
        /// process status). `Sleep` is sent only when the
        /// block's output was silent, so the host may park on it.
        #[serde(default)]
        status: BlockStatus,
        /// The server did not publish the block's audio, for any reason: its
        /// input slot had been recycled before it ran, the block did not fit
        /// the slab or the plugin's ports, an output channel was not written,
        /// or the plugin refused the sample rate. The host reads the block as
        /// silence, and counts a missed one refused.
        #[serde(default)]
        refused: bool,
    },
    /// The subprocess failed at something it was asked to do.
    ///
    /// Fire-and-forget: nothing awaits this, so a request that a caller blocks
    /// on needs its own reply variant rather than relying on this.
    Error {
        /// Human-readable reason, already formatted subprocess-side.
        message: String,
    },
    /// Handshake sent once the subprocess is live. Carries the wire
    /// [`PROTOCOL_VERSION`](super::PROTOCOL_VERSION) so a host and subprocess
    /// built from mismatched commits refuse to proceed rather than mis-parsing
    /// each other's messages (postcard is not self-describing). `#[serde(default)]`
    /// = 0 for an ancient binary that predates the field → treated as a mismatch.
    Ready {
        /// The subprocess's [`PROTOCOL_VERSION`](super::PROTOCOL_VERSION).
        #[serde(default)]
        protocol_version: u32,
    },
    /// The changes of an [`ApplyParameters`](AudioRequest::ApplyParameters) are
    /// in the plugin.
    ParametersApplied,
}

/// A request on the control connection: everything but a block.
///
/// Variant order is wire-significant; see the module docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlRequest {
    /// Instantiate and activate the plugin, ready to process audio. A state
    /// staged before ([`StageStateChunk`](Self::StageStateChunk)) and declared
    /// in `state` is restored between the two; the server says
    /// [`ControlReply::RestoringState`] just before it starts.
    LoadPlugin {
        /// Filesystem path to the plugin bundle or shared library.
        path: PathBuf,
        /// The format whose loader opens the file: the catalog's reading of it,
        /// which a path's extension need not give (a link `Foo.clap` to
        /// `libfoo.so` is CLAP).
        format: PluginFormat,
        /// Which plugin of the file to load, when it exports several. The
        /// server hands it, with the path, to its format's
        /// `OutOfProcess::load_served`. The file's first plugin when it
        /// chooses none; an id of another format than `format` is refused.
        ///
        /// A [`PluginId`] crosses as itself: it is plain data (a CLAP id's text,
        /// a VST3 class id's sixteen bytes, an AU's three codes, a VST2 number),
        /// and a value the peer cannot make is a decode failure.
        plugin: Option<PluginId>,
        /// Sample rate, an `f64` on the wire because plugin ABIs denominate it so.
        sample_rate: SampleRate,
        /// Maximum frames per [`ProcessAudio`](AudioRequest::ProcessAudio) block. The
        /// plugin sizes its internal buffers from this, so a later block may not
        /// exceed it.
        #[serde(default = "default_block_size")]
        block_size: usize,
        /// Sample format the host would rather exchange; the subprocess replies
        /// with what it actually negotiated.
        #[serde(default = "default_format")]
        preferred_format: FloatWidth,
        /// Name of the shared-memory audio slab, empty if not yet set up.
        #[serde(default)]
        shm_name: String,
        /// The state this load restores, staged before it
        /// ([`StageStateChunk`](Self::StageStateChunk)): what the host expects the
        /// server to hold. The server refuses a load whose declaration and stage
        /// do not agree (a state staged and none declared, or the reverse, or
        /// another length), so a stage left over from another load is never
        /// restored by this one.
        #[serde(default)]
        state: Option<StagedState>,
    },
    /// Deactivate and destroy the plugin instance, keeping the subprocess alive.
    UnloadPlugin,
    /// Enter or leave an automation gesture, so the plugin can record it.
    SetAutomationState {
        /// Format-neutral automation mode; the format loader encodes it onto its
        /// own ABI server-side. See [`AutomationMode`](tutti_plugin_types::AutomationMode).
        mode: tutti_plugin_types::AutomationMode,
    },
    /// Read one parameter's current normalized value.
    GetParameter {
        /// Which parameter — opaque handle or positional index, per format.
        param_id: ParamId,
    },
    /// Ask for every parameter the plugin declares.
    GetParameterList,
    /// Re-rate the plugin. Deactivates and reactivates it server-side, and is
    /// answered by [`ControlReply::SampleRateSet`]: the host must learn
    /// whether the plugin runs at the rate before it renders another block.
    SetSampleRate {
        /// Sample rate, an `f64` on the wire because plugin ABIs denominate it so.
        rate: SampleRate,
    },
    /// Ask the plugin to serialize its state for `state_use`, answered by
    /// `ControlReply::StateData`.
    SaveState {
        /// What the state is saved for.
        state_use: StateUse,
    },
    /// One slice of a plugin state being restored, in order.
    ///
    /// State is the only message whose size a plugin chooses, and it routinely
    /// exceeds what a single frame may carry — see [`MAX_STATE_BYTES`]. So it
    /// travels as a sequence rather than one `LoadState`, and the receiver
    /// reassembles. Every frame stays under [`MAX_FRAME_BYTES`], so the
    /// allocation and deadline bounds on the wire are untouched by a large
    /// state.
    ///
    /// `seq` is checked, not trusted: a gap or a repeat means the sequence is
    /// not what the sender thinks it is, and silently reassembling it would
    /// hand the plugin a corrupt blob it would then try to parse.
    ///
    /// [`MAX_STATE_BYTES`]: tutti_plugin_types::MAX_STATE_BYTES
    /// [`MAX_FRAME_BYTES`]: crate::MAX_FRAME_BYTES
    LoadStateChunk {
        /// Position in the sequence, starting at 0 and incrementing by one.
        seq: u32,
        /// `Some` on the final chunk, naming what the whole state is restored
        /// for, and `None` on every other: the sequence names its use once. A
        /// one-chunk state is `Some` on `seq` 0.
        last: Option<StateUse>,
        /// Opaque, format-defined bytes. Never interpret them host-side.
        #[serde(with = "super::wire::bytes")]
        bytes: Vec<u8>,
    },
    /// Point the subprocess at the shared-memory audio slab.
    SetupSharedMemory {
        /// OS name of the shared-memory object to map.
        shm_name: String,
        /// Region geometry, which both sides must agree on exactly.
        layout: SlabLayout,
    },
    /// Ask the subprocess to exit cleanly.
    Shutdown,
    /// Tell the plugin whether it is rendering under realtime pressure.
    ///
    /// A sibling of [`SetSampleRate`](Self::SetSampleRate) rather than a field
    /// on [`ProcessAudio`](AudioRequest::ProcessAudio): three of the four formats can
    /// only accept this while the plugin is deactivated, and two of those
    /// rebuild buffers around it. Sending it per block would be both wasteful
    /// and unrepresentable.
    SetRenderMode {
        /// Realtime or offline.
        mode: tutti_plugin_types::RenderMode,
    },
    /// Ask for the plugin's preset list.
    GetPresetList,
    /// Ask the plugin to load one preset, by an id its list produced.
    ///
    /// [`PresetId`] is opaque and format-shaped —
    /// never construct one host-side. Three of the four formats number presets
    /// in a space that is not a position in the list, so an invented id loads
    /// the wrong preset rather than failing.
    LoadPreset {
        /// Opaque, format-shaped preset id taken from the plugin's own list.
        id: tutti_plugin_types::PresetId,
    },
    /// Ask which preset the plugin considers current.
    GetCurrentPreset,
    /// Ask the plugin to render `value` as the text it would display for it.
    ///
    /// `value` is normalized, as everywhere on this wire; the loader converts
    /// to its format's domain.
    GetParameterText {
        /// Which parameter — opaque handle or positional index, per format.
        param_id: tutti_units::ParamId,
        /// The value to render, normalized.
        #[serde(with = "super::wire::normalized")]
        value: tutti_units::Normalized,
    },
    /// Ask the plugin to parse `text` into a value.
    ///
    /// Note this is not a pure query for VST2, whose `effString2Parameter`
    /// applies the parsed value as it reads it — see that loader's
    /// `parameter_value_from_text`.
    GetParameterValueFromText {
        /// Which parameter — opaque handle or positional index, per format.
        param_id: tutti_units::ParamId,
        /// Display text to parse, in whatever form the plugin renders.
        text: String,
    },
    /// One slice of a plugin state to restore into the plugin the **next**
    /// [`LoadPlugin`](Self::LoadPlugin) opens, in order, as
    /// [`LoadStateChunk`](Self::LoadStateChunk) is for the plugin that is open.
    ///
    /// A load activates the plugin, and a plugin may take its mode from the
    /// state when it is activated; a state restored after is restored too late.
    /// So the state is staged first and the server applies it between opening the
    /// plugin and activating it. Answered once, on `last` (or on a failure), by
    /// [`ControlReply::StateLoaded`]: `error: None` means staged, not
    /// applied.
    StageStateChunk {
        /// Position in the sequence, starting at 0 and incrementing by one.
        seq: u32,
        /// Whether this is the final chunk.
        last: bool,
        /// Opaque, format-defined bytes. Never interpret them host-side.
        #[serde(with = "super::wire::bytes")]
        bytes: Vec<u8>,
    },
    /// Ask the plugin's format for the plain value `value` stands for
    /// (`Control::parameter_plain`): the conversion is the format's, so the
    /// host never computes it from a range. Answered by
    /// [`ControlReply::ParameterPlain`].
    GetParameterPlain {
        /// Which parameter — opaque handle or positional index, per format.
        param_id: tutti_units::ParamId,
        /// The value to convert, normalized.
        #[serde(with = "super::wire::normalized")]
        value: tutti_units::Normalized,
    },
    /// Ask the plugin's format for the normalized value a plain one stands for
    /// (`Control::parameter_normalized`). Answered by
    /// [`ControlReply::ParameterNormalized`].
    GetParameterNormalized {
        /// Which parameter — opaque handle or positional index, per format.
        param_id: tutti_units::ParamId,
        /// The plain value, in the parameter's own unit.
        plain: f64,
    },
    /// Open the plugin's editor on the served instance itself, in this server
    /// process, answered by
    /// [`ControlReply::EditorOpened`].
    ///
    /// `parent` is the host window's platform number
    /// (`WindowHandle::as_u64`), where a window can be embedded across
    /// processes (`WindowHandle::embeds_across_processes`: an X11 window id),
    /// and `None` for the plugin's own floating window. A number the server
    /// cannot embed into is refused in the answer.
    OpenEditor {
        /// The host window to embed into, or `None` for a floating editor.
        parent: Option<u64>,
        /// The parent's backing-store scale; the format takes it where it
        /// can.
        scale: ContentScale,
    },
    /// Close the editor. Nothing happens when none is open; there is no
    /// answer.
    CloseEditor,
    /// Ask the open editor to take `size`, answered by
    /// [`ControlReply::EditorSized`].
    SetEditorSize {
        /// The size the host's window has, in either unit: the server's format
        /// converts it at the scale the editor opened with. A `dpi::Size` crosses
        /// the wire serialized through `dpi`'s `serde` feature, the unit part of
        /// the value; a logical one that is not finite and non-negative is
        /// refused on decode (see `wire::size`).
        #[serde(with = "super::wire::size")]
        size: Size,
    },
    /// What the host says of the track the plugin sits on, for the formats
    /// that tell their plugin (CLAP `track-info`). Fire-and-forget; a format
    /// without the extension drops it.
    SetTrackInfo {
        /// The format-neutral value; the server's format answers the plugin's
        /// reads from it.
        info: tutti_plugin_types::TrackInfo,
    },
}

/// What the server answers a [`ControlRequest`] with.
///
/// Variant order is wire-significant; see the module docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlReply {
    /// The plugin was loaded successfully.
    PluginLoaded {
        /// The plugin the load opened: the chosen one, or the file's first when
        /// it chose none.
        id: PluginId,
        /// Catalog identity (name, vendor, version, native class, editor).
        descriptor: Box<PluginDescriptor>,
        /// Engine-wiring data from instantiation (bus widths, latency, f64).
        #[serde(default)]
        loaded: LoadedPlugin,
        /// The sample format actually agreed on, which may differ from the
        /// host's preference.
        negotiated_format: FloatWidth,
        /// The plugin refused the staged state
        /// ([`ControlRequest::StageStateChunk`]): it loaded at its defaults. `None`
        /// when it took the state, and when none was staged.
        #[serde(default)]
        state_error: Option<String>,
    },
    /// One parameter's normalized value, or `None` if the plugin declined.
    ParameterValue {
        /// Normalized `0.0..=1.0`.
        #[serde(with = "super::wire::normalized_opt")]
        value: Option<Normalized>,
    },
    /// Every parameter the plugin declares, in the plugin's own order.
    ParameterList {
        /// The declarations, empty if the plugin exposes no parameters.
        parameters: Vec<ParameterInfo>,
    },
    /// One slice of the plugin's serialized state, answering
    /// [`ControlRequest::SaveState`], in order.
    ///
    /// The mirror of [`ControlRequest::LoadStateChunk`] and chunked for the same
    /// reason: a sample-embedding instrument's state does not fit one frame.
    /// The host reassembles against [`MAX_STATE_BYTES`].
    ///
    /// An **empty** state — a plugin that declines, or no plugin loaded — is
    /// one chunk with `last: true` and no bytes, not an absent reply. A caller
    /// waiting on a sequence must always see it terminate.
    ///
    /// [`MAX_STATE_BYTES`]: tutti_plugin_types::MAX_STATE_BYTES
    StateChunk {
        /// Position in the sequence, starting at 0 and incrementing by one.
        seq: u32,
        /// Whether this is the final chunk.
        last: bool,
        /// Opaque, format-defined bytes. Never interpret them host-side.
        #[serde(with = "super::wire::bytes")]
        bytes: Vec<u8>,
    },
    /// The plugin's preset list.
    ///
    /// Empty when the format cannot enumerate — CLAP, whose discovery is a
    /// factory-level extension this host does not bind. That is **not** the
    /// same as "this plugin has no presets"; a caller separates the two by
    /// reading `Features::PRESET_LIST`, which is unprobed for CLAP and
    /// `Some(false)` for a format that was asked and declined.
    PresetList {
        /// The plugin's presets, in its own order.
        presets: Vec<Preset>,
    },
    /// Whether the plugin accepted a [`LoadPreset`](ControlRequest::LoadPreset).
    ///
    /// `false` is a refusal rather than an error, and is also the honest
    /// answer for VST3: its programs are selected through the parameter path,
    /// so there is no direct load to report on.
    PresetLoaded {
        /// Whether the plugin accepted the preset.
        ok: bool,
    },
    /// The preset the plugin considers current, if it will say. `None` means
    /// the format has no query (VST3, CLAP) or the plugin declined — never
    /// "the first one".
    CurrentPreset {
        /// The current preset's opaque id, if the plugin will say.
        id: Option<PresetId>,
    },
    /// The subprocess mapped the shared-memory slab and is ready to process.
    SharedMemoryReady,
    /// The subprocess failed at something it was asked to do.
    ///
    /// Fire-and-forget: nothing awaits this, so a request that a caller blocks
    /// on needs its own reply variant rather than relying on this.
    Error {
        /// Human-readable reason, already formatted subprocess-side.
        message: String,
    },
    /// Handshake sent once the subprocess is live. Carries the wire
    /// [`PROTOCOL_VERSION`](super::PROTOCOL_VERSION) so a host and subprocess
    /// built from mismatched commits refuse to proceed rather than mis-parsing
    /// each other's messages (postcard is not self-describing). `#[serde(default)]`
    /// = 0 for an ancient binary that predates the field → treated as a mismatch.
    Ready {
        /// The subprocess's [`PROTOCOL_VERSION`](super::PROTOCOL_VERSION).
        #[serde(default)]
        protocol_version: u32,
    },
    /// The subprocess is exiting.
    Shutdown,
    /// The plugin's display string for the value that was asked about. `None`
    /// means the plugin did not answer — the caller renders the raw number
    /// rather than treating this as an empty label.
    ParameterText {
        /// The plugin's display string, or `None` if it did not answer.
        text: Option<String>,
    },
    /// The value the plugin parsed a string into, normalized. `None` means it
    /// could not parse it, and the caller must leave its field unchanged.
    ParameterValueFromText {
        /// The parsed value, normalized.
        #[serde(with = "super::wire::normalized_opt")]
        value: Option<Normalized>,
    },
    /// Acknowledges a `ControlRequest::LoadState`, carrying the plugin's refusal
    /// if it had one.
    ///
    /// A caller awaits this frame, so the answer reports that the state was
    /// *loaded* rather than merely that the request was sent. It carries the
    /// message rather than a bool because the subprocess has already formatted
    /// it and a caller wants to show the user *why* their preset did not load.
    StateLoaded {
        /// The plugin's refusal, or `None` on success.
        error: Option<String>,
    },
    /// Answers a `ControlRequest::SetSampleRate`: the plugin runs at the rate, or
    /// it does not.
    ///
    /// A refusal is not "keep the old rate": the plugin may have rolled back to
    /// the rate it had or been left deactivated, and the host's blocks are
    /// already at the new one, so neither renders the audio the host asked for.
    /// The server renders silence for a refused plugin rather than let it be
    /// heard at the wrong pitch, and the host retries it at every later rate
    /// change.
    SampleRateSet {
        /// The rate that was asked for, echoed so a reply read late is never
        /// attributed to a later request.
        rate: SampleRate,
        /// The plugin's reason, formatted subprocess-side, or `None` when it
        /// now runs at `rate`.
        error: Option<String>,
    },
    /// Sent while a load is in progress, just before the server restores the
    /// staged state into the plugin it has opened: the point after which a
    /// failure is the state's to answer for, and the host's wait is for a
    /// restore (one opaque plugin call that may take as long as the state is
    /// large) and not for an open.
    RestoringState,
    /// The plain value the format converted a normalized one to. `None` means
    /// the format has none (a parameter it does not declare, or VST2 without an
    /// integer range).
    ParameterPlain {
        /// The plain value, in the parameter's own unit.
        plain: Option<f64>,
    },
    /// The normalized value the format converted a plain one to. `None` as for
    /// [`ParameterPlain`](Self::ParameterPlain).
    ParameterNormalized {
        /// The normalized value.
        #[serde(with = "super::wire::normalized_opt")]
        value: Option<Normalized>,
    },
    /// Answers a [`ControlRequest::OpenEditor`]: the editor's size when it
    /// embedded, `None` for a floating one, or why it did not open.
    EditorOpened {
        /// The size, or the refusal as the server formatted it.
        #[serde(with = "super::wire::size_opt_result")]
        result: Result<Option<Size>, String>,
    },
    /// Answers a [`ControlRequest::SetEditorSize`]: the size the editor took, or
    /// why it did not.
    EditorSized {
        /// The size taken, or the refusal as the server formatted it.
        #[serde(with = "super::wire::size_result")]
        result: Result<Size, String>,
    },
}

/// What the plugin says unprompted, on the control connection.
///
/// Variant order is wire-significant; see the module docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum News {
    /// The plugin edited one of its own parameters, typically from its editor: a
    /// [`ParamEdit`](tutti_plugin_types::ParamEdit) (the parameter by the id the plugin reported,
    /// its normalized value as an `f64`, and where in a gesture it fell). A gesture's `Begin`,
    /// changes and `End` arrive in the order the plugin made them.
    ParameterChanged {
        /// The edit.
        edit: tutti_plugin_types::ParamEdit,
    },
    /// Plugin reported a latency change at runtime. Host updates the
    /// corresponding `PluginClient::set_latency` so the latency the node
    /// declares in its `Shape` is the new value. Note: does NOT trigger PDC
    /// re-analysis; the graph must be committed again for compensation to
    /// update.
    LatencyChanged {
        /// The plugin's new reported latency, in frames.
        samples: Samples,
    },
    /// Plugin reported a new tail length at runtime. The host updates the tail
    /// the node declares in its `Shape`, so a bounce started after the change sizes
    /// its render from the current decay rather than the one loaded with.
    ///
    /// CLAP-only in practice: it is the one format that pairs its tail query
    /// with a host `changed` callback. VST3's restart flags have no tail member
    /// and AU has no tail property listener, so both are settled at load.
    TailChanged {
        /// The plugin's new tail, which may be bounded, unbounded or unreported.
        tail: PluginTail,
    },
    /// Plugin changed its own parameter values at runtime (e.g. an in-plugin
    /// preset load). The host should re-read parameter values from the plugin.
    PluginParamValuesChanged,
    /// Plugin changed parameter titles/units/flags. The host should re-pull the
    /// parameter list.
    PluginParamTitlesChanged,
    /// Plugin's bus arrangement changed and was re-enumerated server-side. The
    /// host should rewire its audio graph from the plugin's refreshed metadata.
    PluginIoChanged {
        /// The buses the plugin has now, when they differ from the ones the
        /// host was last told (at load, or by an earlier message); `None` when
        /// the host should only re-read (a plugin that stayed inactive after a
        /// refusal, or one that was lost).
        layout: Option<tutti_plugin_types::Layout>,
    },
    /// Plugin was torn down and rebuilt in place at the plugin's request
    /// (`kReloadComponent`). The host should resync all plugin state — it is
    /// effectively a fresh instance.
    PluginReloaded,
    /// The plugin asked its window to take `size` (CLAP's
    /// `gui.request_resize`): the host resizes the window it gave the editor
    /// and answers with [`ControlRequest::SetEditorSize`].
    EditorWantsSize {
        /// The size the plugin asked for, in its format's unit.
        #[serde(with = "super::wire::size")]
        size: Size,
    },
    /// The server closed the editor on its own account: the plugin it belongs
    /// to was unloaded or replaced. Not an answer to a
    /// [`ControlRequest::CloseEditor`].
    EditorClosed,
    /// The plugin asked to be processed again (CLAP's `request_process`,
    /// from any of its threads), after it slept. The host wakes the node it
    /// parked, so the next block reaches the plugin.
    ProcessRequested,
    /// The plugin asked the host's transport to do something (CLAP
    /// `transport-control`). Unlike a latency, each request counts: they
    /// arrive in the order the plugin made them.
    TransportRequested {
        /// What it asked.
        request: tutti_plugin_types::TransportRequest,
    },
}

/// One frame the server sends on the control connection: an answer or news.
///
/// Variant order is wire-significant; see the module docs.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlFrame {
    /// The answer to a request.
    Reply(ControlReply),
    /// Something the plugin did on its own.
    News(News),
}

impl From<ControlReply> for ControlFrame {
    fn from(reply: ControlReply) -> Self {
        Self::Reply(reply)
    }
}

impl From<News> for ControlFrame {
    fn from(news: News) -> Self {
        Self::News(news)
    }
}
