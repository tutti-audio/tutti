//! [`PluginRef`]: which plugin, in which file.
//!
//! A file can export several plugins (a CLAP or VST3 bundle, an AU component),
//! so a path alone does not name one. A [`PluginRef`] is the path and the
//! plugin's own [`PluginId`] inside it: what a scan reports for each plugin,
//! what a project file keeps to find the plugin again, and what a loader takes
//! to open one.

use crate::{LoadStage, PluginError, PluginFormat, PluginId, UnknownExtension};
use std::fmt;
use std::path::{Path, PathBuf};

/// One plugin: the file it is in, and its id in that file.
///
/// Made from a scanned plugin or, for a file found by hand,
/// [`file`](Self::file), which names the file's first plugin. With the `serde`
/// feature it persists, and [`is_same_plugin`](Self::is_same_plugin) is how a
/// reloaded project finds out whether two refs name one plugin.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct PluginRef {
    path: PathBuf,
    /// `None`: the file's first plugin, of the format its extension names.
    choice: Option<Choice>,
}

/// What a ref says beyond its file: the plugin, whose id gives the format, or
/// the file's first plugin as a format the extension need not name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
enum Choice {
    Plugin(PluginId),
    Format(PluginFormat),
}

impl PluginRef {
    /// The plugin `id` of the file at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, id: PluginId) -> Self {
        Self {
            path: path.into(),
            choice: Some(Choice::Plugin(id)),
        }
    }

    /// The plugin `id` of the file at `path`, or the file's first plugin when
    /// `id` is `None`: what a wire that carries an optional id builds.
    #[must_use]
    pub fn of(path: impl Into<PathBuf>, id: Option<PluginId>) -> Self {
        match id {
            Some(id) => Self::new(path, id),
            None => Self::file(path),
        }
    }

    /// The first plugin of the file at `path`, whose format is its extension's
    /// ([`format`](Self::format) refuses an extension that is no plugin format).
    ///
    /// For a plugin known only by its file; one found by a scan has its own id,
    /// which is what picks a plugin in a bundle that exports several.
    #[must_use]
    pub fn file(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            choice: None,
        }
    }

    /// The first plugin of the file at `path`, opened as `format`: for a file
    /// whose extension does not name its format (a `Foo.clap` link keyed by its
    /// target `libfoo.so`).
    #[must_use]
    pub fn first_of(path: impl Into<PathBuf>, format: PluginFormat) -> Self {
        Self {
            path: path.into(),
            choice: Some(Choice::Format(format)),
        }
    }

    /// The file that holds the plugin.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The plugin's id in its file; `None` for the file's first plugin
    /// ([`file`](Self::file), [`first_of`](Self::first_of)).
    #[must_use]
    pub fn id(&self) -> Option<&PluginId> {
        match &self.choice {
            Some(Choice::Plugin(id)) => Some(id),
            _ => None,
        }
    }

    /// The format whose loader opens it: its id's, else the one it was made
    /// with ([`first_of`](Self::first_of)), else its file's extension's.
    ///
    /// # Errors
    ///
    /// [`UnknownExtension`] when it names no plugin and no format and its file's
    /// extension names none.
    pub fn format(&self) -> Result<PluginFormat, UnknownExtension> {
        match &self.choice {
            Some(Choice::Plugin(id)) => Ok(id.format()),
            Some(Choice::Format(format)) => Ok(*format),
            None => PluginFormat::from_extension(
                self.path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or_default(),
            ),
        }
    }

    /// The plugin's id for a loader of `format`: `None` for the file's first
    /// plugin.
    ///
    /// # Errors
    ///
    /// [`PluginError::Load`] at [`LoadStage::Opening`] when the id names a
    /// plugin of another format.
    pub fn id_for(&self, format: PluginFormat) -> Result<Option<&PluginId>, PluginError> {
        match self.id() {
            Some(id) if id.format() != format => Err(PluginError::Load {
                stage: LoadStage::Opening,
                reason: format!(
                    "`{id}` is a {} plugin id, and this is the {} loader",
                    id.format().extension_id(),
                    format.extension_id()
                ),
            }),
            id => Ok(id),
        }
    }

    /// Whether `other` names the same plugin: the same id when both have one;
    /// when either has none (it names the file's first plugin), the same format
    /// and file.
    #[must_use]
    pub fn is_same_plugin(&self, other: &Self) -> bool {
        match (self.id(), other.id()) {
            (Some(a), Some(b)) => a == b,
            // Canonical, as a scan keys a file: a link and the file it names
            // are one plugin.
            _ => {
                self.format().ok() == other.format().ok()
                    && canonical_path(&self.path) == canonical_path(&other.path)
            }
        }
    }

    /// The plugin at `path` with this ref's choice: the file a loader opened,
    /// which can be the module inside the bundle the ref names.
    #[must_use]
    pub fn at(&self, path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            choice: self.choice.clone(),
        }
    }

    /// This ref with `id` if it had none (a load of the file's first plugin
    /// learns which plugin that was).
    #[must_use]
    pub fn identified(&self, id: PluginId) -> Self {
        match self.id() {
            Some(_) => self.clone(),
            None => Self::new(self.path.clone(), id),
        }
    }
}

/// The plugin as text, for an error: its id, else its file.
impl fmt::Display for PluginRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.id() {
            Some(id) => write!(f, "{id}"),
            None => write!(f, "{}", self.path.display()),
        }
    }
}

/// `path` as one key whichever way it is spelled: the canonical path, else its
/// canonical parent joined to its name, or at least an absolute path. A link, a
/// relative path and an absolute one to the same file give one key.
#[must_use]
pub fn canonical_path(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match (parent.canonicalize(), path.file_name()) {
        (Ok(parent), Some(name)) => parent.join(name),
        _ => std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuComponentType, FourCc, Uid};

    fn ids() -> [PluginId; 4] {
        [
            PluginId::Clap("com.vendor.synth".into()),
            PluginId::Vst3(Uid::new([
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
                0xEE, 0xFF,
            ])),
            PluginId::Au {
                component_type: AuComponentType::Instrument,
                subtype: FourCc::new(u32::from_be_bytes(*b"dls ")),
                manufacturer: FourCc::new(u32::from_be_bytes(*b"appl")),
            },
            PluginId::Vst2(0x4142_4344),
        ]
    }

    fn at(path: &str, id: Option<PluginId>) -> PluginRef {
        PluginRef::of(path, id)
    }

    /// **An id reads back from its text, in every format**, and each text
    /// starts with the format that opens it.
    #[test]
    fn an_id_round_trips_through_its_text_in_every_format() {
        let want = [
            "clap.com.vendor.synth",
            "vst3.00112233-44556677-8899AABB-CCDDEEFF",
            "au.aumu.dls .appl",
            "vst2.1094861636",
        ];
        for (id, text) in ids().into_iter().zip(want) {
            assert_eq!(id.to_string(), text);
            assert_eq!(text.parse::<PluginId>(), Ok(id));
        }
    }

    /// **Text that is no id is refused with the part that is wrong.**
    #[test]
    fn text_that_is_no_id_is_refused() {
        use crate::PluginIdError as E;
        for (text, error) in [
            ("synth", E::Format("synth".into())),
            ("lv2.x", E::Format("lv2.x".into())),
            ("vst3.00112233", E::Uid("00112233".into())),
            (
                "vst3.0011223G-44556677-8899AABB-CCDDEEFF",
                E::Uid("0011223G-44556677-8899AABB-CCDDEEFF".into()),
            ),
            ("vst2.-1", E::Vst2("-1".into())),
            ("au.aufx.dely", E::Au("aufx.dely".into())),
            ("au.aufx.dely.app", E::FourCc("app".into())),
        ] {
            assert_eq!(text.parse::<PluginId>(), Err(error), "{text}");
        }
    }

    /// **A loader takes the id of its own format and refuses another's at the
    /// call**, naming both; no id is the file's first plugin, in any loader.
    #[test]
    fn a_loader_refuses_an_id_of_another_format() {
        for id in ids() {
            let r = PluginRef::new("/p/x", id.clone());
            for format in PluginFormat::ALL {
                match r.id_for(format) {
                    Ok(got) => {
                        assert_eq!(format, id.format());
                        assert_eq!(got, Some(&id));
                    }
                    Err(PluginError::Load { stage, reason }) => {
                        assert_ne!(format, id.format());
                        assert_eq!(stage, LoadStage::Opening);
                        assert!(reason.contains(&id.to_string()), "{reason}");
                        assert!(reason.contains(format.extension_id()), "{reason}");
                    }
                    Err(other) => panic!("{other:?}"),
                }
            }
        }
        for format in PluginFormat::ALL {
            assert_eq!(at("/p/x", None).id_for(format).unwrap(), None);
        }
    }

    /// **Identity is the id when both have one, the file otherwise.**
    #[test]
    fn two_refs_are_one_plugin_by_id_else_by_file() {
        let [clap, ..] = ids();
        let other = PluginId::Clap("b".into());
        let a = at("/p/bundle.clap", Some(clap.clone()));
        let b = at("/p/bundle.clap", Some(other));
        let first = at("/p/bundle.clap", None);
        let elsewhere = at("/q/bundle.clap", None);
        assert!(a.is_same_plugin(&a));
        assert!(!a.is_same_plugin(&b), "one file, two plugins");
        assert!(a.is_same_plugin(&first), "the file's first plugin: by file");
        assert!(first.is_same_plugin(&a));
        assert!(!first.is_same_plugin(&elsewhere));
        let vst3 = at("/p/bundle.clap", Some(ids()[1].clone()));
        assert!(!a.is_same_plugin(&vst3), "the format is part of it");
        assert!(!vst3.is_same_plugin(&first), "a vst3 id is not a clap file");
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
        let by_file = PluginRef::file(&file);
        let by_link = PluginRef::file(&link);
        assert!(by_file.is_same_plugin(&by_link));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **A ref reads as its id, else its file.**
    #[test]
    fn a_ref_displays_as_its_id_else_its_file() {
        let [clap, ..] = ids();
        assert_eq!(
            at("/p/bundle.clap", Some(clap)).to_string(),
            "clap.com.vendor.synth"
        );
        assert_eq!(at("/p/bundle.clap", None).to_string(), "/p/bundle.clap");
    }

    /// **A file's ref has the extension's format, and refuses what is no plugin.**
    #[test]
    fn a_file_ref_takes_its_format_from_the_extension() {
        let r = PluginRef::file("/p/Synth.clap");
        assert_eq!(r.format(), Ok(PluginFormat::Clap));
        assert_eq!(r.id(), None);
        assert!(PluginRef::file("/p/notes.txt").format().is_err());
        let named = PluginRef::new("/p/Plain.dll", PluginId::Vst2(1));
        assert_eq!(named.format(), Ok(PluginFormat::Vst2));
    }

    /// **A load that learns the plugin keeps a chosen id**: `identified` fills
    /// a missing id and never replaces one.
    #[test]
    fn identified_fills_a_missing_id_and_keeps_a_chosen_one() {
        let [clap, vst3, ..] = ids();
        let first = at("/p/x.clap", None);
        assert_eq!(first.identified(clap.clone()).id(), Some(&clap));
        let chosen = at("/p/x.clap", Some(clap.clone()));
        assert_eq!(chosen.identified(vst3).id(), Some(&clap));
    }

    /// **A four-character code reads back from its text for every byte in every
    /// position**, a `.` or `\` or a byte past ASCII escaped, and no two codes
    /// print alike.
    #[test]
    fn a_four_cc_round_trips_for_every_byte_in_every_position() {
        for position in 0..4 {
            for byte in 0..=255u8 {
                let mut bytes = *b"abcd";
                bytes[position] = byte;
                let code = FourCc::new(u32::from_be_bytes(bytes));
                let text = code.to_string();
                assert!(!text.contains('.'), "{text}: `.` splits an AU id");
                assert_eq!(text.parse::<FourCc>(), Ok(code), "{text}");
            }
        }
        let alike = [
            *b"a.cd",
            *b"a\\cd",
            [b'a', 0xa9, b'c', b'd'],
            [b'a', 0xc2, b'c', b'd'],
        ];
        let texts: std::collections::HashSet<_> = alike
            .iter()
            .map(|b| FourCc::new(u32::from_be_bytes(*b)).to_string())
            .collect();
        assert_eq!(texts.len(), alike.len(), "{texts:?}");
        assert_eq!(
            FourCc::new(u32::from_be_bytes([0xa9, b'n', b'a', b'm'])).to_string(),
            "\\xa9nam"
        );
        // An escape of a byte that prints as itself, a short or long code, a bad
        // escape: no text.
        for text in [
            "\\x41bcd", "abc", "abcde", "\\xa", "\\xZZbcd", "\\XA9nam", "ab.d",
        ] {
            assert!(text.parse::<FourCc>().is_err(), "{text}");
        }
    }

    /// **An AU id with a non-ASCII code round-trips whole**, as the probe's
    /// command line carries it.
    #[test]
    fn an_au_id_with_an_escaped_code_round_trips() {
        let id = PluginId::Au {
            component_type: AuComponentType::Effect,
            subtype: FourCc::new(u32::from_be_bytes([0xa9, b'.', b'\\', b' '])),
            manufacturer: FourCc::new(u32::from_be_bytes(*b"appl")),
        };
        assert_eq!(id.to_string().parse::<PluginId>(), Ok(id));
    }

    /// **Ids have one text each**: a class id reads in either case, a VST2 id is
    /// digits alone (no sign, no leading zero), and a CLAP id is not empty.
    #[test]
    fn an_id_has_one_text() {
        use crate::PluginIdError as E;
        let upper = "vst3.00112233-44556677-8899AABB-CCDDEEFF";
        assert_eq!(upper.to_lowercase().parse::<PluginId>(), upper.parse());
        for text in ["vst2.+5", "vst2.05", "vst2.", "vst2.-1", "vst2.4294967296"] {
            assert!(
                matches!(text.parse::<PluginId>(), Err(E::Vst2(_))),
                "{text}"
            );
        }
        assert_eq!("vst2.0".parse(), Ok(PluginId::Vst2(0)));
        assert_eq!("clap.".parse::<PluginId>(), Err(E::EmptyClap));
    }

    /// **A value the peer cannot make is a decode failure**: an empty CLAP id, a
    /// ref in the old shape (a project file written before ids were typed fails
    /// to read whole), and a known component code is
    /// read as its own type, never as `Unknown`.
    #[cfg(feature = "serde")]
    #[test]
    fn a_document_cannot_make_an_id_the_text_refuses() {
        let read = |json: &str| serde_json::from_str::<PluginRef>(json);
        assert!(read(r#"{"path":"/p/x.clap","choice":{"Plugin":{"Clap":""}}}"#).is_err());
        // The shape before ids were typed (`format` and a string `id`) is
        // refused whole, not read as the file's first plugin.
        assert!(read(r#"{"path":"/p/x.clap","format":"Clap","id":"com.vendor.synth"}"#).is_err());
        assert!(read(r#"{"path":"/p/x.clap","format":"Clap","id":null}"#).is_err());
        assert_eq!(
            read(r#"{"path":"/p/x.clap","choice":null}"#).unwrap(),
            PluginRef::file("/p/x.clap")
        );
        let unknown = AuComponentType::Unknown(AuComponentType::Effect.code());
        let back: AuComponentType =
            serde_json::from_str(&serde_json::to_string(&unknown).unwrap()).unwrap();
        assert_eq!(back, AuComponentType::Effect);
        let postcard = postcard::to_allocvec(&PluginId::Clap(String::new())).unwrap();
        assert!(postcard::from_bytes::<PluginId>(&postcard).is_err());
    }

    /// **A ref made `first_of` a format keeps it whatever the extension**: a
    /// `.so` that is CLAP is CLAP; the extension answers only when nothing else
    /// does; a plugin id's format wins over both.
    #[test]
    fn first_of_keeps_the_format_the_extension_does_not_name() {
        let link = PluginRef::first_of("/p/libfoo.bin", PluginFormat::Clap);
        assert_eq!(link.format(), Ok(PluginFormat::Clap));
        assert_eq!(link.id(), None);
        assert!(PluginRef::file("/p/libfoo.bin").format().is_err());
        assert!(link.is_same_plugin(&PluginRef::first_of("/p/libfoo.bin", PluginFormat::Clap)));
        assert!(!link.is_same_plugin(&PluginRef::first_of("/p/libfoo.bin", PluginFormat::Vst3)));
        let named = link.identified(PluginId::Vst2(3));
        assert_eq!(named.format(), Ok(PluginFormat::Vst2));
        assert_eq!(link.at("/q/libfoo.bin").format(), Ok(PluginFormat::Clap));
    }
}
