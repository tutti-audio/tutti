//! [`PluginFormat`]: which format's loader opens a plugin, and [`PluginId`]:
//! the identity a saved reference to a plugin names.

use crate::AuComponentType;
use std::fmt;
use std::str::FromStr;
use thiserror::Error;

/// A plugin format.
///
/// A file's extension names a format for a scan
/// ([`from_extension`](Self::from_extension)); a load and a probe take the
/// format as a value, since a link whose target has another extension is still
/// the format it was scanned as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum PluginFormat {
    /// Steinberg VST3.
    Vst3,
    /// Steinberg VST2.
    Vst2,
    /// CLAP.
    Clap,
    /// Apple Audio Unit (macOS only).
    AudioUnit,
}

impl PluginFormat {
    /// Every format.
    pub const ALL: [Self; 4] = [Self::Vst3, Self::Vst2, Self::Clap, Self::AudioUnit];

    /// Returns the short format name used to build plugin IDs (`"vst3"` in
    /// `"vst3.my_reverb"`).
    #[must_use]
    pub fn extension_id(&self) -> &'static str {
        match self {
            PluginFormat::Vst3 => "vst3",
            PluginFormat::Vst2 => "vst2",
            PluginFormat::Clap => "clap",
            PluginFormat::AudioUnit => "au",
        }
    }

    /// The kind of module a bundle of this format holds
    /// ([`bundle`](crate::bundle)).
    #[must_use]
    pub fn module_kind(self) -> crate::bundle::ModuleKind {
        use crate::bundle::ModuleKind;
        match self {
            PluginFormat::Vst3 => ModuleKind::Vst3,
            PluginFormat::Vst2 => ModuleKind::Vst2,
            PluginFormat::Clap => ModuleKind::Clap,
            PluginFormat::AudioUnit => ModuleKind::Au,
        }
    }

    /// The format whose [`extension_id`](Self::extension_id) is `id`: the
    /// stable id string (`"vst3"`, `"au"`) a plugin id and the wire carry, not a
    /// file extension; [`from_extension`](Self::from_extension) reads a file's.
    #[must_use]
    pub fn from_extension_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.extension_id() == id)
    }

    /// The format a file extension (without the dot) names, not an id string
    /// (`"au"` is an id, `"component"` the extension;
    /// [`from_extension_id`](Self::from_extension_id) reads ids), matched without
    /// regard to case: `vst3`, `vst`/`dll`/`so` (VST2 ships as a bare shared
    /// library off macOS), `clap` and `component`.
    ///
    /// Case-insensitive because macOS (APFS, HFS+) and Windows (NTFS) volumes
    /// are, so a vendor shipping `Reverb.VST3` is a real case, and a bundle is a
    /// directory a scan would otherwise walk into and find nothing.
    ///
    /// # Errors
    ///
    /// [`UnknownExtension`] naming `ext` when no format claims it.
    pub fn from_extension(ext: &str) -> Result<Self, UnknownExtension> {
        const BY_EXTENSION: [(&str, PluginFormat); 6] = [
            ("vst3", PluginFormat::Vst3),
            ("vst", PluginFormat::Vst2),
            ("dll", PluginFormat::Vst2),
            ("so", PluginFormat::Vst2),
            ("clap", PluginFormat::Clap),
            ("component", PluginFormat::AudioUnit),
        ];
        BY_EXTENSION
            .iter()
            .find(|(known, _)| known.eq_ignore_ascii_case(ext))
            .map(|(_, format)| *format)
            .ok_or_else(|| UnknownExtension {
                extension: ext.to_owned(),
            })
    }

    /// The format the extension of `path` names
    /// ([`from_extension`](Self::from_extension)), or `None` for a path with no
    /// extension or one no format claims.
    #[must_use]
    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        let ext = path.extension().and_then(|e| e.to_str())?;
        Self::from_extension(ext).ok()
    }
}

/// The format as a user knows it: `VST3`, `VST2`, `CLAP`, `AU`.
impl std::fmt::Display for PluginFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PluginFormat::Vst3 => "VST3",
            PluginFormat::Vst2 => "VST2",
            PluginFormat::Clap => "CLAP",
            PluginFormat::AudioUnit => "AU",
        })
    }
}

/// A file extension no plugin format claims.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`.{extension}` is not a plugin extension")]
pub struct UnknownExtension {
    /// The extension as given, without the dot.
    pub extension: String,
}

/// A VST3 class id: the 16 bytes a factory names a class by.
///
/// Text is the four dash-separated groups of four bytes the catalog keeps
/// (`00112233-44556677-8899AABB-CCDDEEFF`, upper-case hex).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Uid([u8; 16]);

impl Uid {
    /// The class id of these bytes.
    #[must_use]
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The 16 bytes.
    #[must_use]
    pub const fn get(self) -> [u8; 16] {
        self.0
    }
}

impl fmt::Display for Uid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if i > 0 && i % 4 == 0 {
                f.write_str("-")?;
            }
            write!(f, "{byte:02X}")?;
        }
        Ok(())
    }
}

impl FromStr for Uid {
    type Err = PluginIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let groups: Vec<&str> = text.split('-').collect();
        let hex = groups.concat();
        if groups.len() != 4
            || groups.iter().any(|g| g.len() != 8)
            || !hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(PluginIdError::Uid(text.to_owned()));
        }
        let mut bytes = [0u8; 16];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16)
                .expect("two hex digits, checked above");
        }
        Ok(Self(bytes))
    }
}

/// A four-character code: an Audio Unit's component subtype or manufacturer.
///
/// Text is the four bytes, each a printable ASCII character as itself (a space
/// kept: `dls `) and any other byte, `.` and `\` included, as `\xNN` in
/// lower-case hex (`\xa9nam`). [`FromStr`] reads the same text back, so every
/// code round-trips and no two codes read alike. The escape keeps `.` free to
/// separate the codes of a [`PluginId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FourCc(u32);

impl FourCc {
    /// The code `raw` spells, its first character the high byte.
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The code as an integer.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0.to_be_bytes() {
            match byte {
                b'.' | b'\\' => write!(f, "\\x{byte:02x}")?,
                0x20..=0x7e => write!(f, "{}", char::from(byte))?,
                _ => write!(f, "\\x{byte:02x}")?,
            }
        }
        Ok(())
    }
}

impl FromStr for FourCc {
    type Err = PluginIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let bad = || PluginIdError::FourCc(text.to_owned());
        let raw = text.as_bytes();
        let mut bytes = Vec::with_capacity(4);
        let mut at = 0;
        while at < raw.len() {
            let byte = match raw[at] {
                b'\\' => {
                    let digits = raw.get(at + 1..at + 4).ok_or_else(bad)?;
                    let lower_hex = |d: &u8| d.is_ascii_digit() || (b'a'..=b'f').contains(d);
                    if digits[0] != b'x' || !digits[1..].iter().all(lower_hex) {
                        return Err(bad());
                    }
                    let hex = std::str::from_utf8(&digits[1..]).map_err(|_| bad())?;
                    let byte = u8::from_str_radix(hex, 16).map_err(|_| bad())?;
                    // Only a byte that prints as itself has no escape: one text per code.
                    if matches!(byte, 0x20..=0x7e) && byte != b'.' && byte != b'\\' {
                        return Err(bad());
                    }
                    at += 3;
                    byte
                }
                b'.' => return Err(bad()),
                byte @ 0x20..=0x7e => byte,
                _ => return Err(bad()),
            };
            bytes.push(byte);
            at += 1;
        }
        <[u8; 4]>::try_from(bytes)
            .map(|bytes| Self(u32::from_be_bytes(bytes)))
            .map_err(|_| bad())
    }
}

/// Which plugin a saved reference names: the format and that format's own id
/// for it. A CLAP plugin id, a VST3 class id, an AU component's type, subtype
/// and manufacturer, a VST2 plugin's `uniqueID`.
///
/// The id is the identity, not the file that holds it: a plugin moves between
/// directories and machines, and a catalog resolves the id to the file it is
/// in now.
///
/// Text is `<format>.<id>` (`clap.com.vendor.synth`,
/// `vst3.00112233-44556677-8899AABB-CCDDEEFF`, `au.aufx.dely.appl`,
/// `vst2.1234`), what [`Display`](fmt::Display) writes and
/// [`FromStr`] reads: for a command line and a message, not for storage.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "serde_form::Raw"))]
pub enum PluginId {
    /// A CLAP plugin id (`com.vendor.synth`).
    Clap(String),
    /// A VST3 class id.
    Vst3(Uid),
    /// An Audio Unit component.
    Au {
        /// The component type (`aufx`, `aumu`, ..): an effect and an
        /// instrument of one manufacturer can share a subtype.
        component_type: AuComponentType,
        /// The component subtype.
        subtype: FourCc,
        /// The component manufacturer.
        manufacturer: FourCc,
    },
    /// A VST2 plugin's `uniqueID` (a shell's sub-plugin has its own).
    Vst2(u32),
}

impl PluginId {
    /// The format whose loader opens a plugin with this id.
    #[must_use]
    pub const fn format(&self) -> PluginFormat {
        match self {
            Self::Clap(_) => PluginFormat::Clap,
            Self::Vst3(_) => PluginFormat::Vst3,
            Self::Au { .. } => PluginFormat::AudioUnit,
            Self::Vst2(_) => PluginFormat::Vst2,
        }
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let format = self.format().extension_id();
        match self {
            Self::Clap(id) => write!(f, "{format}.{id}"),
            Self::Vst3(uid) => write!(f, "{format}.{uid}"),
            Self::Au {
                component_type,
                subtype,
                manufacturer,
            } => write!(
                f,
                "{format}.{}.{subtype}.{manufacturer}",
                FourCc::new(component_type.code())
            ),
            Self::Vst2(id) => write!(f, "{format}.{id}"),
        }
    }
}

impl FromStr for PluginId {
    type Err = PluginIdError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let (format, rest) = text
            .split_once('.')
            .ok_or_else(|| PluginIdError::Format(text.to_owned()))?;
        match PluginFormat::from_extension_id(format) {
            Some(PluginFormat::Clap) if rest.is_empty() => Err(PluginIdError::EmptyClap),
            Some(PluginFormat::Clap) => Ok(Self::Clap(rest.to_owned())),
            Some(PluginFormat::Vst3) => rest.parse().map(Self::Vst3),
            Some(PluginFormat::Vst2) => {
                // Digits only, no sign and no leading zero: one text per id.
                let canonical = !rest.is_empty()
                    && rest.bytes().all(|b| b.is_ascii_digit())
                    && (rest == "0" || !rest.starts_with('0'));
                canonical
                    .then(|| rest.parse().ok())
                    .flatten()
                    .map(Self::Vst2)
                    .ok_or_else(|| PluginIdError::Vst2(rest.to_owned()))
            }
            Some(PluginFormat::AudioUnit) => {
                let mut codes = rest.split('.');
                match (codes.next(), codes.next(), codes.next(), codes.next()) {
                    (Some(t), Some(s), Some(m), None) => Ok(Self::Au {
                        component_type: AuComponentType::from_code(t.parse::<FourCc>()?.get()),
                        subtype: s.parse()?,
                        manufacturer: m.parse()?,
                    }),
                    _ => Err(PluginIdError::Au(rest.to_owned())),
                }
            }
            None => Err(PluginIdError::Format(text.to_owned())),
        }
    }
}

/// What a [`PluginId`] reads from a document or the wire, before it is checked:
/// the same variants, so the shapes agree. A CLAP id of no characters is
/// refused, so a value the peer cannot make is a decode failure.
#[cfg(feature = "serde")]
mod serde_form {
    use super::{AuComponentType, FourCc, PluginId, PluginIdError, Uid};

    #[derive(Debug, serde::Deserialize)]
    pub enum Raw {
        Clap(String),
        Vst3(Uid),
        Au {
            component_type: AuComponentType,
            subtype: FourCc,
            manufacturer: FourCc,
        },
        Vst2(u32),
    }

    impl TryFrom<Raw> for PluginId {
        type Error = PluginIdError;

        fn try_from(raw: Raw) -> Result<Self, Self::Error> {
            Ok(match raw {
                Raw::Clap(id) if id.is_empty() => return Err(PluginIdError::EmptyClap),
                Raw::Clap(id) => Self::Clap(id),
                Raw::Vst3(uid) => Self::Vst3(uid),
                Raw::Au {
                    component_type,
                    subtype,
                    manufacturer,
                } => Self::Au {
                    component_type,
                    subtype,
                    manufacturer,
                },
                Raw::Vst2(id) => Self::Vst2(id),
            })
        }
    }
}

/// Text that is not a [`PluginId`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PluginIdError {
    /// No `<format>.` prefix naming a format.
    #[error("`{0}` does not start with a plugin format (`clap.`, `vst3.`, `au.`, `vst2.`)")]
    Format(String),
    /// Not a VST3 class id.
    #[error("`{0}` is not a VST3 class id (`XXXXXXXX-XXXXXXXX-XXXXXXXX-XXXXXXXX`)")]
    Uid(String),
    /// Not a four-character code.
    #[error("`{0}` is not a four-character code")]
    FourCc(String),
    /// Not `<type>.<subtype>.<manufacturer>`.
    #[error("`{0}` is not an AU id (`<type>.<subtype>.<manufacturer>`)")]
    Au(String),
    /// A CLAP id of no characters.
    #[error("a CLAP plugin id is not empty")]
    EmptyClap,
    /// Not a VST2 `uniqueID`.
    #[error("`{0}` is not a VST2 id (a number)")]
    Vst2(String),
}

#[cfg(test)]
mod tests {
    use super::PluginFormat;
    use std::path::Path;

    /// A path names its format by its extension, whatever its case, and a path
    /// with no plugin extension names none.
    #[test]
    fn a_path_names_the_format_of_its_extension() {
        for (path, format) in [
            ("a.vst3", Some(PluginFormat::Vst3)),
            ("Reverb.VST3", Some(PluginFormat::Vst3)),
            ("a.vst", Some(PluginFormat::Vst2)),
            ("Legacy.DLL", Some(PluginFormat::Vst2)),
            ("Synth1.so", Some(PluginFormat::Vst2)),
            ("a.clap", Some(PluginFormat::Clap)),
            ("Synth.Component", Some(PluginFormat::AudioUnit)),
            ("a.txt", None),
            ("a.wasm", None),
            ("plain", None),
        ] {
            assert_eq!(PluginFormat::from_path(Path::new(path)), format, "{path}");
        }
    }

    /// **`from_extension_id` inverts `extension_id`** for every format, and `ALL`
    /// lists each format once, so the name a caller sends to `plugin-probe --as`
    /// is the one it reads back.
    #[test]
    fn every_extension_id_names_its_own_format() {
        for format in PluginFormat::ALL {
            assert_eq!(
                PluginFormat::from_extension_id(format.extension_id()),
                Some(format)
            );
        }
        // An exhaustive match: a new format fails to compile here until it has
        // a place in `ALL`.
        let place = |f: PluginFormat| match f {
            PluginFormat::Vst3 => 0,
            PluginFormat::Vst2 => 1,
            PluginFormat::Clap => 2,
            PluginFormat::AudioUnit => 3,
        };
        for (i, format) in PluginFormat::ALL.into_iter().enumerate() {
            assert_eq!(place(format), i);
        }
        for unknown in ["vst", "component", "CLAP", ""] {
            assert_eq!(PluginFormat::from_extension_id(unknown), None);
        }
    }

    /// **An extension names its format whatever its case**: `.VST3`, `.Clap`
    /// and `.component` (a case-insensitive volume keeps the vendor's
    /// spelling), and VST2's `vst`, `dll` and `so`.
    #[test]
    fn an_extension_names_its_format_in_any_case() {
        for (ext, format) in [
            ("vst3", PluginFormat::Vst3),
            ("VST3", PluginFormat::Vst3),
            ("Clap", PluginFormat::Clap),
            ("CLAP", PluginFormat::Clap),
            ("component", PluginFormat::AudioUnit),
            ("Component", PluginFormat::AudioUnit),
            ("Vst", PluginFormat::Vst2),
            ("DLL", PluginFormat::Vst2),
            ("So", PluginFormat::Vst2),
        ] {
            assert_eq!(PluginFormat::from_extension(ext), Ok(format), "{ext}");
        }
    }

    /// **An unknown extension is refused, and the refusal names it**: no
    /// default format stands in, and the empty extension is not a format.
    #[test]
    fn an_unknown_extension_is_refused_by_name() {
        for unknown in ["txt", "wasm", "vst4", "au", ""] {
            let err = PluginFormat::from_extension(unknown).unwrap_err();
            assert_eq!(
                err,
                super::UnknownExtension {
                    extension: unknown.to_owned()
                }
            );
            assert!(err.to_string().contains(&format!("`.{unknown}`")), "{err}");
        }
    }
}
