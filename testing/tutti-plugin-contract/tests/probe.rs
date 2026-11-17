//! The plugin probe, a separate process: a `tutti_plugin::catalog::Catalog` task
//! (`register`, `measure`) spawns `plugin-probe`, which loads the file just far enough to read the
//! descriptor of every plugin it holds and writes them to a result file.
//!
//! One test per format reads the shared plugin's descriptor, and one test each
//! reads a file of several plugins (a CLAP factory of two, a VST3 factory of
//! three audio classes, a VST2 shell of two sub-plugins) and loads each plugin by
//! the `PluginRef` its entry gives. The CLAP plugin's
//! `init` also misbehaves by file name (`clap.rs`, `InitMode`): a probe sends
//! no state and a plugin may be configured by no environment variable, so the
//! name is the one lever. Those cases are CLAP's; the other formats have none
//! to pull.
//!
//! Needs `cargo build -p tutti-plugin-probe` first.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tutti_plugin::catalog::{
    Catalog, CatalogConfig, DEFAULT_MEASURE_TIMEOUT, DEFAULT_PROBE_TIMEOUT, Locator, NO_SCAN_DIRS,
    PluginDescriptor, PluginRef, ProbeFailure, ProbeFailureKind, ScanPoll,
};
use tutti_plugin_contract::{
    Clap, Format, Vst2, Vst3, Vst3Cpp, bundle_of, clap_path, library, spec, state_with_fault,
    state_with_rings, vst3_cpp_module, vst3_module,
};
use tutti_plugin_db::PluginDb;
use tutti_plugin_ipc::{BridgeConfig as IpcConfig, Ipc, Launched};
use tutti_plugin_types::{
    BridgeError, BridgeResult, Control, Editor, InProcess, LatencyVerdict, Load, LoadStage,
    MainThreadMarker, MaxBlock, Measured, NotChecked, PROBE_FLOOR, SampleRate, Samples, StateHash,
    TailVerdict,
};
use tutti_vst2_host::Vst2Loaded;

/// A catalog whose probes run in the `plugin-probe` built in this profile, named
/// through [`Locator::at`], so nothing writes the process-global environment. Its
/// sentinel lives in `dir`.
fn config(dir: &Path) -> CatalogConfig {
    CatalogConfig::new(dir, NO_SCAN_DIRS).probe_locator(Locator::at(tutti_fixtures::plugin_probe()))
}

/// Runs the catalog task `start` makes to its end and returns the database it
/// built, or the **first** failure the task reports: every test here probes one
/// file, so a task has at most one.
fn run_task(
    config: CatalogConfig,
    start: impl FnOnce(&mut Catalog) -> tutti_plugin_db::Scan,
) -> Result<PluginDb, ProbeFailure> {
    let mut catalog = Catalog::new(PluginDb::new(), config, |_| Ok(()));
    let mut task = start(&mut catalog);
    let deadline = Instant::now() + Duration::from_secs(300);
    let result = loop {
        match catalog.try_apply(&mut task) {
            ScanPoll::Running => {
                assert!(Instant::now() < deadline, "the task never ended");
                std::thread::sleep(Duration::from_millis(2));
            }
            ScanPoll::Done(result) => break result,
            ScanPoll::Died => panic!("the task's worker died"),
        }
    };
    match result.failures.into_iter().next() {
        Some(failed) => Err(failed.failure),
        None => Ok(catalog.db().clone()),
    }
}

/// The one descriptor a catalog registration found at `path`.
fn probe(path: &Path) -> Result<PluginDescriptor, ProbeFailure> {
    probe_with_timeout(path, DEFAULT_PROBE_TIMEOUT)
}

/// [`probe`] under a deadline of the test's choosing.
fn probe_with_timeout(path: &Path, timeout: Duration) -> Result<PluginDescriptor, ProbeFailure> {
    let found = probe_all_with_timeout(path, timeout)?;
    assert_eq!(found.len(), 1, "one plugin in {}", path.display());
    Ok(found.into_iter().next().unwrap())
}

/// Every plugin a registration found at `path`, as the catalog holds them.
fn probe_db(path: &Path) -> Result<PluginDb, ProbeFailure> {
    probe_db_with_timeout(path, DEFAULT_PROBE_TIMEOUT)
}

fn probe_db_with_timeout(path: &Path, timeout: Duration) -> Result<PluginDb, ProbeFailure> {
    let pedal = tempfile::tempdir().unwrap();
    run_task(config(pedal.path()).probe_timeout(timeout), |catalog| {
        catalog.register(path).expect("a plugin file")
    })
}

fn probe_all_with_timeout(
    path: &Path,
    timeout: Duration,
) -> Result<Vec<PluginDescriptor>, ProbeFailure> {
    let db = probe_db_with_timeout(path, timeout)?;
    Ok(db.plugins().map(|p| p.descriptor.clone()).collect())
}

/// A catalog measure task over `path`, the plugin put in the state `file` holds
/// first, under the deadline `timeout`.
fn measure_db(path: &Path, file: &Path, timeout: Duration) -> Result<PluginDb, ProbeFailure> {
    let pedal = tempfile::tempdir().unwrap();
    let config = config(pedal.path()).measure_timeout(timeout);
    run_task(config, |catalog| {
        let plugin = PluginRef::file(path).expect("a plugin file");
        catalog.measure(&plugin, Some(file)).expect("a plugin file")
    })
}

/// The shared plugin as the spec describes it, whatever the format.
fn assert_shared_plugin(d: &PluginDescriptor, format: &str) {
    assert_named(d, format, "Tutti Shared Test Plugin");
}

fn assert_named(d: &PluginDescriptor, format: &str, name: &str) {
    assert_eq!(d.name, name);
    assert_eq!(d.vendor, "Tutti");
    assert_eq!(d.class.format_name(), format, "{d:?}");
}

#[test]
fn clap_is_probed_through_the_binary() {
    let d = probe(&clap_path()).expect("probes");
    assert_shared_plugin(&d, "clap");
    assert_eq!(d.id, "tutti.shared-test-plugin");
}

/// The shared plugin under a `.vst` name in a directory of its own, so the probe
/// routes it as VST2 whatever the platform's library extension (`.dylib` on macOS
/// names no plugin format).
fn vst2_named(dir: &tempfile::TempDir, file: &str, real: PathBuf) -> PathBuf {
    let path = dir.path().join(file);
    std::fs::copy(real, &path).expect("copy the plugin");
    path
}

#[test]
fn vst2_is_probed_through_the_binary() {
    let dir = tempfile::tempdir().unwrap();
    let d = probe(&vst2_named(&dir, "shared.vst", library())).expect("probes");
    assert_shared_plugin(&d, "vst2");
}

/// **A VST3 factory is every audio class it lists**, one entry per class id, in the
/// factory's order (`class_at` in the shared plugin: the plugin, then a copy that
/// refuses `initialize`, then one that refuses processing). The C++ plugin's factory
/// has one audio class (and a controller class, which is no plugin).
#[test]
fn vst3_is_probed_through_the_binary() {
    let db = probe_db(&bundle_of(&vst3_module())).expect("probes");
    let found: Vec<_> = db.plugins().map(|p| p.descriptor.clone()).collect();
    let names: Vec<_> = found.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Tutti Shared Test Plugin",
            "Tutti Shared Test Plugin (initialize refused)",
            "Tutti Shared Test Plugin (processing refused)",
        ]
    );
    for d in &found {
        assert_named(d, "vst3", &d.name);
        assert!(d.id.starts_with("vst3."), "{}", d.id);
    }
    let mut ids: Vec<_> = found.iter().map(|d| d.id.clone()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3, "one id per class: {found:?}");
}

#[test]
fn vst3_cpp_is_probed_through_the_binary() {
    let d = probe(&bundle_of(&vst3_cpp_module())).expect("probes");
    assert_named(&d, "vst3", "Tutti Shared Test Plugin (C++)");
}

/// No editor in these tests: the served plugin is driven through `Ipc` alone.
fn no_editor(
    _: MainThreadMarker,
    _: &tutti_plugin_types::MainWorkHandle,
    path: &Path,
    _: tutti_plugin_types::PluginFormat,
    _: &Load,
) -> BridgeResult<Box<dyn Editor>> {
    Err(BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage: LoadStage::Opening,
        reason: "these tests load no editor".into(),
    })
}

/// The id of the plugin a `plugin-server` loads for `plugin`: what a served load of
/// the ref picks.
fn served_id(plugin: &PluginRef) -> String {
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
    let Launched { ipc, .. } = Ipc::launch(
        IpcConfig::at(tutti_plugin_ipc::unique_socket_path().expect("the OS has a random source")),
        plugin.path().to_path_buf(),
        plugin.format(),
        plugin.load(),
        SampleRate::SR_48K,
        None,
        tutti_plugin_types::StateUse::Project,
        Some(tutti_plugin_ipc::EditorLoader::new(
            no_editor,
            tutti_fixtures::detached_work(),
        )),
    )
    .unwrap_or_else(|e| panic!("{plugin:?}: the server loads it: {e:?}"));
    Control::info(&ipc).id.clone()
}

/// **A CLAP factory of two plugins is two entries**, the factory's order, and a served
/// load of each entry's ref loads that plugin, not the file's first.
#[test]
fn a_clap_factory_of_two_plugins_is_probed_as_two() {
    // A copy under its own name: `clap_probe()` is a link, which a catalog row keys by
    // what it names (a `.so`, which no server routes as CLAP).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two.clap");
    std::fs::copy(
        tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin"),
        &path,
    )
    .expect("copy the plugin");
    let db = probe_db(&path).expect("probes");
    let ids: Vec<_> = db.plugins().map(|p| p.descriptor.id.clone()).collect();
    // The factory's two descriptors, by the ids the reference plugin declares (the
    // crate is a dev-dependency to be built, not linked: its entry point would clash
    // with the shared plugin's).
    assert_eq!(
        ids,
        ["tutti.conformance-probe", "tutti.conformance-probe.second"]
    );

    for entry in db.plugins() {
        let plugin = PluginRef::from(&entry);
        assert_eq!(served_id(&plugin), entry.descriptor.id, "{ids:?}");
    }
}

/// A served load of a VST3 class's ref loads that class: the third of the shared
/// plugin's three, which a load of the file's first would not be.
#[test]
fn a_served_vst3_load_picks_the_class_its_ref_names() {
    let db = probe_db(&bundle_of(&vst3_module())).expect("probes");
    let entry = db.plugins().nth(2).expect("a third class");
    assert_eq!(
        served_id(&PluginRef::from(&entry)),
        entry.descriptor.id,
        "{:?}",
        entry.descriptor
    );
}

/// **A VST2 shell is its sub-plugins**, each an entry of its own, never the shell:
/// the probe lists them in the shell's order, and a load of each entry's ref (in this
/// process, which is where VST2 runs) creates that sub-plugin. A served VST2 load is
/// refused by the format (`NotOutOfProcess`), so the served pick is CLAP's and VST3's
/// above.
#[test]
fn a_vst2_shell_is_probed_as_its_sub_plugins() {
    // The shell's sub-plugins: `uniqueID`s spelt as four-character codes, and names, as
    // `tutti-vst2-shell-plugin` declares them (a dev-dependency to be built, not linked:
    // its entry point would clash with the shared plugin's).
    let alpha = i32::from_be_bytes(*b"SHAL");
    let beta = i32::from_be_bytes(*b"SHBE");
    let dir = tempfile::tempdir().unwrap();
    let real = tutti_fixtures::cdylib("tutti_vst2_shell_plugin", "tutti-vst2-shell-plugin");
    let path = vst2_named(&dir, "shell.vst", real);
    let db = probe_db(&path).expect("probes");

    let found: Vec<_> = db
        .plugins()
        .map(|p| (p.descriptor.id.clone(), p.descriptor.name.clone()))
        .collect();
    assert_eq!(
        found,
        [
            (format!("vst2.{alpha}"), "Shell Alpha".to_string()),
            (format!("vst2.{beta}"), "Shell Beta".to_string()),
        ]
    );

    let main = tutti_fixtures::main_thread();
    for entry in db.plugins() {
        let plugin = PluginRef::from(&entry);
        let loaded = Vst2Loaded::load_in_process(
            main,
            &tutti_fixtures::detached_work(),
            plugin.path(),
            &plugin.load(),
        )
        .expect("the sub-plugin loads");
        assert_eq!(loaded.metadata().id, entry.descriptor.id);
    }
}

/// The shared plugin under a file name its `init` reads, in a directory of its
/// own.
fn clap_named(dir: &tempfile::TempDir, name: &str) -> PathBuf {
    let path = dir.path().join(format!("{name}.clap"));
    std::fs::copy(library(), &path).expect("copy the plugin");
    path
}

/// **A plugin that aborts in its init is a crash, and a blacklistable one**: no
/// result file, a signal for an exit.
#[test]
fn a_plugin_that_crashes_in_init_is_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let failure = probe(&clap_named(&dir, "init-crash")).expect_err("must not probe");
    assert_eq!(failure.kind, ProbeFailureKind::Crashed, "{failure:?}");
    assert!(failure.blacklistable(), "{failure:?}");
    assert!(failure.reason.contains("crashed"), "{failure:?}");
}

/// **A plugin that never returns from its init is a timeout**, and the probe is
/// killed rather than waited for: the plugin never returns, so a probe that
/// waited for it never ends and [`run_task`]'s hang guard fails the test. The
/// deadline is the caller's: two seconds here.
#[test]
fn a_plugin_that_hangs_in_init_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let failure = probe_with_timeout(&clap_named(&dir, "init-hang"), Duration::from_secs(2))
        .expect_err("must not probe");
    assert_eq!(failure.kind, ProbeFailureKind::TimedOut, "{failure:?}");
    assert!(failure.blacklistable(), "{failure:?}");
    assert!(failure.reason.contains("timed out"), "{failure:?}");
}

/// A plugin whose init refuses is a result: the probe finished, with the
/// plugin's reason, and the scan records it.
#[test]
fn a_plugin_that_refuses_to_load_is_a_typed_failure() {
    let dir = tempfile::tempdir().unwrap();
    let failure = probe(&clap_named(&dir, "init-refuse")).expect_err("must not probe");
    assert_eq!(failure.kind, ProbeFailureKind::LoadFailed, "{failure:?}");
    assert!(failure.reason.contains("entry init failed"), "{failure:?}");
    assert!(failure.blacklistable());
}

/// **A plugin that prints from its init still probes**: stdout and stderr carry
/// nothing the host reads, so two thousand lines of each corrupt nothing and
/// fill no pipe the probe would block on.
#[test]
fn a_plugin_that_prints_during_init_still_probes() {
    let dir = tempfile::tempdir().unwrap();
    let path = clap_named(&dir, "init-print");

    // The plugin does print: run the probe by hand with its stdout captured.
    let out = std::process::Command::new(tutti_fixtures::plugin_probe())
        .arg(&path)
        .arg(dir.path().join("direct.result"))
        .output()
        .expect("spawn");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("shared plugin init: stdout line 1999"),
        "the plugin printed nothing, so this tests nothing"
    );

    let d = probe(&path).expect("probes");
    assert_shared_plugin(&d, "clap");
}

// ---- the measuring probe --------------------------------------

/// The shared plugin of format `F` at `path`, put in the state with ring
/// behaviour `rings` and tail behaviour `tail` and measured through the real
/// `plugin-probe --measure --state`, as the catalog holds it, with the state it
/// was given. The plugin reports a tail of 8191 and no latency (`tail` 0) and
/// rings as `rings` says: the figures are in `spec`, not read from the plugin.
fn measured<F: Format>(path: &Path, rings: u32, tail: u32) -> (Measured, Vec<u8>) {
    let state = state_with_rings::<F>(rings, tail);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("state");
    std::fs::write(&file, &state).unwrap();
    let db = measure_db(path, &file, DEFAULT_MEASURE_TIMEOUT).expect("the probe measures");
    let first = db.plugins().next().expect("a plugin").descriptor.clone();
    (first.measured.expect("the probe measured it"), state)
}

/// The cases of the measuring probe, once per format: the same shared plugin, the
/// same figures, through the format's own host in the probe's process.
macro_rules! measured_cases {
    ($format:ident, $fmt:ty, |$dir:ident| $path:expr) => {
        mod $format {
            use super::*;

            fn run(rings: u32, tail: u32) -> (Measured, Vec<u8>) {
                let $dir = tempfile::tempdir().unwrap();
                measured::<$fmt>(&$path, rings, tail)
            }

            /// **The plugin reports 8191 and rings 8191: `Honest`**, and the record is
            /// what the probe measured: the figure, where it was measured, the floor
            /// and a hash of the state it was told to be in.
            #[test]
            fn a_plugin_that_rings_as_it_reports_is_honest() {
                let (m, state) = run(spec::RINGS_EXACT, 0);
                assert_eq!(m.verdict.tail, TailVerdict::Honest, "{m:?}");
                assert_eq!(m.verdict.latency, LatencyVerdict::Honest, "{m:?}");
                assert_eq!(m.tail, Some(spec::RING));
                assert_eq!(m.latency, Some(Samples(0)));
                assert_eq!(m.config.rate, SampleRate::SR_48K);
                assert_eq!(m.config.block, MaxBlock::new(Samples(512)).unwrap());
                assert_eq!(m.config.floor, PROBE_FLOOR);
                assert_eq!(m.config.state, StateHash::of(&state));
            }

            /// **The plugin reports 8191 and rings 4096 frames more: `TailLonger`**,
            /// with both figures; the latency is as honest as ever.
            #[test]
            fn a_plugin_that_rings_longer_than_it_reports_is_tail_longer() {
                let (m, _) = run(spec::RINGS_LONGER, 0);
                let longer = Samples(spec::RING.get() + spec::RING_EXTRA.get());
                assert_eq!(
                    m.verdict.tail,
                    TailVerdict::TailLonger {
                        reported: spec::TAIL,
                        measured: longer
                    }
                );
                assert_eq!(m.tail, Some(longer));
                assert_eq!(m.verdict.latency, LatencyVerdict::Honest);
            }

            /// **A plugin that adds noise above the floor with silence in is
            /// `NeverSettles`, not `TailLonger`**, at the level it held, and the
            /// tail and latency are not judged.
            #[test]
            fn a_plugin_that_makes_noise_with_silence_in_never_settles() {
                let (m, _) = run(spec::RINGS_HISS, 0);
                let TailVerdict::NeverSettles { level } = m.verdict.tail else {
                    panic!("{m:?}");
                };
                assert!(
                    (spec::HISS_DB - 2.0..=spec::HISS_DB + 0.1).contains(&level.get()),
                    "the level it held: {level:?}"
                );
                assert!(matches!(
                    m.verdict.latency,
                    LatencyVerdict::NeverSettles { .. }
                ));
                assert_eq!((m.tail, m.latency), (None, None));
            }

            /// **An unbounded claim is `NotChecked`** for the tail (nothing to hold a
            /// measurement to) while the latency is still checked; a format that
            /// cannot say "unbounded" (VST2) has no such claim to test.
            #[test]
            fn an_unbounded_claim_is_not_checked() {
                if <$fmt as Format>::SURFACE.infinite_tail.is_none() {
                    return;
                }
                let (m, _) = run(spec::RINGS_EXACT, spec::TAIL_INFINITE);
                assert_eq!(
                    m.verdict.tail,
                    TailVerdict::NotChecked(NotChecked::UnboundedClaim)
                );
                assert_eq!(m.tail, None);
                assert_eq!(m.verdict.latency, LatencyVerdict::Honest);
            }

            /// **A first sound where the plugin does not report one is
            /// `LatencyDiffers`**, and that plugin, which sounds 64 frames late and
            /// rings as long after, is also `TailLonger`: the verdict is two
            /// judgements, not one.
            #[test]
            fn a_late_plugin_differs_in_latency_and_in_tail() {
                let (m, _) = run(spec::RINGS_LATE, 0);
                assert_eq!(
                    m.verdict.latency,
                    LatencyVerdict::LatencyDiffers {
                        reported: Samples(0),
                        measured: spec::LATE
                    }
                );
                assert_eq!(m.latency, Some(spec::LATE));
                let late_tail = Samples(spec::RING.get() + spec::LATE.get());
                assert_eq!(
                    m.verdict.tail,
                    TailVerdict::TailLonger {
                        reported: spec::TAIL,
                        measured: late_tail
                    }
                );
            }
        }
    };
}

measured_cases!(measured_clap, Clap, |_dir| clap_path());
measured_cases!(measured_vst2, Vst2, |dir| vst2_named(
    &dir,
    "shared.vst",
    library()
));
measured_cases!(measured_vst3, Vst3, |_dir| bundle_of(&vst3_module()));
measured_cases!(measured_vst3_cpp, Vst3Cpp, |_dir| bundle_of(
    &vst3_cpp_module()
));

/// **Without `--measure` a probe measures nothing**: the descriptor's
/// `measured` is `None`, "nobody measured it".
#[test]
fn a_plain_probe_leaves_measured_empty() {
    let d = probe(&clap_path()).expect("probes");
    assert_eq!(d.measured, None);
}

/// `--measure` on the shared plugin put in the state with fault `fault`.
fn measure_with_fault(fault: u32, timeout: Duration) -> Result<(), ProbeFailure> {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("state");
    std::fs::write(&file, state_with_fault::<Clap>(fault)).unwrap();
    measure_db(&clap_path(), &file, timeout).map(|_| ())
}

/// **A plugin that crashes while rendering is the crash a load is**: it loads,
/// its descriptor reads (a plain probe is fine), and the measuring probe dies with
/// no result file, which a failure of kind `Crashed` calls blacklistable.
#[test]
fn a_plugin_that_crashes_while_measured_is_a_crash() {
    probe(&clap_path()).expect("it loads and describes");
    let failure = measure_with_fault(spec::FAULT_CRASH, DEFAULT_MEASURE_TIMEOUT)
        .expect_err("must not measure");
    assert_eq!(failure.kind, ProbeFailureKind::Crashed, "{failure:?}");
    assert!(failure.blacklistable(), "{failure:?}");
    assert!(failure.reason.contains("crashed"), "{failure:?}");
}

/// **A plugin that hangs while rendering is the timeout a load's hang is**, and
/// the probe is killed: a probe that waited for the hang never ends and
/// [`run_task`]'s hang guard fails the test.
#[test]
fn a_plugin_that_hangs_while_measured_times_out() {
    let failure =
        measure_with_fault(spec::FAULT_HANG, Duration::from_secs(3)).expect_err("must not measure");
    assert_eq!(failure.kind, ProbeFailureKind::TimedOut, "{failure:?}");
    assert!(failure.blacklistable());
}

/// **A state the plugin refuses is a typed failure, not a measurement of the
/// default**: the caller asked for that state.
#[test]
fn a_state_the_plugin_refuses_is_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("state");
    std::fs::write(&file, b"not a state").unwrap();
    let failure =
        measure_db(&clap_path(), &file, DEFAULT_MEASURE_TIMEOUT).expect_err("must not measure");
    assert_eq!(failure.kind, ProbeFailureKind::LoadFailed, "{failure:?}");
    assert!(failure.reason.contains("refused the state"), "{failure:?}");
}
