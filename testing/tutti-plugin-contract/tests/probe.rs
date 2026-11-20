//! The plugin probe, a separate process: a `tutti_plugin_db::Catalog` task
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
use std::time::Duration;
use tutti_platform::Instant;

use tutti_plugin_contract::{
    Clap, Format, Vst2, Vst3, Vst3Cpp, bundle_of, clap_path, library, spec, state_with_fault,
    state_with_rings, vst3_cpp_module, vst3_module,
};
use tutti_plugin_db::PluginDb;
use tutti_plugin_db::{
    Catalog, CatalogConfig, DEFAULT_MEASURE_TIMEOUT, DEFAULT_PROBE_TIMEOUT, ProbeFailure, ScanPoll,
};
use tutti_plugin_ipc::{Ipc, IpcConfig, Launched};
use tutti_plugin_types::{
    Editor, InProcess, IpcError, IpcResult, LatencyVerdict, LoadStage, MainThreadMarker, MaxBlock,
    Measured, NotChecked, PROBE_FLOOR, SampleRate, Samples, StateHash, TailVerdict,
};
use tutti_plugin_types::{Locator, PluginDescriptor, PluginId, PluginRef};
use tutti_plugin_types::{ProbeDeath, ProbeError, ProbeExit, ProbeOutcome, ProbeRefusal};
use tutti_vst2_author::shell::{ALPHA_ID, ALPHA_NAME, BETA_ID, BETA_NAME};
use tutti_vst2_host::Vst2Loaded;

/// A catalog whose probes run in the `plugin-probe` built in this profile, named
/// through [`Locator::at`], so nothing writes the process-global environment. Its
/// sentinel lives in `dir`.
fn config(dir: &Path) -> CatalogConfig {
    CatalogConfig::new(dir).probe_locator(Locator::at(tutti_fixtures::plugin_probe()))
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

/// A catalog measure task over the first plugin of `path`, which is put in the
/// state `file` holds first, under the deadline `timeout`. The ref names the
/// plugin by the id a plain probe read (a state with no plugin named is refused
/// for a file of several).
fn measure_db(path: &Path, file: &Path, timeout: Duration) -> Result<PluginDb, ProbeFailure> {
    let scanned = probe_db(path)?;
    let entry = scanned.plugins().next().expect("a plugin in the file");
    let plugin = entry.plugin_ref();
    let pedal = tempfile::tempdir().unwrap();
    let config = config(pedal.path()).measure_timeout(timeout);
    run_task(config, |catalog| {
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
    assert_eq!(
        d.widths,
        Some(spec::WIDTHS),
        "the audio ports' 64-bit flags"
    );
    let db = probe_db(&clap_path()).expect("probes");
    let id = db.plugins().next().and_then(|p| p.id.cloned());
    assert_eq!(id, Some(PluginId::Clap("tutti.shared-test-plugin".into())));
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
    assert_eq!(d.widths, Some(spec::WIDTHS), "effFlagsCanDoubleReplacing");
}

/// **A VST3 factory is every audio class it lists**, one entry per class id, in the
/// factory's order (`class_at` in the shared plugin: the plugin, then a copy that
/// refuses `initialize`, then one that refuses processing). The C++ plugin's factory
/// has one audio class (and a controller class, which is no plugin).
#[test]
fn vst3_is_probed_through_the_binary() {
    let db = probe_db(&bundle_of(&vst3_module())).expect("probes");
    let found: Vec<_> = db.plugins().map(|p| p.descriptor.clone()).collect();
    let classes: Vec<_> = db.plugins().map(|p| p.id.cloned()).collect();
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
    }
    assert!(
        classes
            .iter()
            .all(|id| matches!(id, Some(PluginId::Vst3(_)))),
        "{classes:?}"
    );
    let ids: std::collections::HashSet<_> = classes.iter().collect();
    assert_eq!(ids.len(), 3, "one id per class: {found:?}");
}

/// A VST3 plugin's widths are its `canProcessSampleSize(kSample64)` answer; a
/// class that refuses `initialize` never answers, so its widths are unread
/// (`None`), never 32-bit only.
///
/// It is also the pin for "a failed widths load never costs the listing": the
/// refusing class is still listed, and the file's other classes keep theirs. A
/// crash, as against a refusal, is pinned by the host-side
/// `a_listing_written_before_the_probe_died_is_the_result` and by review.
#[test]
fn vst3_widths_are_the_sample_size_answer_and_unread_when_the_class_will_not_load() {
    let db = probe_db(&bundle_of(&vst3_module())).expect("probes");
    let widths: Vec<_> = db.plugins().map(|p| p.descriptor.widths).collect();
    assert_eq!(widths.len(), 3, "the refusing class is still listed");
    assert_eq!(widths[0], Some(spec::WIDTHS));
    assert_eq!(widths[1], None, "initialize refused: nothing to ask");
    assert_eq!(widths[2], Some(spec::WIDTHS), "a later class keeps its own");
}

#[test]
fn vst3_cpp_widths_are_the_sample_size_answer() {
    let d = probe(&bundle_of(&vst3_cpp_module())).expect("probes");
    assert_eq!(d.widths, Some(spec::WIDTHS));
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
    plugin: &PluginRef,
    _: tutti_plugin_types::PluginFormat,
) -> IpcResult<Box<dyn Editor>> {
    Err(IpcError::LoadFailed {
        path: Some(plugin.path().to_path_buf()),
        stage: LoadStage::Opening,
        reason: "these tests load no editor".into(),
    })
}

/// The id of the plugin a `plugin-server` loads for `plugin`: what a served load of
/// the ref picks.
fn served_id(plugin: &PluginRef) -> PluginId {
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server());
    let Launched { ipc, .. } = Ipc::launch(
        IpcConfig::at(tutti_plugin_ipc::unique_socket_path().expect("the OS has a random source")),
        &tutti_plugin_ipc::LoadRequest {
            origin: plugin,
            format: plugin.format().expect("the ref names its format"),
            rate: SampleRate::SR_48K,
            restore: None,
        },
        Some(tutti_plugin_ipc::EditorLoader::new(
            no_editor,
            tutti_fixtures::detached_work(),
        )),
        &tutti_platform::Executor::new(),
    )
    .unwrap_or_else(|e| panic!("{plugin:?}: the server loads it: {e:?}"));
    ipc.origin()
        .and_then(|origin| origin.origin.id().cloned())
        .expect("the server named the plugin it loaded")
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
    let ids: Vec<_> = db.plugins().map(|p| p.id.cloned()).collect();
    // The factory's two descriptors, by the ids the reference plugin declares (the
    // crate is a dev-dependency to be built, not linked: its entry point would clash
    // with the shared plugin's).
    assert_eq!(
        ids,
        [FACTORY_FIRST, FACTORY_SECOND].map(|text| Some(factory_id(text)))
    );

    for entry in db.plugins() {
        let plugin = entry.plugin_ref();
        assert_eq!(Some(served_id(&plugin)), entry.id.cloned(), "{ids:?}");
    }
}

/// A served load of a VST3 class's ref loads that class: the third of the shared
/// plugin's three, which a load of the file's first would not be.
#[test]
fn a_served_vst3_load_picks_the_class_its_ref_names() {
    let db = probe_db(&bundle_of(&vst3_module())).expect("probes");
    let entry = db.plugins().nth(2).expect("a third class");
    assert_eq!(
        Some(served_id(&entry.plugin_ref())),
        entry.id.cloned(),
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
    // The shell plugin itself is a dev-dependency to be built, not linked: its entry point
    // would clash with the shared plugin's. Its ids and names come from the author crate.
    let dir = tempfile::tempdir().unwrap();
    let real = tutti_fixtures::cdylib("tutti_vst2_shell_plugin", "tutti-vst2-shell-plugin");
    let path = vst2_named(&dir, "shell.vst", real);
    let db = probe_db(&path).expect("probes");

    let found: Vec<_> = db
        .plugins()
        .map(|p| (p.id.cloned(), p.descriptor.name.clone()))
        .collect();
    assert_eq!(
        found,
        [
            (
                Some(PluginId::Vst2(ALPHA_ID as u32)),
                ALPHA_NAME.to_string()
            ),
            (Some(PluginId::Vst2(BETA_ID as u32)), BETA_NAME.to_string()),
        ]
    );

    let main = tutti_fixtures::main_thread();
    for entry in db.plugins() {
        let plugin = entry.plugin_ref();
        let loaded = Vst2Loaded::load_in_process(main, &tutti_fixtures::detached_work(), &plugin)
            .expect("the sub-plugin loads");
        assert_eq!(Some(&PluginId::Vst2(loaded.metadata().id)), entry.id);
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
    assert!(
        matches!(
            failure.error,
            ProbeError::Died(ProbeDeath::Signal(_) | ProbeDeath::Status(_))
        ),
        "{failure:?}"
    );
    assert!(failure.blacklistable(), "{failure:?}");
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
    assert!(
        matches!(failure.error, ProbeError::TimedOut { .. }),
        "{failure:?}"
    );
    assert!(failure.blacklistable(), "{failure:?}");
}

/// **A plugin whose init refuses is a result, not a crash**: the probe finished,
/// with the stage it was refused at and the loader's reason, and the scan does not
/// blacklist it, since only a crash or a hang does.
#[test]
fn a_plugin_that_refuses_to_load_is_a_refusal_at_a_stage() {
    let dir = tempfile::tempdir().unwrap();
    let failure = probe(&clap_named(&dir, "init-refuse")).expect_err("must not probe");
    let ProbeError::Refused(ProbeRefusal::Load { reason, .. }) = &failure.error else {
        panic!("a refusal at a load stage: {failure:?}");
    };
    assert!(reason.contains("entry init failed"), "{reason}");
    assert!(!failure.blacklistable(), "{failure:?}");
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
                assert_eq!(m.latency, Some(Samples::new(0)));
                assert_eq!(m.config.rate, SampleRate::SR_48K);
                assert_eq!(m.config.block, MaxBlock::new(Samples::new(512)).unwrap());
                assert_eq!(m.config.floor, PROBE_FLOOR);
                assert_eq!(m.config.state, StateHash::of(&state));
            }

            /// **The plugin reports 8191 and rings 4096 frames more: `TailLonger`**,
            /// with both figures; the latency is as honest as ever.
            #[test]
            fn a_plugin_that_rings_longer_than_it_reports_is_tail_longer() {
                let (m, _) = run(spec::RINGS_LONGER, 0);
                let longer = Samples::new(spec::RING.get() + spec::RING_EXTRA.get());
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
                        reported: Samples::new(0),
                        measured: spec::LATE
                    }
                );
                assert_eq!(m.latency, Some(spec::LATE));
                let late_tail = Samples::new(spec::RING.get() + spec::LATE.get());
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
/// no result file, which a failure that `Died` calls blacklistable.
#[test]
fn a_plugin_that_crashes_while_measured_is_a_crash() {
    probe(&clap_path()).expect("it loads and describes");
    let failure = measure_with_fault(spec::FAULT_CRASH, DEFAULT_MEASURE_TIMEOUT)
        .expect_err("must not measure");
    assert!(matches!(failure.error, ProbeError::Died(_)), "{failure:?}");
    assert!(failure.blacklistable(), "{failure:?}");
}

/// **A plugin that hangs while rendering is the timeout a load's hang is**, and
/// the probe is killed: a probe that waited for the hang never ends and
/// [`run_task`]'s hang guard fails the test.
#[test]
fn a_plugin_that_hangs_while_measured_times_out() {
    let failure =
        measure_with_fault(spec::FAULT_HANG, Duration::from_secs(3)).expect_err("must not measure");
    assert!(
        matches!(failure.error, ProbeError::TimedOut { .. }),
        "{failure:?}"
    );
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
    assert!(
        matches!(
            failure.error,
            ProbeError::Refused(ProbeRefusal::StateRefused { .. })
        ),
        "{failure:?}"
    );
    assert!(
        failure.reason().contains("refused the state"),
        "{failure:?}"
    );
    assert!(!failure.blacklistable(), "{failure:?}");
}

// ---- the probe's own exits and refusals, through the binary ----

/// What `plugin-probe` did with `args` (after the path and the result file): its
/// exit and the result file it left, if any.
fn run_probe(
    path: &Path,
    args: &[&std::ffi::OsStr],
) -> (std::process::ExitStatus, Option<ProbeOutcome>) {
    let dir = tempfile::tempdir().unwrap();
    let result = dir.path().join("result");
    let status = std::process::Command::new(tutti_fixtures::plugin_probe())
        .arg(path)
        .arg(&result)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("spawn");
    (
        status,
        tutti_plugin_types::read_result(&result).expect("a whole result or none"),
    )
}

/// **A refusal is a clean exit with a typed result**, never a crash: the probe
/// writes its result and exits with `ProbeExit::Written`, so a host reads it as an
/// answer about the file.
#[track_caller]
fn assert_refused(path: &Path, args: &[&std::ffi::OsStr], refusal: &ProbeRefusal) {
    let (status, outcome) = run_probe(path, args);
    assert_eq!(status.code(), Some(i32::from(ProbeExit::Written.code())));
    assert_eq!(outcome, Some(ProbeOutcome::Failed(refusal.clone())));
}

/// **A file that is not there is `Missing`**, which no scan blacklists.
#[test]
fn a_missing_file_is_a_missing_refusal() {
    let dir = tempfile::tempdir().unwrap();
    assert_refused(&dir.path().join("Gone.clap"), &[], &ProbeRefusal::Missing);
}

/// **A path that names no format is `UnsupportedFormat`.**
#[test]
fn a_path_with_no_format_is_an_unsupported_format_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, b"x").unwrap();
    assert_refused(&path, &[], &ProbeRefusal::UnsupportedFormat);
}

/// **A format the probe was not built with is `FormatNotCompiled`**, never a
/// load failure of the plugin: Audio Units are macOS's alone.
#[cfg(not(target_os = "macos"))]
mod au_absent {
    use super::*;
    use tutti_plugin_types::PluginFormat;

    #[test]
    fn a_format_not_compiled_in_is_a_refusal() {
        assert_refused(
            &clap_path(),
            &["--as".as_ref(), "au".as_ref()],
            &ProbeRefusal::FormatNotCompiled(PluginFormat::AudioUnit),
        );
    }
}

/// **A `--state` file that cannot be read is `StateUnreadable`**: the caller's
/// fault, written as a result.
#[test]
fn an_unreadable_state_file_is_a_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("no-such-state");
    let (status, outcome) = run_probe(
        &clap_path(),
        &["--measure".as_ref(), "--state".as_ref(), state.as_os_str()],
    );
    assert_eq!(status.code(), Some(i32::from(ProbeExit::Written.code())));
    assert!(
        matches!(
            outcome,
            Some(ProbeOutcome::Failed(ProbeRefusal::StateUnreadable(_)))
        ),
        "{outcome:?}"
    );
}

/// The two plugins of the reference CLAP factory, under a file name of its own.
/// Each as the text of its [`PluginId`], which a command line carries.
const FACTORY_FIRST: &str = "clap.tutti.conformance-probe";
const FACTORY_SECOND: &str = "clap.tutti.conformance-probe.second";

/// The id `text` spells.
fn factory_id(text: &str) -> PluginId {
    text.parse().expect("a plugin id")
}

/// A state file the reference plugins refuse, in a directory of its own.
fn refused_state(dir: &tempfile::TempDir) -> PathBuf {
    let file = dir.path().join("state");
    std::fs::write(&file, b"not a state").unwrap();
    file
}

/// **`--state` goes to the plugin `--plugin` names and to no other**: given for the
/// factory's second plugin, it is the second that refuses it, by id. The first is
/// measured as it loads.
#[test]
fn a_state_goes_only_to_the_plugin_it_names() {
    let dir = tempfile::tempdir().unwrap();
    let factory = dir.path().join("two.clap");
    std::fs::copy(
        tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin"),
        &factory,
    )
    .expect("copy the plugin");
    let state = refused_state(&dir);
    let state_args = |plugin: Option<&'static str>| {
        let mut args: Vec<std::ffi::OsString> =
            vec!["--measure".into(), "--state".into(), state.clone().into()];
        if let Some(plugin) = plugin {
            args.extend(["--plugin".into(), plugin.into()]);
        }
        args
    };
    let refusing = |plugin: Option<&'static str>| {
        let args = state_args(plugin);
        let args: Vec<&std::ffi::OsStr> = args.iter().map(AsRef::as_ref).collect();
        match run_probe(&factory, &args).1 {
            Some(ProbeOutcome::Failed(ProbeRefusal::StateRefused { plugin, .. })) => plugin,
            other => panic!("the state was refused by a plugin: {other:?}"),
        }
    };

    assert_eq!(refusing(Some(FACTORY_SECOND)), factory_id(FACTORY_SECOND));
    assert_eq!(refusing(Some(FACTORY_FIRST)), factory_id(FACTORY_FIRST));

    // No plugin named, and the file holds two: the state would go to one by position.
    let args = state_args(None);
    let args: Vec<&std::ffi::OsStr> = args.iter().map(AsRef::as_ref).collect();
    assert_eq!(
        run_probe(&factory, &args).1,
        Some(ProbeOutcome::Failed(ProbeRefusal::AmbiguousPlugin {
            count: 2
        }))
    );
}

/// **A state with no plugin named goes to the file's only plugin**: the
/// refusal here is the plugin's own, by its id.
#[test]
fn a_state_with_no_plugin_named_goes_to_the_files_only_plugin() {
    let dir = tempfile::tempdir().unwrap();
    let state = refused_state(&dir);
    let (_, outcome) = run_probe(
        &clap_path(),
        &["--measure".as_ref(), "--state".as_ref(), state.as_os_str()],
    );
    assert!(
        matches!(
            outcome,
            Some(ProbeOutcome::Failed(ProbeRefusal::StateRefused { .. }))
        ),
        "{outcome:?}"
    );
}

/// **A spent run budget loads no plugin and costs each its own row**: with no
/// time left (`--budget 0`) every plugin of the factory is described and reported
/// `BudgetSpent`, and the probe writes its result and exits cleanly. That none was
/// loaded is observed through the state: it is one the second plugin refuses, and a
/// plugin that was loaded would have refused it (`StateRefused`, which fails the
/// whole file).
#[test]
fn a_spent_budget_loads_no_plugin_and_the_probe_finishes() {
    use tutti_plugin_types::MeasureFailure;
    let dir = tempfile::tempdir().unwrap();
    let factory = dir.path().join("two.clap");
    std::fs::copy(
        tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin"),
        &factory,
    )
    .expect("copy the plugin");

    let state = refused_state(&dir);
    let (status, outcome) = run_probe(
        &factory,
        &[
            "--measure".as_ref(),
            "--state".as_ref(),
            state.as_os_str(),
            "--plugin".as_ref(),
            FACTORY_SECOND.as_ref(),
            "--budget".as_ref(),
            "0".as_ref(),
        ],
    );

    assert_eq!(status.code(), Some(i32::from(ProbeExit::Written.code())));
    let Some(ProbeOutcome::Described {
        plugins,
        unmeasured,
    }) = outcome
    else {
        panic!("a described result: {outcome:?}");
    };
    let ids: Vec<_> = plugins.iter().map(|p| p.id.clone()).collect();
    assert_eq!(
        ids,
        [FACTORY_FIRST, FACTORY_SECOND].map(|text| Some(factory_id(text)))
    );
    assert!(plugins.iter().all(|p| p.descriptor.measured.is_none()));
    let lost: Vec<_> = unmeasured.iter().map(|u| Some(u.plugin.clone())).collect();
    assert_eq!(lost, ids, "each plugin lost its own rows");
    assert!(
        unmeasured
            .iter()
            .all(|u| u.failure == MeasureFailure::BudgetSpent),
        "{unmeasured:?}"
    );
}

/// **A `--plugin` the file does not hold is `UnknownPlugin`**, not a state given
/// to nobody.
#[test]
fn a_state_for_a_plugin_the_file_does_not_hold_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let state = refused_state(&dir);
    assert_refused(
        &clap_path(),
        &[
            "--measure".as_ref(),
            "--state".as_ref(),
            state.as_os_str(),
            "--plugin".as_ref(),
            "clap.no.such.plugin".as_ref(),
        ],
        &ProbeRefusal::UnknownPlugin(PluginId::Clap("no.such.plugin".into())),
    );
}

/// The probe's watch on its host, through the real binary and the shared plugin
/// hung in its first render block, so only the watch can end it.
#[cfg(unix)]
mod host_watch {
    use super::*;
    use std::io::BufRead as _;
    use std::os::unix::process::ExitStatusExt;

    /// The signal the watch ends the probe with.
    const SIGKILL: i32 = 9;

    /// The command line of a probe that hangs in its first block.
    fn hanging_probe(dir: &tempfile::TempDir) -> std::process::Command {
        let state = dir.path().join("state");
        std::fs::write(&state, state_with_fault::<Clap>(spec::FAULT_HANG)).unwrap();
        let mut probe = std::process::Command::new(tutti_fixtures::plugin_probe());
        probe
            .arg(clap_path())
            .arg(dir.path().join("result"))
            .args(["--measure", "--state"])
            .arg(state)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        probe
    }

    /// **A probe ends with its host**: its host is the shell that spawned it and
    /// stated its pid, and the probe is not this process's child, so only the watch
    /// on the host can end it. The probe's end is learned from the OS (a watch on
    /// its pid), not polled for: a probe that outlives its host fails by hanging.
    #[test]
    fn a_hung_probe_dies_with_its_host() {
        let dir = tempfile::tempdir().unwrap();
        let probe = hanging_probe(&dir);
        let command: Vec<String> = std::iter::once(probe.get_program())
            .chain(probe.get_args())
            .map(|a| format!("{a:?}"))
            .collect();
        let script = format!(
            "{env}=$$ {} </dev/null >/dev/null 2>&1 & echo $!; read _",
            command.join(" "),
            env = tutti_plugin_ipc_ffi::HOST_PID_ENV
        );
        let mut host = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("spawn the host shell");
        let mut line = String::new();
        std::io::BufReader::new(host.stdout.take().expect("piped"))
            .read_line(&mut line)
            .expect("the shell prints the probe's pid");
        let probe: u32 = line.trim().parse().expect("the probe's pid");
        let (gone, ended) = std::sync::mpsc::channel();
        let executor = tutti_platform::Executor::new();
        let _watch = tutti_plugin_ipc_ffi::watch_host(&executor, probe, move || {
            gone.send(()).expect("listening");
        })
        .expect("the watch starts");

        drop(host.stdin.take());
        host.wait().expect("the host ends");
        ended.recv().expect("the probe ends with its host");
    }

    /// **A probe behind a wrapper finishes and writes its result**: a shell that
    /// runs it without `exec` (as a wrapper script, `strace` or a sandbox does)
    /// is its parent, not the host that stated its pid, and the host is alive. The
    /// probe must not take that for a host that ended: ending it would be read as a
    /// crash of every plugin scanned.
    #[test]
    fn a_probe_behind_a_wrapper_finishes_while_its_host_runs() {
        let dir = tempfile::tempdir().unwrap();
        let result = dir.path().join("result");
        // `true` after it keeps the shell from replacing itself with the probe.
        let script = format!(
            "{probe:?} {plugin:?} {result:?} --as clap; true",
            probe = tutti_fixtures::plugin_probe(),
            plugin = clap_path(),
        );
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&script)
            .env(
                tutti_plugin_ipc_ffi::HOST_PID_ENV,
                std::process::id().to_string(),
            )
            .status()
            .expect("the wrapper ends");

        assert!(status.success(), "{status:?}");
        assert!(
            matches!(
                tutti_plugin_types::read_result(&result),
                Ok(Some(ProbeOutcome::Described { .. }))
            ),
            "the probe wrote its result"
        );
    }

    /// **A stated pid that no process holds is a host that ended**, so the probe
    /// ends at once: the pid is that of a child this test has already reaped.
    #[test]
    fn a_stated_pid_that_no_process_holds_ends_the_probe() {
        let dir = tempfile::tempdir().unwrap();
        let mut gone = std::process::Command::new("true").spawn().expect("true");
        let pid = gone.id();
        gone.wait().expect("reaped");
        let status = hanging_probe(&dir)
            .env(tutti_plugin_ipc_ffi::HOST_PID_ENV, pid.to_string())
            .status()
            .expect("the probe ends");

        assert_eq!(status.signal(), Some(SIGKILL), "ended by the watch's kill");
        assert_eq!(ProbeDeath::of(status), ProbeDeath::Signal(SIGKILL));
    }
}
