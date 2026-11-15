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
pub struct Reserved(u8);

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
pub struct ReservedAttribute {
    kind: u8,
    data: u16,
}

impl ReservedAttribute {
    pub(crate) const fn wire(kind: u8, data: u16) -> Self {
        Self { kind, data }
    }

    /// The attribute type, `0x04..=0xFF`.
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
pub struct ReservedAccents([u8; 3]);

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
