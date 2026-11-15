//! A plugin's own version, as its format states it.

use std::fmt;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// The version a plugin declares, in the shape its format declares it.
///
/// A format that states a version as numbers gives the numbers; one that states it as text
/// gives the text, verbatim. A plugin that states none is [`Absent`](Self::Absent), never a
/// placeholder.
///
/// | Format | Source | Variant |
/// |---|---|---|
/// | CLAP | `clap_plugin_descriptor::version`, a string | [`Text`](Self::Text) |
/// | VST3 | `PClassInfo2::version` / `PClassInfoW::version`, a string such as `"1.0.0.512"` | [`Text`](Self::Text) |
/// | VST2 | `AEffect::version` / `effGetVendorVersion`, one `int` the spec gives no layout | [`Integer`](Self::Integer) |
/// | AU | `AudioComponentGetVersion`, `0xMMMMmmdd`: 16-bit major, 8-bit minor, 8-bit dot | [`Dotted`](Self::Dotted) |
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum PluginVersion {
    /// The plugin states no version.
    #[default]
    Absent,
    /// The version as the plugin spelled it.
    Text(VersionText),
    /// A single integer with no layout the format defines.
    Integer(i32),
    /// A major, minor and dot release number.
    Dotted {
        /// The major version.
        major: u16,
        /// The minor version.
        minor: u8,
        /// The dot (bug-fix) version.
        dot: u8,
    },
}

impl PluginVersion {
    /// The version a format that states it as text gives: [`Absent`](Self::Absent) when the
    /// text is empty, the text verbatim otherwise.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        VersionText::new(text.into()).map_or(Self::Absent, Self::Text)
    }

    /// The version `AudioComponentGetVersion` returns, `0xMMMMmmdd`: the major in the top 16
    /// bits, the minor in the next 8 and the dot in the low 8.
    #[must_use]
    pub const fn from_au(packed: u32) -> Self {
        Self::Dotted {
            major: (packed >> 16) as u16,
            minor: (packed >> 8) as u8,
            dot: packed as u8,
        }
    }
}

/// A version the plugin spelled as text: never empty, so [`PluginVersion::Absent`] is the one
/// way to state no version.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "String", into = "String"))]
pub struct VersionText(String);

impl VersionText {
    /// `text` as a version, or `None` when it is empty.
    #[must_use]
    pub fn new(text: String) -> Option<Self> {
        (!text.is_empty()).then_some(Self(text))
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<VersionText> for String {
    fn from(text: VersionText) -> Self {
        text.0
    }
}

/// The text of a version that is not empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a version's text is not empty")]
pub struct EmptyVersionText;

impl TryFrom<String> for VersionText {
    type Error = EmptyVersionText;

    fn try_from(text: String) -> Result<Self, EmptyVersionText> {
        Self::new(text).ok_or(EmptyVersionText)
    }
}

/// The text a catalog lists: empty for [`Absent`](PluginVersion::Absent), the text verbatim,
/// the integer in decimal, `major.minor.dot`.
impl fmt::Display for PluginVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => Ok(()),
            Self::Text(text) => f.write_str(text.as_str()),
            Self::Integer(raw) => write!(f, "{raw}"),
            Self::Dotted { major, minor, dot } => write!(f, "{major}.{minor}.{dot}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text states a version verbatim and empty text states none.
    #[test]
    fn empty_text_is_absent_and_other_text_is_kept_whole() {
        assert_eq!(PluginVersion::text(""), PluginVersion::Absent);
        assert_eq!(
            PluginVersion::text("1.0.0.512"),
            PluginVersion::Text(VersionText::new("1.0.0.512".into()).unwrap())
        );
        assert_eq!(PluginVersion::text("1.0.0.512").to_string(), "1.0.0.512");
        assert_eq!(PluginVersion::default(), PluginVersion::Absent);
        assert_eq!(VersionText::new(String::new()), None);
        assert_eq!(PluginVersion::Absent.to_string(), "");
    }

    /// `AudioComponent.h`: `0xMMMMmmdd`. `0x00010203` is 1.2.3, and a major past 255 keeps
    /// its 16 bits.
    #[test]
    fn an_au_version_unpacks_as_a_16_bit_major_then_minor_then_dot() {
        assert_eq!(
            PluginVersion::from_au(0x0001_0203),
            PluginVersion::Dotted {
                major: 1,
                minor: 2,
                dot: 3
            }
        );
        assert_eq!(PluginVersion::from_au(0x0001_0203).to_string(), "1.2.3");
        assert_eq!(PluginVersion::from_au(0x0102_0000).to_string(), "258.0.0");
        assert_eq!(
            PluginVersion::from_au(0xFFFF_FFFF).to_string(),
            "65535.255.255"
        );
        assert_eq!(PluginVersion::from_au(0).to_string(), "0.0.0");
    }

    /// A VST2 version is one integer the spec gives no layout: `1100` stays `1100`, and a
    /// negative one stays negative.
    #[test]
    fn a_vst2_version_is_one_integer_with_no_invented_layout() {
        assert_eq!(PluginVersion::Integer(1100).to_string(), "1100");
        assert_eq!(PluginVersion::Integer(-1).to_string(), "-1");
        assert_ne!(PluginVersion::Integer(0), PluginVersion::Absent);
    }

    /// A catalog or wire record that spells the text variant with no text is refused, so the
    /// empty string has one meaning only: absent.
    #[cfg(feature = "serde")]
    #[test]
    fn empty_text_does_not_deserialize() {
        assert!(serde_json::from_str::<PluginVersion>(r#"{"Text":""}"#).is_err());
        assert!(serde_json::from_str::<PluginVersion>(r#"{"Text":"1"}"#).is_ok());
        let wire = postcard::to_stdvec(&PluginVersion::Text(VersionText(String::new()))).unwrap();
        assert!(postcard::from_bytes::<PluginVersion>(&wire).is_err());
    }

    /// Every shape survives the catalog's JSON and the wire's postcard unchanged, so a
    /// version read from one format is the version listed from the catalog.
    #[cfg(feature = "serde")]
    #[test]
    fn every_shape_round_trips_through_the_catalog_and_the_wire() {
        for version in [
            PluginVersion::Absent,
            PluginVersion::text("1.0.0.512"),
            PluginVersion::Integer(1100),
            PluginVersion::from_au(0x0001_0203),
        ] {
            let json = serde_json::to_string(&version).unwrap();
            assert_eq!(
                serde_json::from_str::<PluginVersion>(&json).unwrap(),
                version
            );
            let wire = postcard::to_stdvec(&version).unwrap();
            assert_eq!(
                postcard::from_bytes::<PluginVersion>(&wire).unwrap(),
                version
            );
        }
    }
}
