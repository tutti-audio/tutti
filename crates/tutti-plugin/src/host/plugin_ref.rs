//! [`PluginRef`]: which plugin, in which file.
//!
//! A file can export several plugins (a CLAP or VST3 bundle, an AU component),
//! so a path alone does not name one. A [`PluginRef`] is the path, the format
//! and the plugin's own id inside it (its `PluginDescriptor::id`): what a
//! scan reports for each plugin, and what a project file keeps to find the
//! plugin again. A load takes a [`Loadable`](crate::host::loadable::Loadable).

use std::fmt;
use std::path::{Path, PathBuf};
use tutti_plugin_db::{PluginEntry, format_from_path};
use tutti_plugin_types::{Load, PluginFormat};

/// One plugin: the file it is in, its format, and its id in that file.
///
/// Made from a scanned plugin ([`PluginRef::from`] a
/// [`PluginEntry`]) or, for a file found by hand, [`file`](Self::file), which
/// names the file's first plugin. With the `serde` feature it persists, and
/// `is_same_plugin` is how a reloaded project finds
/// out whether two refs name one plugin.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PluginRef {
    path: PathBuf,
    format: PluginFormat,
    /// The descriptor id; `None`: the file's first plugin.
    id: Option<String>,
}

impl PluginRef {
    /// The first plugin of the file at `path`, whose format is its extension's
    /// (`None` for an extension that is no plugin format).
    ///
    /// For a plugin known only by its file; one found by a scan has its own id
    /// (`PluginRef::from(&entry)`), which is what picks a plugin in a bundle
    /// that exports several.
    #[must_use]
    pub fn file(path: impl Into<PathBuf>) -> Option<Self> {
        let path = path.into();
        let format = format_from_path(&path)?;
        Some(Self {
            path,
            format,
            id: None,
        })
    }

    /// The file that holds the plugin.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The format whose loader opens it.
    #[must_use]
    pub fn format(&self) -> PluginFormat {
        self.format
    }

    /// The plugin's id in its file (its descriptor's `id`); `None` for the
    /// file's first plugin ([`file`](Self::file)).
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// The choice of plugin the loaders take: this plugin's id (a CLAP plugin
    /// id, a VST3 class id, an AU unit, a VST2 shell's `vst2.<uniqueID>`), or
    /// the file's first plugin when there is no id.
    #[must_use]
    pub fn load(&self) -> Load {
        match &self.id {
            Some(id) => Load::new().plugin(id.clone()),
            None => Load::new(),
        }
    }

    /// Whether `other` names the same plugin: the same format and, when both
    /// have an id, the same id; when either has none (it names the file's first
    /// plugin), the same file.
    #[must_use]
    pub fn is_same_plugin(&self, other: &Self) -> bool {
        self.format == other.format
            && match (&self.id, &other.id) {
                (Some(a), Some(b)) => a == b,
                // Canonical, as a scan keys a file: a link and the file it
                // names are one plugin.
                _ => {
                    tutti_plugin_db::canonical_path(&self.path)
                        == tutti_plugin_db::canonical_path(&other.path)
                }
            }
    }

    /// The plugin a load of `path` as `format` produced, whose descriptor
    /// reports `descriptor_id`.
    pub(crate) fn loaded(path: &Path, format: PluginFormat, descriptor_id: &str) -> Self {
        Self {
            path: path.to_path_buf(),
            format,
            id: Some(descriptor_id.to_owned()),
        }
    }

    /// This ref with the id the loaded plugin reports, if it had none (an
    /// in-process load, which has no client to ask).
    #[cfg(any(
        feature = "vst2",
        feature = "clap",
        feature = "vst3",
        all(feature = "au", target_os = "macos")
    ))]
    pub(crate) fn identified(&self, descriptor_id: &str) -> Self {
        Self {
            id: self.id.clone().or_else(|| Some(descriptor_id.to_owned())),
            ..self.clone()
        }
    }
}

/// The plugin as text, for an error: its id, else its file.
impl fmt::Display for PluginRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.id {
            Some(id) => f.write_str(id),
            None => write!(f, "{}", self.path.display()),
        }
    }
}

impl From<&PluginEntry<'_>> for PluginRef {
    fn from(entry: &PluginEntry<'_>) -> Self {
        Self {
            path: entry.path.to_path_buf(),
            format: entry.format,
            id: Some(entry.descriptor.id.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_ref(path: &str, format: PluginFormat, id: Option<&str>) -> PluginRef {
        PluginRef {
            path: path.into(),
            format,
            id: id.map(Into::into),
        }
    }

    /// **The load chooses the plugin's id, in every format** (a VST2 shell
    /// holds several plugins too); no id is the file's first plugin.
    #[test]
    fn the_load_chooses_the_id() {
        for format in [
            PluginFormat::Clap,
            PluginFormat::Vst3,
            PluginFormat::Vst2,
            PluginFormat::AudioUnit,
        ] {
            let r = entry_ref("/p/x", format, Some("the.id"));
            assert_eq!(r.load(), Load::new().plugin("the.id"), "{format:?}");
            assert_eq!(entry_ref("/p/x", format, None).load(), Load::new());
        }
    }

    /// **Identity is the id when both have one, the file otherwise.**
    #[test]
    fn two_refs_are_one_plugin_by_id_else_by_file() {
        let a = entry_ref("/p/bundle.clap", PluginFormat::Clap, Some("a"));
        let b = entry_ref("/p/bundle.clap", PluginFormat::Clap, Some("b"));
        let first = entry_ref("/p/bundle.clap", PluginFormat::Clap, None);
        let elsewhere = entry_ref("/q/bundle.clap", PluginFormat::Clap, None);
        assert!(a.is_same_plugin(&a));
        assert!(!a.is_same_plugin(&b), "one file, two plugins");
        assert!(a.is_same_plugin(&first), "the file's first plugin: by file");
        assert!(first.is_same_plugin(&a));
        assert!(!first.is_same_plugin(&elsewhere));
        let other_format = entry_ref("/p/bundle.clap", PluginFormat::Vst3, Some("a"));
        assert!(!a.is_same_plugin(&other_format), "the format is part of it");
    }

    /// **A link and the file it names are one plugin**, as a scan keys them
    /// (macOS's temp dir is such a link: `/var` to `/private/var`).
    #[cfg(unix)]
    #[test]
    fn a_link_and_its_file_are_one_plugin() {
        let dir = std::env::temp_dir().join(format!("tutti-ref-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("synth.clap");
        let link = dir.join("link.clap");
        std::fs::write(&file, b"x").unwrap();
        std::fs::remove_file(&link).ok();
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let by_file = PluginRef::file(&file).unwrap();
        let by_link = PluginRef::file(&link).unwrap();
        assert!(by_file.is_same_plugin(&by_link));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **A ref reads as its id, else its file.**
    #[test]
    fn a_ref_displays_as_its_id_else_its_file() {
        let with_id = entry_ref("/p/bundle.clap", PluginFormat::Clap, Some("the.id"));
        assert_eq!(with_id.to_string(), "the.id");
        let first = entry_ref("/p/bundle.clap", PluginFormat::Clap, None);
        assert_eq!(first.to_string(), "/p/bundle.clap");
    }

    /// **A file's ref has the extension's format, and refuses what is no plugin.**
    #[test]
    fn a_file_ref_takes_its_format_from_the_extension() {
        let r = PluginRef::file("/p/Synth.clap").unwrap();
        assert_eq!(r.format(), PluginFormat::Clap);
        assert_eq!(r.id(), None);
        assert!(PluginRef::file("/p/notes.txt").is_none());
    }
}
