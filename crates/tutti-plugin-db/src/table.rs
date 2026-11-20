//! The file table: the database's source of truth.
//!
//! A row is keyed by file path and holds what the scan learned about that file.
//! A crash blacklists a file before any plugin id is known, so the file, not a
//! plugin, is the unit of record. Plugins are read out of the rows.
//!
//! The table belongs to one thread, the control thread that holds the
//! [`PluginDb`]. It never crosses a thread: a scan works on a clone and sends
//! [`RowChange`]s back, and [`PluginDb::apply`] is the only code that changes a row.

use crate::facts::{self, FileFacts};
use crate::mtime::Mtime;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use tutti_plugin_types::{
    Described, PluginDescriptor, PluginFormat, PluginId, PluginRef, PluginRole,
};

/// What the database knows about one plugin file.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FileRow {
    /// The file's last modification when it was probed, or `None` if the file
    /// had no readable time. A row is out of date when the file's time differs
    /// from this one, and a row without one is always out of date.
    pub mtime: Option<Mtime>,
    /// What the probe found.
    pub state: FileState,
}

/// A probed row being made ([`FileRow::probed`]).
#[derive(Debug, Clone)]
#[must_use = "a Probed does nothing until build makes the row"]
pub struct Probed {
    format: PluginFormat,
    mtime: Option<Mtime>,
    plugins: Vec<Described>,
}

impl Probed {
    /// The plugins the file held, in the probe's order.
    pub fn plugins(mut self, plugins: Vec<Described>) -> Self {
        self.plugins = plugins;
        self
    }

    /// The row.
    ///
    /// # Errors
    ///
    /// [`FormatMismatch`] when a plugin's id is of another format than the
    /// row's: the row would name two formats for one file.
    pub fn build(self) -> Result<FileRow, FormatMismatch> {
        let row = FileRow {
            mtime: self.mtime,
            state: FileState::Probed {
                format: self.format,
                plugins: self.plugins,
            },
        };
        row.check_formats()?;
        Ok(row)
    }
}

/// A probed row holding a plugin id of another format than the row's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("a {} row holds the plugin id `{id}`", row.extension_id())]
pub struct FormatMismatch {
    /// The row's format.
    pub row: PluginFormat,
    /// The id of another format.
    pub id: PluginId,
}

/// What a probe made of a file.
///
/// A sum type, so a blacklisted file cannot also list plugins and a reason
/// cannot outlive the state that justifies it.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum FileState {
    /// The file probed as `format`.
    Probed {
        /// Which format's loader opened the file.
        format: PluginFormat,
        /// The plugins the file holds, in the probe's order: every descriptor
        /// of a CLAP factory, every audio class of a VST3 factory, every unit
        /// of an AU bundle, every sub-plugin of a VST2 shell; one for any other
        /// file.
        plugins: Vec<Described>,
    },
    /// The file brought a scan down and will not be opened.
    Blacklisted {
        /// Why, so a host can report it and offer to load the file anyway.
        reason: String,
    },
}

impl FileRow {
    /// The row of a file that probed as `format`, stamped with `facts`' time:
    /// [`Probed::plugins`] says what it held, and [`Probed::build`] refuses an id
    /// of another format.
    pub fn probed(format: PluginFormat, facts: &FileFacts) -> Probed {
        Probed {
            format,
            mtime: facts.mtime(),
            plugins: Vec::new(),
        }
    }

    /// Whether every plugin id of a probed row is of the row's format.
    ///
    /// # Errors
    ///
    /// [`FormatMismatch`] naming the first id that is not.
    pub fn check_formats(&self) -> Result<(), FormatMismatch> {
        let FileState::Probed { format, plugins } = &self.state else {
            return Ok(());
        };
        match plugins
            .iter()
            .filter_map(|p| p.id.as_ref())
            .find(|id| id.format() != *format)
        {
            Some(id) => Err(FormatMismatch {
                row: *format,
                id: id.clone(),
            }),
            None => Ok(()),
        }
    }

    /// The row for a blacklisted file, stamped with `facts`' time so that a
    /// reinstall or an update re-admits it. A blacklisted row names no format:
    /// the file never probed.
    #[must_use]
    pub fn blacklisted(reason: String, facts: &FileFacts) -> Self {
        Self {
            mtime: facts.mtime(),
            state: FileState::Blacklisted { reason },
        }
    }

    /// The format and plugins of a probed file; `None` when it is blacklisted.
    #[must_use]
    pub fn probe_result(&self) -> Option<(PluginFormat, &[Described])> {
        match &self.state {
            FileState::Probed { format, plugins } => Some((*format, plugins)),
            FileState::Blacklisted { .. } => None,
        }
    }

    /// The plugins the file holds; none when it is blacklisted.
    #[must_use]
    pub fn plugins(&self) -> &[Described] {
        self.probe_result().map_or(&[], |(_, plugins)| plugins)
    }

    /// The format the file probed as; none when it is blacklisted.
    #[must_use]
    pub fn format(&self) -> Option<PluginFormat> {
        self.probe_result().map(|(format, _)| format)
    }

    /// Whether this row is a blacklist and its file has not changed since, by
    /// `facts`: the rule [`PluginDb::is_blacklisted_and_unchanged`] applies to
    /// the row of a path.
    #[must_use]
    pub fn is_blacklisted_and_unchanged(&self, facts: &FileFacts) -> bool {
        facts::is_blacklisted_and_unchanged(self, facts)
    }

    /// Why the file is blacklisted, or `None` if it is not.
    #[must_use]
    pub fn blacklist_reason(&self) -> Option<&str> {
        match &self.state {
            FileState::Probed { .. } => None,
            FileState::Blacklisted { reason } => Some(reason),
        }
    }
}

/// One change to the table: replace a file's row, or drop it.
///
/// A change carries a whole row, so a reader sees a file's old row or its new one
/// and never a mixture.
#[derive(Debug, Clone, PartialEq)]
#[must_use = "a row change does nothing until PluginDb::apply takes it"]
pub struct RowChange {
    path: PathBuf,
    edit: Edit,
}

#[derive(Debug, Clone, PartialEq)]
enum Edit {
    Put(FileRow),
    Remove,
}

impl RowChange {
    /// Replace the row for `path`, or add one.
    pub fn put(path: impl Into<PathBuf>, row: FileRow) -> Self {
        Self {
            path: path.into(),
            edit: Edit::Put(row),
        }
    }

    /// Drop the row for `path`.
    pub fn remove(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            edit: Edit::Remove,
        }
    }

    /// The file the change is about.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The row the change puts, or `None` for a removal.
    #[must_use]
    pub fn row(&self) -> Option<&FileRow> {
        match &self.edit {
            Edit::Put(row) => Some(row),
            Edit::Remove => None,
        }
    }
}

/// One plugin, read out of a file's row.
#[derive(Debug, Clone, Copy)]
pub struct PluginEntry<'a> {
    /// The file that holds the plugin.
    pub path: &'a Path,
    /// Which format's loader opens it.
    pub format: PluginFormat,
    /// The format's own id for it; `None` for a plugin listed by its file name
    /// alone (a scan whose probe was unavailable), which names the file's first
    /// plugin.
    pub id: Option<&'a PluginId>,
    /// What the probe reported.
    pub descriptor: &'a PluginDescriptor,
}

impl PluginEntry<'_> {
    /// The plugin as a ref: its file and id.
    #[must_use]
    pub fn plugin_ref(&self) -> PluginRef {
        match self.id {
            Some(id) => PluginRef::new(self.path, id.clone()),
            None => PluginRef::first_of(self.path, self.format),
        }
    }

    /// What the plugin is (instrument, effect, ...), normalized across formats.
    #[must_use]
    pub fn role(&self) -> PluginRole {
        self.descriptor.class.role()
    }

    /// Whether the plugin is a note-driven sound source.
    ///
    /// A [`Generator`](PluginRole::Generator) is deliberately not an
    /// instrument: it makes sound without being played.
    #[must_use]
    pub fn is_instrument(&self) -> bool {
        self.role() == PluginRole::Instrument
    }
}

/// A blacklisted file and why it is hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlacklistedFile<'a> {
    /// The file.
    pub path: &'a Path,
    /// The recorded reason.
    pub reason: &'a str,
}

/// The plugin database: a table of files, and the plugins read out of it.
///
/// Plain data with no I/O. With feature `serde` it serializes as a versioned
/// record and deserializes only a valid one. Every change goes through
/// [`apply`](Self::apply).
///
/// A path matches exactly; the table never touches the filesystem to resolve
/// one. A scan and the pedal's recovery key
/// rows by [`canonical_path`](crate::canonical_path), so a caller that reads
/// this table directly asks with a canonical path too: an alias of a file finds
/// no row.
#[derive(Debug, Clone, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "crate::wire::Raw"))]
pub struct PluginDb {
    files: BTreeMap<PathBuf, FileRow>,
}

impl PluginDb {
    /// An empty database.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The only code that changes a row.
    pub fn apply(&mut self, change: RowChange) {
        match change.edit {
            Edit::Put(row) => {
                self.files.insert(change.path, row);
            }
            Edit::Remove => {
                self.files.remove(&change.path);
            }
        }
    }

    /// The row for `path`, if any.
    #[must_use]
    pub fn row(&self, path: &Path) -> Option<&FileRow> {
        self.files.get(path)
    }

    /// Every row, blacklisted ones included, in path order.
    pub fn rows(&self) -> impl Iterator<Item = (&Path, &FileRow)> {
        self.files.iter().map(|(path, row)| (path.as_path(), row))
    }

    /// The number of files with a row.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether no file has a row.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The plugins of every file that is not blacklisted.
    pub fn plugins(&self) -> impl Iterator<Item = PluginEntry<'_>> {
        self.rows()
            .filter_map(|(path, row)| {
                row.probe_result()
                    .map(|(format, plugins)| (path, format, plugins))
            })
            .flat_map(|(path, format, plugins)| {
                plugins.iter().map(move |plugin| PluginEntry {
                    path,
                    format,
                    id: plugin.id.as_ref(),
                    descriptor: &plugin.descriptor,
                })
            })
    }

    /// The blacklisted files, so a UI can show what was hidden and why.
    pub fn blacklisted(&self) -> impl Iterator<Item = BlacklistedFile<'_>> {
        self.rows().filter_map(|(path, row)| {
            row.blacklist_reason()
                .map(|reason| BlacklistedFile { path, reason })
        })
    }

    /// Whether `path` has a row and it is blacklisted.
    #[must_use]
    pub fn is_blacklisted(&self, path: &Path) -> bool {
        self.row(path)
            .is_some_and(|row| row.blacklist_reason().is_some())
    }

    /// Whether `path` is blacklisted and its file has not changed since.
    ///
    /// The check a scan and an open must use, not the raw
    /// [`is_blacklisted`](Self::is_blacklisted): blacklisting records the file's
    /// time, so a reinstall or an update re-admits the plugin. A permanent
    /// blacklist would be wrong, because the pedal fires on a force-quit, a power
    /// loss and an out-of-memory kill as readily as on a plugin crash.
    #[must_use]
    pub fn is_blacklisted_and_unchanged(&self, path: &Path, facts: &FileFacts) -> bool {
        self.row(path)
            .is_some_and(|row| row.is_blacklisted_and_unchanged(facts))
    }

    /// Blacklists `path` with a reason, stamped with the file's time in `facts`.
    pub fn blacklist(&mut self, path: &Path, reason: String, facts: &FileFacts) {
        self.apply(RowChange::put(path, FileRow::blacklisted(reason, facts)));
    }

    /// Lifts the blacklist on `path` by dropping its row, so the next scan
    /// probes the file again. A blacklisted row holds no plugins to keep.
    ///
    /// Returns whether a blacklisted row was found. The inverse of
    /// [`blacklist`](Self::blacklist); without it one pedal misfire hides a
    /// plugin until the file is edited by hand.
    pub fn unblacklist(&mut self, path: &Path) -> bool {
        let found = self.is_blacklisted(path);
        if found {
            self.apply(RowChange::remove(path));
        }
        found
    }

    /// Lifts every blacklist and returns the paths that were blacklisted.
    pub fn clear_blacklist(&mut self) -> Vec<PathBuf> {
        let paths: Vec<PathBuf> = self.blacklisted().map(|b| b.path.to_path_buf()).collect();
        for path in &paths {
            self.apply(RowChange::remove(path));
        }
        paths
    }

    /// Drops every row whose path is not in `present` and returns the dropped
    /// paths. `present` is the set of row paths that exist on disk, gathered by
    /// the caller.
    pub fn prune_missing(&mut self, present: &HashSet<PathBuf>) -> Vec<PathBuf> {
        let missing: Vec<PathBuf> = self
            .rows()
            .map(|(path, _)| path)
            .filter(|path| !present.contains(*path))
            .map(Path::to_path_buf)
            .collect();
        for path in &missing {
            self.apply(RowChange::remove(path));
        }
        missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{ScanDecision, classify};
    use tutti_plugin_types::{PluginClass, Uid};

    const T: u64 = 1_700_000_000;

    pub(crate) fn facts(mtime: u64) -> FileFacts {
        FileFacts::Present {
            mtime: Mtime::new(
                std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(mtime),
            ),
        }
    }

    fn row(mtime: u64) -> FileRow {
        FileRow::probed(PluginFormat::Vst3, &facts(mtime))
            .plugins(vec![Described {
                id: Some(PluginId::Vst3(Uid::new([1; 16]))),
                descriptor: PluginDescriptor::new("name", PluginClass::Unknown),
            }])
            .build()
            .expect("ids of the row's format")
    }

    fn db_with(path: &str, row: FileRow) -> PluginDb {
        let mut db = PluginDb::new();
        db.apply(RowChange::put(path, row));
        db
    }

    #[test]
    fn apply_puts_replaces_and_removes_a_row() {
        let mut db = db_with("/p/a.vst3", row(T));
        db.apply(RowChange::put("/p/a.vst3", row(T + 1)));
        assert_eq!(db.len(), 1);
        assert_eq!(
            db.row(Path::new("/p/a.vst3")).unwrap().mtime,
            facts(T + 1).mtime()
        );

        db.apply(RowChange::remove("/p/a.vst3"));
        assert!(db.is_empty());
        db.apply(RowChange::remove("/p/absent.vst3"));
        assert!(db.is_empty());
    }

    #[test]
    fn a_path_with_no_row_needs_a_rescan() {
        let db = PluginDb::new();
        assert_eq!(
            classify(db.row(Path::new("/p/unknown.vst3")), &facts(T)),
            ScanDecision::Probe
        );
    }

    #[test]
    fn needs_rescan_follows_the_facts_time() {
        let db = db_with("/p/a.vst3", row(T));
        let path = Path::new("/p/a.vst3");
        assert_eq!(classify(db.row(path), &facts(T)), ScanDecision::UpToDate);
        assert_eq!(classify(db.row(path), &facts(T + 1)), ScanDecision::Probe);
    }

    #[test]
    fn a_blacklist_holds_only_while_the_file_is_unchanged() {
        let mut db = PluginDb::new();
        db.blacklist(Path::new("/p/a.vst3"), "crashed".into(), &facts(T));
        let a = Path::new("/p/a.vst3");
        assert!(db.is_blacklisted_and_unchanged(a, &facts(T)));
        assert!(!db.is_blacklisted_and_unchanged(a, &facts(T + 1)));
        assert!(db.is_blacklisted(a), "the flag outlives the file");
        assert!(!db.is_blacklisted_and_unchanged(Path::new("/p/other.vst3"), &facts(T)));
    }

    #[test]
    fn blacklist_stamps_the_facts_time_on_a_new_and_on_an_existing_row() {
        let mut db = PluginDb::new();
        db.blacklist(Path::new("/p/new.vst3"), "boom".into(), &facts(T));
        assert!(db.is_blacklisted_and_unchanged(Path::new("/p/new.vst3"), &facts(T)));

        db.apply(RowChange::put("/p/old.vst3", row(T)));
        db.blacklist(Path::new("/p/old.vst3"), "boom".into(), &facts(T + 7));
        assert!(db.is_blacklisted_and_unchanged(Path::new("/p/old.vst3"), &facts(T + 7)));
        assert_eq!(db.len(), 2);
    }

    /// A blacklisted file lists no plugin and is listed as hidden.
    #[test]
    fn a_blacklisted_file_lists_no_plugins_and_a_probed_one_lists_its_own() {
        let mut db = db_with("/p/good.vst3", row(T));
        db.apply(RowChange::put("/p/bad.vst3", row(T)));
        db.blacklist(Path::new("/p/bad.vst3"), "boom".into(), &facts(T));

        let listed: Vec<_> = db.plugins().map(|p| p.path).collect();
        assert_eq!(listed, [Path::new("/p/good.vst3")]);
        let hidden: Vec<_> = db.blacklisted().collect();
        assert_eq!(hidden.len(), 1);
        assert_eq!(hidden[0].reason, "boom");
    }

    /// A blacklisted row names no format: the file never probed.
    #[test]
    fn a_blacklisted_row_names_no_format_and_a_probed_one_names_its_own() {
        let mut db = db_with(
            "/p/good.clap",
            FileRow::probed(PluginFormat::Clap, &facts(T))
                .plugins(vec![])
                .build()
                .expect("ids of the row's format"),
        );
        db.blacklist(Path::new("/p/bad.vst3"), "boom".into(), &facts(T));
        assert_eq!(
            db.row(Path::new("/p/good.clap")).unwrap().format(),
            Some(PluginFormat::Clap)
        );
        assert_eq!(db.row(Path::new("/p/bad.vst3")).unwrap().format(), None);
    }

    /// A file may hold several plugins and each is listed under the file's path.
    #[test]
    fn a_file_with_several_plugins_lists_each() {
        let ids = [1, 2].map(|n| {
            Described::new(
                PluginId::Vst2(n),
                PluginDescriptor::new(["a", "b"][n as usize - 1], PluginClass::Unknown),
            )
        });
        let db = db_with(
            "/p/shell.vst",
            FileRow::probed(PluginFormat::Vst2, &facts(T))
                .plugins(ids.to_vec())
                .build()
                .expect("ids of the row's format"),
        );
        let names: Vec<_> = db.plugins().map(|p| p.descriptor.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
    }

    /// Lifting a blacklist drops the row, so the next scan probes the file
    /// whatever its time.
    #[test]
    fn unblacklist_drops_the_row_so_the_file_is_probed_again() {
        let mut db = PluginDb::new();
        db.blacklist(Path::new("/p/a.vst3"), "boom".into(), &facts(T));
        assert!(db.unblacklist(Path::new("/p/a.vst3")));
        assert!(db.row(Path::new("/p/a.vst3")).is_none());
        assert_eq!(
            classify(db.row(Path::new("/p/a.vst3")), &facts(T)),
            ScanDecision::Probe
        );
        assert!(!db.unblacklist(Path::new("/p/a.vst3")));

        db.apply(RowChange::put("/p/healthy.vst3", row(T)));
        assert!(!db.unblacklist(Path::new("/p/healthy.vst3")));
        assert_eq!(db.len(), 1, "a healthy row is left alone");
    }

    #[test]
    fn clear_blacklist_lifts_exactly_the_blacklisted_rows() {
        let mut db = db_with("/p/ok.vst3", row(T));
        db.blacklist(Path::new("/p/a.vst3"), "x".into(), &facts(T));
        db.blacklist(Path::new("/p/b.vst3"), "y".into(), &facts(T));

        let mut cleared = db.clear_blacklist();
        cleared.sort();
        assert_eq!(
            cleared,
            [PathBuf::from("/p/a.vst3"), PathBuf::from("/p/b.vst3")]
        );
        assert_eq!(db.len(), 1);
        assert_eq!(db.blacklisted().count(), 0);
    }

    #[test]
    fn prune_missing_removes_and_reports_exactly_the_absent_rows() {
        let mut db = db_with("/p/here.vst3", row(T));
        db.blacklist(Path::new("/p/gone.vst3"), "x".into(), &facts(T));
        let present: HashSet<PathBuf> = [PathBuf::from("/p/here.vst3")].into();

        let pruned = db.prune_missing(&present);

        assert_eq!(pruned, vec![PathBuf::from("/p/gone.vst3")]);
        assert_eq!(db.len(), 1);
        assert!(db.row(Path::new("/p/here.vst3")).is_some());
    }

    /// **A row holds one format**: a plugin id of another format is refused when
    /// the row is made, naming both.
    #[test]
    fn a_row_refuses_an_id_of_another_format() {
        let stray = Described::new(
            PluginId::Vst2(1),
            PluginDescriptor::new("x", PluginClass::Unknown),
        );
        let refused = FileRow::probed(PluginFormat::Vst3, &facts(T))
            .plugins(vec![stray])
            .build()
            .expect_err("a VST2 id in a VST3 row");
        assert_eq!(
            refused,
            FormatMismatch {
                row: PluginFormat::Vst3,
                id: PluginId::Vst2(1)
            }
        );
        assert!(refused.to_string().contains("vst3") && refused.to_string().contains("vst2.1"));
    }

    /// **An id-less listing keeps its row's format in its ref**, whatever its
    /// path's extension: a link keyed by its target `libfoo.bin` is still CLAP.
    #[test]
    fn an_id_less_entry_keeps_its_rows_format() {
        let listing = Described {
            id: None,
            descriptor: PluginDescriptor::new("foo", PluginClass::Unknown),
        };
        let db = db_with(
            "/p/libfoo.bin",
            FileRow::probed(PluginFormat::Clap, &facts(T))
                .plugins(vec![listing])
                .build()
                .expect("no ids to disagree"),
        );
        let entry = db.plugins().next().expect("one entry");
        assert_eq!(entry.plugin_ref().format(), Ok(PluginFormat::Clap));
        assert_eq!(entry.plugin_ref().id(), None);
    }
}
