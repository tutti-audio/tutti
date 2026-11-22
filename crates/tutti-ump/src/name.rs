//! The bounded text of a UMP Stream: Endpoint Name, Product Instance Id and
//! Function Block Name, held without allocating.

use alloc::borrow::{Cow, ToOwned};
use alloc::format;
use alloc::string::String;
use core::fmt;
use core::str::FromStr;

use tutti_units::{Text, TextError};
use unicode_normalization::{UnicodeNormalization, is_nfc};

/// Why a text is not a name ([`Name`] reads through [`FromStr`]; a MIDI-CI
/// message text reads through the same error).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum NameError {
    /// The text is longer than `max` bytes (for a name, composed to NFC; for a
    /// MIDI-CI message text, escaped as it goes on the wire).
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
}

/// Why a text is not a [`ProductInstanceId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum InstanceIdError {
    /// The text is longer than `max` bytes.
    #[error("the text is longer than {max} bytes")]
    TooLong {
        /// The most bytes an Id holds.
        max: usize,
    },
    /// The text holds a character outside ASCII `32..=126` (a NUL included).
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
///
/// With the `serde` feature it is its text, read back through [`FromStr`].
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "String", into = "String"))]
pub struct Name<const N: usize>(Text<0, N>);

/// The bytes of an Endpoint Name (M2-104 §7.1.4), which is also the capacity
/// of the assembler that reads one.
pub const ENDPOINT_NAME_BYTES: usize = 98;

/// The bytes of a Function Block Name (M2-104 §7.1.9).
pub const BLOCK_NAME_BYTES: usize = 91;

/// The bytes of a Product Instance Id (M2-104 §7.1.5).
pub const INSTANCE_ID_BYTES: usize = 42;

/// An Endpoint Name: at most 98 bytes, a Start, five Continue and an End
/// packet of 14 bytes (M2-104 §7.1.4).
pub type EndpointName = Name<ENDPOINT_NAME_BYTES>;

/// A Function Block Name: at most 91 bytes.
///
/// M2-104 §7.1.9 bounds the name at 91 bytes, whether it is one Complete
/// message or a Start, up to five Continue messages and an End. A Function
/// Block Name packet carries 13 bytes, and 7 x 13 = 91, one less than an
/// Endpoint Name's 98 (7 x 14).
pub type BlockName = Name<BLOCK_NAME_BYTES>;

impl<const N: usize> Name<N> {
    /// The name, composed to NFC, or `None` when the composed text is longer
    /// than `N` bytes, holds a NUL or starts with a Byte Order Mark (§1.8).
    /// [`FromStr`] says which.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        Self::parse(text).ok()
    }

    /// The text composed to NFC when it is not already.
    fn composed(text: &str) -> Cow<'_, str> {
        // Composed only when it is not NFC already, so the common name does not
        // allocate.
        if is_nfc(text) {
            Cow::Borrowed(text)
        } else {
            Cow::Owned(text.nfc().collect())
        }
    }

    /// The composed text as a name. NFC leaves a leading U+FEFF where it is,
    /// so `Text` refuses it on the composed text as on the given one.
    fn held(text: &str) -> Result<Self, NameError> {
        Text::try_from(text).map(Self).map_err(|error| match error {
            TextError::Nul => NameError::Nul,
            TextError::Bom => NameError::Bom,
            // `Text<0, N>` has no minimum, so `TooShort` cannot occur.
            TextError::TooLong { .. } | TextError::TooShort { .. } => NameError::TooLong { max: N },
        })
    }

    /// The name, composed to NFC, or why the text is not one.
    ///
    /// # Errors
    ///
    /// [`NameError::Bom`] for a leading Byte Order Mark, [`NameError::Nul`] for
    /// a NUL, [`NameError::TooLong`] when the composed text is longer than `N`
    /// bytes.
    fn parse(text: &str) -> Result<Self, NameError> {
        Self::held(&Self::composed(text))
    }

    /// The name for display: composed to NFC, and cut at the last character
    /// boundary within `N` bytes when it is longer. The cut is lossy text for a
    /// reader to show, never an identity: two longer names that share a prefix
    /// share this name. A name a user typed or a peer sent goes through
    /// [`FromStr`], which refuses a longer one.
    ///
    /// `None` for a leading Byte Order Mark or a NUL, which no name holds
    /// ([`FromStr`] says which); never for length.
    #[must_use]
    pub fn truncated(text: &str) -> Option<Self> {
        let text = Self::composed(text);
        if let Err(NameError::Bom | NameError::Nul) = Self::held(&text) {
            return None;
        }
        let mut end = text.len().min(N);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Self::held(&text[..end]).ok()
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// The text's bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_str().as_bytes()
    }

    /// Whether the name is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.as_str().is_empty()
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

impl<const N: usize> TryFrom<String> for Name<N> {
    type Error = NameError;

    fn try_from(text: String) -> Result<Self, NameError> {
        Self::parse(&text)
    }
}

impl<const N: usize> From<Name<N>> for String {
    fn from(name: Name<N>) -> Self {
        name.as_str().to_owned()
    }
}

impl<const N: usize> FromStr for Name<N> {
    type Err = NameError;

    fn from_str(text: &str) -> Result<Self, NameError> {
        Self::parse(text)
    }
}

/// A Product Instance Id: ASCII `32..=126`, at most 42 bytes, a Start, one
/// Continue and an End packet of 14 bytes (M2-104 §7.1.5). It may be empty.
///
/// With the `serde` feature it is its text, read back through
/// [`FromStr`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "String", into = "String"))]
pub struct ProductInstanceId(Text<0, INSTANCE_ID_BYTES>);

impl From<ProductInstanceId> for String {
    fn from(id: ProductInstanceId) -> Self {
        id.as_str().to_owned()
    }
}

impl TryFrom<String> for ProductInstanceId {
    type Error = InstanceIdError;

    fn try_from(text: String) -> Result<Self, InstanceIdError> {
        Self::parse(&text)
    }
}

impl ProductInstanceId {
    /// The Id, or `None` when it is longer than 42 bytes or holds a byte
    /// outside `32..=126`. [`FromStr`] says which.
    #[must_use]
    pub fn new(text: &str) -> Option<Self> {
        Self::parse(text).ok()
    }

    /// The Id, or why the text is not one.
    ///
    /// # Errors
    ///
    /// [`InstanceIdError::NotPrintableAscii`] for any byte outside `32..=126`
    /// (a NUL included), [`InstanceIdError::TooLong`] past 42 bytes.
    fn parse(text: &str) -> Result<Self, InstanceIdError> {
        if !text.bytes().all(|b| (32..=126).contains(&b)) {
            return Err(InstanceIdError::NotPrintableAscii);
        }
        Text::new(text).map(Self).ok_or(InstanceIdError::TooLong {
            max: INSTANCE_ID_BYTES,
        })
    }

    /// An Id for a Device that has no serial number to give (§7.1.5 "should
    /// declare a Product Instance Id"): the 128 `random` bits (from an OS random
    /// source) as 32 lowercase hex digits, so it
    /// is unique per Manufacturer, Family and Model for every practical purpose.
    ///
    /// The Id re-identifies a Device after power cycling only if the Device
    /// keeps it: the owner stores the Id (it is serde with the `serde`
    /// feature) and declares the stored one next time, making one only when
    /// there is none stored.
    #[must_use]
    #[allow(
        clippy::missing_panics_doc,
        reason = "32 lowercase hex digits are printable ASCII and within 42 bytes"
    )]
    pub fn from_random(random: [u8; 16]) -> Self {
        Self::new(&format!("{:032x}", u128::from_be_bytes(random)))
            .expect("32 hex digits are printable ASCII and at most 42 bytes")
    }

    /// The Id a Stream message's `bytes` hold, or `None` for bytes that are
    /// not UTF-8, longer than 42 or outside `32..=126`.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        Self::parse(core::str::from_utf8(bytes).ok()?).ok()
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
    type Error = InstanceIdError;

    fn try_from(text: &str) -> Result<Self, InstanceIdError> {
        Self::parse(text)
    }
}

impl FromStr for ProductInstanceId {
    type Err = InstanceIdError;

    fn from_str(text: &str) -> Result<Self, InstanceIdError> {
        Self::parse(text)
    }
}

#[cfg(test)]
mod tests;
