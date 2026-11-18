//! The parts of a [`Meta`](crate::Meta) that are not a plain field: the wire's
//! 24-bit tempo, the type byte of an untyped meta event, and a meta event the
//! reader could not type.

tutti_units::ranged! {
    /// Microseconds per quarter note as a Set Tempo holds it: 24 bits,
    /// `0..=0xFF_FFFF` (SMF 1.0 section 3.3, `FF 51 03 tttttt`). Zero is no
    /// valid tempo and is kept so a file writes back as it was read.
    #[derive(Eq, Hash, PartialOrd, Ord)]
    MicrosPerQuarter(u32), MicrosPerQuarterOutOfRange, "within 0..=16777215", |raw| raw <= 0xFF_FFFF
}

impl MicrosPerQuarter {
    /// The value of a Set Tempo's three data bytes, most significant first.
    pub(crate) const fn from_wire(bytes: [u8; 3]) -> Self {
        Self(u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]))
    }

    /// The three data bytes of a Set Tempo, most significant first.
    pub(crate) const fn to_wire(self) -> [u8; 3] {
        let [_, a, b, c] = self.0.to_be_bytes();
        [a, b, c]
    }
}

/// Whether `kind` is a meta event type the model types: the text events `01`
/// to `09`, End of Track `2F` (a track's `end`), Set Tempo `51`, Time
/// Signature `58` and Key Signature `59`.
const fn is_typed(kind: u8) -> bool {
    matches!(kind, 0x01..=0x09 | 0x2F | 0x51 | 0x58 | 0x59)
}

tutti_units::ranged! {
    /// The type byte of a [`Meta::Other`](crate::Meta::Other): any of `0..=127`
    /// that [`Meta`](crate::Meta) has no variant for. A typed kind (the text
    /// events `01` to `09`, `2F`, `51`, `58`, `59`) is refused, so an `Other` is
    /// never read back as the variant of its kind, and a type byte has no top
    /// bit (SMF 1.0 section 3.3).
    #[derive(Eq, Hash, PartialOrd, Ord)]
    OtherMetaKind(u8), OtherMetaKindOutOfRange, "a meta type byte below 128 that has no Meta variant", |raw| raw < 0x80 && !is_typed(raw)
}

/// A meta event whose type the model types but whose data is not what the type
/// defines: a Set Tempo, Time Signature or Key Signature shorter than its
/// fixed part, a Key Signature whose mode is neither 0 nor 1, an End of Track
/// with data (SMF 1.0 section 3.3), or a type byte with its top bit set.
///
/// A lenient reader keeps it: the file's other events are read, and a write
/// gives the event's bytes back exactly. Only a read makes one, so a
/// `MalformedMeta` is never one that reads back as a typed event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MalformedMeta {
    kind: u8,
    data: Box<[u8]>,
}

impl MalformedMeta {
    /// The event the reader found: its type byte and data.
    pub(crate) fn new(kind: u8, data: &[u8]) -> Self {
        Self {
            kind,
            data: data.into(),
        }
    }

    /// The type byte as the file has it.
    #[must_use]
    pub const fn kind(&self) -> u8 {
        self.kind
    }

    /// The data as the file has it, without its length.
    #[must_use]
    pub fn data(&self) -> &[u8] {
        &self.data
    }
}
