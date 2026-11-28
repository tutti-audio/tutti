//! What the formats still differ in, that only the server process does.
//!
//! [`ServerSide`] has a default for everything, so a format that differs in
//! nothing is an empty `impl`. What differs is not the format's own behaviour
//! (that is its `-host` crate's, behind [`Control`](tutti_plugin_types::Control)
//! and [`Loaded`](tutti_plugin_types::Loaded)) but what this process does around
//! it: the pool it serves a plugin's thread-pool extension on, what it drops
//! unread, and how a write waiting for a block is delivered early.

use tutti_plugin_types::{
    InProcess, Lifecycle, MainThreadMarker, OutOfProcess, ParamEdit, Request,
};

/// What the server does to a loaded plugin of format `Self`, beyond what the
/// format's [`OutOfProcess::load_served`] and its `Control` give.
pub(crate) trait ServerSide: OutOfProcess + InProcess {
    /// Called once on a plugin just loaded, before it is activated: what this
    /// process lends it (CLAP's `clap.thread-pool`).
    fn adopt(&mut self) {}

    /// Appends the parameter edits the plugin made of its own accord since the
    /// last call to `out`, in order, for the server to forward to the host
    /// ([`Control::take_reported_edits`](tutti_plugin_types::Control::take_reported_edits)),
    /// and drops what else the plugin queued that the server has no use for, so
    /// a bounded queue the plugin fills holds no backlog (VST3's progress and
    /// unit news).
    fn take_edits(
        lifecycle: &mut Lifecycle<Self>,
        main: MainThreadMarker,
        out: &mut Vec<ParamEdit>,
    ) {
        if let Some(control) = lifecycle.control() {
            control.take_reported_edits(main, out);
        }
    }

    /// What the plugin raised since the pass's first take, on a later pass of the
    /// same call (a restart a pass performs may make the plugin ask for more).
    /// The first take is [`Control::take_requests`](tutti_plugin_types::Control::take_requests),
    /// which serves what is due on the main thread (CLAP's callbacks, timers and
    /// descriptors); taking it again would serve a period-0 timer on every
    /// pass. The default takes it again, for a format whose take is only what
    /// the plugin raised.
    fn take_raised(
        lifecycle: &mut Lifecycle<Self>,
        main: MainThreadMarker,
        out: &mut Vec<Request>,
    ) {
        if let Some(control) = lifecycle.control() {
            control.take_requests(main, out);
        }
    }
}

#[cfg(feature = "clap")]
pub(in crate::served) mod clap {
    use tutti_clap_host::ClapLoaded;
    use tutti_plugin_types::{Lifecycle, MainThreadMarker, Request};

    use super::ServerSide;

    /// The pool this process serves `clap.thread-pool` requests on, built the
    /// first time a plugin that implements the extension loads; `None` on a
    /// machine too small for one (below) or that refuses a thread.
    ///
    /// The engine's own pool lives in the host process, and a plugin's `process`
    /// runs here, in another: a request cannot reach it. So the server keeps a
    /// `WorkerPool` of its own, with its helpers asking for the audio thread's
    /// priority (the highest the OS grants, not real time), as `tutti_graph::WorkerPool` requires of helpers. A
    /// plugin that does not implement the extension costs no threads.
    ///
    /// Sized by [`pool_participants`]: half the cores, at most 4, the server's
    /// audio thread included. Each plugin instance has a server process of its
    /// own, so every pooled instance adds a pool, and the engine's pool in the
    /// host wants the same cores; half leaves the other half to them, and the cap
    /// keeps a few pooled instances from oversubscribing a large machine, where
    /// one block's tasks rarely keep more than a few threads busy anyway. Below
    /// two participants (fewer than 4 cores) no pool is built, and requests are
    /// refused: the plugin's own fallback does as well as a pool of one.
    ///
    /// A request is one span of one block, so its helpers spin only briefly and
    /// park soon after (10 µs, then 50 µs of yielding) rather than holding cores
    /// the engine could use between blocks. That costs a request nothing: it
    /// never waits for a helper that has not started a task (the audio thread
    /// runs tasks from the start, and helpers join as they arrive), though a
    /// helper preempted mid-task delays the request.
    pub(in crate::served) fn thread_pool() -> Option<std::sync::Arc<tutti_graph::WorkerPool>> {
        THREAD_POOL
            .get_or_init(|| {
                let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
                let participants = pool_participants(cores);
                if participants < 2 {
                    return None;
                }
                tutti_graph::WorkerPool::builder()
                    .num_threads(participants)
                    .thread_name(|i| format!("tutti-clap-pool-{i}"))
                    .spin_budget(std::time::Duration::from_micros(10))
                    .idle(std::time::Duration::from_micros(50))
                    .start_handler(|_| {
                        crate::server::request_audio_priority("a clap.thread-pool helper")
                    })
                    .build()
                    .inspect_err(|e| {
                        tracing::warn!(
                            "clap.thread-pool: a {participants}-thread pool was refused, \
                             requests are refused: {e}"
                        );
                    })
                    .ok()
                    .map(std::sync::Arc::new)
            })
            .clone()
    }

    /// Participants of the server's pool on a machine with `cores` cores (see
    /// [`thread_pool`]): half of them, at most 4.
    pub(in crate::served) fn pool_participants(cores: usize) -> usize {
        (cores / 2).min(4)
    }

    /// [`thread_pool`]'s pool, once decided.
    pub(in crate::served) static THREAD_POOL: std::sync::OnceLock<
        Option<std::sync::Arc<tutti_graph::WorkerPool>>,
    > = std::sync::OnceLock::new();

    impl ServerSide for ClapLoaded {
        fn adopt(&mut self) {
            if self.supports_thread_pool() {
                self.set_thread_pool(thread_pool());
            }
        }

        /// Timers and descriptors are polled once per pass of the host's loop;
        /// only what the plugin raised is taken again.
        fn take_raised(
            lifecycle: &mut Lifecycle<Self>,
            _main: MainThreadMarker,
            out: &mut Vec<Request>,
        ) {
            match lifecycle {
                Lifecycle::Loaded(plugin) | Lifecycle::Refused { plugin, .. } => {
                    plugin.take_raised(out);
                }
                Lifecycle::Active { controller, .. } => controller.loaded_mut().take_raised(out),
                Lifecycle::Poisoned => {}
            }
        }
    }
}

#[cfg(feature = "vst3")]
impl ServerSide for tutti_vst3_host::Vst3Loaded {
    /// The edits the plugin made go to the host; the progress and unit news it
    /// queues are dropped unread, so the host's bounded queue holds no backlog.
    fn take_edits(
        lifecycle: &mut Lifecycle<Self>,
        main: MainThreadMarker,
        out: &mut Vec<ParamEdit>,
    ) {
        if let Some(control) = lifecycle.control() {
            control.take_reported_edits(main, out);
        }
        match lifecycle {
            Lifecycle::Loaded(plugin) | Lifecycle::Refused { plugin, .. } => {
                plugin.discard_notifications();
            }
            Lifecycle::Active { controller, .. } => {
                controller.loaded_mut().discard_notifications();
            }
            Lifecycle::Poisoned => {}
        }
    }
}
