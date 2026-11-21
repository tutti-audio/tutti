//! The plugin scan on the session: `scan_plugins` is a future the app awaits, it
//! reports what it probed in order, and the session's drop ends a scan that is
//! waiting on a probe with the named error, never a hang.
//!
//! Build the probe first: `cargo build -p tutti-plugin-probe`.

use std::future::IntoFuture as _;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use tutti_buffers::MaxBlock;
use tutti_plugin_db::{Catalog, CatalogConfig, PluginDb, ScanError, ScanProgress};
use tutti_plugin_types::Locator;
use tutti_session::Session;
use tutti_units::Samples;

use crate::support::clap_probe;

fn session() -> Session {
    Session::builder()
        .max_block(MaxBlock::of(Samples::new(512)))
        .build()
        .expect("an offline session builds")
}

/// A catalog scanning a directory that holds one plugin file, probed by `locator`.
fn catalog_over_one_plugin(dir: &tempfile::TempDir, locator: Locator) -> Catalog {
    let plugins = dir.path().join("plugins");
    std::fs::create_dir(&plugins).expect("the scanned directory");
    std::fs::copy(clap_probe(), plugins.join("Only.clap")).expect("drop a plugin in");
    Catalog::new(
        PluginDb::new(),
        CatalogConfig::new(dir.path())
            .scan_dirs(vec![plugins])
            .probe_locator(locator),
        |_| Ok(()),
    )
}

/// **The session's scan finds the plugin and reports it in order**: the directory
/// first, then the one file, as the first of one; the findings apply to the catalog
/// that was scanned.
#[test]
fn a_session_scan_is_awaited_and_reports_its_progress_in_order() {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let mut catalog = catalog_over_one_plugin(&dir, Locator::at(tutti_fixtures::plugin_probe()));
    let session = session();

    let scan = session.scan_plugins(&mut catalog).expect("no scan is held");
    let progress = scan.progress();
    let scanned = tutti_fixtures::block_on(scan).expect("the session lives");
    let report = catalog.apply(scanned).expect("this catalog's own scan");

    assert!(report.failures().is_empty(), "{:?}", report.failures());
    assert_eq!((report.scanned(), report.added()), (1, 1));
    let updates: Vec<_> = progress.read().collect();
    assert!(
        matches!(
            updates.as_slice(),
            [
                ScanProgress::Discovering { .. },
                ScanProgress::Probing {
                    current: 1,
                    total: 1,
                    ..
                }
            ]
        ),
        "{updates:?}"
    );
}

/// **Dropping the session while a scan waits on a probe resolves the named error**:
/// the probe never answers, the session's executor shuts down under the scan, and the
/// scan ends `ScanError::ShutDown`, with the child killed and the catalog's pedal
/// released. A hang guard fails a scan that never resolves.
#[test]
fn dropping_the_session_ends_a_pending_scan_with_the_named_error() {
    let dir = tempfile::TempDir::new().expect("a scratch directory");
    let fifo = dir.path().join("pid");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo runs")
            .success(),
        "mkfifo"
    );
    let script: PathBuf = dir.path().join("plugin-probe");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho $$ > {}\nexec sleep 600\n", fifo.display()),
    )
    .expect("the stand-in");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("executable");
    let mut catalog = catalog_over_one_plugin(&dir, Locator::at(script));
    let session = session();
    let scan = session.scan_plugins(&mut catalog).expect("no scan is held");
    let (done, returned) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(tutti_fixtures::block_on(scan.into_future()));
    });
    // The read returns once the stand-in is running, so the scan is waiting on it.
    let pid = std::fs::read_to_string(&fifo).expect("the probe wrote its pid");

    drop(session);

    let ended = returned
        .recv_timeout(Duration::from_secs(60))
        .expect("the scan never resolved after the session dropped");
    assert_eq!(ended.unwrap_err(), ScanError::ShutDown);
    let alive = std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("kill runs")
        .success();
    assert!(!alive, "the probe (pid {}) outlived the scan", pid.trim());
    assert!(
        pedal_is_free(&mut catalog),
        "the shut-down scan released the pedal"
    );
}

/// Whether a scan can start on `catalog`: its pedal is free.
fn pedal_is_free(catalog: &mut Catalog) -> bool {
    catalog
        .scan(tutti_platform::Executor::new().downgrade())
        .is_ok()
}
