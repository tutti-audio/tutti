//! The reference CLAP plugin loaded in this process and through a
//! `plugin-server`, so a case states a rule once and runs against both.
//!
//! [`in_both_hostings!`] expands one case body into a test per hosting. The
//! body drives a [`HostedProbe`], which loads the plugin one way, renders it as
//! the only node of a graph and collects what its handle reports
//! ([`PluginInvalidation`]); it never names the hosting.
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
use tutti_plugin_host::{PluginHandle, PluginSignal, Subscription};
use tutti_plugin_types::PluginInvalidation;
use tutti_units::{SampleRate, Samples};

/// Frames per block.
pub const BLOCK: usize = 64;
/// A hang guard on waiting for the plugin to report: generous, never a speed.
const REPORT_TIMEOUT: Duration = Duration::from_secs(60);

/// How the plugin is hosted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hosting {
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
pub struct HostedProbe {
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
    pub fn load(hosting: Hosting, rate: f64, refused_hz: f64) -> Self {
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
                let node = client.bind(tempo());
                let layout = ChannelLayout::from_count(2);
                let mut graph = GraphBuilder::new(ChannelLayout::EMPTY, layout);
                let (key, _controls) = graph.add_with_controls(node);
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
    pub fn reprepare(&mut self, rate: f64) {
        self.renderer()
            .editor_mut()
            .reprepare(prepare(rate))
            .expect("the graph re-prepares");
    }

    /// Renders one block, waiting for the server's answer to it when the
    /// plugin is served and alive (the pipeline never waits for one itself).
    pub fn block(&mut self) -> Vec<f32> {
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
    pub fn renders_live(&mut self) -> bool {
        let deadline = Instant::now() + REPORT_TIMEOUT;
        let mut live = 0;
        while Instant::now() < deadline {
            if self.block().iter().any(|&s| s != 0.0) {
                live += 1;
                if live >= 2 {
                    return true;
                }
            } else {
                live = 0;
            }
        }
        false
    }

    /// Asserts the next blocks are silent: the node mutes a plugin that does
    /// not run at the graph's rate.
    pub fn assert_silent(&mut self, what: &str) {
        for block in 0..4 {
            let out = self.block();
            assert!(
                out.iter().all(|&s| s == 0.0),
                "{what}: block {block} is silent, not the plugin at the wrong rate: {:?}",
                &out[..4]
            );
        }
    }

    /// Renders until the handle has reported an invalidation `want` accepts,
    /// and returns the first that it does.
    ///
    /// # Panics
    ///
    /// If none arrives within the hang guard.
    pub fn wait_for_invalidation(
        &mut self,
        what: &str,
        want: impl Fn(&PluginInvalidation) -> bool,
    ) -> PluginInvalidation {
        let deadline = Instant::now() + REPORT_TIMEOUT;
        loop {
            self.handle.editor_idle(main_thread());
            if let Some(found) = self.seen.lock().iter().find(|e| want(e)) {
                return found.clone();
            }
            assert!(Instant::now() < deadline, "no {what} was reported");
            let _ = self.block();
        }
    }

    /// Everything the handle has reported so far, in order.
    pub fn invalidations(&self) -> Vec<PluginInvalidation> {
        self.handle.editor_idle(main_thread());
        self.seen.lock().clone()
    }

    fn renderer(&mut self) -> &mut Renderer {
        match &mut self.stage {
            Stage::Local { renderer, .. } => renderer,
            Stage::Served { rig, .. } => rig.renderer(),
        }
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
            use crate::both_hostings::{HostedProbe, Hosting};

            fn run($probe: &mut HostedProbe) $body

            #[test]
            fn in_process() {
                let _lock = tutti_fixtures::machine_lock();
                run(&mut HostedProbe::load(Hosting::InProcess, $rate, $refused));
            }

            #[test]
            fn out_of_process() {
                let _lock = tutti_fixtures::machine_lock();
                run(&mut HostedProbe::load(
                    Hosting::OutOfProcess,
                    $rate,
                    $refused,
                ));
            }
        }
    };
}
pub(crate) use in_both_hostings;
