//! [`PluginFormat`]: which format's loader opens a plugin, and [`PluginId`]:
//! the identity a saved reference to a plugin names.

/// A plugin format.
///
/// A file's extension names a format for a scan (`format_from_path` in
/// `tutti-plugin-db`); a load and a probe take the format as a value, since a
/// link whose target has another extension is still the format it was
/// scanned as.
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

    /// The format whose [`extension_id`](Self::extension_id) is `id`.
    #[must_use]
    pub fn from_extension_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.extension_id() == id)
    }
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
}
