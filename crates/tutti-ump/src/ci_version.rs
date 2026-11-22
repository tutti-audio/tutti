//! [`CiVersion`]: the MIDI-CI Message Format Version a message or a Function
//! Block declares.

tutti_units::ranged! {
    /// The Message Format Version byte (M2-101 §5.2.1, §5.3, §5.4): `0x01` to
    /// `0x1F`.
    ///
    /// Version 1.1 is 0x01 and version 1.2 is 0x02. Bits 7 to 5 are reserved for
    /// future major revisions, and a device that sees one set replies with a NAK
    /// (error code 0x02), so a value with one set is not a version a message
    /// carries; Message Format Version 0x00 is deprecated (§5.4). A Function Block
    /// that declares no MIDI-CI is `None` where a `CiVersion` is optional
    /// (M2-104 §7.1.8: `0` is none).
    #[derive(Eq, PartialOrd, Ord, Hash)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
    CiVersion(u8), NotAVersion, "a MIDI-CI message format version (0x01 to 0x1F)",
    |raw| raw != 0 && raw & 0xE0 == 0
}

use crate::Field;

/// The version a Function Block Info byte reads as: `None` for `0` (no MIDI-CI),
/// the version when it is one, else the reserved raw as read.
pub(crate) fn read(raw: u8) -> Option<Field<CiVersion>> {
    (raw != 0).then(|| Field::from_raw(raw))
}

/// The Function Block Info byte of `version`.
pub(crate) fn raw(version: Option<Field<CiVersion>>) -> u8 {
    version.map_or(0, |field| field.raw(CiVersion::get))
}
