//! Filesystem primitives for plugin discovery.

use super::facts::FileFacts;
use crate::mtime::Mtime;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use tracing::debug;
use tutti_plugin_types::PluginFormat;

impl FileFacts {
    /// Reads the facts for `path`.
    ///
    /// The scan's filesystem edge: every decision after it is a pure function of
    /// the value. For a plugin bundle (a directory) the modification time
    /// is the later of the directory's and its module's, since replacing the
    /// binary inside leaves the directory's own time alone. The module is found
    /// by `tutti_plugin_types::bundle`, the one bundle walk.
    pub fn read(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        Self::read_as(path, PluginFormat::from_path(path))
    }

    /// Reads the facts for `path`, taking the format from the caller instead of
    /// the extension. A scan uses it: it keys a file by its canonical path but
    /// knows the format from the name it was found under, and a link's target
    /// may be named anything.
    pub(crate) fn read_as(path: impl AsRef<Path>, format: Option<PluginFormat>) -> Self {
        let path = path.as_ref();
        let metadata = std::fs::metadata(path);
        let mut mtime = metadata.as_ref().ok().and_then(modified);
        if metadata.as_ref().is_ok_and(|m| m.is_dir())
            && let Some(module) = module_in_bundle(path, format)
        {
            let inner = std::fs::metadata(module).ok().and_then(|m| modified(&m));
            mtime = mtime.max(inner);
        }
        if metadata.is_ok() {
            Self::Present { mtime }
        } else {
            Self::Missing
        }
    }
}

/// The file's modification time, or `None` if the platform cannot read it or it
/// lies before the Unix epoch, which the stored record cannot hold.
fn modified(metadata: &std::fs::Metadata) -> Option<Mtime> {
    metadata.modified().ok().and_then(Mtime::new)
}

/// The module inside a bundle directory, preferring the one this target loads.
fn module_in_bundle(bundle: &Path, format: Option<PluginFormat>) -> Option<PathBuf> {
    use tutti_plugin_types::bundle::{any_module_in_bundle, native_module_in_bundle};
    let kind = format?.module_kind();
    native_module_in_bundle(bundle, kind).or_else(|| any_module_in_bundle(bundle, kind))
}

pub use tutti_plugin_types::canonical_path;

/// Recursively discovers plugin files across `dirs`, with the format each
/// extension names.
///
/// Each directory and each plugin is visited once, by its canonical path, so a
/// symlink that loops back to an ancestor ends the walk there and two routes to
/// one plugin report it once. A plugin is reported under its canonical path,
/// with the format its walked name carries, so the same file is the same key
/// whichever route or root found it. A path that cannot be resolved (a dangling
/// link) is skipped. Unreadable directories are skipped too, not reported:
/// discovery over a list of standard install locations must not fail because
/// one of them is absent.
pub(crate) fn discover(
    dirs: impl IntoIterator<Item = impl AsRef<Path>>,
) -> Vec<(PathBuf, PluginFormat)> {
    let mut walk = Walk::default();
    for dir in dirs {
        walk.dir(dir.as_ref());
    }
    walk.found
}

#[derive(Default)]
struct Walk {
    /// Canonical paths of the directories walked and the plugins found.
    seen: HashSet<PathBuf>,
    found: Vec<(PathBuf, PluginFormat)>,
}

impl Walk {
    /// VST3 bundles are directories with a `.vst3` extension, so
    /// `PluginFormat::from_path` is checked before recursing.
    fn dir(&mut self, dir: &Path) {
        let Ok(real) = dir.canonicalize() else {
            debug!("cannot resolve directory {:?}", dir);
            return;
        };
        if !self.seen.insert(real) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            debug!("cannot read directory {:?}", dir);
            return;
        };
        for path in entries.flatten().map(|e| e.path()) {
            match PluginFormat::from_path(&path) {
                Some(format) => self.plugin(path, format),
                None if path.is_dir() => self.dir(&path),
                None => {}
            }
        }
    }

    fn plugin(&mut self, path: PathBuf, format: PluginFormat) {
        let Ok(real) = path.canonicalize() else {
            debug!("cannot resolve plugin {:?}", path);
            return;
        };
        if self.seen.insert(real.clone()) {
            self.found.push((real, format));
        }
    }
}

/// Replaces `path` with what `fill` writes, leaving the old file if anything
/// fails. The temp file is a sibling so the rename stays on one filesystem
/// (a cross-device rename fails with EXDEV), named by the process id: the
/// pedal's writers are serialized per path by the scan's claim. Keeps the pedal
/// whole across a crash. Safe only under that claim: a second writer in the
/// process would share the temp name.
pub(super) fn replace_file(
    path: &Path,
    fill: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp-{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        fill(&mut f)?;
        // Durability across power loss cannot be observed by a test; the
        // contract this function's tests pin is the old-or-new one.
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_fake_plugin(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"fake plugin binary").unwrap();
        path
    }

    /// A `.VST3` bundle directory is reported as a plugin, not recursed into:
    /// walking into it would find nothing, silently.
    #[test]
    fn discover_treats_uppercase_bundle_dir_as_plugin() {
        let dir = TempDir::new().unwrap();
        let bundle = dir.path().join("Reverb.VST3");
        std::fs::create_dir_all(bundle.join("Contents").join("MacOS")).unwrap();
        std::fs::write(bundle.join("Contents").join("MacOS").join("Reverb"), b"x").unwrap();

        let found = discover([dir.path()]);
        assert_eq!(found.len(), 1, "expected the bundle itself, got {found:?}");
        assert_eq!(found[0].0, bundle.canonicalize().unwrap());
        assert_eq!(found[0].1, PluginFormat::Vst3);
    }

    #[test]
    fn discover_finds_plugins() {
        let dir = TempDir::new().unwrap();
        create_fake_plugin(dir.path(), "reverb.vst3");
        create_fake_plugin(dir.path(), "delay.clap");
        create_fake_plugin(dir.path(), "readme.txt");

        let found = discover([dir.path()]);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn discover_recurses_into_subdirs() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        create_fake_plugin(&sub, "deep.vst3");

        let found = discover([dir.path()]);
        assert_eq!(found.len(), 1);
    }

    /// The seen set spans the roots: a root inside another is not walked twice.
    #[test]
    fn overlapping_roots_report_each_plugin_once() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        create_fake_plugin(&sub, "a.vst3");

        let found = discover([dir.path(), sub.as_path()]);

        assert_eq!(found.len(), 1, "got {found:?}");
    }

    #[test]
    fn file_facts_read_reports_a_file_and_a_missing_path() {
        let dir = TempDir::new().unwrap();
        let file = create_fake_plugin(dir.path(), "a.clap");

        let present = FileFacts::read(&file);
        assert!(present.exists());
        assert!(present.mtime().is_some());

        let absent = FileFacts::read(dir.path().join("gone.vst3"));
        assert!(!absent.exists());
        assert_eq!(absent, FileFacts::Missing);
    }

    #[cfg(unix)]
    mod unix_tests {
        use super::*;

        /// A symlink back to an ancestor must not be walked again. Without the seen
        /// set the walk follows `loop` until the OS refuses (about 40 links deep) and
        /// reports the plugin once per level.
        #[test]
        fn a_symlink_cycle_ends_the_walk_and_reports_each_plugin_once() {
            let dir = TempDir::new().unwrap();
            let sub = dir.path().join("a").join("b");
            std::fs::create_dir_all(&sub).unwrap();
            create_fake_plugin(&sub, "deep.vst3");
            std::os::unix::fs::symlink(dir.path(), sub.join("loop")).unwrap();

            let found = discover([dir.path()]);

            assert_eq!(found.len(), 1, "got {found:?}");
            assert_eq!(found[0].0, sub.join("deep.vst3").canonicalize().unwrap());
        }

        /// Two routes to one plugin report it once (a file link and a directory
        /// link), and a dangling link not at all.
        #[test]
        fn a_plugin_reached_twice_is_reported_once_and_a_dangling_link_not_at_all() {
            let dir = TempDir::new().unwrap();
            let real = dir.path().join("real");
            std::fs::create_dir(&real).unwrap();
            let synth = create_fake_plugin(&real, "synth.clap");
            std::os::unix::fs::symlink(&synth, dir.path().join("link.clap")).unwrap();
            std::os::unix::fs::symlink(&real, dir.path().join("alias")).unwrap();
            std::os::unix::fs::symlink(dir.path().join("nowhere"), real.join("dead.vst3")).unwrap();

            let found = discover([dir.path()]);

            assert_eq!(found.len(), 1, "got {found:?}");
            assert_eq!(found[0].1, PluginFormat::Clap);
        }

        /// A plugin is reported under its canonical path with the format of the name
        /// it was found under, so a link to a file named anything keeps its format
        /// and two routes give one key.
        #[test]
        fn a_link_is_reported_under_its_target_with_the_links_format() {
            let dir = TempDir::new().unwrap();
            let target = create_fake_plugin(dir.path(), "libsynth.bin");
            let links = dir.path().join("links");
            std::fs::create_dir(&links).unwrap();
            std::os::unix::fs::symlink(&target, links.join("synth.clap")).unwrap();

            let found = discover([links.as_path()]);

            assert_eq!(
                found,
                vec![(target.canonicalize().unwrap(), PluginFormat::Clap)]
            );
        }

        /// A bundle directory holding one bare-stem binary, with the binary's and the
        /// directory's times set; returns the facts read.
        fn at(secs: u64) -> Option<Mtime> {
            Mtime::new(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs))
        }

        fn facts_of_bundle(name: &str, module_secs: u64, dir_secs: u64) -> FileFacts {
            let dir = TempDir::new().unwrap();
            let bundle = dir.path().join(name);
            let arch = bundle
                .join("Contents")
                .join(tutti_plugin_types::bundle::native_arch_subdirs()[0]);
            std::fs::create_dir_all(&arch).unwrap();
            let module = arch.join(name.split('.').next().unwrap());
            std::fs::write(&module, b"x").unwrap();
            let set = |p: &Path, secs: u64| {
                std::fs::File::open(p)
                    .unwrap()
                    .set_modified(at(secs).unwrap().get())
                    .unwrap();
            };
            set(&module, module_secs);
            set(&bundle, dir_secs);
            FileFacts::read(&bundle)
        }

        /// Replacing a bundle's binary leaves the directory's own time alone; the
        /// facts must still move, for every bundle format.
        #[test]
        fn a_bundle_whose_binary_changed_reads_as_changed() {
            for name in ["Reverb.vst3", "Legacy.vst", "Synth.clap", "Verb.component"] {
                let facts = facts_of_bundle(name, 2_000_000_000, 1_000_000_000);
                assert_eq!(facts.mtime(), at(2_000_000_000), "{name}");
                // The directory later than the binary still wins.
                let facts = facts_of_bundle(name, 1_000_000_000, 3_000_000_000);
                assert_eq!(facts.mtime(), at(3_000_000_000), "{name}");
            }
        }
    }
}
