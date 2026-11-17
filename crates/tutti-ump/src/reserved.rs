//! A value a spec table reserves, as the decoder reads it.

/// A value a table of M2-104 reserves, read from the wire: a Flex Data address
/// of 2 or 3, a key letter of 8 to 15, a Function Block direction of 0, a
/// controller number Appendix A leaves open, and so on.
///
/// Only the decoder builds one, so a sender can name no reserved value
/// (M2-104 §2.1.3): every message a caller constructs encodes a defined
/// value. A message read from the wire keeps its reserved value and
/// re-encodes exactly, so a forwarder passes a later revision's message on
/// unchanged.
///
/// ```compile_fail,E0423
/// use tutti_ump::Reserved;
/// let _ = Reserved(2);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
pub struct Reserved(u8);

// Any `u8` is a reserved raw of some field (a value outside that field's
// table), so the serde form reads back through a conversion that cannot fail;
// `Field` and the decoder-shaped types check it against their own table.
#[cfg(feature = "serde")]
#[allow(
    clippy::infallible_try_from,
    reason = "no `From<raw>`: a value enters through `new` or `wire`; this is the serde form"
)]
impl TryFrom<u8> for Reserved {
    type Error = core::convert::Infallible;

    fn try_from(raw: u8) -> Result<Self, Self::Error> {
        Ok(Self(raw))
    }
}

#[cfg(feature = "serde")]
impl From<Reserved> for u8 {
    fn from(reserved: Reserved) -> u8 {
        reserved.0
    }
}

impl Reserved {
    pub(crate) const fn wire(raw: u8) -> Self {
        Self(raw)
    }

    /// The reserved value as the wire carried it.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// A Note On or Note Off attribute of a type that neither M2-104 (Table 8: `0x00`
/// to `0x03`) nor a Profile this crate types (M2-123's `0x10` to `0x1F`) defines,
/// read from the wire with its data. Only the decoder builds one.
///
/// ```compile_fail,E0451
/// use tutti_ump::ReservedAttribute;
/// let _ = ReservedAttribute { kind: 4, data: 0 };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "(u8, u16)", into = "(u8, u16)"))]
pub struct ReservedAttribute {
    kind: u8,
    data: u16,
}

// The serde form is the `(kind, data)` pair, read back only for a kind no
// table defines.
#[cfg(feature = "serde")]
impl TryFrom<(u8, u16)> for ReservedAttribute {
    type Error = &'static str;

    fn try_from((kind, data): (u8, u16)) -> Result<Self, Self::Error> {
        match kind {
            0x04..=0x0F | 0x20..=0xFF => Ok(Self { kind, data }),
            _ => Err("a reserved attribute type is 0x04 to 0x0F or 0x20 to 0xFF"),
        }
    }
}

#[cfg(feature = "serde")]
impl From<ReservedAttribute> for (u8, u16) {
    fn from(attribute: ReservedAttribute) -> Self {
        (attribute.kind, attribute.data)
    }
}

impl ReservedAttribute {
    pub(crate) const fn wire(kind: u8, data: u16) -> Self {
        Self { kind, data }
    }

    /// The attribute type: `0x04..=0x0F` or `0x20..=0xFF`.
    #[must_use]
    pub const fn kind(self) -> u8 {
        self.kind
    }

    /// The attribute data.
    #[must_use]
    pub const fn data(self) -> u16 {
        self.data
    }
}

/// The Bar Accent fields of a Set Metronome of a shape M2-104 §7.5.5 does not
/// have (Part 1 zero, or a part after a zero part), read from the wire. Only
/// the decoder builds one, so the rest of the message stays readable and
/// re-encodes exactly, and no sender writes the shape.
///
/// ```compile_fail,E0423
/// use tutti_ump::ReservedAccents;
/// let _ = ReservedAccents([3, 0, 2]);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "[u8; 3]", into = "[u8; 3]"))]
pub struct ReservedAccents([u8; 3]);

// The serde form is the three parts, read back only for a shape §7.5.5 does
// not have.
#[cfg(feature = "serde")]
impl TryFrom<[u8; 3]> for ReservedAccents {
    type Error = &'static str;

    fn try_from(parts: [u8; 3]) -> Result<Self, Self::Error> {
        match crate::BarAccents::from_parts(parts) {
            None => Ok(Self(parts)),
            Some(_) => Err("these accents are a shape §7.5.5 has, not a reserved one"),
        }
    }
}

#[cfg(feature = "serde")]
impl From<ReservedAccents> for [u8; 3] {
    fn from(accents: ReservedAccents) -> Self {
        accents.0
    }
}

impl ReservedAccents {
    pub(crate) const fn wire(parts: [u8; 3]) -> Self {
        Self(parts)
    }

    /// Part 1, 2 and 3 as the wire carried them.
    #[must_use]
    pub const fn parts(self) -> [u8; 3] {
        self.0
    }
}

/// A field a table bounds, as read from the wire: a value in range, or the
/// [`Reserved`] raw outside it. Only the decoder builds the second, so a sender
/// writes a value in range, and a message a receiver read re-encodes exactly.
///
/// ```compile_fail,E0423
/// use tutti_ump::{Field, FunctionBlockId, Reserved};
/// let _: Field<FunctionBlockId> = Field::Reserved(Reserved(40));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(
        try_from = "FieldRepr<T>",
        bound(deserialize = "T: serde::Deserialize<'de> + TryFrom<u8>")
    )
)]
pub enum Field<T> {
    /// A value in range.
    Value(T),
    /// A raw outside the range, as read.
    Reserved(Reserved),
}

impl<T: Copy> Field<T> {
    /// The value, or `None` for a reserved raw.
    #[must_use]
    pub const fn value(self) -> Option<T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Reserved(_) => None,
        }
    }

    /// The field of the checked `value`, or the reserved `raw` it came from.
    pub(crate) const fn read(value: Option<T>, raw: u8) -> Self {
        match value {
            Some(value) => Self::Value(value),
            None => Self::Reserved(Reserved::wire(raw)),
        }
    }

    /// The raw byte: `get` of the value, or the reserved raw.
    pub(crate) fn raw(self, get: impl FnOnce(T) -> u8) -> u8 {
        match self {
            Self::Value(value) => get(value),
            Self::Reserved(reserved) => reserved.get(),
        }
    }
}

impl<T> From<T> for Field<T> {
    fn from(value: T) -> Self {
        Self::Value(value)
    }
}

/// The serde form of a [`Field`], read back through the table's own check.
#[cfg(feature = "serde")]
#[derive(serde::Deserialize)]
enum FieldRepr<T> {
    Value(T),
    Reserved(Reserved),
}

#[cfg(feature = "serde")]
impl<T: TryFrom<u8>> TryFrom<FieldRepr<T>> for Field<T> {
    type Error = &'static str;

    fn try_from(repr: FieldRepr<T>) -> Result<Self, Self::Error> {
        match repr {
            FieldRepr::Value(value) => Ok(Self::Value(value)),
            FieldRepr::Reserved(reserved) if T::try_from(reserved.get()).is_err() => {
                Ok(Self::Reserved(reserved))
            }
            FieldRepr::Reserved(_) => Err("a reserved raw is outside the field's range"),
        }
    }
}
