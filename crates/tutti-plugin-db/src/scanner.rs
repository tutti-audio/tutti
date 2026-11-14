//! The plugin scanner, with crash recovery.
//!
//! A scan works on a snapshot of the database: a worker discovers plugin files,
//! probes the new or changed ones and sends a [`Delta`] per file. The owner of
//! the [`PluginDb`] applies them as they arrive ([`Scan::poll`]), so the database
//! stays readable for the whole scan. An internal dead-man's pedal
//! auto-blacklists a plugin that takes the host down while it is probed.

use super::facts::{self, FileFacts, ScanDecision};
use super::format::{PluginClass, PluginDescriptor, PluginFormat};
use super::fs::{canonical_path, discover};
use super::pedal::Pedal;
use super::table::{Delta, FileRow, PluginDb};
use crossbeam_channel::{Receiver, Sender, TryRecvError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tracing::{debug, info, warn};

/// Progress information emitted during a scan.
#[derive(Debug, Clone)]
pub struct ScanProgress {
    /// How many plugins have been processed so far.
    pub current: usize,
    /// How many were discovered in total. Zero during
    /// [`Discovery`](ScanPhase::Discovery), when the count is not yet known.
    pub total: usize,
    /// The plugin being processed as this was emitted.
    pub current_path: PathBuf,
    /// Which phase the scan is in.
    pub phase: ScanPhase,
}

/// Which phase the scanner is currently in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScanPhase {
    /// Finding plugin files on disk.
    Discovery,
    /// Loading and validating individual plugins.
    Scanning,
    /// Scan is finished.
    Complete,
}

/// A request to stop, which a probe in flight watches.
///
/// A [`Scan`] holds one and raises it when it drops; the probe it runs checks it
/// while it waits and, once raised, kills its child process and returns. Clones
/// share the request.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// A request that is not raised.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Raises the request.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether the request is raised.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// What a scanner calls to read one plugin file's metadata.
type ProbeFn = dyn Fn(&Path, PluginFormat, &Cancel) -> Result<Vec<PluginDescriptor>, ProbeFailure>
    + Send
    + Sync;

/// Scans plugin directories into a [`PluginDb`], with crash recovery.
///
/// Each new or changed file is probed through the scanner's probe (the host's
/// out-of-process one); unchanged ones (same modification time) are skipped. A
/// plugin that fails to load, hangs or crashes its probe is blacklisted with the
/// reason, and a sentinel file (the "dead-man's pedal") blacklists the plugin a
/// crashed scan was probing on the next run. `tutti_plugin::catalog::Catalog`
/// drives a scanner for most hosts.
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
    /// watches the [`Cancel`] it is given and, once it is raised, ends the
    /// process it is waiting on and returns at once: a dropped [`Scan`] waits for
    /// the probe in flight to return.
    pub fn new(
        pedal_path: impl Into<PathBuf>,
        probe: impl Fn(&Path, PluginFormat, &Cancel) -> Result<Vec<PluginDescriptor>, ProbeFailure>
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

    /// Scans directories on a background thread, returning immediately.
    ///
    /// Runs crash recovery on the calling thread first (see
    /// [`recover_crash`](Self::recover_crash)), then hands the worker a snapshot
    /// of `db`. The worker's findings come back as deltas: call
    /// [`Scan::poll`] with the database to apply them. Until it does, `db` is
    /// unchanged and fully usable.
    ///
    /// Spawns a thread and hands back channels, in the shape of
    /// [`std::process::Command::spawn`]: this is not an `async fn` and returns
    /// no future. See [`scan`](Self::scan) for the blocking form. Dropping the
    /// [`Scan`] raises the worker's [`Cancel`], which ends the probe in flight,
    /// and waits for the worker, so the next scan can start as soon as the drop
    /// returns.
    ///
    /// # Errors
    ///
    /// [`ScanBusy`] while another scan or a recovery is using the same pedal,
    /// whether from this scanner, a clone of it, or any scanner built on that
    /// pedal path. The claim ends when the worker thread exits, which a dropped
    /// [`Scan`] waits for, so a second scan cannot delete the armed sentinel of
    /// a probe still in flight.
    ///
    /// # Panics
    ///
    /// Panics if the OS refuses to spawn the scanner thread.
    pub fn spawn(
        &self,
        db: &mut PluginDb,
        directories: impl IntoIterator<Item = impl AsRef<Path>>,
        save: impl FnOnce(&PluginDb) -> std::io::Result<()>,
    ) -> Result<Scan, ScanBusy> {
        let targets = Targets::Directories(
            directories
                .into_iter()
                .map(|d| d.as_ref().to_path_buf())
                .collect(),
        );
        Ok(Self::start(self.job(db, targets, save)?))
    }

    /// Probes the given plugin files on a background thread, returning
    /// immediately, whether or not a row for them is up to date: the delta of
    /// each replaces its file's row. Each file comes with the format the caller
    /// knows it as, which the probe reads it as whatever its extension says (a
    /// link `Foo.clap` to `libfoo.so` is CLAP). A row is keyed by the file's
    /// canonical path, as a scan keys it. In every other respect as
    /// [`spawn`](Self::spawn): the same pedal, claim, deltas and drop.
    ///
    /// # Errors
    ///
    /// [`FilesError::Missing`] for a path that is not a file, before anything
    /// starts; [`FilesError::Busy`] as for [`spawn`](Self::spawn).
    ///
    /// # Panics
    ///
    /// Panics if the OS refuses to spawn the scanner thread.
    pub fn spawn_files(
        &self,
        db: &mut PluginDb,
        files: impl IntoIterator<Item = (impl AsRef<Path>, PluginFormat)>,
        save: impl FnOnce(&PluginDb) -> std::io::Result<()>,
    ) -> Result<Scan, FilesError> {
        let mut targets = Vec::new();
        for (file, format) in files {
            let file = file.as_ref();
            if !FileFacts::read_as(file, Some(format)).exists {
                return Err(FilesError::Missing(file.to_path_buf()));
            }
            targets.push((file.to_path_buf(), format));
        }
        Ok(Self::start(self.job(db, Targets::Files(targets), save)?))
    }

    fn start(job: Job) -> Scan {
        let cancel = job.cancel.clone();
        let (progress_tx, progress_rx) = crossbeam_channel::unbounded();
        let (tx, rx) = crossbeam_channel::unbounded();

        let worker = std::thread::Builder::new()
            .name("plugin-scanner".into())
            .spawn(move || {
                let result = job.run(Some(&progress_tx), |delta| {
                    tx.send(Message::Delta(delta)).is_ok()
                });
                // Release the pedal before announcing the end, so a caller that
                // sees `Done` can start the next scan.
                drop(job);
                let _ = tx.send(Message::Done(result));
            })
            .expect("failed to spawn plugin scanner thread");

        Scan {
            progress_rx,
            rx,
            cancel,
            worker: Some(worker),
        }
    }

    /// Scans directories on the calling thread into `db`, blocking until done.
    ///
    /// Runs crash recovery first. A probe may spawn a subprocess, so a large
    /// plugin folder can take minutes the first time; see
    /// [`spawn`](Self::spawn) for the background form.
    ///
    /// `directories` can be `&[PathBuf]`, an array of `&str`, or any iterator
    /// of paths.
    ///
    /// # Errors
    ///
    /// [`ScanBusy`], as for [`spawn`](Self::spawn).
    pub fn scan(
        &self,
        db: &mut PluginDb,
        directories: impl IntoIterator<Item = impl AsRef<Path>>,
        save: impl FnOnce(&PluginDb) -> std::io::Result<()>,
    ) -> Result<ScanResult, ScanBusy> {
        let targets = Targets::Directories(
            directories
                .into_iter()
                .map(|d| d.as_ref().to_path_buf())
                .collect(),
        );
        let job = self.job(db, targets, save)?;
        Ok(job.run(None, |delta| {
            db.apply(delta);
            true
        }))
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
            cancel: Cancel::new(),
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

/// Exclusive use of a pedal, released on drop (so also when a worker panics).
#[derive(Debug)]
struct Claim(PathBuf);

impl Claim {
    fn take(pedal: &Pedal) -> Result<Self, ScanBusy> {
        let file = canonical_path(pedal.path());
        let mut in_use = IN_USE.lock().unwrap_or_else(PoisonError::into_inner);
        if in_use.contains(&file) {
            return Err(ScanBusy);
        }
        in_use.push(file.clone());
        Ok(Self(file))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut in_use = IN_USE.lock().unwrap_or_else(PoisonError::into_inner);
        in_use.retain(|p| *p != self.0);
    }
}

/// A scan or a crash recovery is already using this pedal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[error("a plugin scan is already running on this pedal")]
pub struct ScanBusy;

/// Why a task over named files did not start ([`Scanner::spawn_files`], and the
/// catalog's `register` and `measure` over it).
#[derive(Debug, Clone, PartialEq, Eq, Hash, thiserror::Error)]
pub enum FilesError {
    /// A scan or a crash recovery is already using the pedal.
    #[error(transparent)]
    Busy(#[from] ScanBusy),
    /// A path is not a file that exists.
    #[error("no plugin file at {0}")]
    Missing(PathBuf),
    /// A path's extension names no plugin format, and none was given.
    #[error("{0} has no plugin format's extension")]
    UnrecognizedFormat(PathBuf),
}

/// The one task type of discovery: a worker probing files, in flight. A scan of
/// directories ([`Scanner::spawn`]), a registration and a measurement (both
/// [`Scanner::spawn_files`]) each probe files and send a [`Delta`] per file.
///
/// Dropping it cancels the work ([`Cancel`]).
///
/// The worker owns a snapshot of the database; this is the other end. It holds
/// no database, so the owner reads its own throughout and applies what arrives
/// when it chooses.
#[derive(Debug)]
#[must_use]
pub struct Scan {
    /// Per-plugin progress, emitted as the scan advances. Only a scan of
    /// directories emits [`ScanPhase::Discovery`] (one per directory, before the
    /// walk); a task over named files emits [`ScanPhase::Scanning`] and
    /// [`ScanPhase::Complete`] only.
    pub progress_rx: Receiver<ScanProgress>,
    rx: Receiver<Message>,
    cancel: Cancel,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Scan {
    /// Cancels the probe in flight, which kills its child process, and waits for
    /// the worker to exit, so the pedal is released when the drop returns. The
    /// wait is the probe's reaction to the cancel, not its handshake deadline.
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(worker) = self.worker.take() {
            // A worker that panicked is reported by `poll` as `Died`; its
            // claim is released by unwinding.
            let _ = worker.join();
        }
    }
}

/// What the worker sends the owner.
#[derive(Debug)]
enum Message {
    Delta(Delta),
    Done(ScanResult),
}

/// Where a scan stands after [`Scan::poll`].
#[derive(Debug)]
pub enum ScanPoll {
    /// The worker is still going; every delta sent so far has been applied.
    Running,
    /// The worker finished and every delta has been applied. Drop the [`Scan`];
    /// polling it again reports [`Died`](Self::Died).
    Done(ScanResult),
    /// The worker thread ended without finishing, so a probe panicked. Deltas
    /// sent before that have been applied.
    Died,
}

impl Scan {
    /// Applies every delta the worker has sent to `db` and reports where the
    /// scan stands. Never blocks.
    ///
    /// This and [`wait`](Self::wait) are the only places a scan changes a
    /// database, and each delta goes through [`PluginDb::apply`].
    pub fn poll(&self, db: &mut PluginDb) -> ScanPoll {
        loop {
            match self.rx.try_recv() {
                Ok(Message::Delta(delta)) => db.apply(delta),
                Ok(Message::Done(result)) => return ScanPoll::Done(result),
                Err(TryRecvError::Empty) => return ScanPoll::Running,
                Err(TryRecvError::Disconnected) => return ScanPoll::Died,
            }
        }
    }

    /// Blocks until the scan ends, applying deltas to `db` as they arrive.
    /// Returns `None` if the worker died without finishing.
    pub fn wait(self, db: &mut PluginDb) -> Option<ScanResult> {
        while let Ok(message) = self.rx.recv() {
            match message {
                Message::Delta(delta) => db.apply(delta),
                Message::Done(result) => return Some(result),
            }
        }
        None
    }
}

/// What one scan looks at.
enum Targets {
    /// Directories to walk; a file whose row is current is skipped.
    Directories(Vec<PathBuf>),
    /// Files to probe, as given, whether or not their rows are current; their
    /// rows are keyed by canonical path.
    Files(Vec<(PathBuf, PluginFormat)>),
}

/// One scan's inputs, moved onto its worker.
struct Job {
    _claim: Claim,
    cancel: Cancel,
    snapshot: PluginDb,
    targets: Targets,
    pedal: Pedal,
    probe: Arc<ProbeFn>,
}

impl Job {
    /// Walks, classifies (pure), then probes (effectful), passing each delta to
    /// `sink`. A sink that returns `false` stops the scan.
    fn run(
        &self,
        progress_tx: Option<&Sender<ScanProgress>>,
        mut sink: impl FnMut(Delta) -> bool,
    ) -> ScanResult {
        let emit = |p: ScanProgress| {
            if let Some(tx) = progress_tx {
                let _ = tx.send(p);
            }
        };

        let (plugin_paths, force) = match &self.targets {
            Targets::Directories(directories) => {
                for dir in directories {
                    emit(ScanProgress {
                        current: 0,
                        total: 0,
                        current_path: dir.clone(),
                        phase: ScanPhase::Discovery,
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

        let mut result = ScanResult::with_total(total);
        for (i, (path, format)) in plugin_paths.iter().enumerate() {
            if self.cancel.is_cancelled() {
                return result;
            }
            emit(ScanProgress {
                current: i + 1,
                total,
                current_path: path.clone(),
                phase: ScanPhase::Scanning,
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
            let (outcome, delta, failure) = match decision {
                ScanDecision::Blacklisted => {
                    debug!("skipping blacklisted plugin: {:?}", path);
                    (ScanOutcome::Blacklisted, None, None)
                }
                ScanDecision::UpToDate => {
                    debug!("skipping up-to-date plugin: {:?}", path);
                    (ScanOutcome::UpToDate, None, None)
                }
                ScanDecision::Probe => self.probe_one(path, &key, *format, &facts),
            };
            result = result.tally(outcome);
            if let Some(failure) = failure {
                result.failures.push(FileFailure {
                    path: key.clone(),
                    failure,
                });
            }
            if let Some(delta) = delta
                && !sink(delta)
            {
                return result;
            }
        }

        emit(ScanProgress {
            current: total,
            total,
            current_path: PathBuf::new(),
            phase: ScanPhase::Complete,
        });

        info!(
            "scan complete: {} scanned, {} new, {} failed, {} blacklisted",
            result.scanned, result.new, result.failed, result.blacklisted
        );

        result
    }

    /// Probes one file with pedal protection and returns the row to record.
    ///
    /// A failing probe is recorded, not merely warned about. Without a row the
    /// file stays out of date and is re-probed at full cost (a 5 s hang, or a
    /// crashing subprocess) on every scan, forever. A crash, a timeout and a
    /// library that will not load are properties of the plugin: each recurs
    /// identically, so each earns the blacklist, with the loader's own reason for
    /// a UI to show. The pedal covers the other case, the scanning host itself
    /// dying, which out-of-process probing cannot prevent.
    ///
    /// The row carries the file's time, so reinstalling or updating the plugin
    /// re-admits it without the user knowing the blacklist existed.
    fn probe_one(
        &self,
        path: &Path,
        key: &Path,
        format: PluginFormat,
        facts: &FileFacts,
    ) -> (ScanOutcome, Option<Delta>, Option<ProbeFailure>) {
        if let Err(e) = self.pedal.arm(path) {
            warn!("failed to arm dead-man's pedal: {e}");
        }
        let probed = (self.probe)(path, format, &self.cancel);
        self.pedal.disarm(path);
        // A probe a cancel ended says nothing about the plugin.
        if self.cancel.is_cancelled() {
            return (ScanOutcome::Failed, None, None);
        }
        match probed {
            Ok(plugins) => (
                ScanOutcome::New,
                Some(Delta::put(key, FileRow::probed(format, facts, plugins))),
                None,
            ),
            Err(failure) if failure.blacklistable() => {
                warn!("failed to probe plugin {:?}: {}", path, failure.reason);
                (
                    ScanOutcome::NewlyBlacklisted,
                    Some(Delta::put(
                        key,
                        FileRow::blacklisted(failure.reason.clone(), facts),
                    )),
                    Some(failure),
                )
            }
            // Environmental (no plugin-server, an IO error, a protocol
            // mismatch): says nothing about the plugin, so record nothing and
            // retry once the environment is fixed.
            Err(failure) => {
                warn!("failed to probe plugin {:?}: {}", path, failure.reason);
                (ScanOutcome::Failed, None, Some(failure))
            }
        }
    }
}

/// Per-plugin outcome tallied into `ScanResult`.
#[derive(Clone, Copy)]
enum ScanOutcome {
    New,
    Failed,
    /// Already blacklisted before this scan; skipped.
    Blacklisted,
    /// Blacklisted *by* this scan: a crash, a timeout, or a failed load.
    NewlyBlacklisted,
    UpToDate,
}

/// Summary of a completed scan.
#[derive(Debug, Clone)]
pub struct ScanResult {
    /// Total plugins examined (including skipped).
    pub scanned: usize,
    /// Newly added or updated records.
    pub new: usize,
    /// Plugins that failed to load (including those newly blacklisted).
    pub failed: usize,
    /// Plugins that were blacklisted (previously or newly).
    pub blacklisted: usize,
    /// Plugins this scan added to the blacklist because they crashed, hung, or
    /// failed to load. Surface this so a UI can tell the user something was hidden
    /// and point at [`PluginDb::unblacklist`] / [`PluginDb::clear_blacklist`].
    pub newly_blacklisted: usize,
    /// Every file whose probe failed, with the reason, in the order they were
    /// probed. `failed` counts these.
    pub failures: Vec<FileFailure>,
}

/// What went wrong in a probe.
///
/// The first three are properties of the plugin: each recurs identically on every
/// future scan, at the same cost, so recording it beats re-probing forever
/// ([`blacklistable`](Self::blacklistable)). The rest say nothing about the plugin
/// and stay retryable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProbeFailureKind {
    /// The probe process died without a result.
    Crashed,
    /// The probe did not finish by its deadline.
    TimedOut,
    /// The probe ran to its end and the plugin would not load.
    LoadFailed,
    /// The probe executable could not be found, so nothing was looked at.
    ProbeNotFound,
    /// Anything else of the environment: an I/O error, a protocol mismatch.
    Environment,
}

impl ProbeFailureKind {
    /// Whether the failure is a property of the plugin, so a scan blacklists it.
    #[must_use]
    pub const fn blacklistable(self) -> bool {
        matches!(self, Self::Crashed | Self::TimedOut | Self::LoadFailed)
    }
}

/// A probe failure: what kind it was and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeFailure {
    /// Why the probe failed; stored as the blacklist reason.
    pub reason: String,
    /// What kind of failure it was.
    pub kind: ProbeFailureKind,
}

impl ProbeFailure {
    /// Whether this failure earns a blacklist entry
    /// ([`ProbeFailureKind::blacklistable`]).
    #[must_use]
    pub const fn blacklistable(&self) -> bool {
        self.kind.blacklistable()
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

impl ScanResult {
    fn with_total(scanned: usize) -> Self {
        Self {
            scanned,
            new: 0,
            failed: 0,
            blacklisted: 0,
            newly_blacklisted: 0,
            failures: Vec::new(),
        }
    }

    fn tally(mut self, outcome: ScanOutcome) -> Self {
        match outcome {
            ScanOutcome::New => self.new += 1,
            ScanOutcome::Failed => self.failed += 1,
            ScanOutcome::Blacklisted => self.blacklisted += 1,
            // A newly blacklisted plugin is both a failure this scan and a
            // blacklist entry going forward; count it in both tallies so
            // `failed` still means "did not load".
            ScanOutcome::NewlyBlacklisted => {
                self.failed += 1;
                self.blacklisted += 1;
                self.newly_blacklisted += 1;
            }
            ScanOutcome::UpToDate => {}
        }
        self
    }
}

/// Minimal metadata from the file name, for a host whose probe is unavailable.
/// The native classification is unknown without a probe, so the descriptor
/// carries [`PluginClass::Unknown`].
pub fn probe_plugin_fallback(path: &Path, format: PluginFormat) -> PluginDescriptor {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let id = format!(
        "{}.{}",
        format.extension_id(),
        name.to_lowercase().replace(' ', "_")
    );

    PluginDescriptor::new(id, name, PluginClass::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::FileState;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::TempDir;

    fn create_fake_plugin(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, b"fake plugin binary").unwrap();
        path.canonicalize().unwrap()
    }

    fn set_mtime(path: &Path, secs: u64) {
        std::fs::File::open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    fn filename_probe(
        path: &Path,
        format: PluginFormat,
        _: &Cancel,
    ) -> Result<Vec<PluginDescriptor>, ProbeFailure> {
        Ok(vec![probe_plugin_fallback(path, format)])
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
        answer: fn(&Path) -> Result<Vec<PluginDescriptor>, ProbeFailure>,
    ) -> (Scanner, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let scanner = Scanner::new(dir.join(".scanning"), move |path, _, _| {
            seen.fetch_add(1, Ordering::SeqCst);
            answer(path)
        });
        (scanner, calls)
    }

    fn named(path: &Path) -> Result<Vec<PluginDescriptor>, ProbeFailure> {
        Ok(vec![probe_plugin_fallback(path, PluginFormat::Clap)])
    }

    fn crashes(_: &Path) -> Result<Vec<PluginDescriptor>, ProbeFailure> {
        Err(ProbeFailure {
            reason: "crashed during probe".into(),
            kind: ProbeFailureKind::Crashed,
        })
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
        let result = scanner_for(dir.path())
            .scan(&mut db, &[plugins_dir], no_save)
            .unwrap();
        assert_eq!(result.scanned, 2);
        assert_eq!(result.new + result.failed, 2);
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
        assert_eq!(scanner.scan(&mut db, &owned, no_save).unwrap().scanned, 1);

        let as_str = plugins_dir.to_str().unwrap();
        let mut db = PluginDb::new();
        assert_eq!(scanner.scan(&mut db, [as_str], no_save).unwrap().scanned, 1);

        let mut db = PluginDb::new();
        let lazy = owned.iter().filter(|p| p.exists());
        assert_eq!(scanner.scan(&mut db, lazy, no_save).unwrap().scanned, 1);
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

        let result = scanner_for(dir.path())
            .scan(&mut db, &[plugins_dir], no_save)
            .unwrap();

        assert_eq!(result.scanned, 2);
        assert_eq!(
            result.blacklisted - result.newly_blacklisted,
            1,
            "exactly one plugin was hidden before this scan started"
        );
        // `new + failed`, not `+ newly_blacklisted`: `tally` counts a newly
        // blacklisted plugin in both `failed` and `newly_blacklisted`. A scanner
        // that probed the blacklisted file anyway would land it in one of these.
        assert_eq!(result.new + result.failed, 1);
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
        let measured_row = |plugin: &Path, mtime: Option<u64>| {
            let mut descriptor = PluginDescriptor::new("p", "p", PluginClass::Unknown);
            descriptor.measured = Some(Measured {
                tail: None,
                latency: None,
                config: ProbeConfig {
                    rate: SampleRate::SR_48K,
                    block: MaxBlock::new(Samples(512)).unwrap(),
                    floor: PROBE_FLOOR,
                    state: StateHash::of(&[]),
                },
                verdict: Verdict {
                    tail: TailVerdict::Honest,
                    latency: LatencyVerdict::Honest,
                },
            });
            let mut facts = FileFacts::read(plugin);
            facts.mtime = mtime.or(facts.mtime);
            FileRow::probed(PluginFormat::Vst3, &facts, vec![descriptor])
        };
        let same = create_fake_plugin(&plugins_dir, "same.vst3");
        let changed = create_fake_plugin(&plugins_dir, "changed.vst3");
        let mut db = PluginDb::new();
        db.apply(Delta::put(&same, measured_row(&same, None)));
        db.apply(Delta::put(&changed, measured_row(&changed, Some(1))));

        scanner_for(dir.path())
            .scan(&mut db, &[plugins_dir], no_save)
            .unwrap();

        let measured = |path: &Path| db.row(path).unwrap().plugins()[0].measured.is_some();
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
        db.apply(Delta::put(
            &plugin,
            FileRow::probed(
                PluginFormat::Vst3,
                &FileFacts::read(&plugin),
                vec![PluginDescriptor::new(
                    "cached",
                    "cached",
                    PluginClass::Unknown,
                )],
            ),
        ));

        let result = scanner_for(dir.path())
            .scan(&mut db, &[plugins_dir], no_save)
            .unwrap();

        assert_eq!(result.scanned, 1);
        assert_eq!(result.new, 0);
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

    #[test]
    fn spawned_scan_with_progress() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "a.vst3");
        create_fake_plugin(&plugins_dir, "b.clap");

        let mut db = PluginDb::new();
        let scan = scanner_for(dir.path())
            .spawn(&mut db, &[plugins_dir], no_save)
            .unwrap();

        let mut saw_discovery = false;
        let mut saw_scanning = false;
        let mut saw_complete = false;
        while let Ok(p) = scan.progress_rx.recv_timeout(Duration::from_secs(30)) {
            match p.phase {
                ScanPhase::Discovery => saw_discovery = true,
                ScanPhase::Scanning => saw_scanning = true,
                ScanPhase::Complete => {
                    saw_complete = true;
                    break;
                }
            }
        }
        assert!(saw_discovery);
        assert!(saw_scanning);
        assert!(saw_complete);

        let result = scan.wait(&mut db).expect("the worker finishes");
        assert_eq!(result.new + result.failed, 2);
    }

    /// A spawned scan's findings reach the database through `wait`: the worker
    /// holds only a snapshot, so a scan whose deltas were never applied would
    /// leave the caller with nothing.
    #[test]
    fn a_spawned_scan_applies_its_findings_to_the_db() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "a.vst3");

        let mut db = PluginDb::new();
        let scan = scanner_for(dir.path())
            .spawn(&mut db, &[plugins_dir], no_save)
            .unwrap();
        assert!(db.is_empty(), "nothing is applied before the owner polls");
        scan.wait(&mut db).expect("the worker finishes");

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
        row.mtime = row.mtime.map(|t| t - 1);
        db.apply(Delta::put(&bad, row));

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
        let meta = probe_plugin_fallback(Path::new("/plugins/My Reverb.vst3"), PluginFormat::Vst3);
        assert_eq!(meta.name, "My Reverb");
        assert_eq!(meta.id, "vst3.my_reverb");
    }

    /// The consequence rather than the classification: once a load failure is
    /// recorded, the scanner must stop re-probing it. `blacklistable: false`
    /// once meant no write at all, so every scan paid a subprocess spawn to
    /// rediscover the same broken library.
    #[test]
    fn a_recorded_load_failure_is_not_reprobed() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let broken = create_fake_plugin(&plugins_dir, "Broken.vst3");

        let mut db = PluginDb::new();
        // What `probe_one` does for a LoadFailed.
        let failure = ProbeFailure {
            reason: "failed to load during probe: dlopen failed".into(),
            kind: ProbeFailureKind::LoadFailed,
        };
        db.blacklist(&broken, failure.reason, &FileFacts::read(&broken));

        assert!(
            matches!(
                facts::classify(db.row(&broken), &FileFacts::read(&broken)),
                ScanDecision::Blacklisted
            ),
            "a recorded load failure must be skipped, not re-probed at full \
             subprocess cost on every scan"
        );

        // Hidden from `plugins()`, but not silently: the reason is retrievable.
        assert_eq!(db.plugins().count(), 0);
        let entry = db.blacklisted().next().expect("must be listed as hidden");
        assert!(entry.reason.contains("dlopen"));

        // And it is not permanent: reinstalling the plugin re-admits it.
        let mut row = db.row(&broken).unwrap().clone();
        row.mtime = row.mtime.map(|t| t - 1);
        db.apply(Delta::put(&broken, row));
        assert!(
            matches!(
                facts::classify(db.row(&broken), &FileFacts::read(&broken)),
                ScanDecision::Probe
            ),
            "a rebuilt or reinstalled plugin must be re-probed"
        );
    }

    // New tests.

    fn two_plugins(path: &Path) -> Result<Vec<PluginDescriptor>, ProbeFailure> {
        let first = probe_plugin_fallback(path, PluginFormat::Clap);
        let second = PluginDescriptor::new("second", "Second", PluginClass::Unknown);
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

        scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();

        assert_eq!(db.len(), 1, "one file, one row");
        let ids: Vec<_> = db.plugins().map(|p| p.descriptor.id.clone()).collect();
        assert_eq!(ids, ["clap.bundle", "second"]);
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

        scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();
        let second = scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(second.scanned, 1);
        assert_eq!(second.new, 0);
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
        scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();
        assert_eq!(db.row(&plugin).unwrap().mtime, Some(1_000_000_000));

        set_mtime(&plugin, 1_000_000_007);
        let second = scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(second.new, 1);
        assert_eq!(db.len(), 1);
        assert_eq!(db.row(&plugin).unwrap().mtime, Some(1_000_000_007));
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
        scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();
        std::fs::remove_file(&gone).unwrap();

        let second = scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();

        assert_eq!(second.scanned, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "only the two first probes");
        assert!(db.row(&gone).is_some(), "the stale row stays");

        let present = db
            .rows()
            .filter(|(path, _)| FileFacts::read(path).exists)
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

        let first = scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();
        assert_eq!(first.newly_blacklisted, 1);
        assert!(db.is_blacklisted(&plugin));
        assert_eq!(db.plugins().count(), 0);

        let second = scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the second scan skips it");
        assert_eq!(second.blacklisted - second.newly_blacklisted, 1);

        set_mtime(&plugin, 1_000_000_009);
        scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "a changed file is probed");
    }

    /// A failure that says nothing about the plugin (a missing server) records
    /// nothing, so the file is retried.
    #[test]
    fn an_environmental_failure_records_nothing() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let plugin = create_fake_plugin(&plugins_dir, "a.clap");
        let (scanner, calls) = counting_scanner(dir.path(), |_| {
            Err(ProbeFailure {
                reason: "no plugin-server".into(),
                kind: ProbeFailureKind::Environment,
            })
        });
        let mut db = PluginDb::new();

        scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();
        scanner.scan(&mut db, [&plugins_dir], no_save).unwrap();

        assert!(db.row(&plugin).is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// The host dying mid-scan blacklists the file it was probing on the next
    /// start. A probe that panics ends the worker with the pedal still armed,
    /// which is all a dead host leaves behind; a fresh scanner over a fresh
    /// database then recovers it through the table, saves, and skips the file.
    #[test]
    fn a_host_dying_mid_scan_blacklists_the_file_on_the_next_start() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let fine = create_fake_plugin(&plugins_dir, "fine.clap");
        let fatal = create_fake_plugin(&plugins_dir, "fatal.clap");
        let pedal_file = dir.path().join(".scanning");

        let dying = Scanner::new(pedal_file.clone(), |path, format, _| {
            if path.ends_with("fatal.clap") {
                panic!("the host went down inside the plugin");
            }
            Ok(vec![probe_plugin_fallback(path, format)])
        });
        let mut before = PluginDb::new();
        let scan = dying.spawn(&mut before, [&plugins_dir], no_save).unwrap();
        assert!(scan.wait(&mut before).is_none(), "the worker died");
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
        let result = next
            .scan(&mut db, [&plugins_dir], |db| {
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
        assert_eq!(result.blacklisted - result.newly_blacklisted, 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "only `fine` is probed");
        assert!(db.row(&fine).is_some());
    }

    /// The database is usable while a scan runs, and every row a reader sees is
    /// whole. The probe of the second file blocks on a gate the test holds, so
    /// the scan is parked mid-flight with no sleep: the first file's delta is
    /// already there to apply, the second file still shows its old row, and the
    /// owner reads and edits throughout. Whatever the order of the two files,
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
            let facts = FileFacts {
                mtime: Some(old),
                ..FileFacts::read(p)
            };
            let stale = vec![PluginDescriptor::new("old", "old", PluginClass::Unknown)];
            db.apply(Delta::put(
                p,
                FileRow::probed(PluginFormat::Clap, &facts, stale),
            ));
        }

        let (parked_tx, parked_rx) = crossbeam_channel::bounded::<PathBuf>(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
        let probes = AtomicUsize::new(0);
        let scanner = Scanner::new(dir.path().join(".scanning"), move |path, _, _| {
            if probes.fetch_add(1, Ordering::SeqCst) == 1 {
                parked_tx.send(path.to_path_buf()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            }
            Ok(vec![PluginDescriptor::new(
                "new",
                "new",
                PluginClass::Unknown,
            )])
        });
        let scan = scanner.spawn(&mut db, [&plugins_dir], no_save).unwrap();

        let parked = parked_rx
            .recv_timeout(Duration::from_secs(30))
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
            let name = row.plugins()[0].name.as_str();
            matches!((row.mtime, name), (Some(t), "old") if t == old)
                || matches!((row.mtime, name), (Some(t), "new") if t == new)
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut seen_first_new = false;
        while !seen_first_new {
            assert!(
                std::time::Instant::now() < deadline,
                "the first file's delta never arrived while the scan was parked"
            );
            assert!(matches!(scan.poll(&mut db), ScanPoll::Running));
            assert!(whole(&db, first) && whole(&db, second));
            seen_first_new = db.row(first).unwrap().mtime == Some(new);
            std::thread::yield_now();
        }
        assert_eq!(db.row(second).unwrap().mtime, Some(old), "still parked");

        release_tx.send(()).unwrap();
        let result = loop {
            match scan.poll(&mut db) {
                ScanPoll::Done(result) => break result,
                ScanPoll::Running => assert!(whole(&db, first) && whole(&db, second)),
                ScanPoll::Died => panic!("the worker died"),
            }
            std::thread::yield_now();
        };
        assert_eq!(result.new, 2);
        assert_eq!(db.row(second).unwrap().mtime, Some(new));
    }

    /// An edit made while a scan runs (a blacklist the user set) is still there
    /// after the scan's deltas land: the scan replaces the rows it probed and
    /// touches no other.
    #[test]
    fn an_edit_made_during_a_scan_survives_it() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "a.clap");
        let mut db = PluginDb::new();
        let scan = scanner_for(dir.path())
            .spawn(&mut db, [&plugins_dir], no_save)
            .unwrap();

        let elsewhere = Path::new("/elsewhere/user-hid-this.vst3");
        db.blacklist(
            elsewhere,
            "hidden by the user".into(),
            &FileFacts::read(elsewhere),
        );
        scan.wait(&mut db).expect("the worker finishes");

        assert!(db.is_blacklisted(elsewhere));
        assert_eq!(db.len(), 2);
    }

    /// A scanner whose first probe parks until released, then answers `answer`.
    /// Returns the scanner, a receiver that fires when the first probe is
    /// parked, a sender that releases it, and the probe count.
    fn gated_scanner(
        dir: &Path,
        answer: fn(&Path) -> Result<Vec<PluginDescriptor>, ProbeFailure>,
    ) -> (
        Scanner,
        crossbeam_channel::Receiver<PathBuf>,
        crossbeam_channel::Sender<()>,
        Arc<AtomicUsize>,
    ) {
        let (parked_tx, parked_rx) = crossbeam_channel::bounded::<PathBuf>(1);
        let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(1);
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let scanner = Scanner::new(dir.join(".scanning"), move |path, _, _| {
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                parked_tx.send(path.to_path_buf()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            }
            answer(path)
        });
        (scanner, parked_rx, release_tx, calls)
    }

    fn environmental(_: &Path) -> Result<Vec<PluginDescriptor>, ProbeFailure> {
        Err(ProbeFailure {
            reason: "no plugin-server".into(),
            kind: ProbeFailureKind::Environment,
        })
    }

    /// Waits until the worker has exited: its progress sender is the last thing
    /// it drops.
    fn worker_exited(progress: &Receiver<ScanProgress>) {
        loop {
            match progress.recv_timeout(Duration::from_secs(30)) {
                Ok(_) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                Err(e) => panic!("the worker did not exit: {e}"),
            }
        }
    }

    /// While a scan is parked mid-probe, nothing else may use its pedal: not a
    /// second scan from the scanner, a clone or a new scanner on the same path,
    /// not a blocking scan, not a recovery. Each would delete the armed sentinel
    /// and blacklist a plugin that did not crash. The claim ends when the worker
    /// exits, so `Done` already allows the next scan.
    #[test]
    fn nothing_else_may_use_the_pedal_while_a_scan_is_parked_mid_probe() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let a = create_fake_plugin(&plugins_dir, "a.clap");
        let (scanner, parked, release, _) = gated_scanner(dir.path(), named);
        let mut db = PluginDb::new();
        let scan = scanner.spawn(&mut db, [&plugins_dir], no_save).unwrap();
        parked.recv_timeout(Duration::from_secs(30)).unwrap();

        assert_eq!(
            scanner.spawn(&mut db, [&plugins_dir], no_save).err(),
            Some(ScanBusy)
        );
        let clone = scanner.clone();
        assert_eq!(
            clone.spawn(&mut db, [&plugins_dir], no_save).err(),
            Some(ScanBusy)
        );
        let fresh = scanner_for(dir.path());
        assert_eq!(
            fresh.scan(&mut db, [&plugins_dir], no_save).err(),
            Some(ScanBusy)
        );
        assert_eq!(fresh.recover_crash(&mut db, no_save), Err(ScanBusy));
        assert_eq!(
            std::fs::read(dir.path().join(".scanning")).unwrap(),
            a.as_os_str().as_encoded_bytes(),
            "the armed sentinel still names the probe in flight"
        );
        assert!(!db.is_blacklisted(&a), "nothing crashed");

        release.send(()).unwrap();
        assert!(scan.wait(&mut db).is_some());
        assert!(
            scanner.spawn(&mut db, [&plugins_dir], no_save).is_ok(),
            "Done releases the pedal"
        );
    }

    /// A scan whose `Scan` is dropped stops after the file in flight even when
    /// no file produces a delta (every failure environmental), and the pedal is
    /// disarmed. Without the stop flag the worker would probe every remaining
    /// file, each with a `plugin-server` child.
    #[test]
    fn dropping_the_scan_stops_the_worker_even_when_nothing_is_recorded() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        for name in ["a.clap", "b.clap", "c.clap", "d.clap"] {
            create_fake_plugin(&plugins_dir, name);
        }
        let (scanner, parked, release, calls) = gated_scanner(dir.path(), environmental);
        let mut db = PluginDb::new();
        let scan = scanner.spawn(&mut db, [&plugins_dir], no_save).unwrap();
        parked.recv_timeout(Duration::from_secs(30)).unwrap();
        let progress = scan.progress_rx.clone();
        let stop = scan.cancel.clone();

        // The drop waits for the parked probe: release it from this thread,
        // once the drop has asked the worker to stop.
        let dropping = std::thread::spawn(move || drop(scan));
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !stop.is_cancelled() {
            assert!(
                std::time::Instant::now() < deadline,
                "the drop never asked the worker to stop"
            );
            std::thread::yield_now();
        }
        release.send(()).unwrap();
        dropping.join().unwrap();
        worker_exited(&progress);

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            !dir.path().join(".scanning").exists(),
            "the pedal is disarmed"
        );
        assert!(db.is_empty());
    }

    /// **A dropped scan has released the pedal when the drop returns**: the
    /// probe in flight outlives the drop request for 200 ms, and the next scan
    /// still starts at once instead of being refused as busy.
    #[test]
    fn a_new_scan_starts_right_after_a_scan_is_dropped() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "a.clap");
        let (probing_tx, probing_rx) = crossbeam_channel::bounded::<()>(1);
        let scanner = Scanner::new(dir.path().join(".scanning"), move |path, _, _| {
            let _ = probing_tx.try_send(());
            std::thread::sleep(Duration::from_millis(200));
            named(path)
        });
        let mut db = PluginDb::new();
        let scan = scanner.spawn(&mut db, [&plugins_dir], no_save).unwrap();
        probing_rx.recv_timeout(Duration::from_secs(30)).unwrap();

        drop(scan);

        assert!(
            scanner.spawn(&mut db, [&plugins_dir], no_save).is_ok(),
            "the dropped scan's worker had not released the pedal"
        );
    }

    /// **A dropped scan reaches the probe in flight**: the probe waits on its
    /// `Cancel` as a real one waits on its child, so the drop returns as soon as
    /// the probe sees the request, not when its own deadline (here 60 s) passes.
    #[test]
    fn dropping_the_scan_raises_the_cancel_the_probe_in_flight_watches() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "a.clap");
        let (probing_tx, probing_rx) = crossbeam_channel::bounded::<()>(1);
        let scanner = Scanner::new(dir.path().join(".scanning"), move |path, _, cancel| {
            let _ = probing_tx.try_send(());
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            while !cancel.is_cancelled() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(2));
            }
            named(path)
        });
        let mut db = PluginDb::new();
        let scan = scanner.spawn(&mut db, [&plugins_dir], no_save).unwrap();
        probing_rx.recv_timeout(Duration::from_secs(30)).unwrap();

        let started = std::time::Instant::now();
        drop(scan);

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the drop waited for the probe's deadline: {:?}",
            started.elapsed()
        );
        assert!(
            scanner.spawn(&mut db, [&plugins_dir], no_save).is_ok(),
            "the pedal is released"
        );
    }

    /// **A probe a cancel ended records nothing**: no delta, no blacklist row,
    /// not even when the probe reports what a killed child looks like (a crash),
    /// and the row that was there is as it was.
    #[test]
    fn a_cancelled_probe_leaves_the_row_as_it_was() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let file = create_fake_plugin(&plugins_dir, "a.clap");
        let (probing_tx, probing_rx) = crossbeam_channel::bounded::<()>(1);
        let scanner = Scanner::new(dir.path().join(".scanning"), move |_, _, cancel| {
            let _ = probing_tx.try_send(());
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(ProbeFailure {
                reason: "crashed during probe".into(),
                kind: ProbeFailureKind::Crashed,
            })
        });
        let mut db = PluginDb::new();
        scanner_for(dir.path())
            .scan(&mut db, [&plugins_dir], no_save)
            .unwrap();
        let before = db.row(&file).cloned().expect("a row");

        let task = scanner
            .spawn_files(&mut db, [(&file, PluginFormat::Clap)], no_save)
            .unwrap();
        probing_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        task.cancel.cancel();
        worker_exited(&task.progress_rx);
        // The worker's whole output is waiting in the task: apply it.
        let end = task.poll(&mut db);

        assert!(
            matches!(&end, ScanPoll::Done(r) if r.failures.is_empty()),
            "{end:?}"
        );
        assert_eq!(db.row(&file), Some(&before));
        assert!(!db.is_blacklisted(&file));
    }

    /// **A file task probes the files it names whether or not their rows are
    /// current**, and delivers one delta each, keyed by canonical path.
    #[test]
    fn a_file_task_reprobes_a_current_row_and_delivers_its_delta() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        let file = create_fake_plugin(&plugins_dir, "a.clap");
        let (scanner, calls) = counting_scanner(dir.path(), named);
        let mut db = PluginDb::new();
        assert_eq!(
            scanner.scan(&mut db, [&plugins_dir], no_save).unwrap().new,
            1
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let task = scanner
            .spawn_files(&mut db, [(&file, PluginFormat::Clap)], no_save)
            .unwrap();
        let result = task.wait(&mut db).expect("the task finishes");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the current row is probed again"
        );
        assert_eq!((result.scanned, result.new), (1, 1));
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
            Ok(vec![probe_plugin_fallback(path, format)])
        });
        let mut db = PluginDb::new();

        let task = scanner
            .spawn_files(&mut db, [(&file, PluginFormat::Clap)], no_save)
            .unwrap();
        task.wait(&mut db).expect("the task finishes");

        assert_eq!(*seen.lock().unwrap(), [PluginFormat::Clap]);
        assert_eq!(db.row(&file).expect("a row").format, PluginFormat::Clap);
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

        assert_eq!(
            scanner
                .spawn_files(&mut db, [(&missing, PluginFormat::Clap)], no_save)
                .unwrap_err(),
            FilesError::Missing(missing)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            scanner
                .spawn_files(&mut db, Vec::<(PathBuf, PluginFormat)>::new(), no_save)
                .is_ok(),
            "a refused call did not claim the pedal"
        );
    }

    /// A worker that dies is reported by `poll` as `Died`, not as running
    /// forever.
    #[test]
    fn poll_reports_a_dead_worker() {
        let dir = TempDir::new().unwrap();
        let plugins_dir = plugins_dir(&dir);
        create_fake_plugin(&plugins_dir, "a.clap");
        let scanner = Scanner::new(dir.path().join(".scanning"), |_, _, _| {
            panic!("worker dies")
        });
        let mut db = PluginDb::new();
        let scan = scanner.spawn(&mut db, [&plugins_dir], no_save).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let end = loop {
            match scan.poll(&mut db) {
                ScanPoll::Running => {
                    assert!(std::time::Instant::now() < deadline, "poll never ended");
                    std::thread::yield_now();
                }
                other => break other,
            }
        };

        assert!(matches!(end, ScanPoll::Died), "{end:?}");
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

        let result = scanner.scan(&mut db, [&plugins_dir], failing_save).unwrap();

        assert_eq!(result.new, 1);
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
        let scan = dying.spawn(&mut db, [&plugins_dir], failing_save).unwrap();
        assert!(scan.wait(&mut db).is_none());

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

            scanner.scan(&mut db, [&alias, &real], no_save).unwrap();
            scanner.scan(&mut db, [&real, &alias], no_save).unwrap();
            scanner.scan(&mut db, [&alias], no_save).unwrap();

            assert_eq!(db.len(), 1, "{:?}", db.rows().collect::<Vec<_>>());
            assert_eq!(db.plugins().count(), 1);
            assert!(db.row(&synth).is_some());
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }

        /// A link named `.clap` to a file named anything is keyed by the target and
        /// keeps the link's format, even in a row that records only a failure.
        #[test]
        fn a_link_to_a_differently_named_file_keeps_its_format() {
            let dir = TempDir::new().unwrap();
            let plugins_dir = plugins_dir(&dir);
            let target = create_fake_plugin(&plugins_dir, "libsynth.bin");
            let links = dir.path().join("links");
            std::fs::create_dir(&links).unwrap();
            std::os::unix::fs::symlink(&target, links.join("synth.clap")).unwrap();
            let (scanner, _) = counting_scanner(dir.path(), crashes);
            let mut db = PluginDb::new();

            scanner.scan(&mut db, [&links], no_save).unwrap();

            let row = db.row(&target).expect("keyed by the target");
            assert_eq!(row.format, PluginFormat::Clap);
            assert!(row.blacklist_reason().is_some());
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
            let scan = scanner.spawn(&mut db, [&plugins_dir], no_save).unwrap();
            parked.recv_timeout(Duration::from_secs(30)).unwrap();

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
            assert!(scan.wait(&mut db).is_some());
        }
    }
}
