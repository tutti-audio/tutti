//! `Catalog::scan` against a directory of real files: a plugin file added
//! between two scans is found by the second, and only it is new (feature
//! `clap`, the reference plugin; `cargo build -p tutti-plugin-probe` first, or
//! the scan degrades to filename metadata, which still lists the file).

#![cfg(feature = "clap")]

use tutti_plugin::catalog::{Catalog, CatalogConfig};
use tutti_plugin_db::{PluginDb, Scan, ScanPoll, ScanResult};

/// Applies `task` as it arrives until it ends, and returns its result.
fn finish(catalog: &mut Catalog, mut task: Scan) -> ScanResult {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        match catalog.try_apply(&mut task) {
            ScanPoll::Running => {
                assert!(std::time::Instant::now() < deadline, "the task never ended");
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            ScanPoll::Done(result) => break result,
            ScanPoll::Died => panic!("the task's worker died"),
        }
    }
}

/// Runs a scan of the catalog's directories to its end.
fn scan(catalog: &mut Catalog) -> ScanResult {
    let task = catalog.scan().expect("no task is running");
    finish(catalog, task)
}

/// **A scan picks up a file that appeared since the last one**, and counts only
/// that file as new: the catalog starts empty over an empty directory, a
/// plugin dropped in is listed by the next `scan()`, and a second file dropped
/// in after that is added beside the first, which is not probed again.
#[test]
fn scan_picks_up_a_new_file() {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let scanned_dir = dir.path().join("plugins");
    std::fs::create_dir(&scanned_dir).expect("the scanned directory");
    let mut catalog = Catalog::new(
        PluginDb::new(),
        CatalogConfig::new(dir.path(), vec![scanned_dir.clone()]),
        |_| Ok(()),
    );
    let files = |catalog: &Catalog| {
        let mut files: Vec<_> = catalog
            .iter()
            .map(|(plugin, _)| plugin.path().to_path_buf())
            .collect();
        files.sort();
        files.dedup();
        files
    };

    let empty = scan(&mut catalog);
    assert_eq!((empty.scanned, empty.new), (0, 0));
    assert!(files(&catalog).is_empty());

    let first = scanned_dir.join("First.clap");
    std::fs::copy(tutti_fixtures::clap_probe(), &first).expect("drop a plugin in");
    let found = scan(&mut catalog);
    assert_eq!((found.scanned, found.new), (1, 1));
    let canonical = |path: &std::path::Path| tutti_plugin_db::canonical_path(path);
    assert_eq!(files(&catalog), [canonical(&first)]);

    let second = scanned_dir.join("Second.clap");
    std::fs::copy(tutti_fixtures::clap_probe(), &second).expect("drop another in");
    let found = scan(&mut catalog);
    assert_eq!(
        (found.scanned, found.new),
        (2, 1),
        "two files are in the directory and only the new one is probed"
    );
    let mut both = [canonical(&first), canonical(&second)];
    both.sort();
    assert_eq!(files(&catalog), both);
}

/// A catalog over a directory holding one copy of the reference plugin, and the
/// copy's path.
fn catalog_with_one_plugin(dir: &tempfile::TempDir) -> (Catalog, std::path::PathBuf) {
    let scanned_dir = dir.path().join("plugins");
    std::fs::create_dir(&scanned_dir).expect("the scanned directory");
    let file = scanned_dir.join("Only.clap");
    std::fs::copy(tutti_fixtures::clap_probe(), &file).expect("drop a plugin in");
    let catalog = Catalog::new(
        PluginDb::new(),
        CatalogConfig::new(dir.path(), vec![scanned_dir]),
        |_| Ok(()),
    );
    (catalog, file)
}

/// **A scan measures only when the catalog was told to**: a
/// plain scan leaves `measured` empty, a catalog `with_measuring` records what
/// the probe found, the configuration it was found at and a verdict.
#[test]
fn a_scan_measures_only_when_asked() {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let (mut plain, _) = catalog_with_one_plugin(&dir);
    scan(&mut plain);
    assert!(
        plain.iter().all(|(_, d)| d.measured.is_none()),
        "a plain scan measures nothing"
    );

    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let (catalog, _) = catalog_with_one_plugin(&dir);
    let mut measuring = catalog.with_measuring();
    scan(&mut measuring);
    let (_, descriptor) = measuring.iter().next().expect("the plugin is listed");
    let measured = descriptor.measured.expect("the scan measured it");
    assert_eq!(measured.config.rate, tutti_plugin_types::SampleRate::SR_48K);
}

/// **`measure` measures a file a scan already probed**, which a scan never
/// revisits while the file is unchanged, and delivers its row as a delta that
/// replaces the old one.
#[test]
fn measure_measures_a_plugin_a_scan_left_unmeasured() {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let (mut catalog, _) = catalog_with_one_plugin(&dir);
    scan(&mut catalog);
    let (plugin, descriptor) = catalog.iter().next().expect("the plugin is listed");
    assert_eq!(descriptor.measured, None);

    let task = catalog.measure(&plugin).expect("a file that exists");
    let result = finish(&mut catalog, task);
    assert_eq!((result.scanned, result.new), (1, 1));
    let (_, descriptor) = catalog.iter().next().expect("still listed");
    assert!(descriptor.measured.is_some());
}

/// **`register` adds a plugin outside the scanned directories** as a task: the
/// row arrives through `try_apply`, not in the call.
#[test]
fn register_delivers_the_row_of_a_file_outside_the_scanned_directories() {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let (mut catalog, _) = catalog_with_one_plugin(&dir);
    let elsewhere = dir.path().join("Elsewhere.clap");
    std::fs::copy(tutti_fixtures::clap_probe(), &elsewhere).expect("a plugin");

    let task = catalog.register(&elsewhere).expect("a file that exists");
    assert_eq!(catalog.iter().count(), 0, "nothing is applied by the call");
    let result = finish(&mut catalog, task);

    assert_eq!((result.scanned, result.new), (1, 1));
    let rows: Vec<_> = catalog
        .db()
        .rows()
        .map(|(path, _)| path.to_path_buf())
        .collect();
    assert_eq!(rows, [tutti_plugin_db::canonical_path(&elsewhere)]);
    assert!(catalog.iter().count() > 0, "the file's plugins are listed");
}

/// A task names a path that cannot be probed at the call.
#[test]
fn register_and_measure_refuse_a_missing_or_unrecognized_file_at_the_call() {
    use tutti_plugin_db::FilesError;
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let (mut catalog, _) = catalog_with_one_plugin(&dir);
    let gone = dir.path().join("Gone.clap");
    let text = dir.path().join("notes.txt");
    std::fs::write(&text, b"x").expect("a file");

    assert_eq!(
        catalog.register(&gone).unwrap_err(),
        FilesError::Missing(gone)
    );
    assert_eq!(
        catalog.register(&text).unwrap_err(),
        FilesError::UnrecognizedFormat(text)
    );
}

#[cfg(unix)]
mod stand_in_probe {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use tutti_plugin::catalog::Locator;
    use tutti_plugin_db::ProbeFailureKind;

    /// A catalog over one plugin file whose probe is the script `body`, which
    /// gets the pid file's path as `$PID_FILE` replaced in the text. Returns the
    /// catalog, the file and the file the script writes its pid to.
    fn catalog_probed_by(
        dir: &tempfile::TempDir,
        body: &str,
        probe_timeout: Option<Duration>,
    ) -> (Catalog, PathBuf, PathBuf) {
        let pid_file = dir.path().join("pid");
        let script = dir.path().join("plugin-probe");
        let text = body.replace("$PID_FILE", &pid_file.display().to_string());
        std::fs::write(&script, format!("#!/bin/sh\n{text}\n")).expect("the stand-in");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("executable");
        let scanned_dir = dir.path().join("plugins");
        std::fs::create_dir(&scanned_dir).expect("the scanned directory");
        let file = scanned_dir.join("Only.clap");
        std::fs::copy(tutti_fixtures::clap_probe(), &file).expect("drop a plugin in");
        let mut config =
            CatalogConfig::new(dir.path(), vec![scanned_dir]).probe_locator(Locator::at(script));
        if let Some(timeout) = probe_timeout {
            config = config.probe_timeout(timeout).measure_timeout(timeout);
        }
        (
            Catalog::new(PluginDb::new(), config, |_| Ok(())),
            file,
            pid_file,
        )
    }

    const HANG: &str = "echo $$ > $PID_FILE\nexec sleep 600";

    /// The pid the stand-in wrote, once it is running.
    fn running_pid(pid_file: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(pid) = std::fs::read_to_string(pid_file)
                && pid.ends_with('\n')
            {
                return pid.trim().to_owned();
            }
            assert!(Instant::now() < deadline, "the probe never started");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .expect("kill runs")
            .success()
    }

    /// **Dropping a scan kills the probe in flight and returns at once**: the
    /// probe hangs for ten minutes (a scan's deadline is fifteen seconds), and
    /// the drop returns within a few seconds with the child gone and the pedal
    /// released.
    #[test]
    fn dropping_a_scan_kills_a_stuck_probe_promptly() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let (mut catalog, _, pid_file) = catalog_probed_by(&dir, HANG, None);

        let task = catalog.scan().expect("no task is running");
        let pid = running_pid(&pid_file);
        assert!(alive(&pid));

        let started = Instant::now();
        drop(task);

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the drop waited for the probe: {:?}",
            started.elapsed()
        );
        assert!(!alive(&pid), "the probe (pid {pid}) outlived the drop");
        assert!(catalog.scan().is_ok(), "the drop released the pedal");
    }

    /// **Dropping a measure task kills its probe as well**: a measurement's
    /// deadline is three minutes.
    #[test]
    fn dropping_a_measure_kills_a_stuck_probe_promptly() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let (mut catalog, file, pid_file) = catalog_probed_by(&dir, HANG, None);
        let plugin = tutti_plugin::catalog::PluginRef::file(file).expect("a clap file");

        let task = catalog.measure(&plugin).expect("a file that exists");
        let pid = running_pid(&pid_file);

        let started = Instant::now();
        drop(task);

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(!alive(&pid), "the probe (pid {pid}) outlived the drop");
    }

    /// **A registration a drop cancelled leaves no trace**: the file's row is
    /// absent, not blacklisted as the killed probe's crash.
    #[test]
    fn a_dropped_registration_records_nothing() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let (mut catalog, file, pid_file) = catalog_probed_by(&dir, HANG, None);

        let task = catalog.register(&file).expect("a plugin file");
        running_pid(&pid_file);
        drop(task);

        assert!(catalog.db().is_empty());
        assert!(!catalog.is_blacklisted(&file));
    }

    /// **A registration of a plugin that crashes its probe blacklists it**, as a
    /// scan does, and the result names the crash.
    #[test]
    fn registering_a_plugin_that_crashes_its_probe_blacklists_it() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let (mut catalog, file, _) = catalog_probed_by(&dir, "exit 3", None);

        let task = catalog.register(&file).expect("a plugin file");
        let result = finish(&mut catalog, task);

        assert_eq!(result.newly_blacklisted, 1);
        let [failed] = result.failures.as_slice() else {
            panic!("one failure: {:?}", result.failures);
        };
        assert_eq!(failed.path, tutti_plugin_db::canonical_path(&file));
        assert_eq!(failed.failure.kind, ProbeFailureKind::Crashed);
        assert!(catalog.is_blacklisted(&file));
    }

    /// **A registration of a plugin that hangs its probe blacklists it** once
    /// the deadline passes.
    #[test]
    fn registering_a_plugin_that_hangs_its_probe_blacklists_it() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let (mut catalog, file, _) =
            catalog_probed_by(&dir, HANG, Some(Duration::from_millis(500)));

        let task = catalog.register(&file).expect("a plugin file");
        let result = finish(&mut catalog, task);

        let [failed] = result.failures.as_slice() else {
            panic!("one failure: {:?}", result.failures);
        };
        assert_eq!(failed.failure.kind, ProbeFailureKind::TimedOut);
        assert!(catalog.is_blacklisted(&file));
    }
}

/// **A registration with no `plugin-probe` to run says so**: the failure's kind
/// is the probe's absence, not a crash or a timeout, the file is not recorded
/// under its name, and nothing is blacklisted.
#[test]
fn register_names_a_missing_probe_binary() {
    use tutti_plugin::catalog::Locator;
    use tutti_plugin_db::ProbeFailureKind;
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let (catalog, file) = catalog_with_one_plugin(&dir);
    let config = CatalogConfig::new(dir.path(), tutti_plugin::catalog::NO_SCAN_DIRS)
        .probe_locator(Locator::at(dir.path().join("no-such-plugin-probe")));
    drop(catalog);
    let mut catalog = Catalog::new(PluginDb::new(), config, |_| Ok(()));

    let task = catalog.register(&file).expect("a plugin file");
    let result = finish(&mut catalog, task);

    let [failed] = result.failures.as_slice() else {
        panic!("one failure: {:?}", result.failures);
    };
    assert_eq!(failed.failure.kind, ProbeFailureKind::ProbeNotFound);
    assert_eq!(
        (result.new, result.failed, result.newly_blacklisted),
        (0, 1, 0)
    );
    assert!(catalog.db().is_empty());
}
