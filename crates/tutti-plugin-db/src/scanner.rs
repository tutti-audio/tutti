//! The plugin scanner, with crash recovery.
//!
//! A scan works on a snapshot of the database: [`ScanPlugins::run`] discovers
//! plugin files, probes the new or changed ones and collects a [`RowChange`] per
//! file. The owner of the [`PluginDb`] applies them afterwards, so the database
//! stays readable for the whole scan. An internal dead-man's pedal
//! auto-blacklists a plugin that takes the host down while it is probed.

use super::catalog::CatalogId;
use super::facts::{self, FileFacts, ScanDecision};
use super::fs::{canonical_path, discover};
use super::pedal::Pedal;
use super::table::{FileRow, PluginDb, RowChange};
use crate::Stop;
use crossbeam_queue::ArrayQueue;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};
use tutti_plugin_types::{
    Described, PluginClass, PluginDescriptor, PluginFormat, ProbeError, ProbeRefusal,
};
use tutti_rt::{LockExt, Tally, TallyWriter};

/// How many progress updates wait for the owner before the scan drops the
/// next one.
const PROGRESS_CAPACITY: usize = 256;

/// The scan's end of the progress queue: sends the newest update, making room
/// by dropping the oldest, and counts what it drops.
struct Progress {
    queue: Arc<ArrayQueue<ScanProgress>>,
    dropped: TallyWriter<u64>,
}

impl Progress {
    fn send(&self, update: ScanProgress) {
        if self.queue.force_push(update).is_some() {
            self.dropped.add(1);
        }
    }
}

/// Where a scan stands, as it advances.
///
/// Advisory: the owner reads it with [`ScanProgressReader::read`]. When the owner
/// has not kept up and the queue is full, the oldest update is dropped for the
/// newest ([`ScanProgressReader::dropped`] counts them). The end of a scan is
/// the return of [`ScanPlugins::run`], never a progress update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanProgress {
    /// Finding plugin files in a directory. Only a scan of directories reports
    /// it, once per directory, before the walk.
    Discovering {
        /// The directory about to be walked.
        directory: PathBuf,
    },
    /// Probing one plugin file.
    Probing {
        /// The file's place in the scan, counting from one.
        current: usize,
        /// How many files the scan looks at.
        total: usize,
        /// The file being probed.
        path: PathBuf,
    },
}

/// What a scanner calls to read one plugin file's metadata.
type ProbeFn =
    dyn Fn(&Path, PluginFormat, &Stop) -> Result<Vec<Described>, ProbeFailure> + Send + Sync;

/// Scans plugin directories into a [`PluginDb`], with crash recovery.
///
/// Each new or changed file is probed through the scanner's probe (the host's
/// out-of-process one); unchanged ones (same modification time) are skipped. A
/// plugin that hangs or crashes its probe is blacklisted with the
/// reason, and a sentinel file (the "dead-man's pedal") blacklists the plugin a
/// crashed scan was probing on the next run.
///
/// A scanner holds no database. Every scan takes the database it reads from and
/// writes to, so one scanner serves any number of them.
#[derive(Clone)]
pub struct Scanner {
    pedal: Pedal,
    probe: Arc<ProbeFn>,
}

impl std::fmt::Debug for Scanner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scanner")
            .field("pedal", &self.pedal)
            .finish_non_exhaustive()
    }
}

impl Scanner {
    /// Creates a scanner.
    ///
    /// `pedal_path` is where the crash sentinel file lives: a crashed scan
    /// leaves it behind, and the next run blacklists the plugin it names.
    ///
    /// `probe` reads one plugin file's metadata. It runs out of process where
    /// it loads the plugin, so a crash or a hang cannot take the scan down. It
    /// watches the [`Stop`] it is given and, once a stop is requested, ends
    /// the process it is waiting on and returns at once: a cancelled
    /// [`ScanPlugins::run`] waits for the probe in flight to return.
    pub fn new(
        pedal_path: impl Into<PathBuf>,
        probe: impl Fn(&Path, PluginFormat, &Stop) -> Result<Vec<Described>, ProbeFailure>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            pedal: Pedal::new(pedal_path),
            probe: Arc::new(probe),
        }
    }

    /// Blacklists the plugin a previous, crashed scan was probing, in `db`, and
    /// hands `db` to `save` before the sentinel is removed. Returns the file
    /// that were blacklisted. If `save` fails the sentinel stays, and a scan
    /// that follows keeps its entries beside its own.
    ///
    /// Safe to call multiple times; a no-op when no sentinel is present. Every
    /// scan calls it first.
    ///
    /// # Errors
    ///
    /// [`ScanBusy`] while a scan is using the same pedal: its armed sentinel
    /// names a live probe, not a crash.
    pub fn recover_crash(
        &self,
        db: &mut PluginDb,
        save: impl FnOnce(&PluginDb) -> std::io::Result<()>,
    ) -> Result<Vec<PathBuf>, ScanBusy> {
        let _claim = Claim::take(&self.pedal)?;
        Ok(self.pedal.recover(db, save))
    }

    /// Drops the pedal's entries for `file` (a canonical path), so that a
    /// plugin the user took off the blacklist is not put back by the next
    /// recovery from an entry a failed save kept.
    ///
    /// # Errors
    ///
    /// [`ScanBusy`] while a scan is using the pedal: its entries are live
    /// probes, which this must not touch.
    pub fn forget(&self, file: &Path) -> Result<(), ScanBusy> {
        let _claim = Claim::take(&self.pedal)?;
        self.pedal.forget(file);
        Ok(())
    }

    /// Prepares a scan of `directories`, returning the value that runs it.
    ///
    /// Runs crash recovery on the calling thread first (see
    /// [`recover_crash`](Self::recover_crash)), then takes a snapshot of `db`.
    /// The findings come back from [`ScanPlugins::run`] as changes for
    /// `Catalog::apply`; until then `db` is unchanged and fully usable.
    ///
    /// # Errors
    ///
    /// [`ScanBusy`] while another scan, a recovery or an unrun value is using
    /// the same pedal, whether from this scanner, a clone of it, or any scanner
    /// built on that pedal path. The claim ends when the value drops or its run
    /// returns, so a second scan cannot delete the armed sentinel of a probe
    /// still in flight.
    pub fn scan(
        &self,
        db: &mut PluginDb,
        directories: impl IntoIterator<Item = impl AsRef<Path>>,
        save: impl FnOnce(&PluginDb) -> std::io::Result<()>,
    ) -> Result<ScanPlugins, ScanBusy> {
        let targets = Targets::Directories(
            directories
                .into_iter()
                .map(|d| d.as_ref().to_path_buf())
                .collect(),
        );
        Ok(ScanPlugins::new(self.job(db, targets, save)?))
    }

    /// Prepares the probing of the given plugin files, whether or not a row for
    /// them is up to date: the change of each replaces its file's row. Each file
    /// comes with the format the caller knows it as. The probe reads each file
    /// as that format, whatever its extension (a link `Foo.clap` to `libfoo.so`
    /// is CLAP). A row is keyed by the file's canonical path, as a scan keys it.
    /// In every other respect as [`scan`](Self::scan): the same pedal, claim and
    /// changes.
    ///
    /// # Errors
    ///
    /// [`FilesError::Missing`] for a path that is not a file, before anything
    /// is claimed; [`FilesError::Busy`] as for [`scan`](Self::scan).
    pub fn scan_files(
        &self,
        db: &mut PluginDb,
        files: impl IntoIterator<Item = (impl AsRef<Path>, PluginFormat)>,
        save: impl FnOnce(&PluginDb) -> std::io::Result<()>,
    ) -> Result<ScanPlugins, FilesError> {
        let mut targets = Vec::new();
        for (file, format) in files {
            let file = file.as_ref();
            if !FileFacts::read_as(file, Some(format)).exists() {
                return Err(FilesError::Missing(file.to_path_buf()));
            }
            targets.push((file.to_path_buf(), format));
        }
        Ok(ScanPlugins::new(self.job(
            db,
            Targets::Files(targets),
            save,
        )?))
    }

    fn job(
        &self,
        db: &mut PluginDb,
        targets: Targets,
        save: impl FnOnce(&PluginDb) -> std::io::Result<()>,
    ) -> Result<Job, ScanBusy> {
        let claim = Claim::take(&self.pedal)?;
        self.pedal.recover(db, save);
        Ok(Job {
            _claim: claim,
            snapshot: db.clone(),
            targets,
            pedal: self.pedal.clone(),
            probe: Arc::clone(&self.probe),
        })
    }
}

/// The pedal files a scan or a recovery is using right now, by canonical path
/// (a link or a relative path to the same file is the same pedal). One pedal
/// serves one probe at a time: a second user would delete the sentinel of a
/// probe in flight, or blacklist a plugin that did not crash.
static IN_USE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Exclusive use of a pedal, released on drop (so also when a probe panics).
#[derive(Debug)]
struct Claim(PathBuf);

impl Claim {
    fn take(pedal: &Pedal) -> Result<Self, ScanBusy> {
        let file = canonical_path(pedal.path());
        let mut in_use = IN_USE.lock_recover();
        if in_use.contains(&file) {
            return Err(ScanBusy);
        }
        in_use.push(file.clone());
        Ok(Self(file))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut in_use = IN_USE.lock_recover();
        in_use.retain(|p| *p != self.0);
    }
}

/// A scan or a crash recovery is already using this pedal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[error("a plugin scan is already running on this pedal")]
pub struct ScanBusy;

/// Why a task over named files was refused ([`Scanner::scan_files`], and the
/// catalog's `register` and `measure` over it).
#[derive(Debug, thiserror::Error)]
pub enum FilesError {
    /// A scan or a crash recovery is already using the pedal.
    #[error(transparent)]
    Busy(#[from] ScanBusy),
    /// A path is not a file that exists.
    #[error("no plugin file at {0}")]
    Missing(PathBuf),
    /// A path's extension names no plugin format. `Catalog::register` returns it,
    /// after checking that the file exists; [`Scanner::scan_files`] is given the
    /// format and never does.
    #[error("{0} has no plugin format's extension")]
    UnrecognizedFormat(PathBuf),
}

/// One task of discovery, ready to run: a scan of directories
/// ([`Scanner::scan`]), a registration or a measurement (both
/// [`Scanner::scan_files`]). It probes files and collects a [`RowChange`] per
/// file.
///
/// The value owns a snapshot of the database and the pedal's claim, and holds no
/// thread: [`run`](Self::run) blocks the caller for whole probes, so the app
/// runs it off any thread that drives a queue (a thread of the app's, or
/// `Executor::unblock`). Take
/// [`canceller`](Self::canceller) and [`progress`](Self::progress) before `run`
/// to cancel it and watch it from elsewhere. Dropping it unrun releases the
/// pedal and probes nothing.
#[must_use = "a ScanPlugins probes nothing until it is run"]
pub struct ScanPlugins {
    job: Job,
    stop: Stop,
    progress: Progress,
    reader: ScanProgressReader,
    origin: Option<CatalogId>,
}

impl std::fmt::Debug for ScanPlugins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanPlugins").finish_non_exhaustive()
    }
}

impl ScanPlugins {
    fn new(job: Job) -> Self {
        let queue = Arc::new(ArrayQueue::new(PROGRESS_CAPACITY));
        let dropped = tutti_rt::tally::<u64>();
        Self {
            job,
            stop: Stop::new(),
            progress: Progress {
                queue: Arc::clone(&queue),
                dropped: dropped.writer(),
            },
            reader: ScanProgressReader { queue, dropped },
            origin: None,
        }
    }

    /// Marks the output as the catalog `id`'s.
    pub(crate) fn of(mut self, id: CatalogId) -> Self {
        self.origin = Some(id);
        self
    }

    /// Probes the files, one at a time on the calling thread, and returns what it
    /// found. Blocks until the last probe ends or a [`cancel`](ScanCanceller::cancel)
    /// ends the one in flight.
    ///
    /// The pedal's claim is released and its sentinel gone before this returns.
    /// A cancelled run returns the files it reached, the one it was probing
    /// as [`ScanOutcome::Cancelled`] (never blacklisted).
    ///
    /// # Panics
    ///
    /// When the probe panics; the claim is released as the panic unwinds.
    pub fn run(self) -> ScannedPlugins {
        let Self {
            job,
            stop,
            progress,
            origin,
            ..
        } = self;
        let (report, changes) = job.run(&stop, &progress);
        // The claim ends with the job, before the caller sees the output, so the
        // next scan can start.
        drop(job);
        ScannedPlugins {
            origin,
            report,
            changes,
        }
    }

    /// The handle that ends the run from another thread. It outlives this value.
    pub fn canceller(&self) -> ScanCanceller {
        ScanCanceller {
            stop: self.stop.clone(),
        }
    }

    /// The reader of the run's progress updates. It outlives this value.
    pub fn progress(&self) -> ScanProgressReader {
        self.reader.clone()
    }
}

/// Ends a [`ScanPlugins::run`]: the probe in flight is killed and the run
/// returns what it has. `Clone`, `Send` and `Sync`; every clone cancels the same
/// run, and cancelling after the run ended does nothing.
#[derive(Debug, Clone)]
pub struct ScanCanceller {
    stop: Stop,
}

impl ScanCanceller {
    /// Requests the end. Idempotent; never blocks.
    pub fn cancel(&self) {
        self.stop.stop();
    }
}

/// Reads the progress of a [`ScanPlugins::run`]. `Clone` and `Send`; clones share
/// one queue.
///
/// When the reader has not kept up and the queue is full, the oldest update is
/// dropped for the newest ([`dropped`](Self::dropped) counts them). The end of a
/// run is its return, never an update.
#[derive(Debug, Clone)]
pub struct ScanProgressReader {
    queue: Arc<ArrayQueue<ScanProgress>>,
    dropped: Tally<u64>,
}

impl ScanProgressReader {
    /// The updates made since the last call, oldest first. Never blocks.
    pub fn read(&self) -> impl Iterator<Item = ScanProgress> + '_ {
        std::iter::from_fn(|| self.queue.pop())
    }

    /// How many updates were dropped because the queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.get()
    }
}

/// What a [`ScanPlugins::run`] found: each file's outcome and the row changes to
/// apply with `Catalog::apply`. Applying it is the only way it changes a
/// catalog.
#[derive(Debug, Clone)]
#[must_use = "a scan's findings reach the catalog only through Catalog::apply"]
pub struct ScannedPlugins {
    origin: Option<CatalogId>,
    report: ScanReport,
    changes: Vec<RowChange>,
}

// A scan is moved to the thread that runs it, and its handles are read from others.
const _: () = {
    const fn send<T: Send>() {}
    const fn send_sync<T: Send + Sync>() {}
    send::<ScanPlugins>();
    send::<ScannedPlugins>();
    send_sync::<ScanCanceller>();
    send_sync::<ScanProgressReader>();
};

impl ScannedPlugins {
    /// What the run did with each file, in the order they were probed.
    #[must_use]
    pub fn report(&self) -> &ScanReport {
        &self.report
    }

    /// One change per probed file that earns a row, in probe order.
    #[must_use]
    pub fn changes(&self) -> &[RowChange] {
        &self.changes
    }

    pub(crate) fn origin(&self) -> Option<CatalogId> {
        self.origin
    }

    pub(crate) fn into_parts(self) -> (ScanReport, Vec<RowChange>) {
        (self.report, self.changes)
    }
}

/// A scanned output was made for another catalog than the one it was applied to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("this scan's output belongs to another catalog")]
pub struct StaleScan;

/// What one scan looks at.
enum Targets {
    /// Directories to walk; a file whose row is current is skipped.
    Directories(Vec<PathBuf>),
    /// Files to probe, as given, whether or not their rows are current; their
    /// rows are keyed by canonical path.
    Files(Vec<(PathBuf, PluginFormat)>),
}

/// One scan's inputs.
struct Job {
    _claim: Claim,
    snapshot: PluginDb,
    targets: Targets,
    pedal: Pedal,
    probe: Arc<ProbeFn>,
}

impl Job {
    /// Walks, classifies (pure), then probes (effectful), collecting each
    /// change. A stop ends the walk after the probe in flight.
    fn run(&self, stop: &Stop, progress: &Progress) -> (ScanReport, Vec<RowChange>) {
        let emit = |p: ScanProgress| progress.send(p);

        let (plugin_paths, force) = match &self.targets {
            Targets::Directories(directories) => {
                for dir in directories {
                    emit(ScanProgress::Discovering {
                        directory: dir.clone(),
                    });
                }
                let found = discover(directories);
                info!(
                    "discovered {} plugin files across {} directories",
                    found.len(),
                    directories.len()
                );
                (found, false)
            }
            Targets::Files(files) => (files.clone(), true),
        };

        let total = plugin_paths.len();

        let mut result = ScanReport::default();
        let mut changes = Vec::new();
        for (i, (path, format)) in plugin_paths.iter().enumerate() {
            if stop.is_stopped() {
                result.cancelled = true;
                return (result, changes);
            }
            emit(ScanProgress::Probing {
                current: i + 1,
                total,
                path: path.clone(),
            });
            // A named file is probed as named (a link keeps the extension that
            // names its format) and recorded under its canonical path.
            let key = if force {
                canonical_path(path)
            } else {
                path.clone()
            };
            let facts = FileFacts::read_as(path, Some(*format));
            let decision = if force {
                ScanDecision::Probe
            } else {
                facts::classify(self.snapshot.row(&key), &facts)
            };
            let (outcome, change) = match decision {
                ScanDecision::Blacklisted => {
                    debug!("skipping blacklisted plugin: {:?}", path);
                    (ScanOutcome::Blacklisted, None)
                }
                ScanDecision::UpToDate => {
                    debug!("skipping up-to-date plugin: {:?}", path);
                    (ScanOutcome::UpToDate, None)
                }
                ScanDecision::Probe => self.probe_one(stop, path, &key, *format, &facts),
            };
            result.cancelled |= outcome == ScanOutcome::Cancelled;
            result.files.push(ScannedFile { path: key, outcome });
            changes.extend(change);
        }

        info!(
            "scan complete: {} scanned, {} new, {} failed, {} blacklisted",
            result.scanned(),
            result.added(),
            result.failed(),
            result.blacklisted()
        );

        (result, changes)
    }

    /// Probes one file with pedal protection and returns the row to record.
    ///
    /// A probe that crashed or hung is recorded, not merely warned about. Without
    /// a row the file stays out of date and is re-probed at full cost (a 5 s hang,
    /// or a crashing subprocess) on every scan, forever. A crash and a timeout are
    /// properties of the plugin: each recurs identically, so each earns the
    /// blacklist, with its reason for a UI to show. Every other failure says
    /// nothing about the plugin and records nothing
    /// ([`ProbeFailure::blacklistable`]). The pedal covers the other case, the
    /// scanning host itself dying, which out-of-process probing cannot prevent.
    ///
    /// The row carries the file's time, so reinstalling or updating the plugin
    /// re-admits it without the user knowing the blacklist existed.
    fn probe_one(
        &self,
        stop: &Stop,
        path: &Path,
        key: &Path,
        format: PluginFormat,
        facts: &FileFacts,
    ) -> (ScanOutcome, Option<RowChange>) {
        if let Err(e) = self.pedal.arm(path) {
            warn!("failed to arm dead-man's pedal: {e}");
        }
        let probed = (self.probe)(path, format, stop);
        self.pedal.disarm(path);
        // A probe a stop ended says nothing about the plugin.
        if stop.is_stopped() {
            return (ScanOutcome::Cancelled, None);
        }
        match probed {
            Ok(plugins) => {
                match FileRow::probed(format, facts).plugins(plugins).build() {
                    Ok(row) => (ScanOutcome::New, Some(RowChange::put(key, row))),
                    // A probe that read the file as another format than it was asked
                    // for says nothing worth recording: the file is retried.
                    Err(mismatch) => {
                        warn!("probe of {path:?} contradicts its format: {mismatch}");
                        let refusal = ProbeRefusal::Describe(mismatch.to_string());
                        (
                            ScanOutcome::Failed(ProbeFailure::from(ProbeError::Refused(refusal))),
                            None,
                        )
                    }
                }
            }
            Err(failure) if failure.blacklistable() => {
                warn!("failed to probe plugin {:?}: {}", path, failure.reason());
                let row = FileRow::blacklisted(failure.reason(), facts);
                (
                    ScanOutcome::NewlyBlacklisted(failure),
                    Some(RowChange::put(key, row)),
                )
            }
            // A refusal or the environment's failure (no `plugin-probe`, a stale
            // one, a bad command line): says nothing about the plugin, so record
            // nothing and retry once it is fixed.
            Err(failure) => {
                warn!("failed to probe plugin {:?}: {}", path, failure.reason());
                (ScanOutcome::Failed(failure), None)
            }
        }
    }
}

/// What a scan did with one file.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScanOutcome {
    /// Probed, and recorded as a new or updated row.
    New,
    /// Recorded and unchanged since: not probed.
    UpToDate,
    /// Blacklisted before this scan and unchanged since: not probed.
    Blacklisted,
    /// Blacklisted *by* this scan: the probe crashed or hung.
    NewlyBlacklisted(ProbeFailure),
    /// The probe failed in a way that says nothing about the plugin: nothing is
    /// recorded and the file is retried.
    Failed(ProbeFailure),
    /// A stop ended the probe.
    Cancelled,
}

/// One file of a scan and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    /// The file, by the path its row is keyed by.
    pub path: PathBuf,
    /// What the scan did with it.
    pub outcome: ScanOutcome,
}

/// What a scan did: every file it reached, in the order they were probed. The
/// counts are read off them.
#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    files: Vec<ScannedFile>,
    cancelled: bool,
}

/// A probe failure: the typed account of what went wrong.
///
/// A crash and a hang are properties of the plugin: each recurs identically on
/// every future scan, at the same cost, so recording it beats re-probing forever
/// ([`blacklistable`](Self::blacklistable)). Everything else says nothing about the
/// plugin and stays retryable: a refusal (a missing file, a format the probe lacks,
/// a state it would not take, a loader that said no), a missing or stale
/// `plugin-probe`, a probe that exited by one of its own failing codes, a
/// cancelled probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeFailure {
    /// What went wrong.
    pub error: ProbeError,
}

impl ProbeFailure {
    /// Whether this failure earns a blacklist entry: the probe crashed, or hung
    /// until its deadline. A probe that ended by one of its own failing codes did
    /// not crash on the plugin.
    #[must_use]
    pub const fn blacklistable(&self) -> bool {
        match &self.error {
            ProbeError::Died(death) => !death.is_the_probes_own(),
            ProbeError::TimedOut { .. } => true,
            _ => false,
        }
    }

    /// Why the probe failed, as text: what a blacklist row stores.
    #[must_use]
    pub fn reason(&self) -> String {
        self.error.to_string()
    }
}

impl From<ProbeError> for ProbeFailure {
    fn from(error: ProbeError) -> Self {
        Self { error }
    }
}

/// A file a task could not read, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFailure {
    /// The file, by canonical path.
    pub path: PathBuf,
    /// Why its probe failed.
    pub failure: ProbeFailure,
}

impl ScanReport {
    /// Whether a cancel ended the scan before it reached every file.
    #[must_use]
    pub const fn cancelled(&self) -> bool {
        self.cancelled
    }

    /// Every file the scan reached, in the order they were probed.
    #[must_use]
    pub fn files(&self) -> &[ScannedFile] {
        &self.files
    }

    /// Total plugin files examined (including skipped).
    #[must_use]
    pub fn scanned(&self) -> usize {
        self.files.len()
    }

    /// Newly added or updated records.
    #[must_use]
    pub fn added(&self) -> usize {
        self.count(|outcome| matches!(outcome, ScanOutcome::New))
    }

    /// Plugins that failed to load (including those newly blacklisted).
    #[must_use]
    pub fn failed(&self) -> usize {
        self.count(|outcome| {
            matches!(
                outcome,
                ScanOutcome::Failed(_) | ScanOutcome::NewlyBlacklisted(_)
            )
        })
    }

    /// Plugins that were blacklisted (previously or newly).
    #[must_use]
    pub fn blacklisted(&self) -> usize {
        self.count(|outcome| {
            matches!(
                outcome,
                ScanOutcome::Blacklisted | ScanOutcome::NewlyBlacklisted(_)
            )
        })
    }

    /// Plugins this scan added to the blacklist because they crashed or hung.
    /// Surface this so a UI can tell the user something was hidden and point at
    /// [`PluginDb::unblacklist`] / [`PluginDb::clear_blacklist`].
    #[must_use]
    pub fn newly_blacklisted(&self) -> usize {
        self.count(|outcome| matches!(outcome, ScanOutcome::NewlyBlacklisted(_)))
    }

    /// Every file whose probe failed, with the reason, in the order they were
    /// probed. [`failed`](Self::failed) counts these.
    #[must_use]
    pub fn failures(&self) -> Vec<FileFailure> {
        self.files
            .iter()
            .filter_map(|file| match &file.outcome {
                ScanOutcome::Failed(failure) | ScanOutcome::NewlyBlacklisted(failure) => {
                    Some(FileFailure {
                        path: file.path.clone(),
                        failure: failure.clone(),
                    })
                }
                _ => None,
            })
            .collect()
    }

    fn count(&self, of: impl Fn(&ScanOutcome) -> bool) -> usize {
        self.files.iter().filter(|file| of(&file.outcome)).count()
    }
}

/// Minimal metadata from the file name, for a host whose probe is unavailable.
/// The native classification is unknown without a probe, so the descriptor
/// carries [`PluginClass::Unknown`], and with no id read from the file the
/// listing names the file's first plugin.
pub fn probe_plugin_fallback(path: &Path) -> Described {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    Described {
        id: None,
        descriptor: PluginDescriptor::new(name, PluginClass::Unknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mtime::Mtime;

    /// A CLAP plugin listing named `name`.
    fn described(name: &str) -> Described {
        Described::new(
            PluginId::Clap(name.into()),
            PluginDescriptor::new(name, PluginClass::Unknown),
        )
    }
    use crate::table::FileState;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::TempDir;
    use tutti_plugin_types::{
        LoadStage, PluginId, ProbeDeath, ProbeExit, ProbeRefusal, ResultError, Uid,
    };

    fn create_fake_plugin(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"fake plugin binary").unwrap();
        path.canonicalize().unwrap()
    }

    fn at(secs: u64) -> Mtime {
        Mtime::new(UNIX_EPOCH + Duration::from_secs(secs)).unwrap()
    }

    fn earlier(time: Option<Mtime>, by: Duration) -> Option<Mtime> {
        time.and_then(|t| Mtime::new(t.get() - by))
    }

    fn set_mtime(path: &Path, secs: u64) {
        std::fs::File::open(path)
            .unwrap()
            .set_modified(at(secs).get())
            .unwrap();
    }

    fn filename_probe(
        path: &Path,
        _: PluginFormat,
        _: &Stop,
    ) -> Result<Vec<Described>, ProbeFailure> {
        Ok(vec![probe_plugin_fallback(path)])
    }

    /// A generous bound on how long a test waits for an event it knows is
    /// coming; it guards a hang and never measures speed.
    const HANG_GUARD: Duration = Duration::from_secs(30);

    /// Runs a prepared scan and applies its changes to `db`, as a catalog does.
    fn settle(scan: ScanPlugins, db: &mut PluginDb) -> ScanReport {
        let (report, changes) = scan.run().into_parts();
        for change in changes {
            db.apply(change);
        }
        report
    }

    /// Scans `dirs` and applies the findings to `db`.
    fn scan_dirs(
        scanner: &Scanner,
        db: &mut PluginDb,
        dirs: impl IntoIterator<Item = impl AsRef<Path>>,
        save: impl FnOnce(&PluginDb) -> std::io::Result<()>,
    ) -> Result<ScanReport, ScanBusy> {
        let scan = scanner.scan(db, dirs, save)?;
        Ok(settle(scan, db))
    }

    fn no_save(_: &PluginDb) -> std::io::Result<()> {
        Ok(())
    }

    fn scanner_for(dir: &Path) -> Scanner {
        Scanner::new(dir.join(".scanning"), filename_probe)
    }

    /// A scanner whose probe counts its calls and answers `answer`.
    fn counting_scanner(
        dir: &Path,
        answer: fn(&Path) -> Result<Vec<Described>, ProbeFailure>,
    ) -> (Scanner, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let scanner = Scanner::new(dir.join(".scanning"), move |path, _, _| {
            seen.fetch_add(1, Ordering::SeqCst);
            answer(path)
        });
        (scanner, calls)
    }

    fn named(path: &Path) -> Result<Vec<Described>, ProbeFailure> {
        Ok(vec![probe_plugin_fallback(path)])
    }

    /// The failure of a probe whose process was killed by a segfault.
    fn crashed() -> ProbeFailure {
        ProbeError::Died(ProbeDeath::Signal(11)).into()
    }

    /// **Only a crash or a hang blacklists.** A refusal says nothing about the
    /// plugin (a missing file, a format the probe lacks, a loader that said no), nor
    /// does the probe's own failing exit or its absence: each stays retryable.
    #[test]
    fn only_a_crash_or_a_hang_is_blacklistable() {
        let after = Duration::from_secs(15);
        let blacklisted = [
            ProbeError::Died(ProbeDeath::Signal(11)),
            ProbeError::Died(ProbeDeath::Panicked),
            ProbeError::Died(ProbeDeath::Status(7)),
            ProbeError::Died(ProbeDeath::Exited(ProbeExit::Written)),
            // A plugin's own `exit`, even by the codes the probe once used.
            ProbeError::Died(ProbeDeath::Status(2)),
            ProbeError::Died(ProbeDeath::Status(3)),
            ProbeError::Died(ProbeDeath::Status(4)),
            ProbeError::TimedOut { after },
        ];
        for error in blacklisted {
            assert!(
                ProbeFailure::from(error.clone()).blacklistable(),
                "{error:?}"
            );
        }
        let kept = [
            ProbeError::Refused(ProbeRefusal::Missing),
            ProbeError::Refused(ProbeRefusal::UnsupportedFormat),
            ProbeError::Refused(ProbeRefusal::FormatNotCompiled(PluginFormat::AudioUnit)),
            ProbeError::Refused(ProbeRefusal::StateUnreadable("denied".into())),
            ProbeError::Refused(ProbeRefusal::StateRefused {
                plugin: PluginId::Vst2(1),
                reason: "no".into(),
            }),
            ProbeError::Refused(ProbeRefusal::Load {
                stage: LoadStage::Opening,
                reason: "dlopen failed".into(),
            }),
            ProbeError::Refused(ProbeRefusal::Describe("no".into())),
            ProbeError::Refused(ProbeRefusal::UnknownPlugin(PluginId::Vst2(1))),
            ProbeError::Refused(ProbeRefusal::AmbiguousPlugin { count: 2 }),
            ProbeError::Refused(ProbeRefusal::Empty),
            ProbeError::Refused(ProbeRefusal::Usage),
            ProbeError::Refused(ProbeRefusal::NotMainThread),
            ProbeError::Died(ProbeDeath::Exited(ProbeExit::ResultUnwritable)),
            ProbeError::ProbeNotFound,
            ProbeError::Io(std::io::ErrorKind::PermissionDenied),
            ProbeError::Result(ResultError::BadMagic),
            ProbeError::Result(ResultError::ProtocolMismatch {
                expected: 6,
                found: 5,
            }),
            ProbeError::Cancelled,
        ];
        for error in kept {
            assert!(
                !ProbeFailure::from(error.clone()).blacklistable(),
                "{error:?}"
            );
        }
    }

    fn crashes(_: &Path) -> Result<Vec<Described>, ProbeFailure> {
        Err(crashed())
    }

    fn plugins_dir(dir: &TempDir) -> PathBuf {
        let plugins = dir.path().join("plugins");
        std::fs::create_dir(&plugins).unwrap();
        plugins.canonicalize().unwrap()
    }

    #[test]
    fn scan_discovers_and_records() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "synth.vst3");
        create_fake_plugin(&plugins_dir, "comp.clap");

        let mut db = PluginDb::new();
        let result = scan_dirs(&scanner_for(dir.path()), &mut db, &[plugins_dir], no_save).unwrap();
        assert_eq!(result.scanned(), 2);
        assert_eq!(result.added() + result.failed(), 2);
        assert_eq!(db.len(), 2);
    }

    /// `scan` accepts any path-like iterable, not just a `&[PathBuf]`.
    ///
    /// The point of the `IntoIterator<Item = impl AsRef<Path>>` bound is that a
    /// caller with a `Vec<String>`, a borrowed config field, or a filtered
    /// iterator does not have to materialise a `Vec<PathBuf>` first. Each arm
    /// here is a distinct shape that fails to compile under a narrower
    /// signature: `&Vec<PathBuf>` is what `Catalog::scan` passes, `[&str; 1]`
    /// covers the no-PathBuf-in-sight caller, and the `filter` covers a lazy
    /// iterator with no length to read.
    #[test]
    fn scan_accepts_any_path_like_iterable() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "synth.vst3");
        let scanner = scanner_for(dir.path());

        let owned: Vec<PathBuf> = vec![plugins_dir.clone()];
        let mut db = PluginDb::new();
        assert_eq!(
            scan_dirs(&scanner, &mut db, &owned, no_save)
                .unwrap()
                .scanned(),
            1
        );

        let as_str = plugins_dir.to_str().unwrap();
        let mut db = PluginDb::new();
        assert_eq!(
            scan_dirs(&scanner, &mut db, [as_str], no_save)
                .unwrap()
                .scanned(),
            1
        );

        let mut db = PluginDb::new();
        let lazy = owned.iter().filter(|p| p.exists());
        assert_eq!(
            scan_dirs(&scanner, &mut db, lazy, no_save)
                .unwrap()
                .scanned(),
            1
        );
    }

    /// An already-blacklisted plugin is skipped without being probed, and is
    /// counted as blacklisted rather than as a fresh failure: "was hidden before
    /// this scan" and "this scan hid it" are different facts, and only the second
    /// is worth telling the user about.
    #[test]
    fn scan_skips_blacklisted() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let bad = create_fake_plugin(&plugins_dir, "bad.vst3");
        create_fake_plugin(&plugins_dir, "good.vst3");

        let mut db = PluginDb::new();
        db.blacklist(&bad, "known crasher".into(), &FileFacts::read(&bad));

        let result = scan_dirs(&scanner_for(dir.path()), &mut db, &[plugins_dir], no_save).unwrap();

        assert_eq!(result.scanned(), 2);
        assert_eq!(
            result.blacklisted() - result.newly_blacklisted(),
            1,
            "exactly one plugin was hidden before this scan started"
        );
        // `new + failed`, not `+ newly_blacklisted`: `tally` counts a newly
        // blacklisted plugin in both `failed` and `newly_blacklisted`. A scanner
        // that probed the blacklisted file anyway would land it in one of these.
        assert_eq!(result.added() + result.failed(), 1);
    }

    /// **A file whose time changed loses its measurement with its row, and one
    /// that did not keeps it**: a scan re-probes the changed
    /// file, whose new row replaces the measured one, and skips the unchanged
    /// file, whose row stands. Through the scan's own detection of a change.
    #[test]
    fn a_changed_file_loses_its_measurement_in_a_scan() {
        use tutti_plugin_types::{
            LatencyVerdict, MaxBlock, Measured, PROBE_FLOOR, ProbeConfig, SampleRate, Samples,
            StateHash, TailVerdict, Verdict,
        };
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let measured_row = |plugin: &Path, mtime: Option<Mtime>| {
            let mut descriptor = PluginDescriptor::new("p", PluginClass::Unknown);
            descriptor.measured = Some(Measured {
                tail: None,
                latency: None,
                config: ProbeConfig {
                    rate: SampleRate::SR_48K,
                    block: MaxBlock::new(Samples::new(512)).unwrap(),
                    floor: PROBE_FLOOR,
                    state: StateHash::of(&[]),
                },
                verdict: Verdict {
                    tail: TailVerdict::Honest,
                    latency: LatencyVerdict::Honest,
                },
            });
            let facts = FileFacts::Present {
                mtime: mtime.or(FileFacts::read(plugin).mtime()),
            };
            FileRow::probed(PluginFormat::Vst3, &facts)
                .plugins(vec![Described {
                    id: Some(PluginId::Vst3(Uid::new([1; 16]))),
                    descriptor,
                }])
                .build()
                .expect("ids of the row's format")
        };
        let same = create_fake_plugin(&plugins_dir, "same.vst3");
        let changed = create_fake_plugin(&plugins_dir, "changed.vst3");
        let mut db = PluginDb::new();
        db.apply(RowChange::put(&same, measured_row(&same, None)));
        db.apply(RowChange::put(
            &changed,
            measured_row(&changed, Some(at(1))),
        ));

        scan_dirs(&scanner_for(dir.path()), &mut db, &[plugins_dir], no_save).unwrap();

        let measured = |path: &Path| {
            db.row(path).unwrap().plugins()[0]
                .descriptor
                .measured
                .is_some()
        };
        assert!(measured(&same), "an unchanged file keeps its measurement");
        assert!(
            !measured(&changed),
            "a changed file is probed again, unmeasured"
        );
    }

    #[test]
    fn scan_skips_up_to_date() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let plugin = create_fake_plugin(&plugins_dir, "cached.vst3");

        let mut db = PluginDb::new();
        db.apply(RowChange::put(
            &plugin,
            FileRow::probed(PluginFormat::Vst3, &FileFacts::read(&plugin))
                .plugins(vec![Described::new(
                    PluginId::Vst3(Uid::new([1; 16])),
                    PluginDescriptor::new("cached", PluginClass::Unknown),
                )])
                .build()
                .expect("ids of the row's format"),
        ));

        let result = scan_dirs(&scanner_for(dir.path()), &mut db, &[plugins_dir], no_save).unwrap();

        assert_eq!(result.scanned(), 1);
        assert_eq!(result.added(), 0);
    }

    #[test]
    fn recover_crash_blacklists_pedal_plugin() {
        let dir = TempDir::new().unwrap();
        let pedal_file = dir.path().join(".scanning");
        std::fs::write(&pedal_file, "/plugins/crashy.vst3").unwrap();

        let mut db = PluginDb::new();
        let scanner = Scanner::new(pedal_file.clone(), filename_probe);
        scanner.recover_crash(&mut db, no_save).unwrap();

        assert!(db.is_blacklisted(Path::new("/plugins/crashy.vst3")));
        assert!(!pedal_file.exists());
    }

    /// A scan of directories reports each directory it walks, each file it
    /// probes in order with the count.
    #[test]
    fn a_scan_reports_discovery_and_each_probe_as_progress() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "a.vst3");
        create_fake_plugin(&plugins_dir, "b.clap");

        let mut db = PluginDb::new();
        let scan = scanner_for(dir.path())
            .scan(&mut db, [&plugins_dir], no_save)
            .unwrap();
        let progress = scan.progress();
        let result = settle(scan, &mut db);
        assert_eq!(result.added() + result.failed(), 2);

        let updates: Vec<_> = progress.read().collect();
        assert_eq!(updates.len(), 3, "{updates:?}");
        assert_eq!(
            updates[0],
            ScanProgress::Discovering {
                directory: plugins_dir
            }
        );
        let probing: Vec<_> = updates[1..3]
            .iter()
            .map(|u| match u {
                ScanProgress::Probing { current, total, .. } => (*current, *total),
                other => panic!("expected a probe, got {other:?}"),
            })
            .collect();
        assert_eq!(probing, [(1, 2), (2, 2)]);
    }

    /// Progress nobody reads keeps the newest updates: the oldest are dropped
    /// and counted, and the scan still returns with every file recorded.
    #[test]
    fn an_unread_progress_queue_keeps_the_newest_updates_and_counts_the_rest() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let files = PROGRESS_CAPACITY + 10;
        for i in 0..files {
            create_fake_plugin(&plugins_dir, &format!("p{i}.clap"));
        }

        let mut db = PluginDb::new();
        let scan = scanner_for(dir.path())
            .scan(&mut db, [&plugins_dir], no_save)
            .unwrap();
        let progress = scan.progress();
        let result = settle(scan, &mut db);

        assert_eq!(result.added(), files);
        assert_eq!(db.len(), files);
        // One discovery and one probe per file were sent; the queue holds the last
        // `PROGRESS_CAPACITY` of them.
        let sent = files + 1;
        assert_eq!(progress.dropped(), (sent - PROGRESS_CAPACITY) as u64);
        let kept: Vec<_> = progress.read().collect();
        assert_eq!(kept.len(), PROGRESS_CAPACITY);
        assert!(matches!(
            kept.last(),
            Some(ScanProgress::Probing { current, total, .. }) if *current == files && *total == files
        ));
        assert!(matches!(
            kept.first(),
            Some(ScanProgress::Probing { current, .. }) if *current == sent - PROGRESS_CAPACITY
        ));
    }

    /// A scan's findings reach the database only when applied: the scan holds
    /// only a snapshot, so a scan whose changes were never applied would leave
    /// the caller with nothing.
    #[test]
    fn a_scan_changes_the_db_only_when_its_findings_are_applied() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "a.vst3");

        let mut db = PluginDb::new();
        let scan = scanner_for(dir.path())
            .scan(&mut db, &[plugins_dir], no_save)
            .unwrap();
        assert!(
            db.is_empty(),
            "nothing is applied before the owner applies it"
        );
        settle(scan, &mut db);

        assert_eq!(db.len(), 1);
    }

    /// A blacklisted probe result must land in the database and hide the plugin
    /// from `plugins()`.
    #[test]
    fn blacklisting_from_a_failed_probe_hides_the_plugin() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let bad = create_fake_plugin(&plugins_dir, "crashy.vst3");

        let mut db = PluginDb::new();
        db.blacklist(&bad, "crashed during probe".into(), &FileFacts::read(&bad));

        assert!(db.is_blacklisted(&bad));
        assert_eq!(db.plugins().count(), 0);
        // The blacklist stamps the current mtime, so the plugin is skipped
        // while unchanged.
        assert!(db.is_blacklisted_and_unchanged(&bad, &FileFacts::read(&bad)));
        assert!(matches!(
            facts::classify(db.row(&bad), &FileFacts::read(&bad)),
            ScanDecision::Blacklisted
        ));
    }

    /// An mtime change, from a reinstall or a vendor update, must re-admit a
    /// blacklisted plugin. `classify` therefore consults the file time before
    /// the blacklist; checking the flag first makes a false positive permanent
    /// short of hand-editing the JSON.
    #[test]
    fn mtime_change_readmits_a_blacklisted_plugin() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let bad = create_fake_plugin(&plugins_dir, "was-crashy.vst3");

        let mut db = PluginDb::new();
        db.blacklist(&bad, "crashed during probe".into(), &FileFacts::read(&bad));
        assert!(matches!(
            facts::classify(db.row(&bad), &FileFacts::read(&bad)),
            ScanDecision::Blacklisted
        ));

        // Simulate a reinstall: the file's mtime moves.
        let mut row = db.row(&bad).unwrap().clone();
        row.mtime = earlier(row.mtime, Duration::from_secs(1));
        db.apply(RowChange::put(&bad, row));

        assert!(db.is_blacklisted(&bad), "flag is still set");
        assert!(
            !db.is_blacklisted_and_unchanged(&bad, &FileFacts::read(&bad)),
            "but the file changed, so it must be re-probed"
        );
        assert!(
            matches!(
                facts::classify(db.row(&bad), &FileFacts::read(&bad)),
                ScanDecision::Probe
            ),
            "a changed blacklisted plugin must be re-probed, not skipped forever"
        );
    }

    /// Blacklisting must have an inverse.
    #[test]
    fn unblacklist_and_clear_blacklist_are_the_inverse() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let a = create_fake_plugin(&plugins_dir, "a.vst3");
        let b = create_fake_plugin(&plugins_dir, "b.vst3");

        let mut db = PluginDb::new();
        db.blacklist(&a, "pedal misfire".into(), &FileFacts::read(&a));
        db.blacklist(&b, "pedal misfire".into(), &FileFacts::read(&b));
        assert_eq!(db.blacklisted().count(), 2);
        assert_eq!(db.plugins().count(), 0);
        // Blacklisting stamps the live mtime, so the entry is "unchanged".
        assert!(matches!(
            facts::classify(db.row(&a), &FileFacts::read(&a)),
            ScanDecision::Blacklisted
        ));

        assert!(db.unblacklist(&a));
        assert!(!db.is_blacklisted(&a));
        // A lifted file is probed again: a blacklisted row holds no plugins to
        // serve.
        assert!(matches!(
            facts::classify(db.row(&a), &FileFacts::read(&a)),
            ScanDecision::Probe
        ));
        // Second call is a no-op, not a lie.
        assert!(!db.unblacklist(&a));

        let cleared = db.clear_blacklist();
        assert_eq!(cleared, vec![b.clone()]);
        assert_eq!(db.blacklisted().count(), 0);
        assert!(matches!(
            facts::classify(db.row(&b), &FileFacts::read(&b)),
            ScanDecision::Probe
        ));
    }

    #[test]
    fn probe_plugin_fallback_returns_metadata() {
        let meta = probe_plugin_fallback(Path::new("/plugins/My Reverb.vst3"));
        assert_eq!(meta.descriptor.name, "My Reverb");
        assert_eq!(meta.id, None, "a file name holds no id");
    }

    /// The consequence rather than the classification: once a crash is
    /// recorded, the scanner must stop re-probing it. Not writing a row once
    /// meant every scan paid a subprocess spawn to rediscover the same crash.
    #[test]
    fn a_recorded_crash_is_not_reprobed() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let broken = create_fake_plugin(&plugins_dir, "Broken.vst3");

        let mut db = PluginDb::new();
        // What `probe_one` does for a crash.
        let failure = crashed();
        db.blacklist(&broken, failure.reason(), &FileFacts::read(&broken));

        assert!(
            matches!(
                facts::classify(db.row(&broken), &FileFacts::read(&broken)),
                ScanDecision::Blacklisted
            ),
            "a recorded crash must be skipped, not re-probed at full \
             subprocess cost on every scan"
        );

        // Hidden from `plugins()`, but not silently: the reason is retrievable.
        assert_eq!(db.plugins().count(), 0);
        let entry = db.blacklisted().next().expect("must be listed as hidden");
        assert!(entry.reason.contains("signal 11"));

        // And it is not permanent: reinstalling the plugin re-admits it.
        let mut row = db.row(&broken).unwrap().clone();
        row.mtime = earlier(row.mtime, Duration::from_secs(1));
        db.apply(RowChange::put(&broken, row));
        assert!(
            matches!(
                facts::classify(db.row(&broken), &FileFacts::read(&broken)),
                ScanDecision::Probe
            ),
            "a rebuilt or reinstalled plugin must be re-probed"
        );
    }

    // New tests.

    fn two_plugins(path: &Path) -> Result<Vec<Described>, ProbeFailure> {
        let first = probe_plugin_fallback(path);
        let second = described("second");
        Ok(vec![first, second])
    }

    /// **A file that probes into several plugins is one row listing all of them**, in the
    /// probe's order, each its own catalog entry. The scan keeps the whole list: it does
    /// not stop at a file's first plugin.
    #[test]
    fn a_file_with_several_plugins_is_one_row_with_every_plugin() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let bundle = create_fake_plugin(&plugins_dir, "bundle.clap");
        let (scanner, _) = counting_scanner(dir.path(), two_plugins);
        let mut db = PluginDb::new();

        scan_dirs(&scanner, &mut db, [&plugins_dir], no_save).unwrap();

        assert_eq!(db.len(), 1, "one file, one row");
        let names: Vec<_> = db.plugins().map(|p| p.descriptor.name.clone()).collect();
        assert_eq!(names, ["bundle", "second"]);
        assert!(db.plugins().all(|p| p.path == bundle));
    }

    /// A second scan of an unchanged file does not probe it again.
    #[test]
    fn rescanning_an_unchanged_file_does_not_reprobe_it() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "a.clap");
        let (scanner, calls) = counting_scanner(dir.path(), named);
        let mut db = PluginDb::new();

        scan_dirs(&scanner, &mut db, [&plugins_dir], no_save).unwrap();
        let second = scan_dirs(&scanner, &mut db, [&plugins_dir], no_save).unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(second.scanned(), 1);
        assert_eq!(second.added(), 0);
    }

    /// A changed file is probed again and its row replaced, not added to.
    #[test]
    fn a_changed_file_is_reprobed_and_its_row_replaced() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let plugin = create_fake_plugin(&plugins_dir, "a.clap");
        set_mtime(&plugin, 1_000_000_000);
        let (scanner, calls) = counting_scanner(dir.path(), named);
        let mut db = PluginDb::new();
        scan_dirs(&scanner, &mut db, [&plugins_dir], no_save).unwrap();
        assert_eq!(db.row(&plugin).unwrap().mtime, Some(at(1_000_000_000)));

        set_mtime(&plugin, 1_000_000_007);
        let second = scan_dirs(&scanner, &mut db, [&plugins_dir], no_save).unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(second.added(), 1);
        assert_eq!(db.len(), 1);
        assert_eq!(db.row(&plugin).unwrap().mtime, Some(at(1_000_000_007)));
    }

    /// A file deleted between scans keeps its row until `prune_missing`: a scan
    /// that pruned would lose every row under an unmounted volume.
    #[test]
    fn a_file_deleted_between_scans_keeps_its_row_until_pruned() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let kept = create_fake_plugin(&plugins_dir, "kept.clap");
        let gone = create_fake_plugin(&plugins_dir, "gone.clap");
        let (scanner, calls) = counting_scanner(dir.path(), named);
        let mut db = PluginDb::new();
        scan_dirs(&scanner, &mut db, [&plugins_dir], no_save).unwrap();
        std::fs::remove_file(&gone).unwrap();

        let second = scan_dirs(&scanner, &mut db, [&plugins_dir], no_save).unwrap();

        assert_eq!(second.scanned(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "only the two first probes");
        assert!(db.row(&gone).is_some(), "the stale row stays");

        let present = db
            .rows()
            .filter(|(path, _)| FileFacts::read(path).exists())
            .map(|(path, _)| path.to_path_buf())
            .collect();
        assert_eq!(db.prune_missing(&present), vec![gone.clone()]);
        assert!(db.row(&gone).is_none());
        assert!(db.row(&kept).is_some());
    }

    /// A probe that earns a blacklist leaves a row that later scans skip, and a
    /// change to the file lifts it.
    #[test]
    fn a_blacklisted_file_is_skipped_until_it_changes() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let plugin = create_fake_plugin(&plugins_dir, "bad.clap");
        set_mtime(&plugin, 1_000_000_000);
        let (scanner, calls) = counting_scanner(dir.path(), crashes);
        let mut db = PluginDb::new();

        let first = scan_dirs(&scanner, &mut db, [&plugins_dir], no_save).unwrap();
        assert_eq!(first.newly_blacklisted(), 1);
        assert!(db.is_blacklisted(&plugin));
        assert_eq!(db.plugins().count(), 0);

        let second = scan_dirs(&scanner, &mut db, [&plugins_dir], no_save).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the second scan skips it");
        assert_eq!(second.blacklisted() - second.newly_blacklisted(), 1);

        set_mtime(&plugin, 1_000_000_009);
        scan_dirs(&scanner, &mut db, [&plugins_dir], no_save).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "a changed file is probed");
    }

    /// A failure that says nothing about the plugin (a missing server) records
    /// nothing, so the file is retried.
    #[test]
    fn an_environmental_failure_records_nothing() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let plugin = create_fake_plugin(&plugins_dir, "a.clap");
        let (scanner, calls) =
            counting_scanner(dir.path(), |_| Err(ProbeError::ProbeNotFound.into()));
        let mut db = PluginDb::new();

        scan_dirs(&scanner, &mut db, [&plugins_dir], no_save).unwrap();
        scan_dirs(&scanner, &mut db, [&plugins_dir], no_save).unwrap();

        assert!(db.row(&plugin).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// The host dying mid-scan blacklists the file it was probing on the next
    /// start. A probe that panics ends the run with the pedal still armed,
    /// which is all a dead host leaves behind; a fresh scanner over a fresh
    /// database then recovers it through the table, saves, and skips the file.
    #[test]
    fn a_host_dying_mid_scan_blacklists_the_file_on_the_next_start() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let fine = create_fake_plugin(&plugins_dir, "fine.clap");
        let fatal = create_fake_plugin(&plugins_dir, "fatal.clap");
        let pedal_file = dir.path().join(".scanning");

        let dying = Scanner::new(pedal_file.clone(), |path, _, _| {
            if path.ends_with("fatal.clap") {
                panic!("the host went down inside the plugin");
            }
            Ok(vec![probe_plugin_fallback(path)])
        });
        let mut before = PluginDb::new();
        let scan = dying.scan(&mut before, [&plugins_dir], no_save).unwrap();
        let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan.run()));
        assert!(died.is_err(), "the probe's panic reaches the caller");
        assert_eq!(
            std::fs::read_to_string(&pedal_file).unwrap(),
            fatal.to_str().unwrap(),
            "the pedal names the file in flight"
        );

        // Next start: a database from disk (the dead run never saved), a new
        // scanner.
        let (next, calls) = counting_scanner(dir.path(), named);
        let mut db = PluginDb::new();
        let mut saved = None;
        let result = scan_dirs(&next, &mut db, [&plugins_dir], |db| {
            saved = Some(db.clone());
            Ok(())
        })
        .unwrap();

        let row = saved
            .expect("recovery saved before the scan")
            .row(&fatal)
            .cloned()
            .expect("the saved table has the file");
        assert!(matches!(row.state, FileState::Blacklisted { .. }));
        assert!(db.is_blacklisted(&fatal));
        assert!(!pedal_file.exists());
        assert_eq!(result.blacklisted() - result.newly_blacklisted(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "only `fine` is probed");
        assert!(db.row(&fine).is_some());
    }

    /// The database is usable while a scan runs, and every row a reader sees is
    /// whole. The probe of the second file blocks on a gate the test holds, so
    /// the scan is parked mid-flight with no sleep: both files still show their old rows, and the
    /// owner reads and edits throughout, and the findings land when applied. Whatever the order of the two files,
    /// each shows its old row or its new one, never a mixture.
    #[test]
    fn the_db_is_usable_and_rows_are_whole_while_a_scan_is_parked_mid_probe() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let a = create_fake_plugin(&plugins_dir, "a.clap");
        let b = create_fake_plugin(&plugins_dir, "b.clap");
        let (old, new) = (1_000_000_000, 1_000_000_099);
        for p in [&a, &b] {
            set_mtime(p, new);
        }
        let mut db = PluginDb::new();
        for p in [&a, &b] {
            let facts = FileFacts::Present {
                mtime: Some(at(old)),
            };
            let stale = vec![described("old")];
            db.apply(RowChange::put(
                p,
                FileRow::probed(PluginFormat::Clap, &facts)
                    .plugins(stale)
                    .build()
                    .expect("ids of the row's format"),
            ));
        }

        let (parked_tx, parked_rx) = std::sync::mpsc::sync_channel::<PathBuf>(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
        // The probe closure is `Sync`; a `Receiver` is not.
        let release_rx = Mutex::new(release_rx);
        let probes = AtomicUsize::new(0);
        let scanner = Scanner::new(dir.path().join(".scanning"), move |path, _, _| {
            if probes.fetch_add(1, Ordering::SeqCst) == 1 {
                parked_tx.send(path.to_path_buf()).unwrap();
                release_rx.lock().unwrap().recv_timeout(HANG_GUARD).unwrap();
            }
            Ok(vec![described("new")])
        });
        let scan = scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();
        let runner = std::thread::spawn(move || scan.run());

        let parked = parked_rx
            .recv_timeout(HANG_GUARD)
            .expect("the second probe parks");
        let (first, second) = if parked == a { (&b, &a) } else { (&a, &b) };

        // Mid-scan: the owner reads and writes, and applies what has arrived.
        assert_eq!(db.plugins().count(), 2, "readable while the scan is parked");
        db.blacklist(
            Path::new("/elsewhere.vst3"),
            "x".into(),
            &FileFacts::read(first),
        );
        db.unblacklist(Path::new("/elsewhere.vst3"));
        let whole = |db: &PluginDb, path: &Path| {
            let row = db.row(path).unwrap();
            let name = row.plugins()[0].descriptor.name.as_str();
            matches!((row.mtime, name), (Some(t), "old") if t == at(old))
                || matches!((row.mtime, name), (Some(t), "new") if t == at(new))
        };
        // Nothing of the parked scan has reached the table.
        assert!(whole(&db, first) && whole(&db, second));
        assert_eq!(db.row(first).unwrap().mtime, Some(at(old)));
        assert_eq!(db.row(second).unwrap().mtime, Some(at(old)), "still parked");

        release_tx.send(()).unwrap();
        let scanned = runner.join().expect("the run does not panic");
        let (result, changes) = scanned.into_parts();
        for change in changes {
            db.apply(change);
        }
        assert_eq!(result.added(), 2);
        assert!(whole(&db, first) && whole(&db, second));
        assert_eq!(db.row(second).unwrap().mtime, Some(at(new)));
    }

    /// An edit made while a scan runs (a blacklist the user set) is still there
    /// after the scan's changes land: the scan replaces the rows it probed and
    /// touches no other.
    #[test]
    fn an_edit_made_during_a_scan_survives_it() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "a.clap");
        let mut db = PluginDb::new();
        let scan = scanner_for(dir.path())
            .scan(&mut db, [&plugins_dir], no_save)
            .unwrap();

        let elsewhere = Path::new("/elsewhere/user-hid-this.vst3");
        db.blacklist(
            elsewhere,
            "hidden by the user".into(),
            &FileFacts::read(elsewhere),
        );
        settle(scan, &mut db);

        assert!(db.is_blacklisted(elsewhere));
        assert_eq!(db.len(), 2);
    }

    /// A scanner whose first probe parks until released, then answers `answer`.
    /// Returns the scanner, a receiver that fires when the first probe is
    /// parked, a sender that releases it, and the probe count.
    fn gated_scanner(
        dir: &Path,
        answer: fn(&Path) -> Result<Vec<Described>, ProbeFailure>,
    ) -> (
        Scanner,
        std::sync::mpsc::Receiver<PathBuf>,
        std::sync::mpsc::SyncSender<()>,
        Arc<AtomicUsize>,
    ) {
        let (parked_tx, parked_rx) = std::sync::mpsc::sync_channel::<PathBuf>(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
        // The probe closure is `Sync`; a `Receiver` is not.
        let release_rx = Mutex::new(release_rx);
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let scanner = Scanner::new(dir.join(".scanning"), move |path, _, _| {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                parked_tx.send(path.to_path_buf()).unwrap();
                release_rx.lock().unwrap().recv_timeout(HANG_GUARD).unwrap();
            }
            answer(path)
        });
        (scanner, parked_rx, release_tx, calls)
    }

    fn environmental(_: &Path) -> Result<Vec<Described>, ProbeFailure> {
        Err(ProbeError::ProbeNotFound.into())
    }

    /// While a scan is parked mid-probe, nothing else may use its pedal: not a
    /// second scan from the scanner, a clone or a new scanner on the same path,
    /// not a recovery. Each would delete the armed sentinel
    /// and blacklist a plugin that did not crash. The claim ends when the run
    /// returns, so the next scan can start.
    #[test]
    fn nothing_else_may_use_the_pedal_while_a_scan_is_parked_mid_probe() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let a = create_fake_plugin(&plugins_dir, "a.clap");
        let (scanner, parked, release, _) = gated_scanner(dir.path(), named);
        let mut db = PluginDb::new();
        let scan = scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();
        let runner = std::thread::spawn(move || scan.run());
        parked.recv_timeout(HANG_GUARD).unwrap();

        assert!(matches!(
            scanner.scan(&mut db, [&plugins_dir], no_save),
            Err(ScanBusy)
        ));
        let clone = scanner.clone();
        assert!(matches!(
            clone.scan(&mut db, [&plugins_dir], no_save),
            Err(ScanBusy)
        ));
        let fresh = scanner_for(dir.path());
        assert_eq!(fresh.recover_crash(&mut db, no_save), Err(ScanBusy));
        assert_eq!(
            std::fs::read(dir.path().join(".scanning")).unwrap(),
            a.as_os_str().as_encoded_bytes(),
            "the armed sentinel still names the probe in flight"
        );
        assert!(!db.is_blacklisted(&a), "nothing crashed");

        release.send(()).unwrap();
        let _ = runner.join().expect("the run does not panic");
        assert!(
            scanner.scan(&mut db, [&plugins_dir], no_save).is_ok(),
            "the end of the run releases the pedal"
        );
    }

    /// A cancelled run stops after the file in flight even when no file
    /// produces a change (every failure environmental), and the pedal is
    /// disarmed. Without the stop the run would probe every remaining file, each
    /// with a `plugin-server` child.
    #[test]
    fn cancelling_the_scan_stops_the_run_even_when_nothing_is_recorded() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        for name in ["a.clap", "b.clap", "c.clap", "d.clap"] {
            create_fake_plugin(&plugins_dir, name);
        }
        let (scanner, parked, release, calls) = gated_scanner(dir.path(), environmental);
        let mut db = PluginDb::new();
        let scan = scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();
        let canceller = scan.canceller();
        let runner = std::thread::spawn(move || scan.run());
        parked.recv_timeout(HANG_GUARD).unwrap();

        canceller.cancel();
        release.send(()).unwrap();
        let scanned = runner.join().expect("the run does not panic");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(scanned.report().cancelled());
        assert!(scanned.changes().is_empty());
        assert!(
            !dir.path().join(".scanning").exists(),
            "the pedal is disarmed"
        );
    }

    /// **A cancel reaches the probe in flight, and the claim is released when
    /// `run` returns**: the probe waits on its `Stop` as a real one waits on its
    /// child, so `run` returns as soon as the probe sees the request, not when
    /// its own deadline passes (ten minutes here, past the test harness's limit,
    /// so a run that waited for it fails). Its file is `Cancelled`, nothing is
    /// recorded, and the sentinel is gone.
    #[test]
    fn cancelling_the_scan_stops_the_probe_in_flight_and_releases_the_pedal() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let file = create_fake_plugin(&plugins_dir, "a.clap");
        let (probing_tx, probing_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let scanner = Scanner::new(dir.path().join(".scanning"), move |path, _, stop| {
            let _ = probing_tx.try_send(());
            let _ = stop.wait(Duration::from_secs(600));
            named(path)
        });
        let mut db = PluginDb::new();
        let scan = scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();
        let canceller = scan.canceller();
        let runner = std::thread::spawn(move || scan.run());
        probing_rx.recv_timeout(HANG_GUARD).unwrap();

        canceller.cancel();
        let scanned = runner.join().expect("the run does not panic");

        let files = scanned.report().files();
        assert_eq!(
            files,
            [ScannedFile {
                path: file,
                outcome: ScanOutcome::Cancelled
            }]
        );
        assert!(scanned.changes().is_empty());
        assert!(
            !dir.path().join(".scanning").exists(),
            "the sentinel is gone"
        );
        assert!(
            scanner.scan(&mut db, [&plugins_dir], no_save).is_ok(),
            "the pedal is released"
        );
    }

    /// **An unrun scan holds the pedal until it drops**, probes nothing, and
    /// frees the pedal when it does; a run frees it as well.
    #[test]
    fn the_pedal_is_released_on_drop_of_an_unrun_scan_and_after_run() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "a.clap");
        let (scanner, calls) = counting_scanner(dir.path(), named);
        let mut db = PluginDb::new();

        let held = scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();
        assert!(matches!(
            scanner.scan(&mut db, [&plugins_dir], no_save),
            Err(ScanBusy)
        ));
        drop(held);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a dropped scan probes nothing"
        );

        let run = scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();
        let scanned = run.run();
        assert_eq!(scanned.changes().len(), 1);
        assert!(
            scanner.scan(&mut db, [&plugins_dir], no_save).is_ok(),
            "the run released the pedal"
        );
    }

    /// **`run` returns each file's outcome and one change per probed file**, in
    /// probe order: of five files, three probed well, one crashing the probe
    /// (blacklisted), one the environment's failure (nothing recorded).
    #[test]
    fn run_returns_each_files_outcome_and_one_change_per_recorded_file() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        for name in ["a.clap", "b.clap", "c.clap", "crash.clap", "env.clap"] {
            create_fake_plugin(&plugins_dir, name);
        }
        let scanner = Scanner::new(dir.path().join(".scanning"), |path, _, _| {
            match path.file_stem().and_then(|s| s.to_str()) {
                Some("crash") => Err(crashed()),
                Some("env") => Err(ProbeError::ProbeNotFound.into()),
                _ => Ok(vec![probe_plugin_fallback(path)]),
            }
        });
        let mut db = PluginDb::new();

        let scanned = scanner
            .scan(&mut db, [&plugins_dir], no_save)
            .unwrap()
            .run();

        let report = scanned.report();
        assert_eq!(report.scanned(), 5);
        assert_eq!((report.added(), report.newly_blacklisted()), (3, 1));
        assert_eq!(
            report.failed(),
            2,
            "the crash and the environment's failure"
        );
        assert!(!report.cancelled());
        assert_eq!(
            scanned.changes().len(),
            4,
            "three rows and one blacklist row; the environment's failure records nothing"
        );
        assert!(db.is_empty(), "a run changes no database");
    }

    /// **A probe a stop ended records nothing**: no change, no blacklist row,
    /// not even when the probe reports what a killed child looks like (a crash),
    /// and the row that was there is as it was.
    #[test]
    fn a_stopped_probe_leaves_the_row_as_it_was() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let file = create_fake_plugin(&plugins_dir, "a.clap");
        let (probing_tx, probing_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let scanner = Scanner::new(dir.path().join(".scanning"), move |_, _, stop| {
            let _ = probing_tx.try_send(());
            let _ = stop.wait(Duration::from_secs(600));
            Err(crashed())
        });
        let mut db = PluginDb::new();
        scan_dirs(&scanner_for(dir.path()), &mut db, [&plugins_dir], no_save).unwrap();
        let before = db.row(&file).cloned().expect("a row");

        let task = scanner
            .scan_files(&mut db, [(&file, PluginFormat::Clap)], no_save)
            .unwrap();
        let canceller = task.canceller();
        let runner = std::thread::spawn(move || task.run());
        probing_rx.recv_timeout(HANG_GUARD).unwrap();
        canceller.cancel();
        let (result, changes) = runner.join().unwrap().into_parts();
        for change in changes {
            db.apply(change);
        }

        assert!(result.failures().is_empty(), "{result:?}");
        assert_eq!(db.row(&file), Some(&before));
        assert!(!db.is_blacklisted(&file));
    }

    /// **A file task probes the files it names whether or not their rows are
    /// current**, and delivers one change each, keyed by canonical path.
    #[test]
    fn a_file_task_reprobes_a_current_row_and_delivers_its_change() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let file = create_fake_plugin(&plugins_dir, "a.clap");
        let (scanner, calls) = counting_scanner(dir.path(), named);
        let mut db = PluginDb::new();
        assert_eq!(
            scan_dirs(&scanner, &mut db, [&plugins_dir], no_save)
                .unwrap()
                .added(),
            1
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let task = scanner
            .scan_files(&mut db, [(&file, PluginFormat::Clap)], no_save)
            .unwrap();
        let result = settle(task, &mut db);

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the current row is probed again"
        );
        assert_eq!((result.scanned(), result.added()), (1, 1));
        assert!(db.row(&file).is_some());
    }

    /// **A file task hands the probe the format it was given**, whatever the
    /// file's extension says, and records the row under that format: a plugin
    /// whose file is named for another extension (a link's target) is still read
    /// as the format its caller knows it as.
    #[test]
    fn a_file_task_probes_as_the_format_it_was_given() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let file = create_fake_plugin(&plugins_dir, "libsynth.so");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        let scanner = Scanner::new(dir.path().join(".scanning"), move |path, format, _| {
            recorded.lock().unwrap().push(format);
            Ok(vec![probe_plugin_fallback(path)])
        });
        let mut db = PluginDb::new();

        let task = scanner
            .scan_files(&mut db, [(&file, PluginFormat::Clap)], no_save)
            .unwrap();
        settle(task, &mut db);

        assert_eq!(*seen.lock().unwrap(), [PluginFormat::Clap]);
        assert_eq!(
            db.row(&file).expect("a row").format(),
            Some(PluginFormat::Clap)
        );
    }

    /// A file task refuses a path that is not a file at the call, with a named
    /// error, and starts nothing.
    #[test]
    fn a_file_task_names_what_is_wrong_with_a_path_at_the_call() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let missing = plugins_dir.join("gone.clap");
        let (scanner, calls) = counting_scanner(dir.path(), named);
        let mut db = PluginDb::new();

        assert!(matches!(
            scanner
                .scan_files(&mut db, [(&missing, PluginFormat::Clap)], no_save)
                .unwrap_err(),
            FilesError::Missing(path) if path == missing
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            scanner
                .scan_files(&mut db, Vec::<(PathBuf, PluginFormat)>::new(), no_save)
                .is_ok(),
            "a refused call did not claim the pedal"
        );
    }

    /// A probe that panics reaches the caller of `run` and takes the claim down
    /// with it, so the pedal is free for the next scan.
    #[test]
    fn a_panicking_probe_releases_the_claim() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "a.clap");
        let scanner = Scanner::new(dir.path().join(".scanning"), |_, _, _| {
            panic!("the probe dies")
        });
        let mut db = PluginDb::new();
        let scan = scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();

        let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan.run()));

        assert!(died.is_err());
        assert!(scanner.scan(&mut db, [&plugins_dir], no_save).is_ok());
    }

    fn failing_save(_: &PluginDb) -> std::io::Result<()> {
        Err(std::io::Error::other("no store"))
    }

    /// A sentinel whose blacklist could not be saved survives the scan that
    /// follows: the scan arms and disarms its own entry beside it. Overwritten,
    /// the crash record was lost.
    #[test]
    fn a_scan_after_a_failed_recovery_save_keeps_the_retained_sentinel() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "fine.clap");
        let pedal_file = dir.path().join(".scanning");
        std::fs::write(&pedal_file, "/plugins/crashy.vst3").unwrap();
        let (scanner, calls) = counting_scanner(dir.path(), named);
        let mut db = PluginDb::new();

        let result = scan_dirs(&scanner, &mut db, [&plugins_dir], failing_save).unwrap();

        assert_eq!(result.added(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&pedal_file).unwrap(), b"/plugins/crashy.vst3");
        assert!(db.is_blacklisted(Path::new("/plugins/crashy.vst3")));
    }

    /// The retained entry and the crash that follows are both blacklisted by
    /// the next start.
    #[test]
    fn a_crash_after_a_failed_recovery_save_blacklists_both_files_next_start() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let fatal = create_fake_plugin(&plugins_dir, "fatal.clap");
        let pedal_file = dir.path().join(".scanning");
        std::fs::write(&pedal_file, "/plugins/crashy.vst3").unwrap();
        let dying = Scanner::new(pedal_file.clone(), |_, _, _| panic!("host down"));
        let mut db = PluginDb::new();
        let scan = dying.scan(&mut db, [&plugins_dir], failing_save).unwrap();
        let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scan.run()));
        assert!(died.is_err());

        let mut next = PluginDb::new();
        let crashed = scanner_for(dir.path())
            .recover_crash(&mut next, no_save)
            .unwrap();

        assert_eq!(crashed.len(), 2, "{crashed:?}");
        assert!(next.is_blacklisted(Path::new("/plugins/crashy.vst3")));
        assert!(next.is_blacklisted(&fatal));
        assert!(!pedal_file.exists());
    }

    #[cfg(unix)]
    mod unix_tests {
        use super::*;

        /// The same file reached through a link and through its real directory is
        /// one row, whichever order the roots come in, across scans.
        #[test]
        fn roots_that_alias_one_directory_never_duplicate_a_row() {
            let dir = TempDir::new().unwrap();
            let real = plugins_dir(&dir);
            let synth = create_fake_plugin(&real, "s.clap");
            let alias = dir.path().join("alias");
            std::os::unix::fs::symlink(&real, &alias).unwrap();
            let (scanner, calls) = counting_scanner(dir.path(), named);
            let mut db = PluginDb::new();

            scan_dirs(&scanner, &mut db, [&alias, &real], no_save).unwrap();
            scan_dirs(&scanner, &mut db, [&real, &alias], no_save).unwrap();
            scan_dirs(&scanner, &mut db, [&alias], no_save).unwrap();

            assert_eq!(db.len(), 1, "{:?}", db.rows().collect::<Vec<_>>());
            assert_eq!(db.plugins().count(), 1);
            assert!(db.row(&synth).is_some());
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }

        /// A link named `.clap` to a file named anything is keyed by the target and
        /// keeps the link's format.
        #[test]
        fn a_link_to_a_differently_named_file_keeps_its_format() {
            let dir = TempDir::new().unwrap();
            let plugins_dir = plugins_dir(&dir);
            let target = create_fake_plugin(&plugins_dir, "libsynth.bin");
            let links = dir.path().join("links");
            std::fs::create_dir(&links).unwrap();
            std::os::unix::fs::symlink(&target, links.join("synth.clap")).unwrap();
            let scanner = scanner_for(dir.path());
            let mut db = PluginDb::new();

            scan_dirs(&scanner, &mut db, [&links], no_save).unwrap();

            let row = db.row(&target).expect("keyed by the target");
            assert_eq!(row.format(), Some(PluginFormat::Clap));
        }

        /// A pedal reached by another name is the same pedal: through a link to its
        /// directory, or through a `..` path. Compared as raw strings, the second
        /// user got in, blacklisted a plugin that had not crashed and deleted the
        /// live sentinel.
        #[test]
        fn a_pedal_reached_by_another_name_is_still_claimed() {
            let dir = TempDir::new().unwrap();
            let plugins_dir = plugins_dir(&dir);
            let a = create_fake_plugin(&plugins_dir, "a.clap");
            let (scanner, parked, release, _) = gated_scanner(dir.path(), named);
            let mut db = PluginDb::new();
            let scan = scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();
            let runner = std::thread::spawn(move || scan.run());
            parked.recv_timeout(HANG_GUARD).unwrap();

            let link = dir.path().join("link");
            std::os::unix::fs::symlink(dir.path(), &link).unwrap();
            let detour = dir.path().join("plugins").join("..").join(".scanning");
            for name in [link.join(".scanning"), detour] {
                let other = Scanner::new(name.clone(), filename_probe);
                assert_eq!(
                    other.recover_crash(&mut db, no_save),
                    Err(ScanBusy),
                    "{name:?}"
                );
            }
            assert_eq!(
                std::fs::read(dir.path().join(".scanning")).unwrap(),
                a.as_os_str().as_encoded_bytes()
            );
            assert!(!db.is_blacklisted(&a));

            release.send(()).unwrap();
            let _ = runner.join().expect("the run does not panic");
        }
    }
}
