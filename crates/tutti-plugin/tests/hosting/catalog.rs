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
    let mut files: Vec<_> = catalog
        .iter()
        .map(|(plugin, _)| plugin.path().to_path_buf())
        .collect();
    files.dedup();
    assert_eq!(files, [tutti_plugin_db::canonical_path(&elsewhere)]);
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
mod stuck_probe {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// A `plugin-probe` that records its pid and then hangs, as a plugin stuck in
    /// its load does. Returns the file the pid lands in. The environment points
    /// the catalog's probe search at it until `env` drops.
    fn stuck_probe(dir: &Path, env: &mut tutti_fixtures::ScopedEnv) -> PathBuf {
        let pid_file = dir.join("pid");
        let script = dir.join("plugin-probe");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > {}\nexec sleep 600\n",
                pid_file.display()
            ),
        )
        .expect("the stand-in");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("executable");
        env.set("TUTTI_PLUGIN_PROBE", &script);
        pid_file
    }

    /// The pid the stuck probe wrote, once it is running.
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
        let mut env = tutti_fixtures::ScopedEnv::new();
        let pid_file = stuck_probe(dir.path(), &mut env);
        let (mut catalog, _) = catalog_with_one_plugin(&dir);

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
        let mut env = tutti_fixtures::ScopedEnv::new();
        let pid_file = stuck_probe(dir.path(), &mut env);
        let (mut catalog, file) = catalog_with_one_plugin(&dir);
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
}
