//! Plugin parameter descriptors — `ParameterInfo` and its parts.
//!
//! Shared value vocabulary across the host crates and the `tutti-plugin` IPC
//! protocol. Automation points/queues live in [`crate::automation`]. The
//! `Serialize`/`Deserialize` derives are gated behind the `serde` feature.
//!
//! The formats disagree about parameters more than about anything else, so the
//! shape here keeps the disagreement *representable* rather than averaged away:
//!
//! - [`PluginParamRange`] is a sum type because there is no safe number to substitute
//!   when a format declares no range. Every pair a host could invent is a claim
//!   the ABI never made — so the `Normalized` arm has no bounds to read.
//! - [`ParamSteps`] separates "continuous" from "nobody said", which a bare
//!   `step_count: u32` fuses at zero.
//! - [`ParamFlags`] is paired with a `known` mask so a flag the format never
//!   reported reads as [`None`] rather than as `false`.
//!
//! **Representable is not the same as unavoidable.** A format host builds these
//! and may match on them; a *consumer* should not have to. Read a parameter
//! through [`ParameterInfo`]'s own accessors — [`bounds`](ParameterInfo::bounds),
//! [`default_value`](ParameterInfo::default_value),
//! [`step_count`](ParameterInfo::step_count), [`flag`](ParameterInfo::flag) —
//! which answer the question a caller actually has without making them learn
//! which format declined what. A range is **data for display**; converting a
//! value between the plugin's plain domain and [`Normalized`](tutti_units::Normalized) is the format's,
//! asked through [`AnyThread::parameter_plain`](crate::AnyThread::parameter_plain)
//! and [`parameter_normalized`](crate::AnyThread::parameter_normalized). The
//! variants stay for the host that must produce them and for the caller
//! building a coverage report; they are not the reading path.

use bitflags::bitflags;

use crate::{ControlRole, ParamUnit, PlainMap, PlainUnit};
use tutti_units::ParamId;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// What the plugin declared about a parameter's value range.
///
/// A sum type rather than bounds plus a discriminant: with a flag, the
/// misleading `0.0..1.0` stays present and every consumer has to remember to
/// check a sibling field. Here the `Normalized` arm has no bounds *to read*.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum PluginParamRange {
    /// The ABI speaks normalized `0..=1` and the plugin declared no plain range.
    ///
    /// VST2 that declines `effGetParameterProperties`; VST3 with no edit
    /// controller, or whose `normalizedParamToPlain` is not monotonic.
    Normalized {
        /// Default, normalized `0..=1`.
        default: f64,
    },
    /// The plugin's declared range, in the unit [`ParameterInfo::unit`] names,
    /// as data: it says what the extremes are and where the default is, and
    /// maps nothing. A value is converted by the format
    /// ([`AnyThread::parameter_plain`](crate::AnyThread::parameter_plain)),
    /// since the map between `min` and `max` is the plugin's (VST3's
    /// `normalizedParamToPlain` fixes no shape) and may run backwards.
    ///
    /// AU and CLAP report this natively; VST3 from `normalizedParamToPlain` at
    /// the ends (`min` is the smaller, even for a decreasing map) and its
    /// default through the same call; VST2 via `effGetParameterProperties`.
    Plain {
        /// Minimum legal value.
        min: f64,
        /// Maximum legal value.
        max: f64,
        /// Default, in the same units as `min`/`max`.
        default: f64,
    },
}

impl Default for PluginParamRange {
    fn default() -> Self {
        Self::Normalized { default: 0.0 }
    }
}

impl PluginParamRange {
    /// Returns the plugin's declared bounds, or [`None`] if it declared none.
    pub fn bounds(&self) -> Option<(f64, f64)> {
        match *self {
            Self::Normalized { .. } => None,
            Self::Plain { min, max, .. } => Some((min, max)),
        }
    }

    /// Returns the default value, in whichever domain this range speaks.
    #[must_use]
    pub fn default_value(&self) -> f64 {
        match *self {
            Self::Normalized { default } | Self::Plain { default, .. } => default,
        }
    }
}

/// How many positions a parameter has.
///
/// [`Unknown`](Self::Unknown) is separate from [`Continuous`](Self::Continuous)
/// because a bare count fuses them at zero: VST2 without
/// `effGetParameterProperties` reports nothing, which is not the same as
/// reporting "freely variable".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum ParamSteps {
    /// The format reported no step information.
    #[default]
    Unknown,
    /// Any value in range. VST3 `stepCount == 0`.
    Continuous,
    /// Two positions. AU `kAudioUnitParameterUnit_Boolean`, VST3 `stepCount == 1`.
    Toggle,
    /// `n` positions indexing a named list, `n >= 2`.
    ///
    /// AU `kAudioUnitParameterUnit_Indexed`, CLAP `STEPPED`, VST3 `stepCount + 1`.
    Enumerated(u32),
}

impl ParamSteps {
    /// Builds from a span (`max - min` of an integer range), collapsing the
    /// degenerate cases.
    ///
    /// A span below 1 has no positions to step between, so it is
    /// [`Continuous`](Self::Continuous) rather than an invented count; a span of
    /// exactly 1 is two positions, which is a [`Toggle`](Self::Toggle).
    #[must_use]
    pub fn from_span(span: f64) -> Self {
        if !span.is_finite() || span < 1.0 {
            Self::Continuous
        } else if span == 1.0 {
            Self::Toggle
        } else {
            // `span + 1` positions for an inclusive integer range, saturating
            // at u32 rather than wrapping on an absurd declared span.
            Self::Enumerated((span as u64).saturating_add(1).min(u32::MAX as u64) as u32)
        }
    }

    /// Returns the number of positions, or [`None`] if unreported or freely
    /// variable.
    pub fn count(&self) -> Option<u32> {
        match *self {
            Self::Unknown | Self::Continuous => None,
            Self::Toggle => Some(2),
            Self::Enumerated(n) => Some(n),
        }
    }
}

bitflags! {
    /// Per-parameter capabilities, in the same shape as
    /// [`Features`](crate::Features).
    ///
    /// Meaningful only alongside a `known` mask — see
    /// [`ParameterInfo::flag`]. A bit clear here means "the format reported
    /// this as false" *or* "the format never reported it", and only the mask
    /// tells those apart.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
    pub struct ParamFlags: u32 {
        /// The host may write automation to this parameter.
        const AUTOMATABLE = 1 << 0;
        /// Readable but not writable.
        const READ_ONLY = 1 << 1;
        /// This is the plugin's bypass parameter.
        const BYPASS = 1 << 2;
        /// Should not be shown in a generic parameter list.
        const HIDDEN = 1 << 3;
        /// Wraps around at its extremes (VST3 `kIsWrapAround`, CLAP `PERIODIC`).
        const WRAP = 1 << 4;

        // --- CLAP-only below. No other hosted format reports these. ---

        /// Accepts modulation, which is distinct from automation in CLAP.
        const MODULATABLE = 1 << 5;
        /// Automation or modulation may target a single note id.
        const PER_NOTE_ID = 1 << 6;
        /// …a single key.
        const PER_KEY = 1 << 7;
        /// …a single channel.
        const PER_CHANNEL = 1 << 8;
        /// …a single port.
        const PER_PORT = 1 << 9;
        /// Writing this parameter selects a program.
        ///
        /// VST3 only (`kIsProgramChange`). It is how that format loads a
        /// preset — there is no separate call — so the flag is what lets a
        /// caller find the one parameter a program change goes through.
        const PROGRAM_CHANGE = 1 << 10;
    }
}

/// The description of one plugin parameter, shared by every format host.
///
/// Read it through the accessors — [`bounds`](Self::bounds),
/// [`default_value`](Self::default_value), [`step_count`](Self::step_count),
/// [`flag`](Self::flag) — rather than matching
/// on [`range`](Self::range) and [`steps`](Self::steps): they answer the
/// question a caller has without it learning which format declined what.
///
/// # Examples
///
/// ```
/// use tutti_plugin_types::{ParamFlags, ParameterInfo};
/// use tutti_units::ParamId;
///
/// let cutoff = ParameterInfo::new(ParamId::new(7), "Cutoff")
///     .with_plain_range(20.0, 20_000.0, 1_000.0)
///     .with_flags(ParamFlags::AUTOMATABLE, ParamFlags::AUTOMATABLE);
/// assert_eq!(cutoff.bounds(), Some((20.0, 20_000.0)));
/// assert_eq!(cutoff.flag(ParamFlags::AUTOMATABLE), Some(true));
/// assert_eq!(cutoff.flag(ParamFlags::BYPASS), None); // never reported
///
/// // VST2 without `effGetParameterProperties`: no declared range at all.
/// let mix = ParameterInfo::new(ParamId::new(0), "Mix");
/// assert_eq!(mix.bounds(), None);
/// ```
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "ParameterInfoWire"))]
pub struct ParameterInfo {
    /// How this parameter is addressed: the plugin's handle for VST3, CLAP and
    /// AU, its position for VST2. Opaque either way; see [`ParamId`].
    id: ParamId,
    /// The plugin's display name for this parameter, unqualified by its group.
    /// Not unique — see [`qualified_name`](Self::qualified_name).
    name: String,
    /// Display unit (`"dB"`, `"Hz"`, …). Empty when the format carries none —
    /// CLAP has no unit string at all.
    unit: String,
    /// What the plugin declared about the value range, including the case where
    /// it declared nothing. Read through [`bounds`](Self::bounds) and
    /// [`default_value`](Self::default_value) rather than matching.
    range: PluginParamRange,
    /// How many positions the parameter has, distinguishing "continuous" from
    /// "the format never said". Read through [`step_count`](Self::step_count).
    steps: ParamSteps,
    /// Capability bits. Read through [`flag`](Self::flag), not directly, so an
    /// unreported capability cannot be mistaken for a reported `false`.
    flags: ParamFlags,
    /// Which bits of [`flags`](Self::flags) the format actually reported.
    known: ParamFlags,
    /// The group this parameter belongs to, as a display label (a path for VST3 and CLAP). Empty when the
    /// plugin declared none.
    ///
    /// Every hosted format has a grouping mechanism and no two agree on its
    /// shape: VST3 has a real tree of units, CLAP a `/`-separated module path,
    /// VST2 a numbered category, AU an optional integer clump. What they share
    /// is that each resolves to a *name*, so that is what crosses this
    /// boundary: for VST3 and CLAP the group's full path, as
    /// [`ParamGroup::path`](crate::ParamGroup::path) keys it. The tree is
    /// [`PluginWiring::param_groups`](crate::PluginWiring::param_groups).
    ///
    /// Deliberately a `String` rather than an `Option<String>`, unlike the
    /// [`known`](Self::known) mask beside it: "the format never said" and "the
    /// format said none" are the same flat list to every consumer, so there is
    /// no second state to keep. The group *id* is deliberately absent — AU
    /// clump 3, VST2 category 3 and VST3 unit 3 are unrelated numbers, and a
    /// bare number that does not say which model it belongs to is a mistake.
    group: String,
    /// What the number measures, only where the format declared it exactly and
    /// the declared range fits it. `None` is the common case, and the flat
    /// [`unit`](Self::unit) string is still there. Read a value in tutti's own
    /// unit through [`typed`](Self::typed); set it with
    /// [`with_measures`](Self::with_measures), which applies the range check.
    ///
    /// Data saved before this field reads as `None` with no attribute: serde's
    /// derive already reads a missing `Option` field as `None`,
    /// unlike
    /// [`role`](Self::role), whose custom serde needs its own `default`.
    measures: Option<ParamUnit>,
    /// The role the plugin declared for this parameter (VST3's function names),
    /// never inferred from a name.
    role: Option<ControlRole>,
    /// The plugin marks this parameter as one of its headline controls (AU's
    /// `ParametersForOverview`). `false` is both "not marked" and "the format
    /// has no such list": VST3 and CLAP say nothing of it.
    overview: bool,
    /// A short label for a narrow display, where the plugin declares one
    /// (VST3's `shortTitle`, VST2's `shortLabel`); empty when it declares none.
    short_name: String,
    /// How the plain value follows from the normalized position, as the format
    /// declares it. Read through [`plain_map`](Self::plain_map).
    plain_map: Option<PlainMap>,
}

/// What a [`ParameterInfo`] is read from: its fields as the document holds
/// them, before the conversion checks them.
#[cfg(feature = "serde")]
#[derive(Deserialize)]
struct ParameterInfoWire {
    pub id: ParamId,
    pub name: String,
    pub unit: String,
    pub range: PluginParamRange,
    pub steps: ParamSteps,
    pub flags: ParamFlags,
    pub known: ParamFlags,
    pub group: String,
    pub measures: Option<ParamUnit>,
    pub role: Option<ControlRole>,
    #[serde(default)]
    pub overview: bool,
    #[serde(default)]
    pub short_name: String,
    #[serde(default)]
    pub plain_map: Option<PlainMap>,
}

/// A document's parameter that breaks one of [`ParameterInfo`]'s invariants.
#[cfg(feature = "serde")]
#[derive(Debug, thiserror::Error)]
enum ParameterInfoInvalid {
    /// A flag is set that the `known` mask never reported.
    #[error("a flag is set outside the `known` mask")]
    FlagOutsideKnown,
    /// `measures` names a unit the declared range does not fit.
    #[error("`measures` names a unit the declared range does not fit")]
    MeasuresOutsideRange,
}

#[cfg(feature = "serde")]
impl TryFrom<ParameterInfoWire> for ParameterInfo {
    type Error = ParameterInfoInvalid;

    fn try_from(w: ParameterInfoWire) -> Result<Self, Self::Error> {
        if !w.known.contains(w.flags) {
            return Err(ParameterInfoInvalid::FlagOutsideKnown);
        }
        if w.measures.is_some_and(|unit| !unit.fits(&w.range)) {
            return Err(ParameterInfoInvalid::MeasuresOutsideRange);
        }
        Ok(Self {
            id: w.id,
            name: w.name,
            unit: w.unit,
            range: w.range,
            steps: w.steps,
            flags: w.flags,
            known: w.known,
            group: w.group,
            measures: w.measures,
            role: w.role,
            overview: w.overview,
            short_name: w.short_name,
            plain_map: w.plain_map,
        })
    }
}

impl ParameterInfo {
    /// Builds a parameter with an address and a name, everything else
    /// undeclared: no unit, no group, a normalized range defaulting to `0.0`,
    /// [`ParamSteps::Unknown`], and an empty `known` mask so every capability
    /// reads as unreported. The `with_*` builders fill in what a format states.
    pub fn new(id: ParamId, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            unit: String::new(),
            range: PluginParamRange::default(),
            steps: ParamSteps::Unknown,
            flags: ParamFlags::empty(),
            known: ParamFlags::empty(),
            group: String::new(),
            measures: None,
            role: None,
            overview: false,
            short_name: String::new(),
            plain_map: None,
        }
    }

    /// Sets how the plain value follows from the normalized position
    /// ([`PlainMap`]); a format that fills it builds it from the map its own
    /// conversions use.
    #[must_use]
    pub const fn with_plain_map(mut self, plain_map: PlainMap) -> Self {
        self.plain_map = Some(plain_map);
        self
    }

    /// How the plain value follows from the normalized position, as the format
    /// declares it: a map to compute from here, or `None` when the plugin
    /// defines the map (VST3, a VST2 parameter with no integer range), which is
    /// asked through
    /// [`AnyThread::parameter_plain`](crate::AnyThread::parameter_plain). A
    /// parameter built without one reads `None`.
    #[must_use]
    pub const fn plain_map(&self) -> Option<PlainMap> {
        self.plain_map
    }

    /// Sets what the number measures, if the declared [`range`](Self::range)
    /// fits `unit` ([`ParamUnit::fits`]); a unit the range does not fit leaves
    /// [`measures`](Self::measures) `None`, and so does a later range it does
    /// not fit.
    #[must_use]
    pub fn with_measures(mut self, unit: Option<ParamUnit>) -> Self {
        self.measures = unit.filter(|u| u.fits(&self.range));
        self
    }

    /// Sets the display unit string (`"dB"`, `"Hz"`), as the format wrote it.
    #[must_use]
    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        self.unit = unit.into();
        self
    }

    /// How this parameter is addressed: the plugin's handle for VST3, CLAP and
    /// AU, its position for VST2. Opaque either way; see [`ParamId`].
    #[must_use]
    pub const fn id(&self) -> ParamId {
        self.id
    }

    /// The plugin's display name for this parameter, unqualified by its group.
    /// Not unique; see [`qualified_name`](Self::qualified_name).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The display unit (`"dB"`, `"Hz"`), empty when the format carries none
    /// (CLAP has no unit string at all).
    #[must_use]
    pub fn unit(&self) -> &str {
        &self.unit
    }

    /// The group this parameter belongs to; empty when the plugin declared none.
    /// For VST3 and CLAP it is the [`ParamGroup::path`](crate::ParamGroup::path)
    /// of the group.
    #[must_use]
    pub fn group(&self) -> &str {
        &self.group
    }

    /// What the number measures, only where the format declared it exactly and
    /// the declared range fits it ([`with_measures`](Self::with_measures)).
    /// `None` is the common case. Read a value in tutti's own unit through
    /// [`typed`](Self::typed).
    #[must_use]
    pub const fn measures(&self) -> Option<ParamUnit> {
        self.measures
    }

    /// The role the plugin declared for this parameter ([`ControlRole`]), only
    /// where the format declares one itself. `None` for every CLAP and AU
    /// parameter, and for a VST3 parameter no function name points at.
    #[must_use]
    pub const fn role(&self) -> Option<ControlRole> {
        self.role
    }

    /// Whether the plugin marks this parameter as a headline control (AU
    /// `ParametersForOverview`). Always `false` for VST3 and CLAP, which have
    /// no such list.
    #[must_use]
    pub const fn overview(&self) -> bool {
        self.overview
    }

    /// The short label the plugin declares for a narrow display; empty when it
    /// declares none (CLAP and AU have no such field).
    #[must_use]
    pub fn short_name(&self) -> &str {
        &self.short_name
    }

    /// Sets the short label.
    #[must_use]
    pub fn with_short_name(mut self, short_name: impl Into<String>) -> Self {
        self.short_name = short_name.into();
        self
    }

    /// Marks this parameter as one of the plugin's headline controls.
    #[must_use]
    pub const fn with_overview(mut self, overview: bool) -> Self {
        self.overview = overview;
        self
    }

    /// What the plugin declared about the value range, including the case
    /// where it declared nothing. A coverage report matches on it; a consumer
    /// reads [`bounds`](Self::bounds) and [`default_value`](Self::default_value).
    #[must_use]
    pub const fn range(&self) -> PluginParamRange {
        self.range
    }

    /// How many positions the parameter has, distinguishing "continuous" from
    /// "the format never said". A consumer reads [`step_count`](Self::step_count).
    #[must_use]
    pub const fn steps(&self) -> ParamSteps {
        self.steps
    }

    /// Sets the role the plugin declared for this parameter.
    #[must_use]
    pub const fn with_role(mut self, role: Option<ControlRole>) -> Self {
        self.role = role;
        self
    }

    /// A plain value of this parameter as a tutti unit, if the declared unit and
    /// the declared plain range allow it.
    ///
    /// The scaling happens here, once: a percent is divided by 100 for a
    /// [`Mix`](tutti_units::Mix), milliseconds by 1000 for
    /// [`Seconds`](tutti_units::Seconds). `None` for a normalized-only
    /// parameter, one with no declared [`measures`](Self::measures), a unit
    /// `U` does not read, or a `plain` that is not finite. Get `plain` from
    /// [`AnyThread::parameter_plain`](crate::AnyThread::parameter_plain) for a
    /// normalized value.
    ///
    /// ```
    /// use tutti_plugin_types::{ParamUnit, ParameterInfo};
    /// use tutti_units::ParamId;
    /// use tutti_units::{Mix, Seconds};
    ///
    /// let wet = ParameterInfo::new(ParamId::new(1), "Wet")
    ///     .with_plain_range(0.0, 100.0, 50.0)
    ///     .with_measures(Some(ParamUnit::Percent));
    /// assert_eq!(wet.typed::<Mix>(25.0), Some(Mix::new(0.25)));
    /// assert_eq!(wet.typed::<Seconds>(25.0), None);
    /// ```
    #[must_use]
    pub fn typed<U: PlainUnit>(&self, plain: f64) -> Option<U> {
        if !matches!(self.range, PluginParamRange::Plain { .. }) || !plain.is_finite() {
            return None;
        }
        U::from_plain(self.measures?, plain)
    }

    /// Sets the group this parameter belongs to.
    #[must_use]
    pub fn with_group(mut self, group: impl Into<String>) -> Self {
        self.group = group.into();
        self
    }

    /// Sets the range exactly as a format host built it.
    #[must_use]
    pub fn with_range(mut self, range: PluginParamRange) -> Self {
        self.set_range(range);
        self
    }

    /// Sets a declared plain range, in the unit [`unit`](Self::unit) names.
    #[must_use]
    pub fn with_plain_range(mut self, min: f64, max: f64, default: f64) -> Self {
        self.set_range(PluginParamRange::Plain { min, max, default });
        self
    }

    /// Sets a normalized `0..=1` range with the given default, for a parameter
    /// that declared no plain range.
    #[must_use]
    pub fn with_normalized_default(mut self, default: f64) -> Self {
        self.set_range(PluginParamRange::Normalized { default });
        self
    }

    /// Replaces the range, dropping a [`measures`](Self::measures) unit the new
    /// range does not fit: the invariant the deserializer checks.
    fn set_range(&mut self, range: PluginParamRange) {
        self.range = range;
        self.measures = self.measures.filter(|u| u.fits(&self.range));
    }

    /// Sets the step information.
    #[must_use]
    pub fn with_steps(mut self, steps: ParamSteps) -> Self {
        self.steps = steps;
        self
    }

    /// Records `reported` as the values of the `known` capabilities.
    ///
    /// Takes both together so a format cannot set a bit it never reported, nor
    /// report a bit it left unset.
    #[must_use]
    pub fn with_flags(mut self, known: ParamFlags, reported: ParamFlags) -> Self {
        self.known = known;
        self.flags = reported & known;
        self
    }

    /// Returns `Some(value)` if the format reported this capability, [`None`] if
    /// it did not.
    ///
    /// Pass exactly one bit; a multi-bit query answers whether *all* of them are
    /// known and set.
    pub fn flag(&self, f: ParamFlags) -> Option<bool> {
        self.known.contains(f).then(|| self.flags.contains(f))
    }

    /// Returns the plugin's declared bounds, or [`None`] when it declared none.
    ///
    /// The normalized read: a caller asking what this parameter's range *is*
    /// should not have to know that VST2 sometimes declines
    /// `effGetParameterProperties` while AU always answers. [`None`] is the one
    /// fact that survives normalization — it means "no declared bounds". The
    /// bounds are data for display and for [`typed`](Self::typed), never a
    /// formula: the plain value of a normalized one is the format's answer.
    pub fn bounds(&self) -> Option<(f64, f64)> {
        self.range.bounds()
    }

    /// Returns the default value, in whichever domain [`bounds`](Self::bounds)
    /// speaks: plain units when it returns `Some`, normalized `0..=1` when
    /// `None`.
    #[must_use]
    pub fn default_value(&self) -> f64 {
        self.range.default_value()
    }

    /// Returns the number of unassigned positions, or [`None`] when the parameter
    /// is continuous *or* the format never said.
    ///
    /// Those two collapse deliberately: both mean "do not draw this as a
    /// stepped control", which is the only decision a consumer makes from it.
    /// The distinction is still on [`steps`](Self::steps) for a caller building
    /// a capability report rather than a UI.
    pub fn step_count(&self) -> Option<u32> {
        self.steps.count()
    }

    /// Returns the parameter's name qualified by its group (`"Delay / Mix"`), or
    /// the bare name when it has no group.
    ///
    /// Grouping does not make names unique: two parameters that a
    /// plugin genuinely distinguishes but names identically *within one group*
    /// still qualify to the same string. Addressing is unaffected — that always
    /// goes through [`id`](Self::id).
    pub fn qualified_name(&self) -> String {
        if self.group.is_empty() {
            self.name.clone()
        } else {
            format!("{} / {}", self.group, self.name)
        }
    }
}

/// Why a host write to a parameter was refused; nothing was written.
///
/// The one write policy, whatever the format: a write names a parameter the
/// plugin declares and that is not read-only, or it fails at the call
/// ([`Control::set_parameter`](crate::Control::set_parameter)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ParamWriteError {
    /// The plugin declares no parameter with this id.
    #[error("the plugin declares no parameter {0:?}")]
    Undeclared(ParamId),
    /// The parameter is declared read-only: the host may not write it.
    #[error("parameter {0:?} is read-only")]
    ReadOnly(ParamId),
    /// The plugin is lost (crashed, or gone), so there is nothing to write
    /// parameter `0` to and no declaration to check it against.
    #[error("the plugin is lost: parameter {0:?} was not written")]
    Lost(ParamId),
    /// The plugin did not answer in time, so the write could not be checked
    /// against what it declares and was not made.
    #[error("the plugin did not answer in time: parameter {0:?} was not written")]
    Unanswered(ParamId),
}

/// What a format reports of a declared parameter's writability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParamAccess {
    /// The format never reports the read-only bit (VST2 has no such opcode); a
    /// parameter whose state is unknown is writable.
    Unreported,
    /// Declared, and not read-only.
    Writable,
    /// Declared read-only.
    ReadOnly,
}

impl ParamAccess {
    /// The access `info` declares: its [`ParamFlags::READ_ONLY`] bit as
    /// [`ParameterInfo::flag`] reads it.
    #[must_use]
    pub fn of(info: &ParameterInfo) -> Self {
        match info.flag(ParamFlags::READ_ONLY) {
            Some(read_only) => Self::reported(read_only),
            None => Self::Unreported,
        }
    }

    /// The access of a parameter whose read-only bit the format reports.
    #[must_use]
    pub fn reported(read_only: bool) -> Self {
        if read_only {
            Self::ReadOnly
        } else {
            Self::Writable
        }
    }
}

impl ParamWriteError {
    /// The parameter the refused write named.
    #[must_use]
    pub const fn id(self) -> ParamId {
        match self {
            Self::Undeclared(id) | Self::ReadOnly(id) | Self::Lost(id) | Self::Unanswered(id) => id,
        }
    }

    /// The policy: `Ok` for a write to a declared parameter that is not
    /// declared read-only.
    ///
    /// `declared` is what the format knows of the parameter `id`: `None` when
    /// the plugin declares none, else its [`ParamAccess`], which is its
    /// [`ParamFlags::READ_ONLY`] bit as [`ParameterInfo::flag`] reads it.
    ///
    /// # Errors
    ///
    /// [`Undeclared`](Self::Undeclared) for a parameter that is not declared,
    /// [`ReadOnly`](Self::ReadOnly) for one declared read-only.
    pub fn check(id: ParamId, declared: Option<ParamAccess>) -> Result<(), Self> {
        match declared {
            None => Err(Self::Undeclared(id)),
            Some(ParamAccess::ReadOnly) => Err(Self::ReadOnly(id)),
            Some(ParamAccess::Writable | ParamAccess::Unreported) => Ok(()),
        }
    }

    /// [`check`](Self::check) on the declared parameters of a plugin, sorted by id
    /// (a binary search, for a plugin with thousands).
    ///
    /// # Errors
    ///
    /// As [`check`](Self::check).
    pub fn check_sorted(id: ParamId, declared: &[(ParamId, ParamAccess)]) -> Result<(), Self> {
        let found = declared
            .binary_search_by_key(&id, |&(known, _)| known)
            .ok()
            .map(|i| declared[i].1);
        Self::check(id, found)
    }

    /// [`check`](Self::check) on the info the plugin declared for `id`, or
    /// `None` for an id it does not declare.
    ///
    /// # Errors
    ///
    /// As [`check`](Self::check).
    pub(crate) fn check_info(id: ParamId, info: Option<&ParameterInfo>) -> Result<(), Self> {
        Self::check(id, info.map(ParamAccess::of))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A write names a declared, writable parameter, or fails at the call**:
    /// an id the plugin does not declare is `Undeclared`, a declared read-only
    /// one is `ReadOnly`, and a declared one that never reports the bit (VST2)
    /// is writable.
    #[test]
    fn a_write_is_refused_for_an_undeclared_or_a_read_only_parameter() {
        let id = ParamId::new(7);
        let info = |reported: Option<bool>| {
            let known = if reported.is_some() {
                ParamFlags::READ_ONLY
            } else {
                ParamFlags::empty()
            };
            let set = if reported == Some(true) {
                ParamFlags::READ_ONLY
            } else {
                ParamFlags::empty()
            };
            ParameterInfo::new(id, "p").with_flags(known, set)
        };
        assert_eq!(
            ParamWriteError::check_info(id, None),
            Err(ParamWriteError::Undeclared(id))
        );
        assert_eq!(
            ParamWriteError::check_info(id, Some(&info(Some(true)))),
            Err(ParamWriteError::ReadOnly(id))
        );
        assert_eq!(
            ParamWriteError::check_info(id, Some(&info(Some(false)))),
            Ok(())
        );
        assert_eq!(ParamWriteError::check_info(id, Some(&info(None))), Ok(()));
    }

    /// Bounds of `0.0..1.0` that the plugin *declared* stay distinguishable
    /// from a parameter that declared none.
    ///
    /// A bounds pair alone could not express this: the numbers are identical.
    /// The `Normalized` arm has no bounds at all.
    #[test]
    fn a_declared_unit_range_is_not_the_same_as_no_range() {
        let declared = ParameterInfo::new(ParamId::new(1), "Blend").with_plain_range(0.0, 1.0, 0.0);
        let undeclared = ParameterInfo::new(ParamId::new(1), "Blend").with_normalized_default(0.0);
        assert_eq!(declared.range.bounds(), Some((0.0, 1.0)));
        assert_eq!(undeclared.range.bounds(), None);
    }

    /// An unreported flag reads as `None`, not as `false`.
    ///
    /// The reason the `known` mask exists: AUv2 has no automation metadata, so
    /// reporting `automatable: false` would be a claim the ABI never made — and
    /// blanket-reporting `true` is worse.
    #[test]
    fn an_unreported_flag_is_not_a_false_flag() {
        let p = ParameterInfo::new(ParamId::new(1), "Gain")
            .with_flags(ParamFlags::READ_ONLY, ParamFlags::empty());

        assert_eq!(p.flag(ParamFlags::READ_ONLY), Some(false));
        assert_eq!(
            p.flag(ParamFlags::AUTOMATABLE),
            None,
            "a capability the format never reported must not read as false"
        );
    }

    /// `with_flags` cannot set a bit outside the reported mask.
    #[test]
    fn with_flags_cannot_report_an_unknown_bit() {
        let p = ParameterInfo::new(ParamId::new(1), "Gain").with_flags(
            ParamFlags::READ_ONLY,
            ParamFlags::READ_ONLY | ParamFlags::BYPASS,
        );
        assert_eq!(p.flag(ParamFlags::READ_ONLY), Some(true));
        assert_eq!(
            p.flag(ParamFlags::BYPASS),
            None,
            "a bit outside the known mask must not become readable"
        );
        assert!(!p.flags.contains(ParamFlags::BYPASS));
    }

    /// A grouped parameter qualifies its name; an ungrouped one does not.
    ///
    /// The second half is the one that matters: the overwhelming majority of
    /// parameters declare no group, and a `" / Mix"` with a dangling separator
    /// would be visible on every one of them.
    #[test]
    fn a_group_qualifies_the_name_and_its_absence_does_not() {
        let grouped = ParameterInfo::new(ParamId::new(1), "Mix").with_group("Delay");
        assert_eq!(grouped.qualified_name(), "Delay / Mix");

        let ungrouped = ParameterInfo::new(ParamId::new(1), "Mix");
        assert_eq!(
            ungrouped.qualified_name(),
            "Mix",
            "an ungrouped parameter must not gain a separator"
        );
    }

    /// `Unknown` and `Continuous` are different answers.
    #[test]
    fn unknown_steps_are_not_continuous_steps() {
        assert_ne!(ParamSteps::Unknown, ParamSteps::Continuous);
        assert_eq!(ParamSteps::Unknown.count(), None);
        assert_eq!(ParamSteps::Continuous.count(), None);
    }

    /// A span maps onto positions, with the degenerate cases collapsed.
    #[test]
    fn a_span_becomes_positions() {
        assert_eq!(ParamSteps::from_span(0.0), ParamSteps::Continuous);
        assert_eq!(ParamSteps::from_span(0.5), ParamSteps::Continuous);
        assert_eq!(ParamSteps::from_span(f64::NAN), ParamSteps::Continuous);
        assert_eq!(ParamSteps::from_span(f64::INFINITY), ParamSteps::Continuous);
        assert_eq!(ParamSteps::from_span(1.0), ParamSteps::Toggle);
        assert_eq!(ParamSteps::from_span(10.0), ParamSteps::Enumerated(11));
    }

    /// The range survives the postcard wire format both IPC peers speak.
    ///
    /// Note what this does *not* prove: postcard is non-self-describing, so a
    /// peer built against an older layout produces a payload that fails to
    /// decode rather than defaulting. `PROTOCOL_VERSION` governs that.
    #[cfg(feature = "serde")]
    #[test]
    fn the_range_survives_the_postcard_round_trip() {
        for want in [
            PluginParamRange::Normalized { default: 0.25 },
            PluginParamRange::Plain {
                min: -96.0,
                max: 6.0,
                default: 0.0,
            },
        ] {
            let mut info = ParameterInfo::new(ParamId::new(1), "Gain");
            info.range = want;
            let bytes = postcard::to_allocvec(&info).expect("serialize");
            let back: ParameterInfo = postcard::from_bytes(&bytes).expect("deserialize");
            assert_eq!(back.range, want);
        }
    }

    /// The `known` mask survives the wire too — a flag that arrives without its
    /// mask bit would read as a reported `false`.
    #[cfg(feature = "serde")]
    #[test]
    fn the_known_mask_survives_the_postcard_round_trip() {
        let info = ParameterInfo::new(ParamId::new(1), "Gain")
            .with_flags(ParamFlags::READ_ONLY, ParamFlags::READ_ONLY);
        let bytes = postcard::to_allocvec(&info).expect("serialize");
        let back: ParameterInfo = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(back.flag(ParamFlags::READ_ONLY), Some(true));
        assert_eq!(back.flag(ParamFlags::AUTOMATABLE), None);
    }

    /// The group survives the wire — it is decoded in the plugin subprocess and
    /// consumed in the host, so it is only useful if it crosses.
    #[cfg(feature = "serde")]
    #[test]
    fn the_group_survives_the_postcard_round_trip() {
        let info = ParameterInfo::new(ParamId::new(1), "Mix").with_group("Ring Modulation");
        let bytes = postcard::to_allocvec(&info).expect("serialize");
        let back: ParameterInfo = postcard::from_bytes(&bytes).expect("deserialize");
        assert_eq!(back.group, "Ring Modulation");
        assert_eq!(back.qualified_name(), "Ring Modulation / Mix");
    }

    /// A consumer asks `ParameterInfo` and never destructures `PluginParamRange`.
    /// Which format declined to declare a range is the host's problem, not the
    /// caller's — the whole point of normalizing here.
    #[test]
    fn a_consumer_reads_bounds_without_matching_on_provenance() {
        let declared =
            ParameterInfo::new(ParamId::new(1), "Cutoff").with_plain_range(20.0, 20_000.0, 440.0);
        assert_eq!(declared.bounds(), Some((20.0, 20_000.0)));
        assert_eq!(declared.default_value(), 440.0);

        // A plugin that declared nothing answers `None` rather than inventing a
        // span.
        let undeclared = ParameterInfo::new(ParamId::new(2), "Mix").with_normalized_default(0.25);
        assert_eq!(undeclared.bounds(), None);
        assert_eq!(undeclared.default_value(), 0.25);
    }

    /// `Continuous` and `Unknown` both mean "not a stepped control", which is
    /// the only decision a consumer makes from the step count. They collapse on
    /// the normalized read and stay distinct on `steps` itself.
    #[test]
    fn step_count_collapses_continuous_and_unreported() {
        let base = ParameterInfo::new(ParamId::new(3), "Shape");

        for steps in [ParamSteps::Continuous, ParamSteps::Unknown] {
            assert_eq!(
                base.clone().with_steps(steps).step_count(),
                None,
                "{steps:?}"
            );
        }

        assert_eq!(
            base.clone().with_steps(ParamSteps::Toggle).step_count(),
            Some(2)
        );
        assert_eq!(
            base.with_steps(ParamSteps::Enumerated(5)).step_count(),
            Some(5)
        );
    }
}

#[cfg(all(test, feature = "serde"))]
mod measures_serde_tests {
    use super::*;
    use crate::Axis;

    /// What `ParameterInfo` serialized as before `measures`, `role` and
    /// `overview`: the JSON of an older saved catalog.
    const OLD_JSON: &str = r#"{
        "id": 7, "name": "Cutoff", "unit": "Hz",
        "range": {"Plain": {"min": 20.0, "max": 20000.0, "default": 1000.0}},
        "steps": "Continuous", "flags": "AUTOMATABLE", "known": "AUTOMATABLE", "group": "Filter"
    }"#;

    /// An old `ParameterInfo` without the fields still deserializes, with
    /// every one unmarked.
    #[test]
    fn an_old_parameter_without_the_fields_still_deserializes() {
        let p: ParameterInfo = serde_json::from_str(OLD_JSON).expect("old data reads");
        assert_eq!(p.name, "Cutoff");
        assert_eq!((p.measures, p.role, p.overview), (None, None, false));
    }

    /// A unit, a role, the overview mark and the short name survive JSON and the IPC's postcard.
    #[test]
    fn measures_role_and_overview_round_trip() {
        let p = ParameterInfo::new(ParamId::new(3), "Pan")
            .with_plain_range(-1.0, 1.0, 0.0)
            .with_measures(Some(ParamUnit::Pan))
            .with_role(Some(ControlRole::Pan(Axis::Y)))
            .with_overview(true)
            .with_short_name("Pn");
        let want = (
            Some(ParamUnit::Pan),
            Some(ControlRole::Pan(Axis::Y)),
            true,
            "Pn",
        );
        fn of(p: &ParameterInfo) -> (Option<ParamUnit>, Option<ControlRole>, bool, &str) {
            (p.measures, p.role, p.overview, p.short_name.as_str())
        }

        let json: ParameterInfo =
            serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(of(&json), want);

        let bin: ParameterInfo = postcard::from_bytes(&postcard::to_allocvec(&p).unwrap()).unwrap();
        assert_eq!(of(&bin), want);
    }

    /// A role no `ControlRole` names is refused, not mapped to another.
    #[test]
    fn a_role_that_names_no_control_role_is_refused() {
        let json = OLD_JSON.replace(
            r#""group": "Filter""#,
            r#""group": "Filter", "role": "Nope""#,
        );
        assert!(serde_json::from_str::<ParameterInfo>(&json).is_err());
    }

    /// A document that sets a flag its `known` mask never reported is refused:
    /// the flag would read as a reported value nobody reported.
    #[test]
    fn a_flag_outside_the_known_mask_is_refused() {
        let json = OLD_JSON.replace(r#""known": "AUTOMATABLE""#, r#""known": """#);
        assert!(serde_json::from_str::<ParameterInfo>(&json).is_err());
    }

    /// A document whose `measures` unit its range does not fit is refused:
    /// Hertz is non-negative, and this range starts at -1.
    #[test]
    fn a_measures_unit_the_range_does_not_fit_is_refused() {
        let with = |min: &str| {
            OLD_JSON.replace("20.0", min).replace(
                r#""group": "Filter""#,
                r#""group": "Filter", "measures": "Hertz""#,
            )
        };
        assert!(serde_json::from_str::<ParameterInfo>(&with("-1.0")).is_err());
        assert!(serde_json::from_str::<ParameterInfo>(&with("20.0")).is_ok());
    }

    /// No builder order makes a document the deserializer refuses: a range set
    /// after the unit, which the unit does not fit, drops the unit.
    #[test]
    fn a_range_set_after_a_unit_it_does_not_fit_drops_the_unit() {
        let p = ParameterInfo::new(ParamId::new(4), "Cutoff")
            .with_plain_range(20.0, 20_000.0, 1_000.0)
            .with_measures(Some(ParamUnit::Hertz))
            .with_plain_range(-1.0, 1.0, 0.0);
        assert_eq!(p.measures(), None);
        let json = serde_json::to_string(&p).expect("serializes");
        let back: ParameterInfo = serde_json::from_str(&json).expect("what was built reads back");
        assert_eq!(back.measures(), None);
    }
}
