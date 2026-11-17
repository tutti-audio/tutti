//! The two range-checked numbers of a [`Meta`](crate::Meta): the wire's 24-bit
//! tempo and the type byte of an untyped meta event.

tutti_types::ranged! {
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

tutti_types::ranged! {
    /// The type byte of a [`Meta::Other`](crate::Meta::Other): any of `0..=127`
    /// that [`Meta`](crate::Meta) has no variant for. A typed kind (the text
    /// events `01` to `09`, `2F`, `51`, `58`, `59`) is refused, so an `Other` is
    /// never read back as the variant of its kind, and a type byte has no top
    /// bit (SMF 1.0 section 3.3).
    #[derive(Eq, Hash, PartialOrd, Ord)]
    OtherMetaKind(u8), OtherMetaKindOutOfRange, "a meta type byte below 128 that has no Meta variant", |raw| raw < 0x80 && !is_typed(raw)
}
