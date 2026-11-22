//! The pieces the Property Data of the standard Resources are made of: ranged
//! numbers, limited text, the closed sets of their enumerated properties, the
//! manufacturer-specific properties, and the reader of an object's properties.

use std::fmt;
use std::num::TryFromIntError;

use tutti_ump::{Bank, MidiChannelOutOfRange, U7};

use super::super::limits::{PeTextError, PropertyName, PropertyNameError, text_type};
use super::super::tree::{PeJson, PeJsonError, PeObject};
use super::controllers::ListedTwice;
use super::entry::EntryError;
use super::kind::PeResource;
use super::state::NotUnixSeconds;

tutti_units::ranged! {
    /// A MIDI Channel of a Function Block: 1 to 256, counted from the first
    /// Channel of the first Group of the block and on across its Groups
    /// (M2-105 Table 10, M2-117 Table 5).
    #[derive(Eq, Hash, PartialOrd, Ord)]
    PeChannel(u16), NotAPeChannel, "a channel of a function block (1 to 256)",
    |raw| raw >= 1 && raw <= 256
}

tutti_units::ranged! {
    /// The priority of a controller: 1, the most important, to 5, hidden
    /// (M2-117 §2.1.1.1).
    #[derive(Eq, Hash, PartialOrd, Ord)]
    Priority(u8), NotAPriority, "a priority (1 to 5)", |raw| raw >= 1 && raw <= 5
}

tutti_units::ranged! {
    /// The significant bits of a 32-bit controller value: 1 to 32 (M2-117
    /// Table 5).
    #[derive(Eq, Hash, PartialOrd, Ord)]
    SignificantBits(u8), NotSignificantBits, "a count of significant bits (1 to 32)",
    |raw| raw >= 1 && raw <= 32
}

tutti_units::ranged! {
    /// The number of steps of a stepped controller (M2-117 Table 5): at least 1.
    #[derive(Eq, Hash, PartialOrd, Ord)]
    StepCount(u32), NotAStepCount, "a step count (at least 1)", |raw| raw >= 1
}

// Owned: the type has no bound, or counts characters, which no byte-bounded
// `Text` expresses.
text_type! {
    /// A string a Resource requires: not empty (M2-103 §8.3).
    PeText, String, PeTextError,
    |t| if t.is_empty() { Err(PeTextError::Empty) } else { Ok(()) }
}

text_type! {
    /// The identifier of a Mode: 1 to 36 characters (M2-106 §2.2).
    ModeId, String, PeTextError,
    |t| chars_between(t, 36)
}

text_type! {
    /// The name of a Link role this crate has no type for: 1 to 32 characters
    /// (M2-103 Table 64).
    RoleName, String, PeTextError,
    |t| chars_between(t, 32)
}

/// `text` of 1 to `max` characters.
fn chars_between(text: &str, max: usize) -> Result<(), PeTextError> {
    match text.chars().count() {
        0 => Err(PeTextError::Empty),
        n if n > max => Err(PeTextError::TooManyChars { max }),
        _ => Ok(()),
    }
}

/// A JSON Pointer that names a parameter: at most 256 bytes (M2-117 Table 5).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ParamPath(super::super::partial::JsonPointer);

impl ParamPath {
    /// The most bytes a path takes, 256.
    pub const MAX_LEN: usize = 256;

    /// The path `text` spells, or `None` unless it is a JSON Pointer of at most
    /// [`MAX_LEN`](Self::MAX_LEN) bytes.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        if text.len() > Self::MAX_LEN {
            return None;
        }
        super::super::partial::JsonPointer::new(text).map(Self)
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for ParamPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A Bank Select and Program Change, as the `bankPC` property carries them: the
/// three 7-bit values, 0-based (M2-105 Table 10, M2-107 §2.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeBankProgram {
    /// The Bank MSB and LSB.
    pub bank: Bank,
    /// The Program Change number.
    pub program: U7,
}

impl PeBankProgram {
    pub(super) fn to_json(self) -> PeJson {
        ints(&[self.bank.msb.get(), self.bank.lsb.get(), self.program.get()])
    }

    pub(super) fn from_json(json: &PeJson) -> Option<Self> {
        let [msb, lsb, program] = u7_array::<3>(json)?;
        Some(Self {
            bank: Bank { msb, lsb },
            program,
        })
    }
}

/// A MIDI mode: Omni on or off, Poly or Mono (M2-108 §2.2). The numbers are
/// those of the Complete MIDI 1.0 Detailed Specification's modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PeMidiMode {
    /// 1: Omni On, Poly.
    OmniOnPoly,
    /// 2: Omni On, Mono.
    OmniOnMono,
    /// 3: Omni Off, Poly.
    OmniOffPoly,
    /// 4: Omni Off, Mono.
    OmniOffMono,
}

impl PeMidiMode {
    /// The number the property carries, 1 to 4.
    #[must_use]
    pub const fn number(self) -> u8 {
        match self {
            Self::OmniOnPoly => 1,
            Self::OmniOnMono => 2,
            Self::OmniOffPoly => 3,
            Self::OmniOffMono => 4,
        }
    }

    /// The mode a number names, or `None` outside 1 to 4.
    #[must_use]
    pub const fn new(number: u8) -> Option<Self> {
        match number {
            1 => Some(Self::OmniOnPoly),
            2 => Some(Self::OmniOnMono),
            3 => Some(Self::OmniOffPoly),
            4 => Some(Self::OmniOffMono),
            _ => None,
        }
    }
}

/// A raw number that is not a MIDI mode: outside 1 to 4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a MIDI mode (1 to 4)")]
pub struct NotAPeMidiMode(pub i64);

impl TryFrom<i64> for PeMidiMode {
    type Error = NotAPeMidiMode;

    fn try_from(number: i64) -> Result<Self, NotAPeMidiMode> {
        u8::try_from(number)
            .ok()
            .and_then(Self::new)
            .ok_or(NotAPeMidiMode(number))
    }
}

/// Text that is none of the values a closed set of property values names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
#[error("not one of {}", .allowed.join(", "))]
pub struct UnlistedText {
    /// The values the property has.
    pub allowed: &'static [&'static str],
}

macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            /// The property value.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            /// The variant a property value names, or `None` for other text.
            #[must_use]
            pub fn new(text: &str) -> Option<Self> {
                Self::try_from(text).ok()
            }
        }

        impl TryFrom<&str> for $name {
            type Error = UnlistedText;

            fn try_from(text: &str) -> Result<Self, UnlistedText> {
                match text {
                    $($text => Ok(Self::$variant),)+
                    _ => Err(UnlistedText { allowed: &[$($text),+] }),
                }
            }
        }
    };
}

text_enum! {
    /// The kind of a Cluster of Channels (M2-105 Table 10).
    ClusterKind {
        /// `other`: any Cluster.
        Other = "other",
        /// `profile`: a Cluster of a MIDI-CI Profile, MPE's included.
        Profile = "profile",
        /// `mpe1`: an MPE zone of MPE 1.x, not the Profile.
        Mpe1 = "mpe1",
    }
}

text_enum! {
    /// The kind of a controller message (M2-117 Table 5).
    CtrlKind {
        /// `cc`: Control Change.
        ControlChange = "cc",
        /// `chPress`: Channel Pressure.
        ChannelPressure = "chPress",
        /// `pPress`: Poly Pressure.
        PolyPressure = "pPress",
        /// `nrpn`: an Assignable Controller.
        Nrpn = "nrpn",
        /// `rpn`: a Registered Controller.
        Rpn = "rpn",
        /// `pBend`: Pitch Bend.
        PitchBend = "pBend",
        /// `pnrc`: a Per-Note Registered Controller (MIDI 2.0 only).
        PerNoteRegistered = "pnrc",
        /// `pnac`: a Per-Note Assignable Controller (MIDI 2.0 only).
        PerNoteAssignable = "pnac",
        /// `pnp`: Per-Note Pitch Bend (MIDI 2.0 only).
        PerNotePitchBend = "pnp",
    }
}

text_enum! {
    /// The kind of value a controller sends or recognizes (M2-117 Table 5).
    CtrlValueKind {
        /// `absolute`: absolute values, the default.
        Absolute = "absolute",
        /// `relative`: relative values.
        Relative = "relative",
        /// `both`: absolute and relative values.
        Both = "both",
        /// `none`: no value.
        None = "none",
    }
}

text_enum! {
    /// The control that suits a controller, except `valueSelect`, which names a
    /// map and so is a [`CtrlControl`](super::CtrlControl) of its own (M2-117
    /// §2.1.1.2).
    TypeHint {
        /// `continuous`: a contiguous set of values.
        Continuous = "continuous",
        /// `momentary`: a binary switch that follows the control.
        Momentary = "momentary",
        /// `toggle`: a binary switch that alternates.
        Toggle = "toggle",
        /// `relative`: relative values.
        Relative = "relative",
    }
}

/// An array of integers as JSON.
pub(super) fn ints(values: &[u8]) -> PeJson {
    PeJson::Array(
        values
            .iter()
            .map(|&n| PeJson::Integer(i64::from(n)))
            .collect(),
    )
}

/// The integer `n` as the `T` of raw width `R`.
pub(super) fn narrowed<R, T>(n: i64) -> Result<T, InvalidValue>
where
    R: TryFrom<i64, Error = TryFromIntError>,
    T: TryFrom<R>,
    InvalidValue: From<T::Error>,
{
    Ok(T::try_from(R::try_from(n)?)?)
}

/// An array of `N` 7-bit integers.
pub(super) fn u7_array<const N: usize>(json: &PeJson) -> Option<[U7; N]> {
    let items = json.as_array()?;
    if items.len() != N {
        return None;
    }
    let mut out = [U7::of(0); N];
    for (slot, item) in out.iter_mut().zip(items) {
        let PeJson::Integer(n) = item else {
            return None;
        };
        *slot = U7::new(u8::try_from(*n).ok()?)?;
    }
    Some(out)
}

/// A property that cannot be read from Property Data, or a value a Resource
/// refuses to carry.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{place}: {fault}")]
pub struct PeDataError {
    /// Where the fault is.
    pub place: Place,
    /// What is wrong with it.
    pub fault: PeDataFault,
}

/// What is wrong with a property of Property Data.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PeDataFault {
    /// The property is required and absent, or empty where the spec requires it
    /// not to be (M2-103 §8.3).
    #[error("required and missing")]
    Missing,
    /// The value is not of the type the property has.
    #[error("not {0}")]
    WrongType(&'static str),
    /// The value is of the type and outside the range or form the spec allows.
    #[error("not valid: {0}")]
    NotValid(InvalidValue),
    /// Two properties the spec does not allow together.
    #[error("not allowed with {0}")]
    Conflicts(&'static str),
    /// The entry of a list is not an object.
    #[error("not an object")]
    NotAnObject,
    /// The Property Data is another Resource's.
    #[error("the Property Data is of {0}")]
    OfResource(PeResource),
}

/// Why a value of the right JSON type is not valid: the error of the type that
/// refuses it, or the form of the spec that it breaks where no type holds it.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum InvalidValue {
    /// A text that breaks the limit of its text type.
    #[error(transparent)]
    Text(#[from] PeTextError),
    /// A text that is no property name.
    #[error(transparent)]
    PropertyName(#[from] PropertyNameError),
    /// A text that none of the values of a closed set.
    #[error(transparent)]
    Unlisted(#[from] UnlistedText),
    /// A number that is no Channel of a Function Block.
    #[error(transparent)]
    FunctionBlockChannel(#[from] NotAPeChannel),
    /// A number that is no MIDI Channel.
    #[error(transparent)]
    MidiChannel(#[from] MidiChannelOutOfRange),
    /// A number that is no MIDI mode.
    #[error(transparent)]
    MidiMode(#[from] NotAPeMidiMode),
    /// A number that is no controller priority.
    #[error(transparent)]
    Priority(#[from] NotAPriority),
    /// A number that is no count of significant bits.
    #[error(transparent)]
    SignificantBits(#[from] NotSignificantBits),
    /// A number that is no step count.
    #[error(transparent)]
    StepCount(#[from] NotAStepCount),
    /// A number that is no time in Unix seconds.
    #[error(transparent)]
    UnixSeconds(#[from] NotUnixSeconds),
    /// An integer outside the width of the value it is read as.
    #[error("the integer does not fit the width of the value")]
    Width(#[from] TryFromIntError),
    /// Text that is not JSON of the Property Exchange form.
    #[error(transparent)]
    Json(#[from] PeJsonError),
    /// A ResourceList entry the rules of Property Exchange refuse.
    #[error(transparent)]
    Entry(#[from] EntryError),
    /// A controller list with a controller listed twice.
    #[error(transparent)]
    ListedTwice(#[from] ListedTwice),
    /// Three bytes that are no Manufacturer id: a one-byte id followed by a
    /// non-zero byte, or `00 00 00` (M2-104 §7.1.3).
    #[error("the bytes are no Manufacturer id")]
    ManufacturerId,
    /// A `ctrlIndex` that does not fit the `ctrlType` (M2-117 Table 5).
    #[error("the index does not fit the controller type")]
    ControllerIndex,
    /// A `paramPath` that is no JSON Pointer of at most 256 bytes (M2-117
    /// Table 5).
    #[error("the path is no JSON Pointer of at most 256 bytes")]
    ParamPath,
    /// A `minMax` whose minimum is above its maximum (M2-117 Table 5).
    #[error("the minimum is above the maximum")]
    MinAboveMax,
    /// A `defaultCCMap` of a controller that is not a Control Change (M2-117
    /// Table 20).
    #[error("only a Control Change takes the flag")]
    NotControlChange,
    /// The options Resource of a `selectorAction` Link, which is no List Resource
    /// (M2-103 §13.1.2).
    #[error("the options Resource of a selector is a List Resource")]
    NotAListResource,
    /// An `x-` property a list object cannot hold.
    #[error(transparent)]
    Extension(#[from] ExtensionError),
    /// Data of a Resource that has a JSON form, held in none.
    #[error("the data has no JSON form")]
    NoJsonForm,
    /// Data of a Resource the spec defines, where only a Resource without a
    /// definition holds data of any form.
    #[error("the Resource is defined, so its data has a type")]
    ResourceIsTyped,
}

/// Where in Property Data a fault is: what holds it (a Resource, or an entry of
/// one), and the property of it when one is at fault, so the whole Property
/// Data is the fault when there is none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    what: String,
    property: Option<String>,
}

impl Place {
    /// The Resource, or the entry of one, as a whole.
    #[must_use]
    pub fn of(what: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            property: None,
        }
    }

    /// The property `name` of it.
    #[must_use]
    pub fn property(mut self, name: impl Into<String>) -> Self {
        self.property = Some(name.into());
        self
    }

    /// What holds the fault: a Resource, or an entry of one.
    #[must_use]
    pub fn what(&self) -> &str {
        &self.what
    }

    /// The property at fault, or `None` when the whole of [`what`](Self::what) is.
    #[must_use]
    pub fn at_property(&self) -> Option<&str> {
        self.property.as_deref()
    }
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.property {
            Some(property) => write!(f, "{}.{property}", self.what),
            None => f.write_str(&self.what),
        }
    }
}

impl From<&str> for Place {
    fn from(what: &str) -> Self {
        Self::of(what)
    }
}

impl PeDataError {
    pub(super) fn new(place: impl Into<Place>, fault: PeDataFault) -> Self {
        Self {
            place: place.into(),
            fault,
        }
    }
}

/// The manufacturer-specific properties of an object: names that start `x-`
/// (M2-103 §6.1.4), kept and written back.
///
/// On the object of a List Resource a value is a plain value, or an array of plain
/// values (null, a boolean, a number or a string), never an object or a nested array
/// (§8.6.1); `Default` is that form. DeviceInfo's has any JSON value.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PeExtensions {
    items: Vec<(PropertyName, PeJson)>,
    nested: bool,
}

/// A property of a list object that cannot be an extension.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ExtensionError {
    /// A name that does not start with `x-`, so is not manufacturer-specific.
    #[error("{0:?} does not start with \"x-\"")]
    NotVendorName(String),
    /// A value of a list object that is an object or holds one, or an array that
    /// holds an array (M2-103 §8.6.1).
    #[error("a list object holds plain values and arrays of them, not objects")]
    Nested,
}

impl ExtensionError {
    fn check(value: &PeJson) -> Result<(), Self> {
        let plain = |j: &PeJson| j.is_scalar() || matches!(j, PeJson::Null);
        let flat = plain(value)
            || value
                .as_array()
                .is_some_and(|items| items.iter().all(plain));
        if flat { Ok(()) } else { Err(Self::Nested) }
    }
}

impl PeExtensions {
    /// Adds a property, replacing the value of one with the same name.
    ///
    /// # Errors
    ///
    /// [`ExtensionError::NotVendorName`] unless the name starts `x-`, and
    /// [`ExtensionError::Nested`] for a value a list object does not hold.
    pub fn insert(&mut self, name: PropertyName, value: PeJson) -> Result<(), ExtensionError> {
        if !name.as_str().starts_with("x-") {
            return Err(ExtensionError::NotVendorName(name.into()));
        }
        if !self.nested {
            ExtensionError::check(&value)?;
        }
        match self.items.iter_mut().find(|(n, _)| *n == name) {
            Some((_, old)) => *old = value,
            None => self.items.push((name, value)),
        }
        Ok(())
    }

    /// The value of the property `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&PeJson> {
        self.items
            .iter()
            .find(|(n, _)| n.as_str() == name)
            .map(|(_, v)| v)
    }

    /// The properties, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&PropertyName, &PeJson)> {
        self.items.iter().map(|(n, v)| (n, v))
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub(super) fn write(&self, object: &mut PeObject) {
        for (name, value) in &self.items {
            object.insert(name.as_str(), value.clone());
        }
    }
}

/// The properties of one object being read: each is taken once, and what is
/// left is the manufacturer-specific ones (or a name that is none).
pub(super) struct Fields<'a> {
    what: &'static str,
    left: Vec<(&'a str, &'a PeJson)>,
}

impl<'a> Fields<'a> {
    pub(super) fn new(json: &'a PeJson, what: &'static str) -> Result<Self, PeDataError> {
        match json {
            PeJson::Object(object) => Ok(Self {
                what,
                left: object.iter().collect(),
            }),
            _ => Err(PeDataError::new(what, PeDataFault::NotAnObject)),
        }
    }

    pub(super) fn fail(&self, name: &str, fault: PeDataFault) -> PeDataError {
        PeDataError::new(Place::of(self.what).property(name), fault)
    }

    /// The fault of the property `name` whose value is invalid for `why`.
    pub(super) fn invalid(&self, name: &str, why: impl Into<InvalidValue>) -> PeDataError {
        self.fail(name, PeDataFault::NotValid(why.into()))
    }

    fn take(&mut self, name: &str) -> Option<&'a PeJson> {
        let at = self.left.iter().position(|(n, _)| *n == name)?;
        Some(self.left.remove(at).1)
    }

    /// The property `name` read by `read`, or `None` when it is absent.
    ///
    /// # Errors
    ///
    /// The property is there and `read` refuses it, as `expected`.
    pub(super) fn optional<T>(
        &mut self,
        name: &'static str,
        expected: &'static str,
        read: impl FnOnce(&'a PeJson) -> Option<T>,
    ) -> Result<Option<T>, PeDataError> {
        match self.take(name) {
            None => Ok(None),
            Some(json) => read(json)
                .map(Some)
                .ok_or_else(|| self.fail(name, PeDataFault::WrongType(expected))),
        }
    }

    /// The property `name`, which the object must have.
    pub(super) fn required<T>(
        &mut self,
        name: &'static str,
        expected: &'static str,
        read: impl FnOnce(&'a PeJson) -> Option<T>,
    ) -> Result<T, PeDataError> {
        self.optional(name, expected, read)?
            .ok_or_else(|| self.fail(name, PeDataFault::Missing))
    }

    /// The remaining properties: those that start `x-` are the manufacturer's;
    /// another that is a valid property name is a later version's and is
    /// dropped.
    ///
    /// # Errors
    ///
    /// [`PeDataFault::NotValid`] for a name that is no property name.
    pub(super) fn finish(self) -> Result<PeExtensions, PeDataError> {
        self.finish_as(PeExtensions::default())
    }

    /// [`finish`](Self::finish) for an object that is no list object: its
    /// extensions hold any JSON value.
    pub(super) fn finish_unrestricted(self) -> Result<PeExtensions, PeDataError> {
        self.finish_as(PeExtensions {
            items: Vec::new(),
            nested: true,
        })
    }

    fn finish_as(self, mut extensions: PeExtensions) -> Result<PeExtensions, PeDataError> {
        for (name, value) in self.left {
            let valid = PropertyName::try_from(name).map_err(|e| {
                PeDataError::new(
                    Place::of(self.what).property(name),
                    PeDataFault::NotValid(e.into()),
                )
            })?;
            if valid.as_str().starts_with("x-") {
                let place = Place::of(self.what).property(name);
                extensions.insert(valid, value.clone()).map_err(|e| {
                    PeDataError::new(place, PeDataFault::NotValid(InvalidValue::Extension(e)))
                })?;
            }
        }
        Ok(extensions)
    }
}

pub(super) fn text(json: &PeJson) -> Option<&str> {
    json.as_str()
}

pub(super) fn boolean(json: &PeJson) -> Option<bool> {
    match json {
        PeJson::Bool(b) => Some(*b),
        _ => None,
    }
}

pub(super) fn integer(json: &PeJson) -> Option<i64> {
    match json {
        PeJson::Integer(n) => Some(*n),
        _ => None,
    }
}

pub(super) fn unsigned32(json: &PeJson) -> Option<u32> {
    u32::try_from(integer(json)?).ok()
}

/// A required, non-empty text property.
pub(super) fn required_text(
    fields: &mut Fields<'_>,
    name: &'static str,
) -> Result<PeText, PeDataError> {
    let text = fields.required(name, "a string", |j| text(j).map(str::to_owned))?;
    PeText::new(&text).ok_or_else(|| fields.fail(name, PeDataFault::Missing))
}

/// A property that is an array of `T` read by `read`, each element or none.
pub(super) fn list<T>(
    fields: &mut Fields<'_>,
    name: &'static str,
    read: impl Fn(&PeJson) -> Result<T, PeDataFault>,
) -> Result<Vec<T>, PeDataError> {
    let Some(items) =
        fields.optional(name, "an array", |j| j.as_array().map(<[PeJson]>::to_vec))?
    else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .map(|item| read(item).map_err(|fault| fields.fail(name, fault)))
        .collect()
}

/// The unsigned 32-bit integer of a JSON value.
pub(super) fn u32_json(value: u32) -> PeJson {
    PeJson::Integer(i64::from(value))
}
