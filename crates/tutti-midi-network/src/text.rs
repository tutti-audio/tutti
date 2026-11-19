//! The UTF-8 strings a command carries outside the endpoint names:
//! [`ByeText`] and [`NakText`] (a Bye's and a NAK's message) and [`Username`].
//! Each is a [`tutti_units::Text`], whose bounds are in bytes.

use tutti_units::Text;

/// A Bye message: up to 1020 bytes, the most a Bye's payload of 255 words
/// holds (M2-124 Table 26). A string ends at the first NUL or at the end of
/// the command (M2-124 section 5.3), so a NUL cannot be part of one.
pub type ByeText = Text<0, 1020>;

/// A NAK message: up to 1016 bytes, the most 255 payload words hold beside the
/// refused command's header word (M2-124 Table 24).
pub type NakText = Text<0, 1016>;

/// A user name for an Invitation with User Authentication: 1 to 988 bytes, the
/// payload that is left beside the 32-byte digest.
///
/// It is held exactly as given and never NFC-normalised: the digest hashes the
/// peer's bytes, so a normalised name would hash differently from the one the
/// peer sent.
pub type Username = Text<1, 988>;
