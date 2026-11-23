//! What the plugin does, written out. The figures a case expects are here and
//! nowhere else, and they are not imported from the plugin: a case that asked
//! the plugin what to expect could not fail when the plugin is wrong. The
//! plugin's own copy is the spec in `tutti-shared-plugin`'s `core` module.

use tutti_plugin_types::{
    AutomationMode, Axis, ChannelLayout, ControlRole, MidiAssignment, MidiSource, ParamSteps,
    Samples, Widths,
};
use tutti_ump::{MidiChannel, U7};
use tutti_units::Normalized;
use tutti_units::ParamId;

/// A parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Param {
    /// Persistent, default 0.5; the level the Level mode renders.
    Level,
    /// Persistent, default 0.25.
    Aux,
    /// The mode, read at activation.
    Mode,
    /// The one parameter with a plain domain of its own: a decreasing
    /// exponential, `20000 * 1000^(-n)` for the normalized `n`. It changes
    /// nothing a block renders.
    Tone,
    /// The read-only parameter: a meter the host never writes, always 0.
    /// Every format that has the flag declares it read-only; VST2 has none.
    Meter,
}

impl Param {
    /// Every parameter the plugin lists, with its default.
    pub const ALL: [(Param, f64); 5] = [
        (Param::Level, 0.5),
        (Param::Aux, 0.25),
        (Param::Mode, 0.0),
        (Param::Tone, 0.5),
        (Param::Meter, 0.0),
    ];

    /// The id every format lists it under.
    #[must_use]
    pub const fn id(self) -> ParamId {
        ParamId::new(self as u32)
    }
}

impl Param {
    /// The name every format lists it under.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Param::Level => "level",
            Param::Aux => "aux",
            Param::Mode => "mode",
            Param::Tone => "tone",
            Param::Meter => "meter",
        }
    }
}

/// The parameter that holds the plugin's own bypass where the format lists one
/// (CLAP and VST3): `0` processing, `1` bypassed. It is listed after the five
/// of [`Param::ALL`], flagged as the format's bypass, and a write to it reaches
/// the plugin as this parameter's point in the next block.
pub const BYPASS_ID: ParamId = ParamId::new(5);

/// The parameter VST3 lists after [`BYPASS_ID`] to select a program: flagged
/// `kIsProgramChange`, in the unit that owns [`PROGRAMS`], `index / 2` normalized.
/// Only VST3 lists it.
pub const PROGRAM_ID: ParamId = ParamId::new(6);

/// The programs the plugin offers, in order: a list `Factory` (id 1) of `Init`,
/// `Pad` and `Lead`. VST3 declares it as a program list, VST2 as programs and AU as
/// factory presets, numbered from 0; CLAP declares none.
#[derive(Debug)]
pub struct Programs {
    /// The list's id.
    pub list_id: i32,
    /// The list's name.
    pub list: &'static str,
    /// The programs' names, in order.
    pub names: [&'static str; 3],
}

/// The program list of the plugin.
pub const PROGRAMS: Programs = Programs {
    list_id: 1,
    list: "Factory",
    names: ["Init", "Pad", "Lead"],
};

/// How a format names the presets it lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresetIds {
    /// Programs of a named list: `Program { list_id, index }`, the list's name
    /// as the bank (VST3).
    Programs,
    /// One flat set, `Number(index)`, no bank (CLAP, VST2, AU).
    Numbered,
}

/// Where a format keeps the plugin's own bypass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bypass {
    /// A listed parameter flagged as the bypass: VST3 `kIsBypass`, CLAP
    /// `CLAP_PARAM_IS_BYPASS`.
    Parameter,
    /// No parameter: AU `kAudioUnitProperty_BypassEffect`, which the host reads
    /// back. The plugin tells the host's write as [`BYPASS_ID`]'s point.
    Property,
    /// None claimed. VST2's `effSetBypass` writes a bypass but nothing reads it
    /// back, so the host cannot confirm a write and claims no bypass of its own.
    None,
}

/// The controller the plugin maps to a parameter, in the formats that carry a
/// table (VST3 `IMidiMapping`, AU's mapping table): Control Change
/// 74 on channel 0 drives `level`.
pub const MAPPED_CONTROLLER: u8 = 74;

/// The normalized value the write cases send: `level` takes it where a write is
/// allowed, and the meter is refused it.
pub const WRITE: f64 = 0.75;

/// The normalized values the conversion cases ask about.
pub const TONE_AT: [f64; 4] = [0.0, 0.25, 0.5, 1.0];

/// What a format's own conversion answers for [`Param::Tone`]. Each format's
/// figures follow from its spec, not from the plugin: VST3 asks the
/// controller's `normalizedParamToPlain` (`ivsteditcontroller.h:508`), whose map
/// is the plugin's, here `20000 * 1000^(-n)`; CLAP has no normalized value
/// (`ext/params.h` speaks plain `double`s), so its plain value is the host's
/// position over the range the plugin declares, 20 to 20000; VST2 declares no
/// integer range, so it has no plain value at all.
#[derive(Clone, Copy, Debug)]
pub struct Tone {
    /// What the parameter list reports as the default: the plain value where
    /// the format has one, the normalized value where it does not.
    pub default: f64,
    /// The declared bounds, `None` where the format declares none.
    pub bounds: Option<(f64, f64)>,
    /// The plain value at each of [`TONE_AT`], `None` where the format has no
    /// plain value for the parameter.
    pub plain: Option<[f64; 4]>,
}

/// VST3: the plugin's own curve, exact at its default, 20000 / sqrt(1000).
pub const TONE_CURVED: Tone = Tone {
    default: 632.455_532_033_675_8,
    bounds: Some((20.0, 20_000.0)),
    plain: Some([
        20_000.0,
        3_556.558_820_077_845_7,
        632.455_532_033_675_8,
        20.0,
    ]),
};

/// CLAP: the declared range 20 to 20000 as the host's line, the plugin's own
/// plain default.
pub const TONE_LINEAR: Tone = Tone {
    default: 632.455_532_033_675_8,
    bounds: Some((20.0, 20_000.0)),
    plain: Some([20.0, 5_015.0, 10_010.0, 20_000.0]),
};

/// VST2: normalized only.
pub const TONE_NORMALIZED: Tone = Tone {
    default: 0.5,
    bounds: None,
    plain: None,
};

/// Blocks the plugin's transcript keeps; one more overflows it.
pub const LOG_BLOCKS: usize = 16;

/// The automation the segment case sends: a segment from `(offset, value)` to
/// `(offset, value)` on the level, then a step at the second offset to
/// [`SEGMENT_STEP`]. Linear breakpoints: the level moves linearly between the
/// ends, and the step is two points at one offset.
pub const SEGMENT: [(i32, f64); 2] = [(8, 0.25), (40, 0.75)];

/// The value the step at the end of [`SEGMENT`] lands on.
pub const SEGMENT_STEP: f64 = 0.125;

/// An id the plugin does not list.
pub const UNLISTED: ParamId = ParamId::new(77);

/// What a block renders. `tag` is `bus * 1000 + channel + 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `in + tag`.
    Tag,
    /// `in` delayed by [`LATENCY`], plus `tag`; reports [`LATENCY`].
    Delay,
    /// The level, as it stands at each sample.
    Level,
    /// `key + 1000 * channel` while a note is held, else 0.
    Notes,
    /// Writes nothing.
    Silent,
}

impl Mode {
    /// The number a state blob stores.
    #[must_use]
    pub const fn index(self) -> u32 {
        self as u32
    }

    /// The normalized value of the mode parameter that selects it.
    #[must_use]
    pub fn value(self) -> Normalized {
        Normalized::clamped(f64::from(self.index()) / 4.0)
    }
}

/// The plugin's one bus a side: a stereo main, in every format.
pub const MAIN_BUS: ChannelLayout = ChannelLayout::STEREO;

/// The audio widths the plugin processes in CLAP, VST3 and VST2: 32-bit and
/// 64-bit (an AUv2 unit renders 32-bit only). CLAP's ports declare 64-bit
/// support, VST3 answers `canProcessSampleSize` for `kSample64`, VST2 sets
/// `effFlagsCanDoubleReplacing`.
pub const WIDTHS: Widths = Widths::BOTH;

/// The Delay mode's latency.
pub const LATENCY: Samples = Samples::new(137);
/// The tail, in every mode.
pub const TAIL: Samples = Samples::new(8191);

/// The tag on channel `channel` of the main bus.
#[must_use]
pub fn tag(channel: usize) -> f32 {
    (channel + 1) as f32
}

/// The editor behaviour that offers a view (`EDITOR_TIDY` in the plugin's
/// state word, second byte).
pub const EDITOR_TIDY: u32 = 1;
/// The notice behaviour that raises one notice per state load (`NOTICE_ON_LOAD`,
/// third byte).
pub const NOTICE_ON_LOAD: u32 = 1;
/// The notice behaviour that reports one gesture on `aux` per state load
/// (`NOTICE_GESTURE_ON_LOAD`, third byte).
pub const NOTICE_GESTURE_ON_LOAD: u32 = 2;
/// The tail behaviour that reports the formats' infinite marker (`TAIL_INFINITE`,
/// fourth byte).
pub const TAIL_INFINITE: u32 = 1;
/// The tail behaviour that reports a finite tail past what the graph bounds
/// (`TAIL_PAST_CAP`, fourth byte).
pub const TAIL_PAST_CAP: u32 = 2;
/// What `TAIL_PAST_CAP` reports: 61 s at 48 kHz.
pub const TAIL_PAST_CAP_FRAMES: Samples = Samples::new(48_000 * 61);

/// The ring behaviour that rings [`RING`] after an impulse (`RINGS_EXACT`, the
/// high nibble of the fourth byte of the mode word).
pub const RINGS_EXACT: u32 = 1;
/// The ring behaviour that rings [`RING`] and [`RING_EXTRA`] more (`RINGS_LONGER`).
pub const RINGS_LONGER: u32 = 2;
/// The ring behaviour that rings [`RING`] over noise (`RINGS_HISS`).
pub const RINGS_HISS: u32 = 3;
/// The ring behaviour that rings [`RING`] starting [`LATE`] frames after its
/// impulse (`RINGS_LATE`).
pub const RINGS_LATE: u32 = 4;
/// The fault that aborts the process in the first `process` (`FAULT_CRASH`, the
/// high nibble of the mode word's low byte).
pub const FAULT_CRASH: u32 = 1;
/// The fault that never returns from the first `process` (`FAULT_HANG`).
pub const FAULT_HANG: u32 = 2;
/// The behaviour that is no fault: the plugin records on which thread it closed
/// (`FAULT_RECORD_CLOSE`).
pub const FAULT_RECORD_CLOSE: u32 = 3;
/// How long a ring lasts after an impulse at frame 0: its last frame is this one.
/// It equals [`TAIL`] on purpose, so the plugin is honest unless a case says not.
pub const RING: Samples = Samples::new(8191);
/// How much longer than [`RING`] `RINGS_LONGER` rings.
pub const RING_EXTRA: Samples = Samples::new(4096);
/// Frames `RINGS_LATE` waits before it sounds; the plugin reports no latency
/// in its default mode.
pub const LATE: Samples = Samples::new(64);
/// The peak of `RINGS_HISS`'s noise: 0.001, which is -60 dB.
pub const HISS_DB: f64 = -60.0;

/// One edit a host must read from a plugin, as the spec states it: the figures
/// are written here, not taken from the host's own type.
#[derive(Clone, Copy, Debug)]
pub enum Edit {
    /// A gesture's begin on a parameter. No value: no format carries one.
    Begin(Param),
    /// A change of a parameter to a normalized value.
    Change(Param, f64),
    /// A gesture's end on a parameter. No value.
    End(Param),
}

impl Edit {
    /// The host's edit this stands for.
    #[must_use]
    pub fn expected(self) -> tutti_plugin_types::ParamEdit {
        use tutti_plugin_types::ParamEdit;
        use tutti_units::Normalized;
        match self {
            Self::Begin(p) => ParamEdit::Begin(p.id()),
            Self::Change(p, v) => ParamEdit::change(p.id(), Normalized::of(v)),
            Self::End(p) => ParamEdit::End(p.id()),
        }
    }
}

/// The edits the gesture notice reports, in the order the plugin makes them: the
/// gesture's begin, a change to 0.375, a change to 0.625, the gesture's end,
/// all of `aux`. Every one is reported: none is dropped, and the begin and the
/// end hold no value.
pub const GESTURE: [Edit; 4] = [
    Edit::Begin(Param::Aux),
    Edit::Change(Param::Aux, 0.375),
    Edit::Change(Param::Aux, 0.625),
    Edit::End(Param::Aux),
];

/// What a notice's edit of `aux` to 0.75 is as `take_reported_edits` reads it
/// through VST3's component handler: the gesture's begin, the change, the end.
const AUX_EDITED: [Edit; 3] = [
    Edit::Begin(Param::Aux),
    Edit::Change(Param::Aux, 0.75),
    Edit::End(Param::Aux),
];

const AUTOMATION_CLAP: [(AutomationMode, u32); 4] = [
    (AutomationMode::Off, 0),
    (AutomationMode::Reading, 2),
    (AutomationMode::Writing, 3),
    (AutomationMode::ReadWriting, 3),
];

const AUTOMATION_VST2: [(AutomationMode, u32); 4] = [
    (AutomationMode::Off, 1),
    (AutomationMode::Reading, 2),
    (AutomationMode::Writing, 3),
    (AutomationMode::ReadWriting, 4),
];

const AUTOMATION_VST3: [(AutomationMode, u32); 4] = [
    (AutomationMode::Off, 0),
    (AutomationMode::Reading, 1),
    (AutomationMode::Writing, 2),
    (AutomationMode::ReadWriting, 3),
];

/// What a format's control surface answers for the shared plugin, the same in
/// a loaded plugin and an active one. One row per format ([`Surface`]).
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    /// Whether the plugin's shell reads the state's editor byte: only the Rust
    /// VST3 shell does, offering a view once it says [`EDITOR_TIDY`]. Where
    /// `false` the case does not send the byte (the C++ plugin refuses it).
    pub selects_editor: bool,
    /// What `has_editor` answers once the case has sent what the shell reads.
    /// The Rust VST3 shell's view embeds only an X11 window, so a host on
    /// Windows or macOS is right to see none.
    pub has_editor: bool,
    /// What `render_mode_support` answers: `Live` where the format takes the
    /// mode while active (VST2: the host's own flag), `OnActivation` where it
    /// is read at activation (VST3), `Unsupported` where the plugin has no
    /// render mode (the shared CLAP shell exports no `clap.render`). The AU shell
    /// keeps `kAudioUnitProperty_OfflineRender`, which the host writes at
    /// activation (`OnActivation`).
    pub render_mode: tutti_plugin_types::RenderModeSupport,
    /// Whether the shell reads the notice byte ([`NOTICE_ON_LOAD`]): CLAP, VST2
    /// and VST3, which tell the host something on each state load.
    pub notices: bool,
    /// The request a notice raises, as `take_requests` reports it: CLAP's
    /// `request_callback`. Nothing for every other format.
    pub request: Option<tutti_plugin_types::Request>,
    /// The edits a notice reports, as `take_reported_edits` reads them: VST2's
    /// `audioMasterAutomate(0, 0.5)` (one change: VST2 reports no gesture here),
    /// VST3's `beginEdit`, `performEdit` of `aux` to 0.75 and `endEdit` through
    /// the host's component handler. Nothing for CLAP, which reports
    /// edits in a block's output and so not from a state load alone, and for
    /// AU.
    pub edits: &'static [Edit],
    /// The raw automation state the host's mode reaches the plugin as, in the
    /// format's own number, for each mode: VST3's `IAutomationState` bitmask
    /// (read 1, write 2), CLAP's `param-indication` states (playing 2, recording
    /// 3: writing, alone or with reading, is recording), VST2's
    /// `audioMasterGetAutomationState` answers (off 1, read 2, write 3, read and
    /// write 4). Empty where the format has no such call: AU, not applicable by its spec.
    pub automation: &'static [(tutti_plugin_types::AutomationMode, u32)],
    /// What the format's conversion answers for [`Param::Tone`].
    pub tone: Tone,
    /// Whether the shell reads the state's tail byte (fourth byte of the mode
    /// word): every shell of ours.
    pub selects_tail: bool,
    /// What the format reports when the plugin states its infinite marker
    /// (`TAIL_INFINITE`): `Unbounded` where the format has one (CLAP's
    /// `u32::MAX`, VST3's `kInfiniteTail`), nothing where the plugin cannot say
    /// it (VST2's tail size is a count).
    pub infinite_tail: Option<tutti_plugin_types::PluginTail>,
    /// Where the format keeps the plugin's own bypass.
    pub bypass: Bypass,
    /// What the format says `aux`'s group is: the plugin puts it in the
    /// `Filter` unit, at the end of the path `Voices/Oscillator/Filter`, where
    /// the format has groups. VST3 and CLAP name that whole path,
    /// AU the clump, VST2 has none (and the plugin declares none). The other
    /// parameters have no group.
    pub aux_group: &'static str,
    /// The roles the plugin declares, by parameter: VST3's function names, per
    /// unit. No other format declares one.
    pub roles: &'static [(Param, ControlRole)],
    /// The parameters the plugin puts in its overview, the shortlist of its
    /// headline controls: AU's `ParametersForOverview`. No other format has one.
    pub overview: &'static [Param],
    /// The step information of `mode`, the plugin's five-position selector.
    pub mode_steps: ParamSteps,
    /// The step information of `level`, `aux` and `tone`, which are continuous
    /// where the format says so at all (VST2 does not).
    pub continuous_steps: ParamSteps,
    /// The controller table the plugin declares. Empty where the format has no
    /// such table: CLAP and VST2.
    pub assignments: &'static [MidiAssignment],
    /// The names the plugin gives to MIDI keys, `(key, channel, name)`: the
    /// drum pads CLAP, the Rust VST3 plugin and VST2 name for every channel,
    /// the C++ plugin's keyswitches on channel 0. AU declares none.
    pub note_names: &'static [(u8, Option<u8>, &'static str)],
    /// The groups the plugin declares, `(name, parent)`, a parent first: VST3's
    /// unit tree, CLAP's module path and its prefixes, AU's one flat clump.
    /// VST2 has no tree.
    pub groups: &'static [(&'static str, Option<&'static str>)],
    /// Whether the format reports a parameter's short name ([`short_name`]):
    /// VST3's `shortTitle` of the Rust plugin, VST2's `shortLabel` and AU's
    /// `kAudioUnitProperty_ParameterIDName`.
    pub short_names: bool,
    /// Whether the format reports a parameter's read-only bit: every format but
    /// VST2, whose `effGetParameterProperties` has none. Where `true`, the
    /// parameter list reports [`Param::Meter`] read-only and every other
    /// parameter not. Where `false` the format has no read-only parameter, and
    /// the cases about one do not apply.
    pub reports_access: bool,
    /// Whether the format reports a parameter's automatable bit: CLAP and VST3.
    /// An AUv2 unit advertises only `kAudioUnitParameterFlag_IsWritable`, and VST2 says nothing per
    /// parameter. Where `true`, the meter is not automatable and every other
    /// parameter is.
    pub reports_automatable: bool,
    /// How the format names the presets it lists ([`PROGRAMS`]).
    pub preset_ids: PresetIds,
    /// The plain map the format declares for `level`, `aux`, `tone` and `meter`.
    pub plain_map: MapKind,
    /// The plain map the format declares for `mode`, the five-position selector.
    pub mode_plain_map: MapKind,
}

/// Which plain map a format declares for a parameter
/// (`ParameterInfo::plain_map`): CLAP and AU speak plain values with no
/// normalized one, so the host's line over the declared range, stepped on its
/// integer for CLAP's stepped `mode`; VST3 and a VST2 with no integer range
/// leave the map to the plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapKind {
    /// The line between the declared ends.
    Linear,
    /// The line, each value on its integer.
    Stepped,
    /// The plugin's own map.
    PluginDefined,
}

/// The role every VST3 plugin of ours declares, by parameter (function names
/// asked per unit: `aux` is in the `Filter` unit, the others in the root).
pub const VST3_ROLES: [(Param, ControlRole); 4] = [
    (Param::Level, ControlRole::Mix),
    (Param::Aux, ControlRole::Pan(Axis::X)),
    (Param::Mode, ControlRole::LowLatency),
    (Param::Tone, ControlRole::Pan(Axis::Z)),
];

/// The short name of a parameter the plugin declares, where the format has one.
#[must_use]
pub const fn short_name(param: Param) -> &'static str {
    match param {
        Param::Level => "lvl",
        Param::Aux => "aux",
        Param::Mode => "mod",
        Param::Tone => "tne",
        Param::Meter => "mtr",
    }
}

/// The two pads the plugin names, on every channel.
const DRUM_KEYS: [(u8, Option<u8>, &str); 2] = [(36, None, "Kick"), (38, None, "Snare")];

/// The C++ plugin's keyswitches: two keys each, on channel 0 alone.
const CPP_KEYS: [(u8, Option<u8>, &str); 4] = [
    (0, Some(0), "Legato"),
    (1, Some(0), "Legato"),
    (2, Some(0), "Staccato"),
    (3, Some(0), "Staccato"),
];

/// VST3's units, "Voices" > "Oscillator" > "Filter", by path.
const UNIT_GROUPS: [(&str, Option<&str>); 3] = [
    ("Voices", None),
    ("Voices/Oscillator", Some("Voices")),
    ("Voices/Oscillator/Filter", Some("Voices/Oscillator")),
];

/// CLAP's module `Voices/Oscillator/Filter` and its prefixes.
const MODULE_GROUPS: [(&str, Option<&str>); 3] = [
    ("Voices", None),
    ("Voices/Oscillator", Some("Voices")),
    ("Voices/Oscillator/Filter", Some("Voices/Oscillator")),
];

/// CC 74 on channel 0 drives `level`, on the channel the table says.
const fn assignment(channel: Option<MidiChannel>) -> MidiAssignment {
    MidiAssignment {
        channel,
        source: MidiSource::ControlChange(U7::of(MAPPED_CONTROLLER)),
        param: Param::Level.id(),
    }
}

/// CLAP: no editor in the plugin, no `clap.render` (so no render mode), a
/// callback request.
pub const CLAP: Surface = Surface {
    automation: &AUTOMATION_CLAP,
    selects_editor: false,
    has_editor: false,
    render_mode: tutti_plugin_types::RenderModeSupport::Unsupported,
    notices: true,
    request: Some(tutti_plugin_types::Request::Callback),
    edits: &[],
    tone: TONE_LINEAR,
    selects_tail: true,
    infinite_tail: Some(tutti_plugin_types::PluginTail::Unbounded),
    bypass: Bypass::Parameter,
    aux_group: "Voices/Oscillator/Filter",
    roles: &[],
    overview: &[],
    mode_steps: ParamSteps::Enumerated(5),
    continuous_steps: ParamSteps::Continuous,
    assignments: &[],
    note_names: &DRUM_KEYS,
    groups: &MODULE_GROUPS,
    short_names: false,
    reports_access: true,
    reports_automatable: true,
    preset_ids: PresetIds::Numbered,
    plain_map: MapKind::Linear,
    mode_plain_map: MapKind::Stepped,
};

/// VST2: no editor in the plugin, a live render mode, an automate report.
pub const VST2: Surface = Surface {
    automation: &AUTOMATION_VST2,
    selects_editor: false,
    has_editor: false,
    render_mode: tutti_plugin_types::RenderModeSupport::Live,
    notices: true,
    request: None,
    edits: &[Edit::Change(Param::Level, 0.5)],
    tone: TONE_NORMALIZED,
    selects_tail: true,
    infinite_tail: None,
    bypass: Bypass::None,
    aux_group: "",
    roles: &[],
    overview: &[],
    mode_steps: ParamSteps::Unknown,
    continuous_steps: ParamSteps::Unknown,
    assignments: &[],
    note_names: &DRUM_KEYS,
    groups: &[],
    short_names: true,
    reports_access: false,
    reports_automatable: false,
    preset_ids: PresetIds::Numbered,
    plain_map: MapKind::PluginDefined,
    mode_plain_map: MapKind::PluginDefined,
};

/// VST3 from the Rust plugin: an editor once selected, the mode read at activation, an edit of `aux` to 0.75 on each state
/// load.
pub const VST3: Surface = Surface {
    automation: &AUTOMATION_VST3,
    selects_editor: true,
    has_editor: true,
    render_mode: tutti_plugin_types::RenderModeSupport::OnActivation,
    notices: true,
    request: None,
    edits: &AUX_EDITED,
    tone: TONE_CURVED,
    selects_tail: true,
    infinite_tail: Some(tutti_plugin_types::PluginTail::Unbounded),
    bypass: Bypass::Parameter,
    aux_group: "Voices/Oscillator/Filter",
    roles: &VST3_ROLES,
    overview: &[],
    mode_steps: ParamSteps::Enumerated(5),
    continuous_steps: ParamSteps::Continuous,
    assignments: &[assignment(Some(MidiChannel::of(0)))],
    note_names: &DRUM_KEYS,
    groups: &UNIT_GROUPS,
    short_names: true,
    reports_access: true,
    reports_automatable: true,
    preset_ids: PresetIds::Programs,
    plain_map: MapKind::PluginDefined,
    mode_plain_map: MapKind::PluginDefined,
};

/// VST3 from the C++ plugin: no editor, an edit of `aux` to 0.75 on each state
/// load.
pub const VST3_CPP: Surface = Surface {
    automation: &AUTOMATION_VST3,
    selects_editor: false,
    has_editor: false,
    render_mode: tutti_plugin_types::RenderModeSupport::OnActivation,
    notices: true,
    request: None,
    edits: &AUX_EDITED,
    tone: TONE_CURVED,
    selects_tail: true,
    infinite_tail: Some(tutti_plugin_types::PluginTail::Unbounded),
    bypass: Bypass::Parameter,
    aux_group: "Voices/Oscillator/Filter",
    roles: &VST3_ROLES,
    overview: &[],
    mode_steps: ParamSteps::Enumerated(5),
    continuous_steps: ParamSteps::Continuous,
    assignments: &[assignment(Some(MidiChannel::of(0)))],
    note_names: &CPP_KEYS,
    groups: &UNIT_GROUPS,
    short_names: false,
    reports_access: true,
    reports_automatable: true,
    preset_ids: PresetIds::Programs,
    plain_map: MapKind::PluginDefined,
    mode_plain_map: MapKind::PluginDefined,
};

/// AU, from the shared plugin's Audio Unit shell: no editor (the shell offers no
/// Cocoa view), the render mode on activation (the shell keeps
/// `kAudioUnitProperty_OfflineRender`), no notice (the shell raises none), the tail
/// byte read, and the host's own linear map over the declared range for `tone`
/// (an AU has no normalized value). The infinite marker is `+inf` seconds.
pub const AU: Surface = Surface {
    automation: &[],
    selects_editor: false,
    has_editor: false,
    render_mode: tutti_plugin_types::RenderModeSupport::OnActivation,
    notices: false,
    request: None,
    edits: &[],
    tone: TONE_LINEAR,
    selects_tail: true,
    infinite_tail: Some(tutti_plugin_types::PluginTail::Unbounded),
    bypass: Bypass::Property,
    aux_group: "Filter",
    roles: &[],
    overview: &[Param::Level, Param::Tone],
    mode_steps: ParamSteps::Enumerated(5),
    continuous_steps: ParamSteps::Continuous,
    assignments: &[assignment(Some(MidiChannel::of(0)))],
    note_names: &[],
    groups: &[("Filter", None)],
    short_names: true,
    reports_access: true,
    reports_automatable: false,
    preset_ids: PresetIds::Numbered,
    plain_map: MapKind::Linear,
    mode_plain_map: MapKind::Linear,
};
