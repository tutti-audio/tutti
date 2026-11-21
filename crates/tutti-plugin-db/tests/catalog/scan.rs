//! `Catalog::scan` against a directory of real files: a plugin file added
//! between two scans is found by the second, and only it is new (feature
//! `clap`, the reference plugin; `cargo build -p tutti-plugin-probe` first, or
//! every file fails as `ProbeNotFound` and the scan records nothing).

use tutti_plugin_db::{Catalog, CatalogConfig, PluginDb, ScanPlugins, ScanReport};
use tutti_plugin_types::Locator;
use tutti_plugin_types::PluginRef;

/// The executor every scan of this binary runs on.
fn exec() -> tutti_platform::WeakExecutor {
    tutti_fixtures::executor().downgrade()
}

/// Awaits `task` on this thread and applies it, returning its report. The probe's
/// own deadline bounds the wait.
fn finish(catalog: &mut Catalog, task: ScanPlugins) -> ScanReport {
    use std::future::IntoFuture;
    let scanned = futures_lite::future::block_on(task.into_future()).expect("the executor lives");
    catalog
        .apply(scanned)
        .expect("the output of this catalog's own scan")
}

/// Runs a scan of the catalog's directories to its end.
fn scan(catalog: &mut Catalog) -> ScanReport {
    let task = catalog.scan(exec()).expect("no scan is held");
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
        CatalogConfig::new(dir.path())
            .scan_dirs(vec![scanned_dir.clone()])
            .probe_locator(Locator::at(tutti_fixtures::plugin_probe())),
        |_| Ok(()),
    );
    // The files the catalog holds a row for: one row per file, whatever the
    // number of plugins in it.
    let files = |catalog: &Catalog| {
        let mut files: Vec<_> = catalog
            .db()
            .rows()
            .map(|(path, _)| path.to_path_buf())
            .collect();
        files.sort();
        files
    };

    let empty = scan(&mut catalog);
    assert_eq!((empty.scanned(), empty.added()), (0, 0));
    assert!(files(&catalog).is_empty());

    let first = scanned_dir.join("First.clap");
    std::fs::copy(tutti_fixtures::clap_probe(), &first).expect("drop a plugin in");
    let found = scan(&mut catalog);
    assert_eq!((found.scanned(), found.added()), (1, 1));
    let canonical = |path: &std::path::Path| tutti_plugin_db::canonical_path(path);
    assert_eq!(files(&catalog), [canonical(&first)]);

    let second = scanned_dir.join("Second.clap");
    std::fs::copy(tutti_fixtures::clap_probe(), &second).expect("drop another in");
    let found = scan(&mut catalog);
    assert_eq!(
        (found.scanned(), found.added()),
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
    catalog_over_one_plugin(dir, CatalogConfig::new(dir.path()))
}

/// [`catalog_with_one_plugin`] under `config`, whose scan directories and probe
/// locator it sets.
fn catalog_over_one_plugin(
    dir: &tempfile::TempDir,
    config: CatalogConfig,
) -> (Catalog, std::path::PathBuf) {
    let scanned_dir = dir.path().join("plugins");
    std::fs::create_dir(&scanned_dir).expect("the scanned directory");
    let file = scanned_dir.join("Only.clap");
    std::fs::copy(tutti_fixtures::clap_probe(), &file).expect("drop a plugin in");
    let catalog = Catalog::new(
        PluginDb::new(),
        config
            .scan_dirs(vec![scanned_dir])
            .probe_locator(Locator::at(tutti_fixtures::plugin_probe())),
        |_| Ok(()),
    );
    (catalog, file)
}

/// **A scan measures only when the config says to**: a
/// plain scan leaves `measured` empty, a config that is `measuring()` records what
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
    let (mut measuring, _) =
        catalog_over_one_plugin(&dir, CatalogConfig::new(dir.path()).measuring());
    scan(&mut measuring);
    let (_, descriptor) = measuring.iter().next().expect("the plugin is listed");
    let measured = descriptor.measured.expect("the scan measured it");
    assert_eq!(measured.config.rate, tutti_plugin_types::SampleRate::SR_48K);
}

/// **`measure` measures a file a scan already probed**, which a scan never
/// revisits while the file is unchanged, and delivers its row as a change that
/// replaces the old one.
#[test]
fn measure_measures_a_plugin_a_scan_left_unmeasured() {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let (mut catalog, _) = catalog_with_one_plugin(&dir);
    scan(&mut catalog);
    let (plugin, descriptor) = catalog.iter().next().expect("the plugin is listed");
    assert_eq!(descriptor.measured, None);

    let task = catalog
        .measure(&plugin, None, exec())
        .expect("a file that exists");
    let result = finish(&mut catalog, task);
    assert_eq!((result.scanned(), result.added()), (1, 1));
    let (_, descriptor) = catalog.iter().next().expect("still listed");
    assert!(descriptor.measured.is_some());
}

/// **`register` adds a plugin outside the scanned directories** as a scan: the
/// row arrives through `apply` of the run's output, not in the call.
#[test]
fn register_delivers_the_row_of_a_file_outside_the_scanned_directories() {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let (mut catalog, _) = catalog_with_one_plugin(&dir);
    let elsewhere = dir.path().join("Elsewhere.clap");
    std::fs::copy(tutti_fixtures::clap_probe(), &elsewhere).expect("a plugin");

    let task = catalog
        .register(&elsewhere, exec())
        .expect("a file that exists");
    assert_eq!(catalog.iter().count(), 0, "nothing is applied by the call");
    let result = finish(&mut catalog, task);

    assert_eq!((result.scanned(), result.added()), (1, 1));
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

    assert!(matches!(
        catalog.register(&gone, exec()).unwrap_err(),
        FilesError::Missing(path) if path == gone
    ));
    assert!(matches!(
        catalog.register(&text, exec()).unwrap_err(),
        FilesError::UnrecognizedFormat(path) if path == text
    ));
}

#[cfg(unix)]
mod stand_in_probe {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tutti_plugin_db::ScanBusy;
    use tutti_plugin_types::{ProbeDeath, ProbeError};

    /// A catalog over one plugin file whose probe is the script `body`, which
    /// gets the pid file's path as `$PID_FILE` replaced in the text. Returns the
    /// catalog, the file and the file the script writes its pid to.
    fn catalog_probed_by(
        dir: &tempfile::TempDir,
        body: &str,
        probe_timeout: Option<Duration>,
    ) -> (Catalog, PathBuf, PathBuf) {
        let pid_file = dir.path().join("pid");
        // A fifo: the script's write of its pid blocks until the test reads it,
        // so a read is the event of the probe running.
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&pid_file)
                .status()
                .expect("mkfifo runs")
                .success(),
            "mkfifo"
        );
        let script = dir.path().join("plugin-probe");
        let text = body.replace("$PID_FILE", &pid_file.display().to_string());
        std::fs::write(&script, format!("#!/bin/sh\n{text}\n")).expect("the stand-in");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("executable");
        let scanned_dir = dir.path().join("plugins");
        std::fs::create_dir(&scanned_dir).expect("the scanned directory");
        let file = scanned_dir.join("Only.clap");
        std::fs::copy(tutti_fixtures::clap_probe(), &file).expect("drop a plugin in");
        let mut config = CatalogConfig::new(dir.path())
            .scan_dirs(vec![scanned_dir])
            .probe_locator(Locator::at(script));
        if let Some(timeout) = probe_timeout {
            config = config.probe_timeout(timeout).measure_timeout(timeout);
        }
        (
            Catalog::new(PluginDb::new(), config, |_| Ok(())),
            file,
            pid_file,
        )
    }

    // A stand-in whose pid is never read blocks on opening the fifo.
    const HANG: &str = "echo $$ > $PID_FILE\nexec sleep 600";

    /// The pid the stand-in wrote, once it is running. The read blocks until
    /// the script opens the fifo; a hang guard fails a probe that never ran.
    fn running_pid(pid_file: &Path) -> String {
        let (sent, received) = std::sync::mpsc::channel();
        let pid_file = pid_file.to_owned();
        std::thread::spawn(move || {
            let _ = sent.send(std::fs::read_to_string(pid_file));
        });
        received
            .recv_timeout(Duration::from_secs(30))
            .expect("the probe never started")
            .expect("the probe wrote its pid")
            .trim()
            .to_owned()
    }

    /// Polls `task` until its probe is running, then drops it, and returns the
    /// probe's pid. The scan loses a race to the pid arriving; its future is dropped
    /// when the race ends.
    fn dropped_in_flight(task: ScanPlugins, pid_file: &Path) -> String {
        let (sent, received) = futures_channel::oneshot::channel();
        let pid_file = pid_file.to_owned();
        std::thread::spawn(move || {
            let _ = sent.send(std::fs::read_to_string(pid_file));
        });
        let pid = futures_lite::future::block_on(futures_lite::future::or(
            async {
                let ended = task.into_future().await;
                panic!("the scan ended before it was dropped: {ended:?}");
            },
            async {
                received
                    .await
                    .expect("the reader sends")
                    .expect("the probe wrote its pid")
                    .trim()
                    .to_owned()
            },
        ));
        assert!(!pid.is_empty());
        pid
    }

    fn alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .expect("kill runs")
            .success()
    }

    /// **Dropping a scan kills the probe in flight**: the probe hangs for ten
    /// minutes, and once the scan's future is dropped the child is gone, nothing is
    /// blacklisted, the sentinel is removed and the pedal released.
    #[test]
    fn dropping_a_scan_kills_a_stuck_probe() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let (mut catalog, file, pid_file) = catalog_probed_by(&dir, HANG, None);

        let task = catalog.scan(exec()).expect("no scan is held");
        let pid = dropped_in_flight(task, &pid_file);

        assert!(!alive(&pid), "the probe (pid {pid}) outlived the scan");
        assert!(
            !catalog.is_blacklisted(&file),
            "a dropped scan blacklists nothing"
        );
        assert!(
            !CatalogConfig::new(dir.path()).pedal_path().exists(),
            "the sentinel outlived the scan"
        );
        assert!(catalog.scan(exec()).is_ok(), "the drop released the pedal");
    }

    /// **Dropping a measurement kills its probe as well**: a measurement's
    /// deadline is three minutes.
    #[test]
    fn dropping_a_measure_kills_a_stuck_probe() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let (mut catalog, file, pid_file) = catalog_probed_by(&dir, HANG, None);
        let plugin = PluginRef::file(file);

        let task = catalog
            .measure(&plugin, None, exec())
            .expect("a file that exists");
        let pid = dropped_in_flight(task, &pid_file);

        assert!(!alive(&pid), "the probe (pid {pid}) outlived the measure");
    }

    /// **A scan whose executor shuts down mid-probe resolves the named error**, and
    /// the probe's child is killed: no hang, and no partial list passed off as a
    /// scan. The scan holds the executor weakly, so dropping its last clone is what
    /// ends it.
    #[test]
    fn a_scan_whose_executor_shuts_down_mid_probe_resolves_shut_down() {
        use std::future::IntoFuture;
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let (mut catalog, file, pid_file) = catalog_probed_by(&dir, HANG, None);
        let executor = tutti_platform::Executor::new();
        let task = catalog.scan(executor.downgrade()).expect("no scan is held");
        let (done, returned) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(futures_lite::future::block_on(task.into_future()));
        });
        let pid = running_pid(&pid_file);
        assert!(alive(&pid));

        drop(executor);

        let ended = returned
            .recv_timeout(Duration::from_secs(60))
            .expect("the scan never resolved after the executor shut down");
        assert_eq!(ended.unwrap_err(), tutti_plugin_db::ScanError::ShutDown);
        assert!(!alive(&pid), "the probe (pid {pid}) outlived the executor");
        assert!(
            !catalog.is_blacklisted(&file),
            "a shutdown blacklists nothing"
        );
        assert!(catalog.scan(exec()).is_ok(), "the claim was released");
    }

    /// **An unawaited scan holds the pedal until it drops**, and probes nothing: a
    /// second scan is `ScanBusy` while it is held and starts after the drop.
    #[test]
    fn an_unawaited_scan_holds_the_pedal_until_it_drops() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let (mut catalog, _, _) = catalog_probed_by(&dir, HANG, None);

        let held = catalog.scan(exec()).expect("no scan is held");
        assert!(matches!(catalog.scan(exec()), Err(ScanBusy)));
        drop(held);

        assert!(catalog.scan(exec()).is_ok(), "the drop released the pedal");
    }

    /// **A registration of a plugin that crashes its probe blacklists it**, as a
    /// scan does, and the result names the crash.
    #[test]
    fn registering_a_plugin_that_crashes_its_probe_blacklists_it() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let (mut catalog, file, _) = catalog_probed_by(&dir, "kill -SEGV $$", None);

        let task = catalog.register(&file, exec()).expect("a plugin file");
        let result = finish(&mut catalog, task);

        assert_eq!(result.newly_blacklisted(), 1);
        let failures = result.failures();
        let [failed] = failures.as_slice() else {
            panic!("one failure: {failures:?}");
        };
        assert_eq!(failed.path, tutti_plugin_db::canonical_path(&file));
        assert_eq!(
            probe_failure(failed).error,
            ProbeError::Died(ProbeDeath::Signal(11))
        );
        assert!(catalog.is_blacklisted(&file));
    }

    /// **A registration of a plugin that hangs its probe blacklists it** once
    /// the deadline passes. The deadline is long enough that the probe is
    /// running (its pid is read) before it fires.
    #[test]
    fn registering_a_plugin_that_hangs_its_probe_blacklists_it() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let (mut catalog, file, pid_file) =
            catalog_probed_by(&dir, HANG, Some(Duration::from_secs(2)));

        let task = catalog.register(&file, exec()).expect("a plugin file");
        let runner = std::thread::spawn(move || {
            use std::future::IntoFuture;
            futures_lite::future::block_on(task.into_future())
        });
        running_pid(&pid_file);
        let scanned = runner
            .join()
            .expect("the scan does not panic")
            .expect("the executor lives");
        let result = catalog.apply(scanned).expect("this catalog's scan");

        let failures = result.failures();

        let [failed] = failures.as_slice() else {
            panic!("one failure: {failures:?}");
        };
        assert!(
            matches!(probe_failure(failed).error, ProbeError::TimedOut { .. }),
            "{:?}",
            probe_failure(failed)
        );
        assert!(catalog.is_blacklisted(&file));
    }
}

/// **A scan with no `plugin-probe` to run fails each file, as `register` does**:
/// the failure's kind is the probe's absence, no row is recorded (not one named
/// after the file), and nothing is blacklisted, so a later scan with the probe
/// found lists the plugin.
#[test]
fn scan_fails_each_file_when_the_probe_binary_is_missing() {
    use tutti_plugin_types::ProbeError;
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let (catalog, _) = catalog_with_one_plugin(&dir);
    drop(catalog);
    let config = CatalogConfig::new(dir.path())
        .scan_dirs(vec![dir.path().join("plugins")])
        .probe_locator(Locator::at(dir.path().join("no-such-plugin-probe")));
    let mut catalog = Catalog::new(PluginDb::new(), config, |_| Ok(()));

    let result = scan(&mut catalog);

    let failures = result.failures();
    let [failed] = failures.as_slice() else {
        panic!("one failure: {failures:?}");
    };
    assert_eq!(probe_failure(failed).error, ProbeError::ProbeNotFound);
    assert_eq!(
        (result.added(), result.failed(), result.newly_blacklisted()),
        (0, 1, 0)
    );
    assert!(catalog.db().is_empty());
}

/// **A registration with no `plugin-probe` to run says so**: the failure's kind
/// is the probe's absence, not a crash or a timeout, the file is not recorded
/// under its name, and nothing is blacklisted.
#[test]
fn register_names_a_missing_probe_binary() {
    use tutti_plugin_types::ProbeError;
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let (catalog, file) = catalog_with_one_plugin(&dir);
    let config = CatalogConfig::new(dir.path())
        .probe_locator(Locator::at(dir.path().join("no-such-plugin-probe")));
    drop(catalog);
    let mut catalog = Catalog::new(PluginDb::new(), config, |_| Ok(()));

    let task = catalog.register(&file, exec()).expect("a plugin file");
    let result = finish(&mut catalog, task);

    let failures = result.failures();

    let [failed] = failures.as_slice() else {
        panic!("one failure: {failures:?}");
    };
    assert_eq!(probe_failure(failed).error, ProbeError::ProbeNotFound);
    assert_eq!(
        (result.added(), result.failed(), result.newly_blacklisted()),
        (0, 1, 0)
    );
    assert!(catalog.db().is_empty());
}

/// **A state belongs to the one `measure` call that gave it**: a plugin measured
/// with a state it refuses fails, and the next `measure` of another plugin, given
/// none, measures it as it loads. A state that outlived its call would hand the
/// second plugin the first's bytes.
#[test]
fn a_measure_state_does_not_reach_the_next_measure() {
    use tutti_plugin_types::{ProbeError, ProbeRefusal};
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let (mut catalog, a) = catalog_with_one_plugin(&dir);
    let b = dir.path().join("plugins").join("Other.clap");
    std::fs::copy(tutti_fixtures::clap_probe(), &b).expect("a second plugin");
    let refused = dir.path().join("state");
    std::fs::write(&refused, b"not a state").expect("a state the plugin refuses");

    // The file holds two plugins, so the state is for the one the ref names.
    let registered = catalog.register(&a, exec()).expect("a plugin file");
    finish(&mut catalog, registered);
    let named = catalog
        .db()
        .plugins()
        .next()
        .expect("a registered plugin")
        .plugin_ref();
    let first = catalog
        .measure(&named, Some(&refused), exec())
        .expect("a file that exists");
    let first = finish(&mut catalog, first);
    let failures = first.failures();
    let [failed] = failures.as_slice() else {
        panic!("one failure: {failures:?}");
    };
    assert!(
        matches!(
            probe_failure(failed).error,
            ProbeError::Refused(ProbeRefusal::StateRefused { .. })
        ),
        "{:?}",
        probe_failure(failed)
    );
    assert!(probe_failure(failed).reason().contains("refused the state"));
    assert!(
        !catalog.is_blacklisted(&a),
        "a state the plugin refused says nothing about the plugin"
    );

    let second = catalog
        .measure(&PluginRef::file(&b), None, exec())
        .expect("a file that exists");
    let second = finish(&mut catalog, second);

    assert!(second.failures().is_empty(), "{:?}", second.failures());
    assert!(!catalog.is_blacklisted(&b));
    let row = catalog.db().row(&tutti_plugin_db::canonical_path(&b));
    assert!(row.is_some(), "the second plugin was measured and recorded");
}

/// **`measure` of a plugin with no `plugin-probe` to run says so**, as `register`
/// does, and records nothing.
#[test]
fn measure_names_a_missing_probe_binary() {
    use tutti_plugin_types::ProbeError;
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let (_, file) = catalog_with_one_plugin(&dir);
    let config = CatalogConfig::new(dir.path())
        .probe_locator(Locator::at(dir.path().join("no-such-plugin-probe")));
    let mut catalog = Catalog::new(PluginDb::new(), config, |_| Ok(()));

    let plugin = PluginRef::file(&file);
    let task = catalog
        .measure(&plugin, None, exec())
        .expect("a plugin file");
    let result = finish(&mut catalog, task);

    let failures = result.failures();

    let [failed] = failures.as_slice() else {
        panic!("one failure: {failures:?}");
    };
    assert_eq!(probe_failure(failed).error, ProbeError::ProbeNotFound);
    assert!(catalog.db().is_empty());
}

#[cfg(unix)]
mod links {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// A probe that logs the arguments it was started with, then is the real one.
    fn logging_probe(dir: &Path) -> (Locator, PathBuf) {
        let log = dir.join("probe-args");
        let script = dir.join("logging-probe");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$@\" >> {}\nexec {} \"$@\"\n",
                log.display(),
                tutti_fixtures::plugin_probe().display()
            ),
        )
        .expect("the wrapper");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("executable");
        (Locator::at(script), log)
    }

    fn catalog_with(dir: &Path, locator: Locator) -> Catalog {
        let config = CatalogConfig::new(dir).probe_locator(locator);
        Catalog::new(PluginDb::new(), config, |_| Ok(()))
    }

    /// **A link is probed as named and recorded under its target**: the probe is
    /// started on the link, and the row is keyed by the canonical path, as a scan
    /// keys the same file.
    #[test]
    fn a_registered_link_is_probed_as_named_and_keyed_by_its_target() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let real = dir.path().join("Real.clap");
        std::fs::copy(tutti_fixtures::clap_probe(), &real).expect("a plugin");
        let link = dir.path().join("Link.clap");
        std::os::unix::fs::symlink(&real, &link).expect("a link");
        let (locator, log) = logging_probe(dir.path());
        let mut catalog = catalog_with(dir.path(), locator);

        let task = catalog.register(&link, exec()).expect("a plugin file");
        let result = finish(&mut catalog, task);

        assert!(result.failures().is_empty(), "{:?}", result.failures());
        let rows: Vec<_> = catalog.db().rows().map(|(p, _)| p.to_path_buf()).collect();
        assert_eq!(rows, [tutti_plugin_db::canonical_path(&real)]);
        let args = std::fs::read_to_string(log).expect("the probe ran");
        assert!(
            args.starts_with(&link.display().to_string()),
            "the probe was started on {args:?}, not on the link {link:?}"
        );
    }

    /// **A link whose target has another extension is still the format it was
    /// named as**, through `measure`: registered under the target's canonical
    /// path, the row says CLAP, and a `measure` of the plugin that row lists
    /// probes the target as CLAP, not as the format its `.so` extension names.
    #[test]
    fn a_link_to_a_differently_named_target_is_measured_by_its_key() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let target = dir.path().join("libfoo.so");
        std::fs::copy(tutti_fixtures::clap_probe(), &target).expect("a plugin");
        let link = dir.path().join("Foo.clap");
        std::os::unix::fs::symlink(&target, &link).expect("a link");
        let mut catalog = catalog_with(dir.path(), Locator::at(tutti_fixtures::plugin_probe()));

        let task = catalog.register(&link, exec()).expect("a plugin file");
        let result = finish(&mut catalog, task);
        assert!(result.failures().is_empty(), "{:?}", result.failures());
        let canonical = tutti_plugin_db::canonical_path(&target);
        let (plugin, _) = catalog.iter().next().expect("the plugin is listed");
        assert_eq!(plugin.path(), canonical);

        let task = catalog
            .measure(&plugin, None, exec())
            .expect("the key is a file");
        let result = finish(&mut catalog, task);

        assert!(result.failures().is_empty(), "{:?}", result.failures());
        let (_, descriptor) = catalog.iter().next().expect("still listed");
        assert!(descriptor.measured.is_some());
        assert!(!catalog.is_blacklisted(&canonical));
    }

    /// **A scan of a directory holding a link `Foo.clap` to `libfoo.so`** finds
    /// one row, keyed by the target's canonical path, typed CLAP, and the probe
    /// is started `--as clap` on it.
    #[test]
    fn a_scanned_link_to_a_differently_named_target_is_one_clap_row() {
        let dir = tempfile::TempDir::new().expect("a scratch directory");
        let target = dir.path().join("libfoo.so");
        std::fs::copy(tutti_fixtures::clap_probe(), &target).expect("a plugin");
        let scanned = dir.path().join("plugins");
        std::fs::create_dir(&scanned).expect("the scanned directory");
        std::os::unix::fs::symlink(&target, scanned.join("Foo.clap")).expect("a link");
        let (locator, log) = logging_probe(dir.path());
        let config = CatalogConfig::new(dir.path())
            .scan_dirs(vec![scanned])
            .probe_locator(locator);
        let mut catalog = Catalog::new(PluginDb::new(), config, |_| Ok(()));

        let result = scan(&mut catalog);

        assert_eq!(
            (result.scanned(), result.added()),
            (1, 1),
            "{:?}",
            result.failures()
        );
        let canonical = tutti_plugin_db::canonical_path(&target);
        let rows: Vec<_> = catalog.db().rows().map(|(p, _)| p.to_path_buf()).collect();
        assert_eq!(rows, std::slice::from_ref(&canonical));
        let row = catalog.db().row(&canonical).expect("the row");
        assert_eq!(row.format(), Some(tutti_plugin_types::PluginFormat::Clap));
        let args = std::fs::read_to_string(log).expect("the probe ran");
        assert!(args.contains("--as clap"), "{args:?}");
    }
}

/// The probe failure a listed file failed with; a file that went unprobed fails
/// the test.
fn probe_failure(failed: &tutti_plugin_db::FileFailure) -> &tutti_plugin_db::ProbeFailure {
    match &failed.cause {
        tutti_plugin_db::FileFailureCause::Probe(failure) => failure,
        other => panic!("{:?} was not probed: {other:?}", failed.path),
    }
}
