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
use crate::{BlobWire, LaneWire, SlabWire};
use dpi::Size;
use tutti_buffers::FloatWidth;
use tutti_plugin_types::Samples;
use tutti_plugin_types::{BlockStatus, ContentScale, PluginId, StateUse};
use tutti_plugin_types::{EditorError, MidiEventVec, RateRefusalCause, ServerError, StateError};
use tutti_plugin_types::{
    ParameterInfo, PluginDescriptor, PluginFormat, PluginPreset, PluginTail, PluginWiring, PresetId,
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

/// The number a request on a connection carries and its answer echoes, which is
/// how the host knows what a reply answers: a reply is the answer to the request
/// of its id, whenever it comes and whatever else came before it.
///
/// The host numbers the requests of a connection from 1 and never repeats one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReqId(u64);

impl ReqId {
    /// The request numbered `raw`.
    #[must_use]
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A request on the control connection, with the id its answer carries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// What the answer to this request echoes.
    pub id: ReqId,
    /// What is asked.
    pub body: ControlRequest,
}

/// What the server is doing for a request that has no answer yet: a sign of
/// life the host waits longer on, and for a load the point it has reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Working {
    /// At work before any restore: opening the plugin (its library, instance and
    /// initialisation), or saving or applying a state.
    Opening,
    /// Restoring the staged state into the opened plugin: one opaque plugin
    /// call, after which a failure is the state's to answer for.
    Restoring,
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
        /// What the acknowledgement echoes.
        id: ReqId,
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
    /// The changes of an [`ApplyParameters`](AudioRequest::ApplyParameters) are
    /// in the plugin.
    ParametersApplied {
        /// The `id` of the request.
        id: ReqId,
    },
}

/// A lane the broker connection opens to the server.
///
/// Variant order is wire-significant; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Lane {
    /// The audio lane: blocks in, block replies out, served by the render thread.
    Audio,
}

/// A request on the control connection: everything but a block.
///
/// Variant order is wire-significant; see the module docs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlRequest {
    /// Instantiate and activate the plugin, ready to process audio. A state
    /// staged before ([`StageState`](Self::StageState)) and declared
    /// in `state` is restored between the two; the server says
    /// [`ControlFrame::Working`] (`Restoring`) just before it starts.
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
        /// The state this load restores, staged before it
        /// ([`StageState`](Self::StageState)): what the host expects the
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
    /// Restore a plugin state, which follows this frame as a memory object of its
    /// own (`send_attached` writes both, and the server's transport reads both
    /// before this reaches its session), answered by
    /// [`ControlReply::StateLoaded`].
    ///
    /// State is the one message whose size a plugin chooses, and it routinely
    /// exceeds what a frame may carry (see [`MAX_STATE_BYTES`]), so it travels as
    /// an object and the frame only declares it. The receiver refuses an object
    /// of another length than `len`, and a `len` over the limit, before it
    /// copies a byte.
    ///
    /// [`MAX_STATE_BYTES`]: tutti_plugin_types::MAX_STATE_BYTES
    LoadState {
        /// What the state is restored for.
        state_use: StateUse,
        /// The length in bytes of the state, which the object must have.
        len: u64,
        /// What the frame carries of the object: nothing on unix, where the
        /// object follows, and its checked name on Windows.
        blob: BlobWire,
    },
    /// Point the subprocess at the shared-memory audio slab, which follows this
    /// frame as a descriptor of its own on unix (`send_setup` writes both, and the
    /// server's transport reads both before this reaches its session).
    SetupSharedMemory {
        /// What the frame carries of the slab: nothing on unix, where the
        /// descriptor follows, and the slab's checked name on Windows.
        slab: SlabWire,
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
    /// A plugin state to restore into the plugin the **next**
    /// [`LoadPlugin`](Self::LoadPlugin) opens, which follows this frame as a
    /// memory object, as [`LoadState`](Self::LoadState)'s does for the plugin
    /// that is open.
    ///
    /// A load activates the plugin, and a plugin may take its mode from the
    /// state when it is activated; a state restored after is restored too late.
    /// So the state is staged first and the server applies it between opening the
    /// plugin and activating it. Answered by [`ControlReply::StateLoaded`]:
    /// `error: None` means staged, not applied.
    StageState {
        /// The length in bytes of the state, which the object must have.
        len: u64,
        /// What the frame carries of the object, as for [`LoadState`](Self::LoadState).
        blob: BlobWire,
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
    /// Open a lane to the server: the audio lane's stream, whose server end is
    /// the descriptor that follows this frame on unix (`send_lane`) and the pipe
    /// `wire` names on Windows. Answered by [`ControlReply::LaneOpened`]; a lane
    /// is opened once, and a frame without its descriptor is refused.
    OpenLane {
        /// Which lane the stream is.
        lane: Lane,
        /// What the frame carries of the stream's server end: nothing on unix,
        /// where the descriptor follows, and the pipe's checked name on Windows.
        wire: LaneWire,
    },
    /// The app's controller entries, replacing the last set
    /// ([`Control::set_app_midi_map`](tutti_plugin_types::Control::set_app_midi_map)).
    /// Answered by [`ControlReply::AppMidiMapApplied`].
    SetAppMidiMap {
        /// The entries.
        app: Vec<tutti_plugin_types::MidiAssignment>,
    },
    /// Arms or disarms the plugin's own MIDI learn
    /// ([`Control::arm_plugin_midi_learn`](tutti_plugin_types::Control::arm_plugin_midi_learn)).
    /// Answered by [`ControlReply::PluginMidiLearn`].
    ArmPluginMidiLearn {
        /// Arm (`true`) or disarm.
        armed: bool,
    },
    /// The plugin's wiring read now
    /// ([`Control::wiring`](tutti_plugin_types::Control::wiring)), not the
    /// one announced at load. Answered by [`ControlReply::Wiring`].
    GetWiring,
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
        loaded: Box<PluginWiring>,
        /// The sample format actually agreed on, which may differ from the
        /// host's preference.
        negotiated_format: FloatWidth,
        /// The plugin refused the staged state
        /// ([`ControlRequest::StageState`]): it loaded at its defaults. `None`
        /// when it took the state, and when none was staged.
        #[serde(default)]
        state_error: Option<StateError>,
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
    /// The plugin's serialized state, answering [`ControlRequest::SaveState`]:
    /// the state follows this frame as a memory object, as
    /// [`ControlRequest::LoadState`]'s does, and the host refuses one of another
    /// length than `len`, or a `len` over [`MAX_STATE_BYTES`].
    ///
    /// An **empty** state (a plugin that declines, or no plugin loaded) is an
    /// object of length 0, not an absent reply.
    ///
    /// [`MAX_STATE_BYTES`]: tutti_plugin_types::MAX_STATE_BYTES
    StateReady {
        /// The length in bytes of the state, which the object must have.
        len: u64,
        /// What the frame carries of the object, as for
        /// [`ControlRequest::LoadState`].
        blob: BlobWire,
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
        presets: Vec<PluginPreset>,
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
        /// What failed.
        error: ServerError,
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
    /// refusal rather than a bool because a caller wants to show the user *why*
    /// their preset did not load, and whether another blob may do.
    StateLoaded {
        /// The refusal, or `None` on success.
        error: Option<StateError>,
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
        /// Why the plugin refused `rate`, or `None` when it now runs at it.
        refusal: Option<RateRefusalCause>,
    },
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
        /// The size, or why the editor did not open.
        #[serde(with = "super::wire::size_opt_result")]
        result: Result<Option<Size>, EditorError>,
    },
    /// Answers a [`ControlRequest::SetEditorSize`]: the size the editor took, or
    /// why it did not.
    EditorSized {
        /// The size taken, or why the editor did not take one.
        #[serde(with = "super::wire::size_result")]
        result: Result<Size, EditorError>,
    },
    /// The server holds its end of the lane an
    /// [`OpenLane`](ControlRequest::OpenLane) opened and serves it.
    LaneOpened,
    /// Who applies the entries of a [`ControlRequest::SetAppMidiMap`].
    AppMidiMapApplied {
        /// The plugin's answer.
        applied: tutti_plugin_types::MidiMapApplied,
    },
    /// Answers a [`ControlRequest::ArmPluginMidiLearn`].
    PluginMidiLearn {
        /// Armed, or why not.
        result: Result<(), tutti_plugin_types::PluginLearnError>,
    },
    /// Answers a [`ControlRequest::GetWiring`].
    Wiring {
        /// The plugin's wiring as it reads now.
        wiring: tutti_plugin_types::PluginWiring,
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
    /// The plugin was rebuilt in place ([`PluginReloaded`](Self::PluginReloaded)
    /// precedes it) and did not take the state it was saved with: it is at its
    /// defaults.
    PluginStateNotRestored {
        /// Why the state was not restored.
        reason: String,
    },
    /// The plugin did not take the render mode the host asked for
    /// ([`ControlRequest::SetRenderMode`]): it runs in the mode it had. Not
    /// sent for a plugin that has no render-mode capability, which the host
    /// already knows.
    RenderModeNotApplied {
        /// The mode that was asked for.
        mode: tutti_plugin_types::RenderMode,
    },
    /// A restart the host asked for (a rate or a render mode) did not happen:
    /// the plugin would not stop and runs as it was.
    PluginWouldNotStop {
        /// Why the plugin would not stop.
        reason: String,
    },
    /// What the server's own work lost, over its life, sent when any count
    /// has moved since it was last sent (at most once per control tick). The
    /// figures are cumulative: a later one replaces an earlier one.
    ServerLosses {
        /// Commands the server could not give its render thread.
        commands_refused: u64,
        /// One-way edits the plugin refused: an automation state, a track's
        /// info or a parameter write.
        edits_refused: u64,
        /// `[main-thread]` callbacks the plugin made from another thread, which
        /// the server refused.
        refused_off_thread_calls: tutti_units::Calls,
    },
    /// Plugin re-declared its controller table (VST3 `kMidiCCAssignmentChanged`).
    /// The host asks for the wiring again ([`ControlRequest::GetWiring`]).
    PluginMidiMapChanged,
}

/// One frame the server sends on the control connection: an answer, a sign of
/// life or news.
///
/// Variant order is wire-significant; see the module docs.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlFrame {
    /// The answer to request `id`.
    Reply {
        /// The `id` of the request.
        id: ReqId,
        /// The answer.
        reply: ControlReply,
    },
    /// Something the plugin did on its own.
    News(News),
    /// Request `id` has no answer yet and the server is at work on it: the
    /// host's wait for it restarts.
    Working {
        /// The `id` of the request.
        id: ReqId,
        /// What the server is doing.
        stage: Working,
    },
    /// Sent once the subprocess is live: the first frame the server sends on
    /// the broker connection, and the only one of its kind. Carries the wire
    /// [`PROTOCOL_VERSION`](super::PROTOCOL_VERSION) so a host and subprocess
    /// built from mismatched commits refuse to proceed rather than mis-parsing
    /// each other's messages (postcard is not self-describing). Every lane is
    /// opened over this connection, so the version is checked once.
    Hello {
        /// The subprocess's [`PROTOCOL_VERSION`](super::PROTOCOL_VERSION).
        protocol_version: u32,
    },
}

impl From<News> for ControlFrame {
    fn from(news: News) -> Self {
        Self::News(news)
    }
}
