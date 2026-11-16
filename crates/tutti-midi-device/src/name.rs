//! The bounded text of a UMP Stream: Endpoint Name, Product Instance Id and
//! Function Block Name, held without allocating.

use std::fmt;
use std::str::FromStr;

use unicode_normalization::{UnicodeNormalization, is_nfc};

/// Why a text is not a name ([`Name::parse`], [`ProductInstanceId`] and
/// [`MessageText`](crate::MessageText)'s `TryFrom<&str>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum NameError {
    /// The text is longer than `max` bytes (for a name, composed to NFC; for a
    /// [`MessageText`](crate::MessageText), escaped as it goes on the wire).
    #[error("the text is longer than {max} bytes")]
    TooLong {
        /// The most bytes the type holds.
        max: usize,
    },
    /// A name holds a NUL, which ends one on the wire.
    #[error("the text holds a NUL, which ends a name on the wire")]
    Nul,
    /// A name starts with a Byte Order Mark (M2-104 §1.8).
    #[error("the text starts with a Byte Order Mark")]
    Bom,
    /// A [`ProductInstanceId`] holds a byte outside ASCII `32..=126` (other
    /// than the NUL that [`Nul`](Self::Nul) names).
    #[error("the text holds a byte outside printable ASCII (32 to 126)")]
    NotPrintableAscii,
}

/// A UTF-8 name of at most `N` bytes with no NUL, held inline.
///
/// A NUL ends a name on the wire (M2-104 §7.1.4: unused bytes of the last
/// packet are `0x00`), so a name cannot contain one. [`EndpointName`] and
/// [`BlockName`] are the two sizes.
///
/// M2-104 §1.8 (Unicode in Message Fields) has MIDI messages carry only
/// NFC-normalized UTF-8 text, with no Byte Order Mark. So a name is always held in NFC and never starts with U+FEFF:
/// [`new`](Self::new) composes the text it is given and refuses a BOM, and a
/// name a peer sent is read the same way (a BOM is refused, text that is not
/// NFC is composed: §1.8 states a rule for what is sent and none for a
/// receiver, so a receiver that composes keeps the name it displays and compares
/// one thing, where one that refuses would lose a name a sender spelled
/// loosely). The length bound is on the composed bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Name<const N: usize> {
    bytes: [u8; N],
    len: u8,
}

/// An Endpoint Name: at most 98 bytes, a Start, five Continue and an End
/// packet of 14 bytes (M2-104 §7.1.4).
pub type EndpointName = Name<98>;

/// A Function Block Name: at most 91 bytes.
///
/// M2-104 §7.1.9 bounds the name at 91 bytes, whether it is one Complete
/// message or a Start, up to five Continue messages and an End. A Function
/// Block Name packet carries 13 bytes, and 7 x 13 = 91, one less than an
/// Endpoint Name's 98 (7 x 14).
pub type BlockName = Name<91>;

impl<const N: usize> Name<N> {
    /// The name, composed to NFC, or `None` when the composed text is longer
    /// than `N` bytes, holds a NUL or starts with a Byte Order Mark (§1.8).
    /// [`parse`](Self::parse) says which.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        Self::parse(text).ok()
    }

    /// The name, composed to NFC, or why the text is not one.
    ///
    /// # Errors
    ///
    /// [`NameError::Bom`] for a leading Byte Order Mark, [`NameError::Nul`] for
    /// a NUL, [`NameError::TooLong`] when the composed text is longer than `N`
    /// bytes.
    pub fn parse(text: &str) -> Result<Self, NameError> {
        if text.starts_with('\u{FEFF}') {
            return Err(NameError::Bom);
        }
        // Composed only when it is not NFC already, so the common name does not
        // allocate.
        let composed;
        let bytes = if is_nfc(text) {
            text.as_bytes()
        } else {
            composed = text.nfc().collect::<String>();
            composed.as_bytes()
        };
        if bytes.contains(&0) {
            return Err(NameError::Nul);
        }
        let too_long = NameError::TooLong { max: N };
        if bytes.len() > N {
            return Err(too_long);
        }
        let mut held = [0; N];
        held[..bytes.len()].copy_from_slice(bytes);
        Ok(Self {
            bytes: held,
            len: u8::try_from(bytes.len()).map_err(|_| too_long)?,
        })
    }

    /// A name read off the wire: UTF-8 bytes, read as [`parse`](Self::parse)
    /// reads a text.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        Self::parse(std::str::from_utf8(bytes).ok()?).ok()
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Checked UTF-8 in `from_bytes`, the only constructor.
        std::str::from_utf8(self.as_bytes()).unwrap_or("")
    }

    /// The text's bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// Whether the name is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> fmt::Debug for Name<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl<const N: usize> fmt::Display for Name<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<const N: usize> AsRef<str> for Name<N> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl<const N: usize> TryFrom<&str> for Name<N> {
    type Error = NameError;

    fn try_from(text: &str) -> Result<Self, NameError> {
        Self::parse(text)
    }
}

impl<const N: usize> FromStr for Name<N> {
    type Err = NameError;

    fn from_str(text: &str) -> Result<Self, NameError> {
        Self::parse(text)
    }
}

/// A Product Instance Id: ASCII `32..=126`, at most 42 bytes, a Start, one
/// Continue and an End packet of 14 bytes (M2-104 §7.1.5).
///
/// With the `serde` feature it is its text, read back through
/// [`parse`](Self::parse).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "String", into = "String"))]
pub struct ProductInstanceId(Name<42>);

impl From<ProductInstanceId> for String {
    fn from(id: ProductInstanceId) -> Self {
        id.as_str().to_owned()
    }
}

impl TryFrom<String> for ProductInstanceId {
    type Error = NameError;

    fn try_from(text: String) -> Result<Self, NameError> {
        Self::parse(&text)
    }
}

impl ProductInstanceId {
    /// The Id, or `None` when it is longer than 42 bytes or holds a byte
    /// outside `32..=126`. [`parse`](Self::parse) says which.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        Self::parse(text).ok()
    }

    /// The Id, or why the text is not one.
    ///
    /// # Errors
    ///
    /// [`NameError::Nul`] for a NUL, [`NameError::NotPrintableAscii`] for any
    /// other byte outside `32..=126`, [`NameError::TooLong`] past 42 bytes.
    pub fn parse(text: &str) -> Result<Self, NameError> {
        if text.bytes().any(|b| b == 0) {
            return Err(NameError::Nul);
        }
        if !text.bytes().all(|b| (32..=126).contains(&b)) {
            return Err(NameError::NotPrintableAscii);
        }
        Name::parse(text).map(Self)
    }

    /// An Id for a Device that has no serial number to give (§7.1.5 "should
    /// declare a Product Instance Id"): the 128 `random` bits (from
    /// [`random_bytes`](crate::random_bytes)) as 32 lowercase hex digits, so it
    /// is unique per Manufacturer, Family and Model for every practical purpose.
    ///
    /// The Id re-identifies a Device after power cycling only if the Device
    /// keeps it: the owner stores the Id (it is serde with the `serde`
    /// feature) and declares the stored one next time, making one only when
    /// there is none stored.
    #[must_use]
    pub fn from_random(random: [u8; 16]) -> Self {
        Self::hex(u128::from_be_bytes(random))
    }

    fn hex(bits: u128) -> Self {
        Self::new(&format!("{bits:032x}"))
            .expect("32 hex digits are printable ASCII and at most 42 bytes")
    }

    pub(crate) fn from_bytes(bytes: &[u8]) -> Option<Self> {
        Self::parse(std::str::from_utf8(bytes).ok()?).ok()
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for ProductInstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for ProductInstanceId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<&str> for ProductInstanceId {
    type Error = NameError;

    fn try_from(text: &str) -> Result<Self, NameError> {
        Self::parse(text)
    }
}

impl FromStr for ProductInstanceId {
    type Err = NameError;

    fn from_str(text: &str) -> Result<Self, NameError> {
        Self::parse(text)
    }
}

#[cfg(test)]
mod tests;
