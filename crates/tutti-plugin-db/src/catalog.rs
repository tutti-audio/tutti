//! The [`Catalog`]: scans plugin directories, keeps what it found and says what
//! is blacklisted. It does not open a plugin: [`Catalog::resolve`] names the
//! plugin an id stands for as a [`Loadable`], the only thing a load takes, and
//! the load asks the catalog's blacklist where it opens the plugin.
//!
//! The caller supplies the persistence and which directories to scan: this
//! crate has no opinion on OS conventions, app names or file formats. The
//! persistence is a closure from `&PluginDb` to a typed `Result`; with the
//! `serde` feature a `PluginDb` is `Serialize` and `Deserialize`, so the app
//! picks the format and where the bytes live.
//!
//! ```no_run
//! # fn ex() -> Result<(), tutti_plugin_db::ScanError> {
//! use tutti_plugin_db::{Catalog, CatalogConfig, PersistError};
//! use tutti_plugin_db::{PluginDb, ScanPoll};
//! use std::path::PathBuf;
//!
//! let mut catalog = Catalog::new(
//!     PluginDb::new(), // or the one the app deserialized
//!     CatalogConfig::new(PathBuf::from("/my/app/state"))
//!         .scan_dirs([PathBuf::from("/Library/Audio/Plug-Ins/VST3")]),
//!     |db| {
//!         // Write `db` wherever the app keeps it.
//!         let _ = db;
//!         Ok::<(), PersistError>(())
//!     },
//! );
//! let mut scan = catalog.scan()?;
//! loop {
//!     match catalog.try_apply(&mut scan) {
//!         ScanPoll::Running => std::thread::sleep(std::time::Duration::from_millis(10)),
//!         ScanPoll::Done(result) => break println!("{} new", result.new),
//!         ScanPoll::Died => break,
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # Threads
//!
//! A [`Catalog`] is owned by one thread, the one that calls its methods. Its
//! three discovery operations, [`scan`](Catalog::scan),
//! [`register`](Catalog::register) and [`measure`](Catalog::measure), each
//! return a task ([`Scan`]) that works on a snapshot on its own thread and sends
//! [`RowChange`]s back; [`try_apply`](Catalog::try_apply) applies them on the owner
//! and never blocks. The catalog is readable throughout. Dropping a task
//! cancels it: the probe process in flight is killed and the drop returns
//! without waiting for the probe's deadline. Nothing here runs on or reaches
//! the audio thread.

use crate::loadable::{Loadable, Resolved};
use crate::probe::ProbeState;
use crate::probing::{Probing, probe_fn};
use crate::{
    BlacklistedFile, CatalogConfig, FileFacts, FileRow, FilesError, PluginDb, PluginEntry,
    RowChange, Scan, ScanBusy, ScanError, ScanPoll, Scanner,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use tutti_plugin_types::{IpcError, IpcResult};
use tutti_plugin_types::{PluginDescriptor, PluginFormat, PluginId, PluginRef};
use tutti_rt::LockExt;

/// Why the persistence a catalog was given could not store the database.
///
/// The persistence closure builds one from whatever failed (an I/O error, a
/// serializer's error); the catalog hands it back from [`Catalog::save`] and
/// [`Catalog::take_persist_error`].
#[derive(Debug, thiserror::Error)]
#[error("the plugin database could not be persisted: {source}")]
pub struct PersistError {
    #[source]
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl PersistError {
    /// A failure of the persistence, from its underlying error.
    pub fn new(source: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self {
            source: source.into(),
        }
    }
}

/// How a catalog stores its database: the host's own closure.
type Persist = Arc<dyn Fn(&PluginDb) -> Result<(), PersistError> + Send + Sync>;

/// The failure of the last save a catalog started itself, until it is taken.
type Unreported = Arc<Mutex<Option<PersistError>>>;

/// A catalog of scanned plugins that can list and blacklist them, and says
/// whether a load may open one ([`check`](Self::check)).
///
/// Wraps a [`PluginDb`] and the persistence the host supplies
/// ([`Catalog::new`]). Every change is in memory until [`save`](Self::save) is
/// called, or a scan or a crash recovery saves it. A scan's save runs on its
/// own thread: an app polls [`take_persist_error`](Self::take_persist_error)
/// after each scan's [`Done`](ScanPoll::Done).
pub struct Catalog {
    db: PluginDb,
    /// The blacklisted rows of `db`, republished by every method that changes
    /// the table: what a [`BlacklistView`] reads.
    blacklist: Arc<RwLock<PluginDb>>,
    config: CatalogConfig,
    persist: Persist,
    unreported: Unreported,
    /// The write a finished rescan started on its own thread.
    pending_save: Option<tutti_platform::ThreadHandle<()>>,
}

impl std::fmt::Debug for Catalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Catalog")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Drop for Catalog {
    fn drop(&mut self) {
        self.finish_pending_save();
    }
}

/// Runs `persist` for a save the catalog starts itself. A failure is logged and
/// kept in `unreported` for [`Catalog::take_persist_error`]; a success clears
/// an older failure, since the newer table is what is stored.
fn persist_unattended(
    persist: &Persist,
    unreported: &Unreported,
    db: &PluginDb,
) -> Result<(), String> {
    let outcome = persist(db);
    let mut slot = unreported.lock_recover();
    match outcome {
        Ok(()) => {
            *slot = None;
            Ok(())
        }
        Err(e) => {
            tracing::warn!("{e}");
            let message = e.to_string();
            *slot = Some(e);
            Err(message)
        }
    }
}

/// The key a path has in the database: what a scan keys the file by, so an
/// alias and the real path are one row, also when the file is gone (its parent
/// is resolved and its name kept).
fn key(path: &Path) -> PathBuf {
    crate::canonical_path(path)
}

/// A database holding only the blacklisted rows of `db`.
fn blacklisted_rows(db: &PluginDb) -> PluginDb {
    let mut rows = PluginDb::new();
    for (path, row) in db
        .rows()
        .filter(|(_, row)| row.blacklist_reason().is_some())
    {
        rows.apply(RowChange::put(path, row.clone()));
    }
    rows
}

/// `db`'s blacklisted rows when they are not what `published` holds, else
/// `None`: what a change has to publish.
fn moved_blacklist(published: &PluginDb, db: &PluginDb) -> Option<PluginDb> {
    let rows = blacklisted_rows(db);
    (rows != *published).then_some(rows)
}

/// The row `db` holds for `canonical` when it is a blacklist: an owned copy,
/// so a view's read guard is a statement temporary dropped before the file is
/// read. The return type is what keeps a lock off the file read.
fn blacklist_row(db: &PluginDb, canonical: &Path) -> Option<FileRow> {
    db.row(canonical)
        .filter(|row| row.blacklist_reason().is_some())
        .cloned()
}

/// The refusal of a blacklist's `row` for `plugin`, whose file is read now: the
/// one rule behind [`Catalog::check`] and
/// [`BlacklistView::check`].
fn refusal(row: Option<FileRow>, canonical: &Path, plugin: &PluginRef) -> IpcResult<()> {
    let Some(row) = row else {
        return Ok(());
    };
    if !row.is_blacklisted_and_unchanged(&FileFacts::read(canonical)) {
        return Ok(());
    }
    Err(IpcError::Blacklisted {
        path: plugin.path().to_path_buf(),
        reason: row
            .blacklist_reason()
            .unwrap_or("no reason recorded")
            .to_string(),
    })
}

/// A catalog's blacklist as it is when asked: made by
/// [`Catalog::blacklist_view`], `Send` and cheap to clone, so a load that runs
/// on another thread asks it when it opens, after any change the owning thread
/// made since.
#[derive(Debug, Clone)]
pub struct BlacklistView {
    rows: Arc<RwLock<PluginDb>>,
}

impl BlacklistView {
    /// A blacklist that holds nothing: what a plugin no catalog resolved
    /// ([`Loadable::uncatalogued`]) is checked against.
    pub(crate) fn empty() -> Self {
        Self {
            rows: Arc::new(RwLock::new(PluginDb::new())),
        }
    }

    /// As [`Catalog::check`], against the catalog's blacklist now.
    ///
    /// # Errors
    ///
    /// [`IpcError::Blacklisted`] with the recorded reason when the plugin's
    /// file is blacklisted and unchanged.
    pub fn check(&self, plugin: &PluginRef) -> IpcResult<()> {
        let canonical = key(plugin.path());
        let row = blacklist_row(
            &self.rows.read().unwrap_or_else(PoisonError::into_inner),
            &canonical,
        );
        refusal(row, &canonical, plugin)
    }
}

impl Catalog {
    /// The one way the table changes: runs `f` over the table and the
    /// configuration, then republishes the blacklisted rows to the views, so no
    /// change leaves a [`BlacklistView`] behind.
    fn change_with<R>(&mut self, f: impl FnOnce(&mut PluginDb, &CatalogConfig) -> R) -> R {
        let out = f(&mut self.db, &self.config);
        self.republish_blacklist();
        out
    }

    /// [`change_with`](Self::change_with) for a caller that scans: it gets a
    /// scanner on the catalog's own probe. Only this builds one.
    fn change<R>(&mut self, f: impl FnOnce(&mut PluginDb, &Scanner, &CatalogConfig) -> R) -> R {
        let scanner = self.scanner(self.scanning());
        self.change_with(|db, config| f(db, &scanner, config))
    }

    /// Publishes the table's blacklisted rows to the views when they differ
    /// from what is published. Each call builds a table of the blacklisted
    /// rows, before any lock is taken; the write lock is held for the swap
    /// alone and only when the blacklist moved, so the poll that ends every UI
    /// tick of a scan does not stop a reader otherwise.
    fn republish_blacklist(&self) {
        let moved = moved_blacklist(
            &self
                .blacklist
                .read()
                .unwrap_or_else(PoisonError::into_inner),
            &self.db,
        );
        if let Some(rows) = moved {
            *self
                .blacklist
                .write()
                .unwrap_or_else(PoisonError::into_inner) = rows;
        }
    }

    /// What a scan's probe does: measures if the catalog was told to, and lists
    /// a file by its name when `plugin-probe` is missing.
    fn scanning(&self) -> Probing {
        Probing {
            measure: self.config.measuring,
            state: None,
            fallback: true,
        }
    }

    /// A scanner on this catalog's pedal whose probe does what `how` says.
    fn scanner(&self, how: Probing) -> Scanner {
        Scanner::new(self.config.pedal_path(), probe_fn(&self.config, how))
    }

    /// The blacklist as a handle that outlives this borrow and is `Send`: what
    /// a load that runs on another thread asks, so it sees a plugin blacklisted
    /// after the load was made.
    pub fn blacklist_view(&self) -> BlacklistView {
        BlacklistView {
            rows: Arc::clone(&self.blacklist),
        }
    }

    /// Creates a catalog over `db`, which `persist` stores.
    ///
    /// `persist` receives the table and stores it wherever the host keeps it. It
    /// runs on the owning thread for [`save`](Self::save) and for a crash
    /// recovery, and on a short-lived thread for the save after a rescan, which
    /// gets a copy of the table. A crash recovery removes its sentinel only
    /// after `persist` succeeds.
    #[must_use]
    pub fn new(
        db: PluginDb,
        config: CatalogConfig,
        persist: impl Fn(&PluginDb) -> Result<(), PersistError> + Send + Sync + 'static,
    ) -> Self {
        let blacklist = Arc::new(RwLock::new(blacklisted_rows(&db)));
        Self {
            db,
            blacklist,
            config,
            persist: Arc::new(persist),
            unreported: Unreported::default(),
            pending_save: None,
        }
    }

    /// The database, for reading rows this catalog has no method for.
    ///
    /// Its rows are keyed by [`canonical_path`](crate::canonical_path),
    /// which is what a scan and every method of this catalog key by. The table
    /// itself matches paths exactly, so look a file up by its canonical path
    /// (an alias finds nothing); the catalog's own `is_blacklisted`, `blacklist`
    /// and the rest canonicalize for you.
    pub fn db(&self) -> &PluginDb {
        &self.db
    }

    /// The save the scanner and the pedal's recovery run: `persist`, with its
    /// failure kept for [`take_persist_error`](Self::take_persist_error).
    fn autosave(&self) -> impl FnOnce(&PluginDb) -> std::io::Result<()> + use<> {
        let (persist, unreported) = (Arc::clone(&self.persist), Arc::clone(&self.unreported));
        move |db| persist_unattended(&persist, &unreported, db).map_err(std::io::Error::other)
    }

    /// Starts a scan of the plugin directories on a background thread.
    ///
    /// Recovers a crashed previous scan first, then hands the scanner a
    /// snapshot. This catalog stays whole and usable while the scan runs; call
    /// [`try_apply`](Self::try_apply) on the owning thread (a frame tick, a UI
    /// loop) to apply what the scan found. Dropping the task cancels the scan:
    /// the probe in flight is killed and the drop returns promptly.
    ///
    /// # Errors
    ///
    /// [`ScanError::Busy`] while a task, or a recovery, is already using this
    /// catalog's pedal: its worker has not exited yet. [`ScanError::Spawn`] when
    /// the operating system cannot spawn the scanner thread.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use tutti_plugin_db::Catalog;
    /// # use tutti_plugin_db::ScanPoll;
    /// # fn ex(mut plugins: Catalog) -> Result<(), tutti_plugin_db::ScanError> {
    /// let mut scan = plugins.scan()?;
    /// loop {
    ///     for progress in scan.progress() {
    ///         println!("{progress:?}");
    ///     }
    ///     match plugins.try_apply(&mut scan) {
    ///         ScanPoll::Running => std::thread::sleep(std::time::Duration::from_millis(10)),
    ///         ScanPoll::Done(result) => break println!("{} new", result.new),
    ///         ScanPoll::Died => break,
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn scan(&mut self) -> std::result::Result<Scan, ScanError> {
        self.finish_pending_save();
        let save = self.autosave();
        self.change(|db, scanner, config| scanner.spawn(db, &config.scan_dirs, save))
    }

    /// Probes the file at `path` on a background thread, whether or not the
    /// catalog holds a current row for it, and records what it holds, as a scan
    /// would; the task is the one [`scan`](Self::scan) returns, so
    /// [`try_apply`](Self::try_apply) applies its change. The way to add a plugin
    /// outside the scanned directories. The file is probed as a scan probes it,
    /// and a plugin that crashes, hangs or will not load is **blacklisted**, as in
    /// a scan; a failure that says nothing about the plugin (the probe binary is
    /// missing, an I/O error) leaves the row as it was. Unlike a scan, a missing
    /// `plugin-probe` does not record the file under its name: it is a failure.
    ///
    /// A link whose target has another extension (`Foo.clap` to `libfoo.so`) is
    /// probed as named and recorded under its target, as CLAP; a load of that
    /// row opens the target as CLAP ([`Loadable::format`]).
    ///
    /// # Errors
    ///
    /// [`FilesError::Missing`] when `path` is not a file (checked first),
    /// [`FilesError::UnrecognizedFormat`] when its extension names no plugin
    /// format, and [`FilesError::Scan`] as for [`scan`](Self::scan). A file the
    /// probe fails on is not an error of the call: the result's
    /// [`failures`](crate::ScanResult::failures) name it and say why
    /// ([`ProbeError`](tutti_plugin_types::ProbeError)), a missing
    /// `plugin-probe` among them.
    pub fn register(&mut self, path: impl AsRef<Path>) -> std::result::Result<Scan, FilesError> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(FilesError::Missing(path.to_path_buf()));
        }
        let format = PluginFormat::from_path(path)
            .ok_or_else(|| FilesError::UnrecognizedFormat(path.to_path_buf()))?;
        let probing = Probing {
            fallback: false,
            ..self.scanning()
        };
        self.spawn_file(probing, path, format)
    }

    /// Starts a task probing the one file `path`, read as `format`, as `probing`
    /// says, on one scanner of this catalog's pedal.
    fn spawn_file(
        &mut self,
        probing: Probing,
        path: &Path,
        format: PluginFormat,
    ) -> std::result::Result<Scan, FilesError> {
        self.finish_pending_save();
        let save = self.autosave();
        let scanner = self.scanner(probing);
        self.change_with(|db, _| scanner.spawn_files(db, [(path, format)], save))
    }

    /// Applies what a task ([`scan`](Self::scan), [`register`](Self::register) or
    /// [`measure`](Self::measure)) has sent so far and reports where it stands.
    /// Never blocks.
    ///
    /// A finished task is saved when this returns [`ScanPoll::Done`]: the table
    /// is copied here and written on a short-lived thread, so the fsync is not
    /// on the owner's thread. A failed save is logged and kept for
    /// [`take_persist_error`](Self::take_persist_error). The write is finished
    /// before the next task, `save` or drop. Drop the task once it reports
    /// `Done` or `Died`.
    pub fn try_apply(&mut self, task: &mut Scan) -> ScanPoll {
        let poll = self.change_with(|db, _| task.poll(db));
        if matches!(poll, ScanPoll::Done(_)) {
            self.save_in_background();
        }
        poll
    }

    /// Writes a copy of the table on its own thread. The clone is the only cost
    /// on the owner's thread. A write still in flight is finished first, so two
    /// writes never race and the newer table is the one stored.
    fn save_in_background(&mut self) {
        self.finish_pending_save();
        let copy = Arc::new(self.db.clone());
        let for_thread = (
            Arc::clone(&copy),
            Arc::clone(&self.persist),
            Arc::clone(&self.unreported),
        );
        let spawned = tutti_platform::spawn_named("plugin-db-save", None, move |_| {
            let (copy, persist, unreported) = for_thread;
            let _ = persist_unattended(&persist, &unreported, &copy);
        });
        match spawned {
            Ok(handle) => self.pending_save = Some(handle),
            Err(e) => {
                tracing::warn!(
                    "could not start a thread for the database save, saving inline: {e}"
                );
                let _ = persist_unattended(&self.persist, &self.unreported, &copy);
            }
        }
    }

    fn finish_pending_save(&mut self) {
        if let Some(handle) = self.pending_save.take()
            && handle.join().is_err()
        {
            let panicked = PersistError::new("the persistence panicked");
            tracing::warn!("{panicked}");
            *self.unreported.lock_recover() = Some(panicked);
        }
    }

    /// The failure of the last save this catalog started itself (after
    /// a task's [`try_apply`](Self::try_apply), or in a
    /// [`recover_crash`](Self::recover_crash)), or `None` if it succeeded or
    /// the failure was already taken. A later successful save clears an older
    /// failure: the table it stored is newer. Waits for a rescan's write still in
    /// flight.
    #[must_use]
    pub fn take_persist_error(&mut self) -> Option<PersistError> {
        self.finish_pending_save();
        self.unreported.lock_recover().take()
    }

    /// Iterates the refs and descriptors of all non-blacklisted plugins.
    pub fn iter(&self) -> impl Iterator<Item = (PluginRef, &PluginDescriptor)> {
        self.db.plugins().map(|p| (p.plugin_ref(), p.descriptor))
    }

    /// Iterates all non-blacklisted plugins with their file and format.
    ///
    /// Use this when you need `format` or `role`, for example to group plugins
    /// by format in a browser.
    pub fn entries(&self) -> impl Iterator<Item = PluginEntry<'_>> {
        self.db.plugins()
    }

    /// What `id` names among the non-blacklisted plugins of the catalog now:
    /// the plugin of that format with that id, wherever its file is.
    ///
    /// The answer is [`Exact`](Resolved::Exact), [`Ambiguous`](Resolved::Ambiguous)
    /// (the id in several files) or [`Missing`](Resolved::Missing). A saved
    /// `PluginId` holds no version, so there is nothing to compare a plugin's
    /// [`version`](PluginDescriptor::version) with, and no near match.
    ///
    /// A row whose file no longer exists is skipped, so a plugin moved on disk
    /// resolves to its new file once a scan has found it, while its old row
    /// waits for [`prune_missing`](Self::prune_missing).
    ///
    /// A blacklisted file keeps no descriptors, so its plugins resolve
    /// [`Missing`](Resolved::Missing). A plugin blacklisted after this call is
    /// refused by the [`Loadable`] when the load opens it. To load a blacklisted
    /// plugin anyway: [`blacklisted`](Self::blacklisted) names the file,
    /// [`unblacklist`](Self::unblacklist) drops its row, a rescan or
    /// [`register`](Self::register) probes it again, and then resolve.
    #[must_use]
    pub fn resolve(&self, id: &PluginId) -> Resolved {
        let mut found: Vec<PluginRef> = self
            .db
            .plugins()
            .filter(|p| p.id == Some(id) && FileFacts::read(p.path).exists)
            .map(|p| p.plugin_ref())
            .collect();
        found.sort_by(|a, b| a.path().cmp(b.path()));
        let mut loadables = found
            .into_iter()
            .map(|plugin| Loadable::new(plugin, self.blacklist_view()));
        match (loadables.next(), loadables.next()) {
            (None, _) => Resolved::Missing,
            (Some(only), None) => Resolved::Exact(only),
            (Some(first), Some(second)) => {
                Resolved::Ambiguous([first, second].into_iter().chain(loadables).collect())
            }
        }
    }

    /// Returns the descriptor of a non-blacklisted plugin: the one in the
    /// ref's file with the ref's id, or the file's first when the ref has
    /// none.
    pub fn info(&self, plugin: &PluginRef) -> Option<&PluginDescriptor> {
        // Rows are keyed by canonical path; a ref keeps the path it was given
        // (a link's extension is what names its format).
        let file = key(plugin.path());
        self.db
            .plugins()
            .find(|p| p.path == file && plugin.id().is_none_or(|id| p.id == Some(id)))
            .map(|p| p.descriptor)
    }

    /// Iterates the blacklisted files, including the recorded reason.
    ///
    /// [`iter`](Self::iter) and [`entries`](Self::entries) exclude these; use
    /// this to show hidden plugins and offer to [`unblacklist`](Self::unblacklist)
    /// them.
    pub fn blacklisted(&self) -> impl Iterator<Item = BlacklistedFile<'_>> {
        self.db.blacklisted()
    }

    /// Returns `true` if [`check`](Self::check) would refuse the plugin at
    /// `path`.
    ///
    /// A blacklisted plugin whose file has changed since it was blacklisted
    /// (a reinstall or update) is not refused, so this returns `false` for it.
    /// The check is mtime-aware ([`PluginDb::is_blacklisted_and_unchanged`]).
    #[must_use]
    pub fn is_blacklisted(&self, path: impl AsRef<Path>) -> bool {
        let path = key(path.as_ref());
        self.db
            .is_blacklisted_and_unchanged(&path, &FileFacts::read(&path))
    }

    /// Whether a load may open `plugin`: the catalog's one refusal.
    ///
    /// The path need not have been scanned. The check is mtime-aware, so a
    /// reinstall or update re-admits a plugin without clearing anything. The
    /// catalog does not open a plugin; the session's load asks the same rule
    /// through a [`BlacklistView`] when it runs, so a blacklisted plugin is
    /// refused when it is loaded, before any server starts.
    ///
    /// # Errors
    ///
    /// [`IpcError::Blacklisted`] with the recorded reason when the plugin's
    /// file is blacklisted and unchanged; a host can offer to load it anyway
    /// (a load that does not name the catalog) or to
    /// [`unblacklist`](Self::unblacklist) it.
    pub fn check(&self, plugin: &PluginRef) -> IpcResult<()> {
        let canonical = key(plugin.path());
        refusal(blacklist_row(&self.db, &canonical), &canonical, plugin)
    }

    /// Blacklists a plugin by path, hiding it from `iter` and `entries`,
    /// resolving its plugins [`Missing`](Resolved::Missing) and making
    /// [`check`](Self::check) refuse it.
    pub fn blacklist(&mut self, path: impl AsRef<Path>, reason: impl Into<String>) {
        let path = key(path.as_ref());
        let reason = reason.into();
        self.change_with(|db, _| db.blacklist(&path, reason, &FileFacts::read(&path)));
    }

    /// Clears one blacklist entry so the plugin is probed again on the next
    /// scan.
    ///
    /// The file's row is dropped, so the plugin is **absent** from
    /// [`iter`](Self::iter), [`entries`](Self::entries) and
    /// [`resolve`](Self::resolve) until that scan has probed it again. A host's
    /// "load anyway" is [`blacklisted`](Self::blacklisted), this, a rescan, then
    /// `resolve`.
    ///
    /// Returns `true` if a blacklisted file was found and cleared. Offer this
    /// to users: crash recovery also blacklists after a force-quit, power loss
    /// or out-of-memory kill, so false positives are expected.
    ///
    /// The file's entry in the crash sentinel is dropped too (one a failed or
    /// absent save kept), so the next recovery does not blacklist it again. While
    /// a rescan is running the sentinel is that scan's, and is left alone: the
    /// entry is then dropped by the next `unblacklist`.
    pub fn unblacklist(&mut self, path: impl AsRef<Path>) -> bool {
        let path = key(path.as_ref());
        self.forget_in_pedal(&path);
        self.change_with(|db, _| db.unblacklist(&path))
    }

    fn forget_in_pedal(&self, file: &Path) {
        if self.scanner(self.scanning()).forget(file).is_err() {
            tracing::warn!("a rescan is running; not touching its crash sentinel for {file:?}");
        }
    }

    /// Clears every blacklist entry and returns the paths cleared.
    pub fn clear_blacklist(&mut self) -> Vec<PathBuf> {
        self.change_with(|db, _| db.clear_blacklist())
    }

    /// Removes one file, blacklisted or not.
    ///
    /// Unlike [`Self::unblacklist`] this forgets the plugin entirely, so the
    /// next scan treats it as new.
    pub fn remove(&mut self, path: impl AsRef<Path>) {
        let path = key(path.as_ref());
        self.forget_in_pedal(&path);
        self.change_with(|db, _| db.apply(RowChange::remove(path)));
    }

    /// Measures the file `plugin` names, as the format `plugin` has, on a
    /// background thread and records the result: the tail and latency each of its
    /// plugins reports, against what it renders, in each descriptor's
    /// [`measured`](PluginDescriptor::measured). The row is replaced, so the file
    /// is probed again too. The task is the one [`scan`](Self::scan) returns:
    /// [`try_apply`](Self::try_apply) applies its change.
    ///
    /// `state` is a file of the bytes the plugin's own `save_state` wrote: the
    /// plugin `plugin` names is put in that state before it is measured, so the
    /// figure is of that patch (the row records a hash of it). A ref that names no
    /// plugin ([`PluginRef::file`]) is for a file of one plugin: with several, the
    /// probe refuses the state ([`ProbeRefusal::AmbiguousPlugin`](tutti_plugin_types::ProbeRefusal::AmbiguousPlugin)).
    /// The file's other plugins are measured as they load. It belongs to this call alone: another
    /// `measure` without one measures its plugin as it loads.
    ///
    /// The probe renders each plugin at two configurations, up to
    /// [`DEFAULT_MEASURE_TIMEOUT`](crate::DEFAULT_MEASURE_TIMEOUT).
    /// As in a measuring scan, a plugin that crashes or hangs while being
    /// measured is **blacklisted**; one that cannot be measured (it would not
    /// activate, has no audio output, or is too slow to render) is logged and its
    /// row has no measurement. Dropping the task kills the probe. A
    /// missing `plugin-probe` is a failure in the result, as for
    /// [`register`](Self::register).
    ///
    /// # Errors
    ///
    /// [`FilesError`] as for [`register`](Self::register), the format being the
    /// ref's.
    pub fn measure(
        &mut self,
        plugin: &PluginRef,
        state: Option<&Path>,
    ) -> std::result::Result<Scan, FilesError> {
        let probing = Probing {
            measure: true,
            state: state.map(|file| ProbeState {
                file: file.to_path_buf(),
                plugin: plugin.id().cloned(),
            }),
            fallback: false,
        };
        let format = plugin
            .format()
            .map_err(|_| FilesError::UnrecognizedFormat(plugin.path().to_path_buf()))?;
        self.spawn_file(probing, plugin.path(), format)
    }

    /// Applies a change the caller built to the catalog, such as a
    /// [`RowChange::put`] of a row from its own source. A task's changes go through
    /// [`try_apply`](Self::try_apply). Call [`save`](Self::save) to persist.
    pub fn apply(&mut self, change: RowChange) {
        self.change_with(|db, _| db.apply(change));
    }

    /// Writes the in-memory catalog through the catalog's persistence.
    ///
    /// # Errors
    ///
    /// The [`PersistError`] the persistence returned; what it stored before is
    /// then as the persistence leaves it.
    pub fn save(&mut self) -> Result<(), PersistError> {
        self.finish_pending_save();
        let outcome = (self.persist)(&self.db);
        if outcome.is_ok() {
            *self.unreported.lock_recover() = None;
        }
        outcome
    }

    /// Blacklists the plugin a previous scan was probing when it died.
    ///
    /// A scan writes a sentinel file before each probe and removes it after, so
    /// a sentinel left over means the scanning process went down: a plugin
    /// crash, but equally a force-quit, power loss or out-of-memory kill. False
    /// positives are therefore expected; offer
    /// [`unblacklist`](Self::unblacklist) to the user.
    ///
    /// Every scan already calls this. Call it directly to report "a plugin
    /// brought your last session down" at startup without a full rescan.
    /// Idempotent, and a no-op when no sentinel is present. Returns the files
    /// that were blacklisted.
    ///
    /// The sentinel is removed only after the blacklist is saved. If the
    /// persistence fails, the blacklist lives in memory, the sentinel stays armed
    /// (the next start blacklists the plugin again rather than forgetting it) and
    /// the failure is kept for [`take_persist_error`](Self::take_persist_error).
    ///
    /// # Errors
    ///
    /// [`ScanBusy`] while a rescan is in flight: its armed sentinel names a live
    /// probe, not a crash.
    pub fn recover_crash(&mut self) -> std::result::Result<Vec<PathBuf>, ScanBusy> {
        self.finish_pending_save();
        let save = self.autosave();
        self.change(|db, scanner, _| scanner.recover_crash(db, save))
    }

    /// Removes rows whose plugin file does not exist and returns their paths.
    ///
    /// A scan only adds what it finds, so an uninstalled plugin keeps its
    /// row until this is called. Scans do not prune on their own because a
    /// temporarily unavailable directory (an unmounted volume, a network share)
    /// would lose all its rows and need a full re-probe.
    pub fn prune_missing(&mut self) -> Vec<PathBuf> {
        let present: HashSet<PathBuf> = self
            .db
            .rows()
            .filter(|(path, _)| FileFacts::read(path).exists)
            .map(|(path, _)| path.to_path_buf())
            .collect();
        self.change_with(|db, _| db.prune_missing(&present))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::Described;

    fn clap(id: &str) -> PluginId {
        PluginId::Clap(id.into())
    }
    use crate::loadable::BindError;
    use crate::{FileRow, ScanPoll, ScanResult};

    /// Runs a scan to its end on the calling thread, applying as it goes.
    fn rescan(plugins: &mut Catalog) -> ScanResult {
        let mut scan = plugins.scan().unwrap();
        loop {
            match plugins.try_apply(&mut scan) {
                ScanPoll::Running => std::thread::yield_now(),
                ScanPoll::Done(result) => break result,
                ScanPoll::Died => panic!("the scan's worker died"),
            }
        }
    }

    /// A blacklisted row whose `mtime` matches what the file system reports,
    /// i.e. the file has not changed since it was recorded.
    fn blacklisted_row(path: &Path, reason: &str) -> FileRow {
        FileRow::blacklisted(reason.to_string(), &FileFacts::read(path))
    }

    fn catalog_with(path: &Path, row: FileRow) -> Catalog {
        let mut db = PluginDb::new();
        // Rows are keyed by canonical path, as a scan keys them.
        db.apply(RowChange::put(key(path), row));
        Catalog::new(db, CatalogConfig::new("/nonexistent"), |_| Ok(()))
    }

    /// **A catalog names each plugin of a bundle, and `info` finds the one the
    /// ref names**, not the file's first: two plugins in one file are two refs
    /// with their own ids, and a ref with no id is the first.
    #[test]
    fn a_bundle_of_two_plugins_is_two_refs_and_info_finds_each() {
        use crate::FileFacts;
        use tutti_plugin_types::{PluginClass, PluginDescriptor};
        let path = std::env::temp_dir().join("tutti-two-plugins.clap");
        std::fs::write(&path, b"x").unwrap();
        let describe = |id: &str| Described {
            id: Some(PluginId::Clap(id.into())),
            descriptor: PluginDescriptor::new(id, PluginClass::Unknown),
        };
        let row = FileRow::probed(PluginFormat::Clap, &FileFacts::read(&path))
            .plugins(vec![describe("one"), describe("two")])
            .build()
            .expect("ids of the row's format");
        let plugins = catalog_with(&path, row);

        let refs: Vec<_> = plugins.iter().map(|(r, _)| r).collect();
        let ids: Vec<_> = refs.iter().map(|r| r.id().unwrap()).collect();
        assert_eq!(ids, [&clap("one"), &clap("two")]);
        assert_eq!(plugins.info(&refs[1]).map(|d| d.name.as_str()), Some("two"));
        let by_file = PluginRef::file(&path);
        assert_eq!(plugins.info(&by_file).map(|d| d.name.as_str()), Some("one"));
        // A ref through a link names the scanned file (macOS's temp dir is
        // one: `/var` links to `/private/var`).
        #[cfg(unix)]
        {
            let link = std::env::temp_dir().join("tutti-two-plugins-link.clap");
            std::fs::remove_file(&link).ok();
            std::os::unix::fs::symlink(&path, &link).unwrap();
            let by_link = PluginRef::file(&link);
            assert_eq!(plugins.info(&by_link).map(|d| d.name.as_str()), Some("one"));
            std::fs::remove_file(&link).ok();
        }

        std::fs::remove_file(&path).ok();
    }

    /// The check refuses a plugin the scanner recorded as a crasher,
    /// and says which one and why.
    ///
    /// The reason is the whole point of carrying it: a host has to be able to
    /// name the plugin and offer to load it anyway.
    #[test]
    fn a_blacklisted_plugin_is_refused_with_its_recorded_reason() {
        let dir = std::env::temp_dir().join("tutti-bl-refused");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Crasher.vst3");
        std::fs::write(&path, b"not a real plugin").unwrap();

        let plugins = catalog_with(&path, blacklisted_row(&path, "SIGSEGV during probe"));

        match plugins.check(&PluginRef::file(&path)) {
            Err(IpcError::Blacklisted { path: p, reason }) => {
                assert_eq!(p, path);
                assert_eq!(reason, "SIGSEGV during probe");
            }
            other => panic!("expected a Blacklisted refusal, got {other:?}"),
        }

        std::fs::remove_file(&path).ok();
    }

    /// A blacklisted plugin whose file has since changed is admitted again.
    ///
    /// Blacklisting records the file's mtime precisely so a reinstall or a
    /// vendor update lifts it without the host clearing anything. Checking the
    /// raw flag instead would hide the plugin permanently, and false positives
    /// are expected — the scanner's pedal fires on a force-quit or an OOM kill
    /// as readily as on a real crash.
    #[test]
    fn a_blacklisted_plugin_is_readmitted_once_its_file_changes() {
        let dir = std::env::temp_dir().join("tutti-bl-readmit");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Updated.vst3");
        std::fs::write(&path, b"not a real plugin").unwrap();

        // Record the blacklist against a *different* mtime than the file has,
        // which is what a reinstall produces.
        let mut row = blacklisted_row(&path, "crashed once");
        row.mtime = row
            .mtime
            .and_then(|t| crate::Mtime::new(t.get() - std::time::Duration::from_secs(1_000)));
        let plugins = catalog_with(&path, row);

        plugins
            .check(&PluginRef::file(&path))
            .expect("a changed file must not be refused as blacklisted");

        std::fs::remove_file(&path).ok();
    }

    /// The query and the check agree about a plugin whose file has changed.
    ///
    /// Were the query to read the raw flag while `check` used the mtime-aware
    /// check, an updated plugin would report blacklisted and open anyway. A
    /// browser greying entries out on the query then hides a plugin it could
    /// load — permanently, because the reinstall meant to lift the blacklist is
    /// exactly what makes the two diverge.
    ///
    /// Asserting *agreement* rather than a fixed answer is what makes this
    /// survive a future change to the staleness rule: whatever `check` decides,
    /// the query has to say the same thing.
    #[test]
    fn the_query_and_the_check_agree_after_the_file_changes() {
        let dir = std::env::temp_dir().join("tutti-bl-stale");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Updated.vst3");
        std::fs::write(&path, b"not a real plugin").unwrap();

        // Unchanged first: both halves must still refuse.
        let fresh = catalog_with(&path, blacklisted_row(&path, "SIGSEGV during probe"));
        assert!(
            fresh.is_blacklisted(&path),
            "unchanged and blacklisted: the query must say so"
        );
        assert!(
            matches!(
                fresh.check(&PluginRef::file(&path)),
                Err(IpcError::Blacklisted { .. })
            ),
            "unchanged and blacklisted: the check must refuse"
        );

        // Now the vendor ships a fix. Backdating the *record* rather than
        // rewriting the file is what the sibling test does, and for a reason:
        // a rewrite inside the filesystem's timestamp granularity leaves the
        // times equal and the test proves nothing.
        let mut stale = blacklisted_row(&path, "SIGSEGV during probe");
        stale.mtime = stale
            .mtime
            .and_then(|t| crate::Mtime::new(t.get() - std::time::Duration::from_secs(1_000)));
        let plugins = catalog_with(&path, stale);

        assert!(
            !plugins.is_blacklisted(&path),
            "a changed file lifts the blacklist — this is the half that was wrong"
        );
        plugins
            .check(&PluginRef::file(&path))
            .expect("the check must admit it too, so the two answers agree");

        std::fs::remove_file(&path).ok();
    }

    /// A catalog of one file holding the plugins `ids` of `format`.
    fn catalog_of(path: &Path, format: PluginFormat, ids: &[PluginId]) -> Catalog {
        use tutti_plugin_types::{PluginClass, PluginDescriptor};
        std::fs::write(path, b"x").unwrap();
        let plugins = ids
            .iter()
            .map(|id| Described {
                id: Some(id.clone()),
                descriptor: PluginDescriptor::new(id.to_string(), PluginClass::Unknown),
            })
            .collect();
        catalog_with(
            path,
            FileRow::probed(format, &FileFacts::read(path))
                .plugins(plugins)
                .build()
                .expect("ids of the row's format"),
        )
    }

    /// **Only a move of the blacklist is published**: a probed row changing
    /// leaves the published rows alone, a blacklisted row appearing or going
    /// replaces them.
    #[test]
    fn a_change_publishes_the_blacklist_only_when_it_moved() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("Crasher.clap");
        std::fs::write(&path, b"x").unwrap();
        let facts = FileFacts::read(&path);
        let mut db = PluginDb::new();
        let published = blacklisted_rows(&db);

        db.apply(RowChange::put(
            key(&path),
            FileRow::probed(PluginFormat::Clap, &facts)
                .plugins(Vec::new())
                .build()
                .expect("ids of the row's format"),
        ));
        assert_eq!(moved_blacklist(&published, &db), None, "a probed row");

        db.blacklist(&key(&path), "hung".into(), &facts);
        let moved = moved_blacklist(&published, &db).expect("a blacklisted row appeared");
        assert!(moved.is_blacklisted(&key(&path)));
        assert_eq!(moved_blacklist(&moved, &db), None, "nothing since");

        assert!(db.unblacklist(&key(&path)));
        assert_eq!(
            moved_blacklist(&moved, &db),
            Some(PluginDb::new()),
            "the blacklist went"
        );
    }

    /// **An id resolves to the one plugin that has it, wherever its file is**: the
    /// loadable names that file, the format that opens it and the id.
    #[test]
    fn an_id_resolves_to_the_plugin_that_has_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("Bundle.clap");
        let plugins = catalog_of(&path, PluginFormat::Clap, &[clap("one"), clap("two")]);

        let Resolved::Exact(two) = plugins.resolve(&clap("two")) else {
            panic!("the id is in the catalog");
        };

        assert_eq!(two.format(), Ok(PluginFormat::Clap));
        assert_eq!(two.plugin().id(), Some(&clap("two")));
        assert_eq!(two.plugin().path(), key(&path));
    }

    /// **The format is part of the identity**: the id of a CLAP plugin names
    /// nothing among VST3 plugins.
    #[test]
    fn an_id_in_another_format_is_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("Synth.clap");
        let plugins = catalog_of(&path, PluginFormat::Clap, &[clap("synth")]);

        assert!(matches!(
            plugins.resolve(&PluginId::Vst2(7)),
            Resolved::Missing
        ));
        assert!(matches!(plugins.resolve(&clap("other")), Resolved::Missing));
    }

    /// **A plugin blacklisted at the resolve is missing**, not bound to a file
    /// the load would refuse.
    #[test]
    fn a_blacklisted_plugin_resolves_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("Crasher.clap");
        let mut plugins = catalog_of(&path, PluginFormat::Clap, &[clap("crasher")]);
        let id = clap("crasher");

        plugins.blacklist(&path, "hung");
        assert!(matches!(plugins.resolve(&id), Resolved::Missing));
    }

    /// **An id held by several files is ambiguous, ordered by file**: the app
    /// asks which one, and neither is bound unasked.
    #[test]
    fn an_id_in_two_files_is_ambiguous_ordered_by_file() {
        use tutti_plugin_types::{PluginClass, PluginDescriptor};
        let dir = tempfile::TempDir::new().unwrap();
        let (a, b) = (dir.path().join("A.clap"), dir.path().join("B.clap"));
        let mut db = PluginDb::new();
        for path in [&b, &a] {
            std::fs::write(path, b"x").unwrap();
            db.apply(RowChange::put(
                key(path),
                FileRow::probed(PluginFormat::Clap, &FileFacts::read(path))
                    .plugins(vec![Described {
                        id: Some(clap("twice")),
                        descriptor: PluginDescriptor::new("twice", PluginClass::Unknown),
                    }])
                    .build()
                    .expect("ids of the row's format"),
            ));
        }
        let plugins = Catalog::new(db, CatalogConfig::new("/nonexistent"), |_| Ok(()));

        let resolved = plugins.resolve(&clap("twice"));

        let Resolved::Ambiguous(candidates) = resolved.clone() else {
            panic!("{resolved:?}");
        };
        let files: Vec<_> = candidates.iter().map(|c| c.plugin().path()).collect();
        assert_eq!(files, [key(&a), key(&b)]);
        assert!(matches!(
            resolved.bind(),
            Err(BindError::Ambiguous(c)) if c.len() == 2
        ));
    }

    /// **A moved plugin resolves to its new file**: the old row, whose file is
    /// gone, is skipped rather than making the id ambiguous.
    #[test]
    fn a_row_whose_file_is_gone_is_not_a_candidate() {
        use tutti_plugin_types::{PluginClass, PluginDescriptor};
        let dir = tempfile::TempDir::new().unwrap();
        let (old, new) = (dir.path().join("Old.clap"), dir.path().join("New.clap"));
        let mut db = PluginDb::new();
        for path in [&old, &new] {
            std::fs::write(path, b"x").unwrap();
            db.apply(RowChange::put(
                key(path),
                FileRow::probed(PluginFormat::Clap, &FileFacts::read(path))
                    .plugins(vec![Described {
                        id: Some(clap("moved")),
                        descriptor: PluginDescriptor::new("moved", PluginClass::Unknown),
                    }])
                    .build()
                    .expect("ids of the row's format"),
            ));
        }
        let new_key = key(&new);
        std::fs::remove_file(&old).unwrap();
        let plugins = Catalog::new(db, CatalogConfig::new("/nonexistent"), |_| Ok(()));

        let Resolved::Exact(found) = plugins.resolve(&clap("moved")) else {
            panic!("the stale row made the id ambiguous");
        };
        assert_eq!(found.plugin().path(), new_key);

        std::fs::remove_file(&new).unwrap();
        assert!(matches!(plugins.resolve(&clap("moved")), Resolved::Missing));
    }

    /// The real edge: `prune_missing` stats each row's path and removes only
    /// the absent one.
    #[test]
    fn prune_missing_removes_only_rows_whose_file_is_gone() {
        let dir = tempfile::TempDir::new().unwrap();
        let here = dir.path().join("here.vst3");
        std::fs::write(&here, b"x").unwrap();
        let gone = dir.path().join("gone.vst3");

        let mut db = PluginDb::new();
        for path in [&here, &gone] {
            db.apply(RowChange::put(
                path,
                FileRow::probed(PluginFormat::Vst3, &FileFacts::read(path))
                    .plugins(vec![Described {
                        id: None,
                        descriptor: PluginDescriptor::default(),
                    }])
                    .build()
                    .expect("ids of the row's format"),
            ));
        }
        let mut plugins = Catalog::new(db, CatalogConfig::new("/nonexistent"), |_| Ok(()));

        assert_eq!(plugins.prune_missing(), vec![gone]);
        assert_eq!(plugins.iter().count(), 1);
    }

    /// A catalog's persistence gets the blacklist before the pedal
    /// goes, and the pedal goes after.
    #[test]
    fn the_persistence_receives_the_recovered_blacklist_before_the_pedal_goes() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = CatalogConfig::new(dir.path());
        let crashy = dir.path().join("Crashy.vst3");
        std::fs::write(&crashy, b"x").unwrap();
        std::fs::write(config.pedal_path(), crashy.to_str().unwrap()).unwrap();
        let pedal = config.pedal_path();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let key_path = key(&crashy);

        let mut plugins = Catalog::new(PluginDb::new(), config, move |db| {
            log.lock()
                .unwrap()
                .push((db.is_blacklisted(&key_path), pedal.exists()));
            Ok(())
        });
        assert_eq!(plugins.recover_crash(), Ok(vec![key(&crashy)]));

        assert_eq!(*seen.lock().unwrap(), vec![(true, true)]);
        assert!(!plugins.config.pedal_path().exists());
    }

    /// A persistence that always fails cannot have saved the blacklist, so the
    /// pedal stays armed, the blacklist is in memory only, and the failure
    /// reaches the caller as the persistence's own error.
    #[test]
    fn a_failing_persistence_keeps_the_pedal_armed_and_reports_its_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = CatalogConfig::new(dir.path());
        let crashy = dir.path().join("Crashy.vst3");
        std::fs::write(&crashy, b"x").unwrap();
        std::fs::write(config.pedal_path(), crashy.to_str().unwrap()).unwrap();

        let mut plugins = Catalog::new(PluginDb::new(), config.clone(), |_| {
            Err(PersistError::new("disk full"))
        });
        assert_eq!(plugins.recover_crash(), Ok(vec![key(&crashy)]));
        let recovery = plugins
            .take_persist_error()
            .expect("the recovery's save failed");
        assert!(recovery.to_string().contains("disk full"), "{recovery}");
        assert_eq!(plugins.take_persist_error().map(|_| ()), None, "taken once");
        // A rescan recovers first; it must not delete what it could not save.
        rescan(&mut plugins);
        let scan = plugins
            .take_persist_error()
            .expect("the scan's save failed");
        assert!(scan.to_string().contains("disk full"), "{scan}");

        assert!(plugins.db().is_blacklisted(&key(&crashy)));
        assert!(config.pedal_path().exists());
        let saved = plugins.save().unwrap_err();
        assert!(saved.to_string().contains("disk full"), "{saved}");
        assert!(std::error::Error::source(&saved).is_some());
    }

    /// The save after a rescan is off the owner's thread: `try_apply` reports
    /// `Done` while the persistence is still running, and the next `save` waits for
    /// it, and dropping the catalog waits for it.
    #[test]
    fn the_save_after_a_rescan_does_not_block_the_owner() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = CatalogConfig::new(dir.path());
        let (started_tx, started_rx) = crossbeam_channel::bounded::<()>(8);
        let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let done = Arc::clone(&writes);
        let mut plugins = Catalog::new(PluginDb::new(), config, move |_| {
            started_tx.send(()).unwrap();
            // Held far past the hang guard: an owner that waited for this save
            // would outlast it.
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(300));
            done.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });

        let deadline = tutti_platform::Instant::now() + std::time::Duration::from_secs(60);
        let mut scan = plugins.scan().unwrap();
        let end = loop {
            match plugins.try_apply(&mut scan) {
                ScanPoll::Running => std::thread::yield_now(),
                other => break other,
            }
        };
        assert!(matches!(end, ScanPoll::Done(_)), "{end:?}");
        // A hang guard: `try_apply` returns at once, and one that waited for the
        // save would take the save's 300 s hold.
        assert!(
            tutti_platform::Instant::now() < deadline,
            "try_apply waited for the save"
        );
        started_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the save started");
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 0);

        release_tx.send(()).unwrap();
        drop(scan);
        drop(plugins); // joins the save
        assert_eq!(writes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// A plugin the user unblacklists, or removes, is not put back by the next
    /// recovery from an entry a failed save kept in the sentinel.
    #[test]
    fn unblacklist_and_remove_drop_the_retained_sentinel_entry() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = CatalogConfig::new(dir.path());
        let (a, b) = (dir.path().join("A.vst3"), dir.path().join("B.vst3"));
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"x").unwrap();
        let entries = format!("{}\0{}", a.display(), b.display());
        std::fs::write(config.pedal_path(), entries).unwrap();
        let mut plugins = Catalog::new(PluginDb::new(), config.clone(), |_| {
            Err(PersistError::new("no disk"))
        });
        assert_eq!(plugins.recover_crash().unwrap().len(), 2);
        assert!(config.pedal_path().exists(), "kept: the save failed");

        assert!(plugins.unblacklist(&a));
        rescan(&mut plugins);
        assert!(!plugins.is_blacklisted(&a), "the user's unblacklist held");
        assert!(plugins.is_blacklisted(&b), "the other entry is still kept");

        plugins.remove(&b);
        rescan(&mut plugins);
        assert!(plugins.db().row(&key(&b)).is_none(), "the removal held");
        assert!(!config.pedal_path().exists());
    }

    #[cfg(unix)]
    mod unix_tests {
        use super::*;

        /// An alias of a blacklisted plugin is refused like the plugin itself: the
        /// catalog keys by canonical path.
        #[test]
        fn a_blacklist_set_through_an_alias_holds_for_the_real_path() {
            let dir = tempfile::TempDir::new().unwrap();
            let real = dir.path().join("Real.vst3");
            std::fs::write(&real, b"x").unwrap();
            let alias = dir.path().join("Alias.vst3");
            std::os::unix::fs::symlink(&real, &alias).unwrap();
            let mut plugins =
                Catalog::new(PluginDb::new(), CatalogConfig::new(dir.path()), |_| Ok(()));

            plugins.blacklist(&alias, "boom");

            assert!(plugins.is_blacklisted(&real));
            assert!(plugins.is_blacklisted(&alias));
            assert_eq!(plugins.db().len(), 1);
        }

        /// `remove` and `unblacklist` through an alias reach the row of a file that
        /// is gone: the key resolves the parent and keeps the name.
        #[test]
        fn remove_through_an_alias_reaches_a_deleted_file() {
            let dir = tempfile::TempDir::new().unwrap();
            let real_dir = dir.path().join("real");
            std::fs::create_dir(&real_dir).unwrap();
            let file = real_dir.join("Gone.vst3");
            std::fs::write(&file, b"x").unwrap();
            let alias_dir = dir.path().join("alias");
            std::os::unix::fs::symlink(&real_dir, &alias_dir).unwrap();
            let mut plugins =
                Catalog::new(PluginDb::new(), CatalogConfig::new(dir.path()), |_| Ok(()));
            plugins.blacklist(&file, "boom");
            plugins.blacklist(real_dir.join("Also.vst3"), "boom");
            std::fs::remove_file(&file).unwrap();

            plugins.remove(alias_dir.join("Gone.vst3"));
            assert!(plugins.unblacklist(alias_dir.join("Also.vst3")));

            assert_eq!(plugins.db().len(), 0);
        }

        /// The whole catalog works through a root that is a link, as macOS's temp
        /// dir is: a sentinel entry, a blacklist, a lookup and an unblacklist all
        /// name the file by the link path and meet at the canonical key. Linux has
        /// no such temp dir, so the link is made here.
        #[test]
        fn a_catalog_under_a_symlinked_root_agrees_with_itself() {
            let dir = tempfile::TempDir::new().unwrap();
            let real = dir.path().join("real");
            std::fs::create_dir(&real).unwrap();
            std::fs::write(real.join("A.vst3"), b"x").unwrap();
            std::fs::write(real.join("B.vst3"), b"x").unwrap();
            let root = dir.path().join("root");
            std::os::unix::fs::symlink(&real, &root).unwrap();
            let config = CatalogConfig::new(&root);
            std::fs::write(config.pedal_path(), root.join("A.vst3").to_str().unwrap()).unwrap();
            let mut plugins = Catalog::new(PluginDb::new(), config, |_| Ok(()));

            let crashed = plugins.recover_crash().unwrap();
            plugins.blacklist(root.join("B.vst3"), "boom");

            let real_a = real.canonicalize().unwrap().join("A.vst3");
            assert_eq!(crashed, vec![real_a.clone()]);
            for name in ["A.vst3", "B.vst3"] {
                assert!(plugins.is_blacklisted(root.join(name)), "{name} by link");
                assert!(plugins.is_blacklisted(real.join(name)), "{name} by path");
            }
            assert!(plugins.unblacklist(root.join("A.vst3")));
            assert!(!plugins.is_blacklisted(real_a));
            assert_eq!(plugins.db().len(), 1);
        }
    }
}
