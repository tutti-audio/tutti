//! The reference CLAP plugin loaded in this process and through a
//! `plugin-server`, so a case states a rule once and runs against both.
//!
//! [`in_both_hostings!`] expands one case body into a test per hosting. The
//! body drives a [`HostedProbe`], which loads the plugin one way, renders it as
//! the only node of a graph and collects what its handle reports
//! ([`PluginInvalidation`]). What only a served plugin has (the handle's status
//! of a refusal, the bridge's dropout count) is read through methods that say
//! so and answer `None` in process.
//!
//! The case here: **a plugin that refuses the graph's rate is rolled back to
//! the rate it had, silent, reported, and retried; a rate it takes clears it.**
//!
//! Needs `cargo build -p tutti-plugin-server` first.

#![cfg(feature = "clap")]

use crate::clap_probe::{self, ProbeEnv, Rig, load_probe, render};
use tutti_fixtures::main_thread;

use std::sync::Arc;
use std::time::Duration;
use tutti_platform::Instant;
use tutti_rt::Mutex;

use tutti_buffers::{ChannelLayout, MaxBlock};
use tutti_clap_host::ClapLoaded;
use tutti_clap_test_plugin::{ClapProbe, ClapSwitch};
use tutti_graph::{GraphBuilder, Prepare, Renderer};
use tutti_plugin_host::{PluginHandle, PluginSignal, PluginStatus, Subscription};
use tutti_plugin_types::{PluginInvalidation, RateRefusalCause};
use tutti_units::{SampleRate, Samples};

const BLOCK: usize = 64;
/// The rate the plugin is loaded at, and accepts.
const LOAD_RATE: f64 = 48_000.0;
/// The rate the probe refuses.
const REFUSED_RATE: f64 = 96_000.0;
/// Another rate the probe accepts.
const OTHER_RATE: f64 = 44_100.0;
/// A hang guard on waiting for the plugin to report: generous, never a speed.
const REPORT_TIMEOUT: Duration = Duration::from_secs(60);

/// How the plugin is hosted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hosting {
    /// Loaded into this process.
    InProcess,
    /// Served by a `plugin-server` process.
    OutOfProcess,
}

/// What each hosting holds to render and to tear down.
enum Stage {
    /// The node alone in a graph; the probe lifts the refusal it armed.
    Local {
        renderer: Renderer,
        probe: ClapProbe,
    },
    /// The served node in a rig; the environment ends with it.
    Served { rig: Rig, _env: ProbeEnv },
}

/// The reference plugin loaded one way, as the only node of a graph, with its
/// handle's invalidations collected.
pub(crate) struct HostedProbe {
    stage: Stage,
    handle: PluginHandle,
    seen: Arc<Mutex<Vec<PluginInvalidation>>>,
    _subscription: Subscription,
}

impl Drop for HostedProbe {
    fn drop(&mut self) {
        if let Stage::Local { probe, .. } = &self.stage {
            probe.set_activate_refusal(u64::MAX);
        }
    }
}

impl HostedProbe {
    /// Loads the plugin at `rate`, hosted as `hosting`, refusing an activation
    /// at `refused_hz` and writing its tag to every output. The caller holds
    /// `machine_lock`: the refusal is process-wide, and the served one rides in
    /// the environment.
    pub(crate) fn load(hosting: Hosting, rate: f64, refused_hz: f64) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = {
            let seen = Arc::clone(&seen);
            move |signal| {
                if let PluginSignal::Invalidate(e) = signal {
                    seen.lock().push(e);
                }
            }
        };
        let (stage, handle, subscription) = match hosting {
            Hosting::InProcess => {
                let probe = ClapProbe::open(
                    &tutti_fixtures::cdylib("tutti_clap_test_plugin", "tutti-clap-test-plugin"),
                    tutti_fixtures::clap_probe(),
                );
                probe.set(ClapSwitch::RenderMode, render::TAG_ONLY);
                probe.set_activate_refusal(refused_hz.to_bits());
                let (client, handle) = tutti_plugin_host::load_client::<ClapLoaded, f32>(
                    main_thread(),
                    &tutti_fixtures::detached_work(),
                    &tutti_plugin_types::PluginRef::file(tutti_fixtures::clap_probe()),
                    SampleRate::of(rate),
                )
                .expect("the reference plugin loads in process");
                let subscription = handle.subscribe(record);
                let mut graph =
                    GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::from_count(2));
                let key = graph.add_with_controls(client.bind(tempo())).0;
                graph.pipe_output(key);
                let renderer = graph
                    .renderer(prepare(rate))
                    .expect("a one-plugin graph compiles");
                (Stage::Local { renderer, probe }, handle, subscription)
            }
            Hosting::OutOfProcess => {
                let env = ProbeEnv::new()
                    .render_mode(render::TAG_ONLY)
                    .refuse_rate(refused_hz);
                let loaded = load_probe(rate);
                let handle = loaded.handle.clone();
                let subscription = handle.subscribe(record);
                let rig = Rig::new(loaded.client.bind(tempo()), rate, BLOCK);
                (Stage::Served { rig, _env: env }, handle, subscription)
            }
        };
        Self {
            stage,
            handle,
            seen,
            _subscription: subscription,
        }
    }

    /// The graph moves to `rate`; the node hears of it at the next block.
    pub(crate) fn reprepare(&mut self, rate: f64) {
        let editor = match &mut self.stage {
            Stage::Local { renderer, .. } => renderer.editor_mut(),
            Stage::Served { rig, .. } => rig.renderer().editor_mut(),
        };
        editor
            .reprepare(prepare(rate))
            .expect("the graph re-prepares");
    }

    /// Renders one block, waiting for the server's answer to it when the
    /// plugin is served and alive (the pipeline never waits for one itself).
    fn block(&mut self) -> Vec<f32> {
        let out = match &mut self.stage {
            Stage::Local { renderer, .. } => renderer.render(BLOCK),
            Stage::Served { rig, .. } => {
                let out = rig.run(BLOCK, |_, _| 0.0);
                let handle = &self.handle;
                clap_probe::await_answer_or(
                    &rig.controls,
                    "the server's answer, or the plugin to stop being alive",
                    || !handle.status().is_alive(),
                );
                out
            }
        };
        out.into_iter().flatten().collect()
    }

    /// Renders until two consecutive blocks carry the plugin's sound.
    fn renders_live(&mut self) -> bool {
        let deadline = Instant::now() + REPORT_TIMEOUT;
        let mut live = 0;
        while Instant::now() < deadline {
            live = if self.block().iter().any(|&s| s != 0.0) {
                live + 1
            } else {
                0
            };
            if live >= 2 {
                return true;
            }
        }
        false
    }

    /// The bridge's count of chunks collected unanswered: served only.
    fn missed(&self) -> Option<tutti_units::Chunks> {
        match &self.stage {
            Stage::Served { rig, .. } => Some(rig.controls.chunk_outcomes().missed),
            Stage::Local { .. } => None,
        }
    }

    /// Asserts the next blocks are silent (the node mutes a plugin that does
    /// not run at the graph's rate) and, served, that the silence is
    /// deliberate: no chunk was collected unanswered.
    fn assert_silent(&mut self, what: &str) {
        let missed = self.missed();
        for block in 0..4 {
            assert!(
                self.block().iter().all(|&s| s == 0.0),
                "{what}: block {block} is silent, not the plugin at the wrong rate"
            );
        }
        assert_eq!(self.missed(), missed, "{what}: silence is not a dropout");
    }

    /// Renders until the handle has reported `n` invalidations `want`
    /// accepts, and returns them.
    ///
    /// # Panics
    ///
    /// If they do not arrive within the hang guard.
    fn wait_for(
        &mut self,
        what: &str,
        n: usize,
        want: fn(&PluginInvalidation) -> bool,
    ) -> Vec<PluginInvalidation> {
        let deadline = Instant::now() + REPORT_TIMEOUT;
        loop {
            self.handle.editor_idle(main_thread());
            let found: Vec<_> = self
                .seen
                .lock()
                .iter()
                .filter(|e| want(e))
                .cloned()
                .collect();
            if found.len() >= n {
                return found;
            }
            assert!(Instant::now() < deadline, "{n} {what} never reported");
            let _ = self.block();
        }
    }

    /// Everything the handle has reported so far, in order.
    fn invalidations(&self) -> Vec<PluginInvalidation> {
        self.handle.editor_idle(main_thread());
        self.seen.lock().clone()
    }

    /// The handle's status of a refusal: served only (an in-process handle
    /// reports a refusal through its invalidations alone).
    fn status(&self) -> Option<PluginStatus> {
        matches!(self.stage, Stage::Served { .. }).then(|| self.handle.status())
    }

    /// Drops the probe and the plugin it holds; a served one under a hang
    /// guard, since a refused plugin's teardown must return.
    fn dropped(self) {
        if matches!(self.stage, Stage::Local { .. }) {
            return drop(self);
        }
        let (done, returned) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(self);
            let _ = done.send(());
        });
        returned
            .recv_timeout(REPORT_TIMEOUT)
            .expect("dropping a refused plugin never returned");
    }
}

fn prepare(rate: f64) -> Prepare {
    Prepare::new(SampleRate::of(rate), MaxBlock::of(Samples::new(BLOCK)))
}

fn tempo() -> tutti_timeline::TempoReader {
    tutti_timeline::TempoReader::constant(tutti_units::Bpm::of(120.0)).expect("a tempo")
}

/// Runs a case body against the plugin loaded in this process and through a
/// `plugin-server`: a test for each, named `<case>::in_process` and
/// `<case>::out_of_process`, with the body's `$probe` a [`HostedProbe`] loaded
/// at `$rate` that refuses an activation at `$refused`.
macro_rules! in_both_hostings {
    ($case:ident, rate = $rate:expr, refusing = $refused:expr, |$probe:ident| $body:block) => {
        mod $case {
            use super::*;

            fn run($probe: &mut HostedProbe) $body

            #[test]
            fn in_process() {
                let _lock = tutti_fixtures::machine_lock();
                let mut probe = HostedProbe::load(Hosting::InProcess, $rate, $refused);
                run(&mut probe);
                probe.dropped();
            }

            #[test]
            fn out_of_process() {
                let _lock = tutti_fixtures::machine_lock();
                let mut probe = HostedProbe::load(Hosting::OutOfProcess, $rate, $refused);
                run(&mut probe);
                probe.dropped();
            }
        }
    };
}

fn is_refusal(e: &PluginInvalidation) -> bool {
    matches!(e, PluginInvalidation::RateRefused(_))
}

fn is_restored(e: &PluginInvalidation) -> bool {
    matches!(e, PluginInvalidation::RateRestored)
}

/// The refusal names `rate` and says the plugin runs again at `running`.
fn refused_at(e: &PluginInvalidation, rate: f64, running: f64) -> bool {
    matches!(
        e,
        PluginInvalidation::RateRefused(r) if r.rate.get() == rate
            && matches!(r.cause, RateRefusalCause::RolledBack { running: now, .. } if now.get() == running)
    )
}

in_both_hostings! {
    a_refused_rate_is_rolled_back_silent_reported_retried_and_cleared_by_a_rate_it_takes,
    rate = LOAD_RATE,
    refusing = REFUSED_RATE,
    |probe| {
        assert!(probe.renders_live(), "the plugin renders at {LOAD_RATE}");

        // The graph moves to a rate the plugin refuses.
        probe.reprepare(REFUSED_RATE);
        let refusals = probe.wait_for("refused rate", 1, is_refusal);
        assert!(refused_at(&refusals[0], REFUSED_RATE, LOAD_RATE), "{refusals:?}");
        if let Some(status) = probe.status() {
            assert!(
                matches!(&status, PluginStatus::RateRefused(r) if r.rate.get() == REFUSED_RATE),
                "{status:?}"
            );
        }
        assert!(!probe.handle.status().is_dead() && !probe.handle.is_crashed());
        // Running at the old rate under a graph at the new one would sound
        // pitch-shifted: the node stays silent.
        probe.assert_silent("refused");

        // Asked again at the same rate: refused again, announced again.
        probe.reprepare(REFUSED_RATE);
        let refusals = probe.wait_for("refused rate", 2, is_refusal);
        assert_eq!(refusals.len(), 2, "the second ask is refused and announced too");
        probe.assert_silent("refused again");

        // A rate the plugin takes brings it back, announced once.
        probe.reprepare(OTHER_RATE);
        probe.wait_for("restored rate", 1, is_restored);
        assert!(probe.renders_live(), "the plugin renders again");
        if let Some(status) = probe.status() {
            assert!(status.is_alive(), "the error clears: {status:?}");
        }

        // Refused once more: it runs at the rate it last took.
        probe.reprepare(REFUSED_RATE);
        let refusals = probe.wait_for("refused rate", 3, is_refusal);
        assert!(refused_at(&refusals[2], REFUSED_RATE, OTHER_RATE), "{refusals:?}");
        if let Some(status) = probe.status() {
            assert!(
                matches!(&status, PluginStatus::RateRefused(r) if r.rate.get() == REFUSED_RATE),
                "{status:?}"
            );
        }
        let reports = probe.invalidations();
        assert_eq!(reports.iter().filter(|e| is_restored(e)).count(), 1, "{reports:?}");
    }
}
