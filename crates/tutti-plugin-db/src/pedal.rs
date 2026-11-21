//! Dead-man's pedal: a sentinel file whose presence signals the scanner
//! crashed while probing a specific plugin. Next startup blacklists that
//! plugin so the scan can make progress.

use super::facts::FileFacts;
use super::fs::{canonical_path, replace_file};
use super::os_path;
use super::table::PluginDb;
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::warn;

#[derive(Debug, Clone)]
pub(super) struct Pedal {
    path: PathBuf,
}

impl Pedal {
    /// Pedal file lives at `path` for the duration of a single probe.
    pub(super) fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// The plugins the file names, in order: a crashed probe, and any entry a
    /// recovery could not save and kept. An absent file names none.
    ///
    /// # Errors
    ///
    /// The read's error when the file exists and cannot be read: its entries
    /// are unknown, which is not the same as none.
    fn entries(&self) -> std::io::Result<Vec<PathBuf>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(parse_pedal(&bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Writes `entries` (NUL-separated, which no path holds), or removes the
    /// file when there are none.
    fn write(&self, entries: &[PathBuf]) -> std::io::Result<()> {
        if entries.is_empty() {
            return match std::fs::remove_file(&self.path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        // Whole-file replace: a host dying mid-write must leave the old
        // entries, which may be the only record of a crash. (That window cannot
        // be hit from a test; `read_only_pedal_is_replaced_not_truncated`
        // distinguishes the two ways of writing.)
        replace_file(&self.path, |f| {
            for (i, entry) in entries.iter().enumerate() {
                if i > 0 {
                    f.write_all(&[0])?;
                }
                f.write_all(&os_path::stored_bytes(entry))?;
            }
            Ok(())
        })
    }

    /// Record the plugin that is about to be probed, keeping any entry a
    /// recovery could not save: overwriting it would forget a crash. Creates
    /// the pedal's directory, so the first run is protected too.
    pub(super) fn arm(&self, plugin: impl AsRef<Path>) -> std::io::Result<()> {
        let plugin = plugin.as_ref();
        let mut entries = self.entries()?;
        entries.retain(|p| p != plugin);
        entries.push(plugin.to_path_buf());
        self.write(&entries)
    }

    /// Drops every entry that names `file` (compared by canonical path): the
    /// user has taken the file off the blacklist, and a kept entry would put it
    /// back at the next recovery.
    ///
    /// # Errors
    ///
    /// The error of reading or rewriting the file; the entries are then as they
    /// were.
    pub(super) fn forget(&self, file: &Path) -> std::io::Result<()> {
        let file = canonical_path(file);
        let mut entries = self.entries()?;
        let before = entries.len();
        entries.retain(|p| canonical_path(p) != file);
        if entries.len() != before {
            self.write(&entries)?;
        }
        Ok(())
    }

    /// Remove the probe's entry on clean completion; a retained entry stays.
    ///
    /// # Errors
    ///
    /// The error of reading or rewriting the file; the entry then stays, and the
    /// next recovery blacklists the plugin.
    pub(super) fn disarm(&self, plugin: &Path) -> std::io::Result<()> {
        let mut entries = self.entries()?;
        entries.retain(|p| p != plugin);
        self.write(&entries)
    }

    /// If a pedal file survives from a crashed previous scan, blacklists the
    /// recorded files in `db`, hands `db` to `save`, and removes the pedal.
    /// Returns the files that were blacklisted, and what stopped the recovery
    /// short of removing the pedal, if anything.
    ///
    /// The pedal stays until `save` succeeds, so a host that dies again before
    /// the blacklist is on disk still finds it next start, and a scan that
    /// follows a failed save adds its own entry beside the kept ones. A pedal
    /// that cannot be read is left as it is.
    pub(super) fn recover(
        &self,
        db: &mut PluginDb,
        save: impl FnOnce(&PluginDb) -> std::io::Result<()>,
    ) -> Recovered {
        let entries = match self.entries() {
            Ok(entries) => entries,
            Err(e) => {
                warn!("could not read the dead-man's pedal: {e}");
                return Recovered {
                    files: Vec::new(),
                    failure: Some(RecoveryFailure::Unreadable(e.kind())),
                };
            }
        };
        // Keyed like a scan keys a file, whatever name the entry was written
        // under, so lookups by any alias find the row.
        let crashed: Vec<PathBuf> = entries.iter().map(|p| canonical_path(p)).collect();
        for p in &crashed {
            warn!(
                "dead-man's pedal found: blacklisting {:?} (crashed during previous scan)",
                p
            );
            db.blacklist(
                p,
                "crashed during previous scan".into(),
                &FileFacts::read(p),
            );
        }
        if !crashed.is_empty()
            && let Err(e) = save(db)
        {
            warn!("could not save the blacklist for {crashed:?}; keeping the pedal: {e}");
            return Recovered {
                files: crashed,
                failure: Some(RecoveryFailure::Unsaved(e.kind())),
            };
        }
        let failure = match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Some(RecoveryFailure::Kept(e.kind()))
            }
            _ => None,
        };
        Recovered {
            files: crashed,
            failure,
        }
    }
}

/// What a crash recovery did: the files it blacklisted, and what kept it from
/// finishing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[must_use = "a recovery that blacklisted files is something to tell the user"]
pub struct Recovered {
    /// The files blacklisted because the previous scan crashed on them, by
    /// canonical path.
    pub files: Vec<PathBuf>,
    /// Why the recovery did not finish, if it did not.
    pub failure: Option<RecoveryFailure>,
}

/// Why a crash recovery did not finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RecoveryFailure {
    /// The sentinel exists and could not be read, so the crash it records is
    /// unknown. It is left in place.
    #[error("the crash sentinel could not be read: {0}")]
    Unreadable(std::io::ErrorKind),
    /// The recovered blacklist could not be saved. It stands in memory, and the
    /// sentinel stays, so the next start blacklists the files again.
    #[error("the recovered blacklist could not be saved: {0}")]
    Unsaved(std::io::ErrorKind),
    /// The blacklist was saved and the sentinel could not be removed; the next
    /// start blacklists the same files again.
    #[error("the crash sentinel could not be removed: {0}")]
    Kept(std::io::ErrorKind),
}

/// The crash sentinel could not be read or rewritten: its entries are as they
/// were.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the crash sentinel could not be updated: {0}")]
pub struct PedalError(pub std::io::ErrorKind);

impl From<std::io::Error> for PedalError {
    fn from(error: std::io::Error) -> Self {
        Self(error.kind())
    }
}

/// Reads the plugins a pedal file names: NUL-separated paths, each trimmed of
/// surrounding ASCII whitespace; blank entries name nothing.
pub(super) fn parse_pedal(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|b| *b == 0)
        .map(<[u8]>::trim_ascii)
        .filter(|entry| !entry.is_empty())
        .map(os_path::from_bytes)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn arm_writes_and_disarm_removes() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        let plugin = Path::new("/plugins/test.vst3");

        pedal.arm(plugin).unwrap();
        assert!(pedal.path.exists());
        let contents = std::fs::read_to_string(&pedal.path).unwrap();
        assert_eq!(contents, "/plugins/test.vst3");

        pedal.disarm(plugin).unwrap();
        assert!(!pedal.path.exists());
    }

    #[test]
    fn recover_blacklists_crashed_plugin_and_saves_before_clearing_the_pedal() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        std::fs::write(&pedal.path, "/plugins/crashy.vst3").unwrap();

        let mut db = PluginDb::new();
        let mut saved = None;
        let crashed = pedal.recover(&mut db, |db| {
            assert!(pedal.path.exists(), "the pedal outlives the save");
            saved = Some(db.clone());
            Ok(())
        });

        assert_eq!(crashed.files, vec![PathBuf::from("/plugins/crashy.vst3")]);
        assert_eq!(crashed.failure, None);
        assert!(db.is_blacklisted(Path::new("/plugins/crashy.vst3")));
        assert_eq!(saved, Some(db));
        assert!(!pedal.path.exists());
    }

    #[test]
    fn recover_keeps_the_pedal_when_the_save_fails() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        std::fs::write(&pedal.path, "/plugins/crashy.vst3").unwrap();

        let mut db = PluginDb::new();
        let crashed = pedal.recover(&mut db, |_| Err(std::io::Error::other("disk full")));

        assert_eq!(crashed.files.len(), 1);
        assert_eq!(
            crashed.failure,
            Some(RecoveryFailure::Unsaved(std::io::ErrorKind::Other))
        );
        assert!(db.is_blacklisted(Path::new("/plugins/crashy.vst3")));
        assert!(pedal.path.exists());
    }

    #[test]
    fn recover_returns_none_when_no_pedal() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        let mut db = PluginDb::new();

        assert_eq!(pedal.recover(&mut db, |_| Ok(())), Recovered::default());
    }

    #[test]
    fn parse_pedal_names_the_plugin_without_surrounding_whitespace() {
        assert_eq!(
            parse_pedal("/plugins/crashy.vst3".as_bytes()),
            vec![PathBuf::from("/plugins/crashy.vst3")]
        );
        assert_eq!(
            parse_pedal("  /plugins/crashy.vst3\n".as_bytes()),
            vec![PathBuf::from("/plugins/crashy.vst3")]
        );
    }

    #[test]
    fn parse_pedal_names_nothing_for_blank_text() {
        assert!(parse_pedal("".as_bytes()).is_empty());
        assert!(parse_pedal(" \n\t ".as_bytes()).is_empty());
    }

    /// A path with interior spaces is one path.
    #[test]
    fn parse_pedal_keeps_interior_spaces() {
        assert_eq!(
            parse_pedal("/Library/My Plugins/A B.vst3".as_bytes()),
            vec![PathBuf::from("/Library/My Plugins/A B.vst3")]
        );
    }

    /// An unreadable or blank pedal is removed without blacklisting anything.
    #[test]
    fn recover_removes_a_blank_pedal_and_blacklists_nothing() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        std::fs::write(&pedal.path, "  \n").unwrap();

        let mut db = PluginDb::new();
        assert_eq!(pedal.recover(&mut db, |_| Ok(())), Recovered::default());
        assert!(db.is_empty());
        assert!(!pedal.path.exists());
    }

    /// On the first run the pedal's directory does not exist yet; arming must
    /// still protect the probe.
    #[test]
    fn arm_creates_the_pedals_directory() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join("fresh").join("state").join(".scanning"));

        pedal.arm("/plugins/test.vst3").unwrap();

        assert!(pedal.path.exists());
    }

    /// Arming beside an entry keeps it, and disarming removes only its own.
    #[test]
    fn entries_sit_side_by_side_and_disarm_removes_only_its_own() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        let (a, b) = (Path::new("/p/a.vst3"), Path::new("/p/b.vst3"));

        pedal.arm(a).unwrap();
        pedal.arm(b).unwrap();
        assert_eq!(pedal.entries().unwrap(), [a, b]);
        pedal.disarm(b).unwrap();
        assert_eq!(pedal.entries().unwrap(), [a]);
        pedal.disarm(a).unwrap();
        assert!(!pedal.path.exists());
    }

    /// `forget` drops the entries for a file and keeps the rest.
    #[test]
    fn forget_drops_only_the_named_files_entries() {
        let dir = TempDir::new().unwrap();
        let pedal = Pedal::new(dir.path().join(".scanning"));
        let (a, b) = (dir.path().join("a.vst3"), dir.path().join("b.vst3"));
        pedal.arm(&a).unwrap();
        pedal.arm(&b).unwrap();

        pedal.forget(&a).unwrap();

        assert_eq!(pedal.entries().unwrap(), std::slice::from_ref(&b));
        pedal.forget(&b).unwrap();
        assert!(!pedal.path.exists());
    }

    /// **An unreadable pedal is not an empty one**: a pedal path that is a
    /// directory exists and cannot be read, so its entries are unknown, and
    /// `arm`, `forget`, `disarm` and `recover` each say so (a recovery leaves the
    /// pedal in place) where an absent pedal reads as no entries.
    #[test]
    fn an_unreadable_pedal_is_an_error_and_an_absent_one_is_empty() {
        let dir = TempDir::new().unwrap();
        let absent = Pedal::new(dir.path().join("absent"));
        assert_eq!(absent.entries().unwrap(), Vec::<PathBuf>::new());

        let unreadable = Pedal::new(dir.path().join("a-directory"));
        std::fs::create_dir(&unreadable.path).unwrap();
        assert!(unreadable.entries().is_err());
        assert!(unreadable.arm("/p/a.vst3").is_err());
        assert!(unreadable.forget(Path::new("/p/a.vst3")).is_err());
        assert!(unreadable.disarm(Path::new("/p/a.vst3")).is_err());

        let mut db = PluginDb::new();
        let recovered = unreadable.recover(&mut db, |_| panic!("nothing to save"));
        assert!(recovered.files.is_empty());
        assert!(matches!(
            recovered.failure,
            Some(RecoveryFailure::Unreadable(_))
        ));
        assert!(unreadable.path.exists(), "an unreadable pedal is kept");
    }

    #[cfg(unix)]
    mod unix_tests {
        use super::*;

        /// **A pedal that cannot be rewritten fails `forget` and `disarm`** and keeps
        /// its entries: a read-only directory refuses the replacement file. Running as
        /// root defeats the permission check, so there the test cannot fail.
        #[test]
        fn forget_and_disarm_report_a_pedal_that_cannot_be_rewritten() {
            use std::os::unix::fs::PermissionsExt;
            let dir = TempDir::new().unwrap();
            let state = dir.path().join("state");
            std::fs::create_dir(&state).unwrap();
            let pedal = Pedal::new(state.join(".scanning"));
            let a = Path::new("/p/a.vst3");
            pedal.arm(a).unwrap();
            std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o555)).unwrap();
            let rewritable = std::fs::File::create(state.join("probe")).is_ok();

            let forgot = pedal.forget(a);
            let disarmed = pedal.disarm(a);
            std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();

            if !rewritable {
                assert!(forgot.is_err());
                assert!(disarmed.is_err());
                assert_eq!(pedal.entries().unwrap(), [a]);
            }
        }

        /// A name that is not UTF-8 survives the pedal, so the file that crashed is
        /// the file that is blacklisted.
        #[test]
        fn a_non_utf8_path_round_trips_through_the_pedal() {
            use std::os::unix::ffi::OsStrExt;
            let dir = TempDir::new().unwrap();
            let pedal = Pedal::new(dir.path().join(".scanning"));
            let plugin = PathBuf::from(std::ffi::OsStr::from_bytes(b"/plugins/caf\xe9.vst3"));
            assert!(plugin.to_str().is_none());

            pedal.arm(&plugin).unwrap();
            let mut db = PluginDb::new();
            let crashed = pedal.recover(&mut db, |_| Ok(()));

            assert_eq!(crashed.files, vec![plugin.clone()]);
            assert!(db.is_blacklisted(&plugin));
        }

        /// The pedal is replaced whole, never truncated in place: a read-only
        /// pedal file (replaceable, because the directory is writable) is rewritten
        /// without error, which an in-place `File::create` cannot do. Running as
        /// root defeats the permission check, so there the test cannot fail.
        #[test]
        fn read_only_pedal_is_replaced_not_truncated() {
            use std::os::unix::fs::PermissionsExt;
            let dir = TempDir::new().unwrap();
            let pedal = Pedal::new(dir.path().join(".scanning"));
            let (a, b) = (Path::new("/p/a.vst3"), Path::new("/p/b.vst3"));
            pedal.arm(a).unwrap();
            std::fs::set_permissions(&pedal.path, std::fs::Permissions::from_mode(0o444)).unwrap();

            pedal.arm(b).unwrap();

            assert_eq!(pedal.entries().unwrap(), [a, b]);
            assert_eq!(
                std::fs::read_dir(dir.path()).unwrap().count(),
                1,
                "no temp file"
            );
        }

        /// Entries written under an alias of the file (a link to its directory) are
        /// blacklisted under the canonical key a scan uses, and `forget` takes any
        /// name for the file. This is what a symlinked temp root (macOS `/var`)
        /// exercises.
        #[test]
        fn entries_under_an_alias_are_keyed_canonically() {
            let dir = TempDir::new().unwrap();
            let real = dir.path().join("real");
            std::fs::create_dir(&real).unwrap();
            let alias = dir.path().join("alias");
            std::os::unix::fs::symlink(&real, &alias).unwrap();
            let canonical = real.canonicalize().unwrap().join("a.vst3");
            let pedal = Pedal::new(dir.path().join(".scanning"));
            pedal.arm(alias.join("a.vst3")).unwrap();

            pedal.forget(&alias.join("a.vst3")).unwrap();
            assert!(!pedal.path.exists(), "forget took the alias");

            pedal.arm(alias.join("a.vst3")).unwrap();
            let mut db = PluginDb::new();
            let crashed = pedal.recover(&mut db, |_| Ok(()));
            assert_eq!(crashed.files, std::slice::from_ref(&canonical));
            assert!(db.is_blacklisted(&canonical));
            assert_eq!(db.len(), 1);
        }
    }
}
