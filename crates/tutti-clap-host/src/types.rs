//! Common types for CLAP plugin hosting.
//!
//! These are safe, idiomatic Rust counterparts to the C structs exposed by
//! `clap-sys`. They are used throughout the crate's public API so callers
//! never have to touch raw CLAP types directly. Cross-format shared types
//! (buffers, parameter automation, window/editor primitives, transport
//! snapshot, MIDI events) come from `tutti_plugin_types`.

use bitflags::bitflags;
use std::fmt;

pub use tutti_plugin_types::{
    ChannelLayout, ContentScale, EditorCapabilities, NoteExpressionType, ParameterChanges,
    ParameterPoint, ParameterQueue, PluginVersion, TransportInfo, WindowHandle, WireMidi,
};

/// Metadata describing a loaded plugin, returned from
/// [`ClapLoaded::probe`](crate::ClapLoaded::probe) and
/// [`ClapLoaded::info`](crate::ClapLoaded::info).
#[derive(Debug, Clone)]
pub struct PluginInfo {
    /// Reverse-DNS plugin identifier from `clap_plugin_descriptor.id`, stable
    /// across versions and the key a host stores to re-find this plugin.
    pub id: String,
    /// Display name for user-facing lists.
    pub name: String,
    /// Vendor name; empty when the descriptor omits it.
    pub vendor: String,
    /// The `clap_plugin_descriptor` version string verbatim, not parsed or ordered by this
    /// crate; [`PluginVersion::Absent`] when the descriptor omits it.
    pub version: PluginVersion,
    /// Plugin homepage; empty when the descriptor omits it.
    pub url: String,
    /// One-line description; empty when the descriptor omits it.
    pub description: String,
    /// CLAP feature/category tags (`"audio-effect"`, `"instrument"`, …) used
    /// for browser categorisation.
    pub features: Vec<String>,
}

impl PluginInfo {
    /// Creates a new [`PluginInfo`] with the given plugin ID and display name.
    /// Empty metadata fields but the id and name.
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            vendor: String::new(),
            version: PluginVersion::Absent,
            url: String::new(),
            description: String::new(),
            features: Vec::new(),
        }
    }

    /// Sets the plugin vendor (builder style).
    #[must_use]
    pub fn vendor(mut self, vendor: impl Into<String>) -> Self {
        self.vendor = vendor.into();
        self
    }

    /// Sets the plugin version (builder style).
    #[must_use]
    pub fn version(mut self, version: PluginVersion) -> Self {
        self.version = version;
        self
    }

    /// Sets the plugin homepage URL (builder style).
    #[must_use]
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }

    /// Sets the plugin description (builder style).
    #[must_use]
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = desc.into();
        self
    }

    /// Sets the CLAP feature/category tags (builder style).
    #[must_use]
    pub fn features(mut self, features: Vec<String>) -> Self {
        self.features = features;
        self
    }
}

impl fmt::Display for PluginInfo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} v{} by {}", self.name, self.version, self.vendor)
    }
}

/// A single note expression applied at a sample offset within a block.
///
/// CLAP's voice addressing is richer than the cross-format
/// [`NoteExpressionValue`](tutti_plugin_types::NoteExpressionValue): besides
/// `note_id` it can scope to a `port_index` / `channel` / `key`. This local
/// wrapper carries those extra fields; [`expression_type`](Self::expression_type)
/// uses the shared [`NoteExpressionType`]. Consumers crossing the crate
/// boundary use the shared `NoteExpressionValue`; the loader converts to/from
/// this CLAP-native shape at its edge.
#[derive(Debug, Clone, Copy)]
pub struct ClapNoteExpression {
    /// Frame offset within the current block at which the expression fires.
    pub sample_offset: i32,
    /// The voice this targets, matching the `note_id` of the note-on that
    /// started it. `-1` addresses every voice matching the key/channel scope.
    pub note_id: i32,
    /// Note port the voice was started on.
    pub port_index: i16,
    /// MIDI channel to scope to; `-1` means any channel.
    pub channel: i16,
    /// MIDI key to scope to; `-1` means any key.
    pub key: i16,
    /// Which expression dimension this carries (tuning, brightness, …).
    pub expression_type: NoteExpressionType,
    /// The expression value. Raw `f64` because the valid range is
    /// per-`expression_type` (semitones for tuning, `0..1` for most others) —
    /// this is a C-ABI-shaped value and unit types stop here.
    pub value: f64,
}

/// Cap for a note-expression pool filled on the audio thread.
///
/// Note expressions are per-note-per-block; a plugin emitting more than this
/// in a single block is well outside normal use.
pub const RT_NOTE_EXPR_CAPACITY: usize = 16;

/// A per-block note-expression pool filled inside `process` and lent back out
/// as `&[ClapNoteExpression]`. Capped for the same reason as
/// [`RtMidiEvents`](tutti_plugin_types::RtMidiEvents): the plugin, not the
/// host, decides how many events arrive.
pub type RtNoteExpressions = tutti_rt::RtVec<ClapNoteExpression, RT_NOTE_EXPR_CAPACITY>;

impl ClapNoteExpression {
    /// Creates a new note expression. Defaults to port 0, any channel, any key;
    /// refine with the `port`/`on_channel`/`on_key`/`at` builders.
    #[must_use]
    pub fn new(expression_type: NoteExpressionType, note_id: i32, value: f64) -> Self {
        Self {
            sample_offset: 0,
            note_id,
            port_index: 0,
            channel: -1,
            key: -1,
            expression_type,
            value,
        }
    }

    /// Sample offset (within the current block) at which the expression fires.
    #[must_use]
    pub fn at(mut self, sample_offset: i32) -> Self {
        self.sample_offset = sample_offset;
        self
    }

    /// Sets the note port index.
    #[must_use]
    pub fn port(mut self, port_index: i16) -> Self {
        self.port_index = port_index;
        self
    }

    /// Builds a CLAP-native expression from the shared, format-agnostic
    /// [`NoteExpressionValue`](tutti_plugin_types::NoteExpressionValue). The
    /// shared form carries no voice addressing, so `port`/`channel`/`key`
    /// default to port 0 / any-channel / any-key.
    #[must_use]
    pub(crate) fn from_shared(expr: &tutti_plugin_types::NoteExpressionValue) -> Self {
        Self::new(expr.expression_type, expr.note_id, expr.value).at(expr.sample_offset)
    }
}

bitflags! {
    /// Parameter behaviour flags from `clap_param_info`. See the CLAP spec
    /// for precise semantics of each bit. CLAP-native; the loader projects the
    /// subset it needs onto the shared `tutti_plugin_types::ParamFlags` at
    /// the crate boundary.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct ClapParamFlags: u32 {
        /// Only integral values within the range are valid.
        const STEPPED                 = 1 << 0;
        /// The range wraps: max and min denote the same point.
        const PERIODIC                = 1 << 1;
        /// Not to be shown in a generic parameter list.
        const HIDDEN                  = 1 << 2;
        /// The host must not write this value; the plugin owns it.
        const READONLY                = 1 << 3;
        /// This parameter is the plugin's bypass switch.
        const BYPASS                  = 1 << 4;
        /// The host may automate this parameter.
        const AUTOMATABLE             = 1 << 5;
        /// Automatable with per-voice addressing by `note_id`.
        const AUTOMATABLE_PER_NOTE_ID = 1 << 6;
        /// Automatable with per-key addressing.
        const AUTOMATABLE_PER_KEY     = 1 << 7;
        /// Automatable with per-channel addressing.
        const AUTOMATABLE_PER_CHANNEL = 1 << 8;
        /// Automatable with per-port addressing.
        const AUTOMATABLE_PER_PORT    = 1 << 9;
        /// The host may send modulation offsets for this parameter.
        const MODULATABLE             = 1 << 10;
        /// Modulatable with per-voice addressing by `note_id`.
        const MODULATABLE_PER_NOTE_ID = 1 << 11;
        /// Modulatable with per-key addressing.
        const MODULATABLE_PER_KEY     = 1 << 12;
        /// Modulatable with per-channel addressing.
        const MODULATABLE_PER_CHANNEL = 1 << 13;
        /// Modulatable with per-port addressing.
        const MODULATABLE_PER_PORT    = 1 << 14;
        /// Changes must be delivered in event order through `process()`
        /// rather than out-of-band via `flush()`. Gates
        /// [`set_parameter`](crate::ClapLoaded::set_parameter) on an actively
        /// processing instance.
        const REQUIRES_PROCESS        = 1 << 15;
        /// The value is an enumerated choice (`CLAP_PARAM_IS_ENUM`): an index
        /// into names the plugin gives through `value_to_text`. Implies
        /// [`STEPPED`](Self::STEPPED).
        const IS_ENUM                 = clap_sys::ext::params::CLAP_PARAM_IS_ENUM;
    }
}

/// Description of a single plugin parameter, CLAP-native. Richer than the
/// shared `tutti_plugin_types::ParameterInfo`: it carries CLAP's full
/// [`ClapParamFlags`] and a `module` grouping path. The loader projects it
/// down to the shared shape at the crate boundary.
#[derive(Debug, Clone)]
pub struct ClapParamInfo {
    /// Stable parameter id. Chosen by the plugin and persisted by the host —
    /// it is the key in automation and state, not the enumeration index.
    pub id: u32,
    /// Display name.
    pub name: String,
    /// Slash-separated grouping path (`"Filter/Cutoff"`); empty for ungrouped.
    pub module: String,
    /// Low end of the valid range, in the parameter's own plain units.
    pub min_value: f64,
    /// High end of the valid range, in the parameter's own plain units.
    pub max_value: f64,
    /// Value the plugin starts at, within `min_value..=max_value`.
    pub default_value: f64,
    /// Behaviour bits governing automation, modulation and visibility.
    pub flags: ClapParamFlags,
}

impl From<tutti_clap_ffi::ParamInfo> for ClapParamInfo {
    fn from(info: tutti_clap_ffi::ParamInfo) -> Self {
        Self {
            id: info.id,
            name: info.name,
            module: info.module,
            min_value: info.min_value,
            max_value: info.max_value,
            default_value: info.default_value,
            flags: ClapParamFlags::from_bits_truncate(info.flags),
        }
    }
}

impl ClapParamInfo {
    /// Creates a new parameter with the given ID and display name. Defaults
    /// to range `[0.0, 1.0]` with default `0.0` and no flags — use the
    /// builder methods to refine.
    pub fn new(id: u32, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            module: String::new(),
            min_value: 0.0,
            max_value: 1.0,
            default_value: 0.0,
            flags: ClapParamFlags::default(),
        }
    }

    /// Sets the grouping path (slash-separated, e.g. `"Filter/Cutoff"`).
    #[must_use]
    pub fn module(mut self, module: impl Into<String>) -> Self {
        self.module = module.into();
        self
    }

    /// Sets the value range and default all at once.
    #[must_use]
    pub fn range(mut self, min: f64, max: f64, default: f64) -> Self {
        self.min_value = min;
        self.max_value = max;
        self.default_value = default;
        self
    }

    /// Sets the parameter flags.
    #[must_use]
    pub fn flags(mut self, flags: ClapParamFlags) -> Self {
        self.flags = flags;
        self
    }
}

/// Description of an audio port exposed by the plugin.
#[derive(Debug, Clone)]
pub struct AudioPortInfo {
    /// Stable port id, distinct from the port's enumeration index.
    pub id: u32,
    /// Display name for the port.
    pub name: String,
    /// The port's channel layout. Carries the count directly; a CLAP port tag
    /// this host does not recognize as mono/stereo becomes
    /// `Multi(channel_count)` — the tag string itself is dropped (nothing
    /// consumes it).
    pub layout: ChannelLayout,
    /// Capability bits for this port.
    pub flags: AudioPortFlags,
    /// Id of the opposite-direction port this one can process in place with,
    /// or CLAP's invalid-id sentinel when in-place is unsupported.
    pub in_place_pair_id: u32,
}

pub use tutti_clap_ffi::AudioPortFlags;

/// Description of a note (MIDI) port exposed by the plugin.
#[derive(Debug, Clone)]
pub struct NotePortInfo {
    /// Stable port id, distinct from the port's enumeration index.
    pub id: u32,
    /// Display name for the port.
    pub name: String,
    /// Every dialect the port can accept.
    pub supported_dialects: NoteDialects,
    /// The port's preferred encoding, or `None` when the plugin named no
    /// dialect this host recognises.
    ///
    /// `Option` rather than a default variant because there is no dialect a
    /// host can substitute here without making a claim the plugin never made:
    /// `preferred_dialect == 0` means the plugin stated no preference, and a
    /// dialect added to CLAP after this host was built is equally unreadable.
    /// Route through [`dialect_to_send`](Self::dialect_to_send) rather than
    /// reading this field, so the absent case cannot be mistaken for a choice.
    pub preferred_dialect: Option<NoteDialect>,
}

impl NotePortInfo {
    /// What this host sends the port, from its supported and preferred
    /// dialects.
    ///
    /// The plugin's stated preference when this host can speak it and the
    /// port lists it as supported; otherwise a dialect the port supports,
    /// CLAP first, then MIDI 1.0. [`NoteSend::Nothing`] when the port supports
    /// neither, which is the case a caller must not paper over: it can accept
    /// only MPE or MIDI 2.0, and `host_note_ports_supported_dialects` tells
    /// the plugin this host sends neither, so there is no encoding both sides
    /// agree on.
    pub fn dialect_to_send(&self) -> NoteSend {
        let supports = |dialect| self.supported_dialects.contains(dialect);
        let clap = NoteSend::Clap {
            midi: supports(NoteDialects::MIDI),
        };
        match self.preferred_dialect {
            Some(NoteDialect::Clap) if supports(NoteDialects::CLAP) => clap,
            Some(NoteDialect::Midi) if supports(NoteDialects::MIDI) => NoteSend::Midi,
            // A preference this host cannot send, or the port does not list,
            // is no more usable than an absent one, so all take the
            // supported-dialect fallback.
            _ if supports(NoteDialects::CLAP) => clap,
            _ if supports(NoteDialects::MIDI) => NoteSend::Midi,
            _ => NoteSend::Nothing,
        }
    }
}

/// What this host sends an input note port, from the dialects the port
/// supports ([`NotePortInfo::dialect_to_send`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoteSend {
    /// CLAP note events. With `midi`, the port also lists the MIDI dialect, so
    /// `clap_event_midi` (controllers, bend, pressure, system messages) and
    /// `clap_event_midi_sysex` go to it too.
    Clap {
        /// Whether the port also supports MIDI 1.0.
        midi: bool,
    },
    /// MIDI 1.0 as `clap_event_midi`, notes included.
    Midi,
    /// The port supports neither dialect above: nothing is sent to it.
    Nothing,
}

impl NoteSend {
    /// CLAP note events only: a port that does not list the MIDI dialect.
    pub const CLAP_ONLY: Self = Self::Clap { midi: false };
    /// CLAP note events, and MIDI for everything else.
    pub const CLAP_AND_MIDI: Self = Self::Clap { midi: true };

    /// Whether `clap_event_midi` and SysEx may be sent to the port.
    #[must_use]
    pub const fn takes_midi(self) -> bool {
        matches!(self, Self::Clap { midi: true } | Self::Midi)
    }
}

bitflags! {
    /// Bitset of note-event dialects a port can accept.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct NoteDialects: u32 {
        /// CLAP's own note events, carrying `note_id` voice addressing.
        const CLAP     = 1 << 0;
        /// MIDI 1.0 byte messages.
        const MIDI     = 1 << 1;
        /// MIDI 1.0 under the MPE conventions.
        const MIDI_MPE = 1 << 2;
        /// MIDI 2.0 UMP packets.
        const MIDI2    = 1 << 3;
    }
}

/// A single note-event dialect (the port's preferred encoding).
///
/// This host can encode only [`Clap`](Self::Clap) and [`Midi`](Self::Midi);
/// the other two are recognised when a plugin names them but never sent. See
/// [`NoteSend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoteDialect {
    /// CLAP native note events.
    Clap,
    /// MIDI 1.0.
    Midi,
    /// MIDI 1.0 with MPE conventions.
    MidiMpe,
    /// MIDI 2.0 UMP.
    Midi2,
}

/// Voice allocation information reported by instruments that implement
/// `CLAP_EXT_VOICE_INFO`.
#[derive(Debug, Clone, Copy)]
pub struct VoiceInfo {
    /// Voices the plugin currently expects to use, at most `voice_capacity`.
    pub voice_count: u32,
    /// Hard upper bound on simultaneous voices; the host must not allocate
    /// `note_id`s expecting more than this to sound at once.
    pub voice_capacity: u32,
    /// Whether two notes on the same key may overlap. When false, the host
    /// must end the sounding note before starting another on that key.
    pub supports_overlapping_notes: bool,
}

/// A custom name for a single (port, channel, key) triple — used by
/// drum kits and similar instruments.
#[derive(Debug, Clone)]
pub struct NoteName {
    /// The name to display for this key, e.g. `"Kick"`.
    pub name: String,
    /// Note port the name applies to; `-1` means every port.
    pub port: i16,
    /// MIDI channel the name applies to; `-1` means every channel.
    pub channel: i16,
    /// MIDI key the name applies to; `-1` means every key.
    pub key: i16,
}

pub use tutti_clap_ffi::StateContext;

/// 32-bit ARGB color used by parameter indication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    /// Opacity; `255` is fully opaque.
    pub alpha: u8,
    /// Red channel.
    pub red: u8,
    /// Green channel.
    pub green: u8,
    /// Blue channel.
    pub blue: u8,
}

impl Color {}

pub use tutti_clap_ffi::AutomationState as ParamAutomationState;

/// Surround speaker positions (matches CLAP's `CLAP_SURROUND_*` constants).
///
/// # Why an `Unknown` arm, and why no `#[repr(u8)]`
///
/// The channel map is **positional**: `map[i]` is the speaker fed by channel
/// `i`. So a position this crate cannot name must still occupy its slot —
/// dropping it renumbers every channel after it, turning one unnameable
/// speaker into a silently wrong routing for the whole tail of the bus. That is
/// what [`Unknown`](Self::Unknown) is for: it carries the raw value verbatim so
/// the map keeps its length and its indices.
///
/// The realistic source of one is a CLAP revision adding a position past
/// `CLAP_SURROUND_TSR` — the same open-catalog reason `AuLayoutTag` carries an
/// `Unknown` arm. A closed enum would turn that release into either a decode
/// failure or a shifted map.
///
/// A payload variant rules out `#[repr(u8)]` with explicit discriminants, so
/// the wire values live in [`from_position`](Self::from_position)
/// and [`position`](Self::position), which is where a reader checks them
/// against the header anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurroundChannel {
    /// `CLAP_SURROUND_FL` (0).
    FrontLeft,
    /// `CLAP_SURROUND_FR` (1).
    FrontRight,
    /// `CLAP_SURROUND_FC` (2).
    FrontCenter,
    /// `CLAP_SURROUND_LFE` (3).
    LowFrequency,
    /// `CLAP_SURROUND_BL` (4).
    BackLeft,
    /// `CLAP_SURROUND_BR` (5).
    BackRight,
    /// `CLAP_SURROUND_FLC` (6).
    FrontLeftCenter,
    /// `CLAP_SURROUND_FRC` (7).
    FrontRightCenter,
    /// `CLAP_SURROUND_BC` (8).
    BackCenter,
    /// `CLAP_SURROUND_SL` (9).
    SideLeft,
    /// `CLAP_SURROUND_SR` (10).
    SideRight,
    /// `CLAP_SURROUND_TC` (11).
    TopCenter,
    /// `CLAP_SURROUND_TFL` (12).
    TopFrontLeft,
    /// `CLAP_SURROUND_TFC` (13).
    TopFrontCenter,
    /// `CLAP_SURROUND_TFR` (14).
    TopFrontRight,
    /// `CLAP_SURROUND_TBL` (15).
    TopBackLeft,
    /// `CLAP_SURROUND_TBC` (16).
    TopBackCenter,
    /// `CLAP_SURROUND_TBR` (17).
    TopBackRight,
    /// `CLAP_SURROUND_TSL` (18).
    TopSideLeft,
    /// `CLAP_SURROUND_TSR` (19) — the last position CLAP defines.
    TopSideRight,
    /// A position this crate does not name, carried verbatim.
    ///
    /// Keeps the channel's slot in a positional map (see the type docs) and
    /// lets the raw value be echoed back or logged.
    Unknown(u8),
}

impl SurroundChannel {
    /// Maps a raw CLAP surround channel ID to a [`SurroundChannel`].
    ///
    /// Total: an unrecognised id becomes [`Unknown`](Self::Unknown) rather than
    /// `None`, because the caller decodes a *positional* map and has no way to
    /// represent "channel 4's speaker is unreadable" other than by keeping the
    /// slot.
    #[must_use]
    pub fn from_position(pos: u8) -> Self {
        match pos {
            0 => Self::FrontLeft,
            1 => Self::FrontRight,
            2 => Self::FrontCenter,
            3 => Self::LowFrequency,
            4 => Self::BackLeft,
            5 => Self::BackRight,
            6 => Self::FrontLeftCenter,
            7 => Self::FrontRightCenter,
            8 => Self::BackCenter,
            9 => Self::SideLeft,
            10 => Self::SideRight,
            11 => Self::TopCenter,
            12 => Self::TopFrontLeft,
            13 => Self::TopFrontCenter,
            14 => Self::TopFrontRight,
            15 => Self::TopBackLeft,
            16 => Self::TopBackCenter,
            17 => Self::TopBackRight,
            18 => Self::TopSideLeft,
            19 => Self::TopSideRight,
            other => Self::Unknown(other),
        }
    }

    /// Returns the raw CLAP position id, the inverse of
    /// [`from_position`](Self::from_position).
    ///
    /// Use this instead of `as u8`, which the enum does not support (see the
    /// type docs).
    #[must_use]
    pub fn position(self) -> u8 {
        match self {
            Self::FrontLeft => 0,
            Self::FrontRight => 1,
            Self::FrontCenter => 2,
            Self::LowFrequency => 3,
            Self::BackLeft => 4,
            Self::BackRight => 5,
            Self::FrontLeftCenter => 6,
            Self::FrontRightCenter => 7,
            Self::BackCenter => 8,
            Self::SideLeft => 9,
            Self::SideRight => 10,
            Self::TopCenter => 11,
            Self::TopFrontLeft => 12,
            Self::TopFrontCenter => 13,
            Self::TopFrontRight => 14,
            Self::TopBackLeft => 15,
            Self::TopBackCenter => 16,
            Self::TopBackRight => 17,
            Self::TopSideLeft => 18,
            Self::TopSideRight => 19,
            Self::Unknown(raw) => raw,
        }
    }
}
