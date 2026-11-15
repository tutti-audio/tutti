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

/// A Note On or Note Off attribute of a reserved type (`0x04` to `0xFF`,
/// Table 8), read from the wire with its data. Only the decoder builds one.
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
