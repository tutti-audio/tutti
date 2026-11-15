//! [`PluginFormat`]: which format's loader opens a plugin, and [`PluginId`]:
//! the identity a saved reference to a plugin names.

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
}

/// A file extension no plugin format claims.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`.{extension}` is not a plugin extension")]
pub struct UnknownExtension {
    /// The extension as given, without the dot.
    pub extension: String,
}

/// Which plugin a saved reference names: its format and the format's own id
/// for it ([`PluginDescriptor::id`](crate::PluginDescriptor): a CLAP plugin id,
/// a VST3 class id, an AU descriptor id, a VST2 `vst2.<uniqueID>`).
///
/// The id is the identity, not the file that holds it: a plugin moves between
/// directories and machines, and a catalog resolves the id to the file it is
/// in now.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PluginId {
    /// Which format's loader opens it.
    pub format: PluginFormat,
    /// The format's own id for the plugin.
    pub id: String,
}

impl PluginId {
    /// The plugin `id` of `format`.
    #[must_use]
    pub fn new(format: PluginFormat, id: impl Into<String>) -> Self {
        Self {
            format,
            id: id.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PluginFormat;

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
