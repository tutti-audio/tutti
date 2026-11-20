//! Plugin specific structures.

use crate::{UnknownPrimitive, primitive_enum::primitive_enum};

primitive_enum! {
    /// Plugin type. Generally either Effect or Synth.
    ///
    /// Other types are not necessary to build a plugin and are only useful for the host to categorize
    /// the plugin.
    #[derive(Clone, Copy, Debug)]
    pub enum Category: isize {
        /// Unknown / not implemented
        Unknown,
        /// Any effect
        Effect,
        /// VST instrument
        Synth,
        /// Scope, tuner, spectrum analyser, etc.
        Analysis,
        /// Dynamics, etc.
        Mastering,
        /// Panners, etc.
        Spacializer,
        /// Delays and Reverbs
        RoomFx,
        /// Dedicated surround processor.
        SurroundFx,
        /// Denoiser, etc.
        Restoration,
        /// Offline processing.
        OfflineProcess,
        /// Contains other plugins.
        Shell,
        /// Tone generator, etc.
        Generator,
    }
}

primitive_enum! {
    #[derive(Clone, Copy, Debug)]
    #[doc(hidden)]
    pub enum EffectOpCode: i32 {
        /// Called when plugin is initialized.
        Initialize,
        /// Called when plugin is being shut down.
        Shutdown,

        /// [value]: preset number to change to.
        ChangePreset,
        /// [return]: current preset number.
        GetCurrentPresetNum,
        /// [ptr]: char array with new preset name, limited to `MAX_PRESET_NAME_LEN`.
        SetCurrentPresetName,
        /// [ptr]: char buffer for current preset name, limited to `MAX_PRESET_NAME_LEN`.
        GetCurrentPresetName,

        /// [index]: parameter
        /// [ptr]: char buffer, limited to `MAX_PARAM_STR_LEN` (e.g. "db", "ms", etc)
        GetParameterLabel,
        /// [index]: parameter
        /// [ptr]: char buffer, limited to `MAX_PARAM_STR_LEN` (e.g. "0.5", "ROOM", etc).
        GetParameterDisplay,
        /// [index]: parameter
        /// [ptr]: char buffer, limited to `MAX_PARAM_STR_LEN` (e.g. "Release", "Gain")
        GetParameterName,

        /// Deprecated.
        _GetVu,

        /// [opt]: new sample rate.
        SetSampleRate,
        /// [value]: new maximum block size.
        SetBlockSize,
        /// [value]: 1 when plugin enabled, 0 when disabled.
        StateChanged,

        /// [ptr]: Rect** receiving pointer to editor size.
        EditorGetRect,
        /// [ptr]: system dependent window pointer, eg HWND on Windows.
        EditorOpen,
        /// Close editor. No arguments.
        EditorClose,

        /// Deprecated.
        _EditorDraw,
        /// Deprecated.
        _EditorMouse,
        /// Deprecated.
        _EditorKey,

        /// Idle call from host.
        EditorIdle,

        /// Deprecated.
        _EditorTop,
        /// Deprecated.
        _EditorSleep,
        /// Deprecated.
        _EditorIdentify,

        /// [ptr]: pointer for chunk data address (void**).
        /// [index]: 0 for bank, 1 for program
        GetData,
        /// [ptr]: data (void*)
        /// [value]: data size in bytes
        /// [index]: 0 for bank, 1 for program
        SetData,

        /// [ptr]: VstEvents*
        ProcessEvents,
        /// [index]: param index
        /// [return]: 1=true, 0=false
        CanBeAutomated,
        ///  [index]: param index
        ///  [ptr]: parameter string
        ///  [return]: true for success
        StringToParameter,

        /// Deprecated.
        _GetNumCategories,

        /// [index]: program name
        /// [ptr]: char buffer for name, limited to `MAX_PRESET_NAME_LEN`
        /// [return]: true for success
        GetPresetName,

        /// Deprecated.
        _CopyPreset,
        /// Deprecated.
        _ConnectIn,
        /// Deprecated.
        _ConnectOut,

        /// [index]: input index
        /// [ptr]: `VstPinProperties`
        /// [return]: 1 if supported
        GetInputInfo,
        /// [index]: output index
        /// [ptr]: `VstPinProperties`
        /// [return]: 1 if supported
        GetOutputInfo,
        /// [return]: `PluginCategory` category.
        GetCategory,

        /// Deprecated.
        _GetCurrentPosition,
        /// Deprecated.
        _GetDestinationBuffer,

        /// [ptr]: `VstAudioFile` array
        /// [value]: count
        /// [index]: start flag
        OfflineNotify,
        /// [ptr]: `VstOfflineTask` array
        /// [value]: count
        OfflinePrepare,
        /// [ptr]: `VstOfflineTask` array
        /// [value]: count
        OfflineRun,

        /// [ptr]: `VstVariableIo`
        /// [use]: used for variable I/O processing (offline e.g. timestretching)
        ProcessVarIo,
        /// [value]: input `*mut VstSpeakerArrangement`.
        /// [ptr]: output `*mut VstSpeakerArrangement`.
        /// [return]: 1 when the plugin takes both.
        SetSpeakerArrangement,

        /// Deprecated.
        _SetBlocksizeAndSampleRate,

        /// Soft bypass (automatable).
        /// [value]: 1 = bypass, 0 = nobypass.
        SoftBypass,
        // [ptr]: buffer for effect name, limited to `kVstMaxEffectNameLen`
        GetEffectName,

        /// Deprecated.
        _GetErrorText,

        /// [ptr]: buffer for vendor name, limited to `MAX_VENDOR_STR_LEN`.
        GetVendorName,
        /// [ptr]: buffer for product name, limited to `MAX_PRODUCT_STR_LEN`.
        GetProductName,
        /// [return]: vendor specific version.
        GetVendorVersion,
        /// no definition, vendor specific.
        VendorSpecific,
        /// [ptr]: "Can do" string.
        /// [return]: 1 = yes, 0 = maybe, -1 = no.
        CanDo,
        /// [return]: tail size (e.g. reverb time). 0 is default, 1 means no tail.
        GetTailSize,

        /// Deprecated.
        _Idle,
        /// Deprecated.
        _GetIcon,
        /// Deprecated.
        _SetVewPosition,

        /// [index]: param index
        /// [ptr]: `*mut VstParamInfo`
        /// [return]: 1 if supported
        GetParamInfo,

        /// Deprecated.
        _KeysRequired,

        /// [return]: 2400 for vst 2.4.
        GetApiVersion,

        /// [index]: ASCII char.
        /// [value]: `Key` keycode.
        /// [opt]: `flags::modifier_key` bitmask.
        /// [return]: 1 if used.
        EditorKeyDown,
        /// [index]: ASCII char.
        /// [value]: `Key` keycode.
        /// [opt]: `flags::modifier_key` bitmask.
        /// [return]: 1 if used.
        EditorKeyUp,
        /// [value]: 0 = circular, 1 = circular relative, 2 = linear.
        EditorSetKnobMode,

        /// [index]: MIDI channel.
        /// [ptr]: `*mut MidiProgramName`.
        /// [return]: number of used programs, 0 = unsupported.
        GetMidiProgramName,
        /// [index]: MIDI channel.
        /// [ptr]: `*mut MidiProgramName`.
        /// [return]: index of current program.
        GetCurrentMidiProgram,
        /// [index]: MIDI channel.
        /// [ptr]: `*mut MidiProgramCategory`.
        /// [return]: number of used categories.
        GetMidiProgramCategory,
        /// [index]: MIDI channel.
        /// [return]: 1 if `MidiProgramName` or `MidiKeyName` has changed.
        HasMidiProgramsChanged,
        /// [index]: MIDI channel.
        /// [ptr]: `*mut MidiKeyName`.
        /// [return]: 1 = supported 0 = not.
        GetMidiKeyName,

        /// Called before a preset is loaded.
        BeginSetPreset,
        /// Called after a preset is loaded.
        EndSetPreset,

        /// [value]: input `*mut *mut VstSpeakerArrangement`, which the plugin points at its own
        /// input arrangement.
        /// [ptr]: output `*mut *mut VstSpeakerArrangement`, likewise.
        /// [return]: 1 when both are written.
        ///
        /// `aeffectx.h` words this opcode's comment as a copy of `effSetSpeakerArrangement`'s;
        /// the pointer-to-pointer form is the SDK's own practice: its AudioEffectX class
        /// takes two `VstSpeakerArrangement**` in getSpeakerArrangement, and its dispatcher
        /// forwards `value` and `ptr` to them unchanged.
        GetSpeakerArrangement,
        /// [ptr]: buffer for plugin name, limited to `MAX_PRODUCT_STR_LEN`.
        /// [return]: next plugin's uniqueID.
        ShellGetNextPlugin,

        /// No args. Called once before start of process call. This indicates that the process call
        /// will be interrupted (e.g. Host reconfiguration or bypass when plugin doesn't support
        /// SoftBypass)
        StartProcess,
        /// No arguments. Called after stop of process call.
        StopProcess,
        /// [value]: number of samples to process. Called in offline mode before process.
        SetTotalSampleToProcess,
        /// [value]: pan law `PanLaw`.
        /// [opt]: gain.
        SetPanLaw,

        /// [ptr]: `*mut VstPatchChunkInfo`.
        /// [return]: -1 = bank cant be loaded, 1 = can be loaded, 0 = unsupported.
        BeginLoadBank,
        /// [ptr]: `*mut VstPatchChunkInfo`.
        /// [return]: -1 = bank cant be loaded, 1 = can be loaded, 0 = unsupported.
        BeginLoadPreset,

        /// [value]: 0 if 32 bit, anything else if 64 bit.
        SetPrecision,

        /// [return]: number of used MIDI Inputs (1-15).
        GetNumMidiInputs,
        /// [return]: number of used MIDI Outputs (1-15).
        GetNumMidiOutputs,
    }
}

/// What a plugin says about itself in its `AEffect` and `effGet*String` answers, read when
/// the instance is made, before `effOpen`.
///
/// The pin, parameter and program counts and the latency are not here: a plugin may change
/// them after `effOpen`, so the instance reads them from the `AEffect` when asked.
#[derive(Clone, Debug)]
pub struct Info {
    /// The plugin's name (`effGetEffectName`, else `effGetProductString`).
    pub name: String,

    /// The plugin's vendor (`effGetVendorString`).
    pub vendor: String,

    /// Unique plugin ID, which identifies the plugin in a saved preset or project.
    pub unique_id: i32,

    /// The plugin's version (`AEffect::version`).
    pub version: i32,

    /// The plugin's `effGetPlugCategory` answer: the [`Category`] it names, or the code it
    /// answered that VST 2.4 gives no meaning (`kPlugCategUnknown` is a named [`Category`]).
    pub category: Result<Category, UnknownPrimitive>,

    /// Indicates that preset data is handled in formatless chunks (`effFlagsProgramChunks`).
    pub preset_chunks: bool,

    /// Indicates whether this plugin can process `f64` buffers (`effFlagsCanDoubleReplacing`).
    pub f64_precision: bool,
}

/// Features which are optionally supported by a plugin. These are queried by the host at run time.
#[derive(Debug)]
pub enum CanDo {
    /// `sendVstEvents`.
    SendEvents,
    /// `sendVstMidiEvent`.
    SendMidiEvent,
    /// `receiveVstEvents`.
    ReceiveEvents,
    /// `receiveVstMidiEvent`.
    ReceiveMidiEvent,
    /// `receiveVstTimeInfo`.
    ReceiveTimeInfo,
    /// `offline`.
    Offline,
    /// `midiProgramNames`.
    MidiProgramNames,
    /// `bypass`.
    Bypass,
    /// `receiveVstSysexEvent`.
    ReceiveSysExEvent,
    /// `midiSingleNoteTuningChange`.
    MidiSingleNoteTuningChange,
    /// `midiKeyBasedInstrumentControl`.
    MidiKeyBasedInstrumentControl,
    /// Any other query string, verbatim.
    Other(String),
}

impl CanDo {
    #![allow(clippy::should_implement_trait)]
    /// Converts a string to a `CanDo` instance. Any given string that does not match the predefined
    /// values will return a `CanDo::Other` value.
    pub fn from_str(s: &str) -> CanDo {
        use self::CanDo::*;

        match s {
            "sendVstEvents" => SendEvents,
            "sendVstMidiEvent" => SendMidiEvent,
            "receiveVstEvents" => ReceiveEvents,
            "receiveVstMidiEvent" => ReceiveMidiEvent,
            "receiveVstTimeInfo" => ReceiveTimeInfo,
            "offline" => Offline,
            "midiProgramNames" => MidiProgramNames,
            "bypass" => Bypass,

            "receiveVstSysexEvent" => ReceiveSysExEvent,
            "midiSingleNoteTuningChange" => MidiSingleNoteTuningChange,
            "midiKeyBasedInstrumentControl" => MidiKeyBasedInstrumentControl,
            otherwise => Other(otherwise.to_string()),
        }
    }
}

impl From<CanDo> for String {
    fn from(val: CanDo) -> Self {
        use self::CanDo::*;

        match val {
            SendEvents => "sendVstEvents".to_string(),
            SendMidiEvent => "sendVstMidiEvent".to_string(),
            ReceiveEvents => "receiveVstEvents".to_string(),
            ReceiveMidiEvent => "receiveVstMidiEvent".to_string(),
            ReceiveTimeInfo => "receiveVstTimeInfo".to_string(),
            Offline => "offline".to_string(),
            MidiProgramNames => "midiProgramNames".to_string(),
            Bypass => "bypass".to_string(),

            ReceiveSysExEvent => "receiveVstSysexEvent".to_string(),
            MidiSingleNoteTuningChange => "midiSingleNoteTuningChange".to_string(),
            MidiKeyBasedInstrumentControl => "midiKeyBasedInstrumentControl".to_string(),
            Other(other) => other,
        }
    }
}
