//! The main-thread half of an activation, and the transitions that split a
//! [`ClapLoaded`] into the two halves and rejoin them.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use tutti_plugin_types::MainThreadMarker;

use tutti_plugin_types::{
    Control, Controller, EditWriter, Loaded, PluginError, PluginTail, Prepare, Refused,
    ReportDrain, RestartRules, Samples, Stuck, Widths,
};
use tutti_rt::Publisher;
use tutti_units::Normalized;
use tutti_units::ParamId;

use super::ClapLoaded;
use super::config::RangeTable;
use super::processor::ClapProcessor;
use super::thread_pool::PoolCell;

/// The main-thread half of an activation: the control surface of a
/// [`ClapLoaded`] (the [`Control`] trait and the methods
/// below), with parameter writes redirected to the Processor. It does not
/// `Deref` to the loaded plugin; a caller that needs the format's own surface
/// (the editor, ports, parameter indication) asks for
/// [`loaded`](Self::loaded) or [`loaded_mut`](Self::loaded_mut).
///
/// `Send` so a server can build it on one thread and run it on another, not
/// `Sync`, and never used from two threads at once. Dropping it ends the
/// activation: it stops processing, deactivates and destroys the plugin, so a
/// Processor that outlives it renders nothing.
///
/// ```compile_fail
/// // No `Deref`: a `&ClapController` is not a `&ClapLoaded`.
/// fn loaded(c: &tutti_clap_host::ClapController) -> &tutti_clap_host::ClapLoaded {
///     c
/// }
/// ```
pub struct ClapController {
    /// The plugin. `None` only once `deactivate` has taken it.
    loaded: Option<ClapLoaded>,
    /// The writes to the Processor: one slot per declared parameter.
    edits: EditWriter,
    /// What the Processor's drain has swept of `edits`.
    swept: tutti_rt::SweptWatch,
    /// The ranges the Processor denormalizes against, republished after a
    /// rescan that changed the parameters.
    pub(crate) ranges: Publisher<RangeTable>,
    /// The pool the Processor serves `clap.thread-pool` requests on.
    pools: Publisher<PoolCell>,
    /// The plugin's own parameter edits, which the Processor queued from its
    /// output events ([`take_reported_edits`](Control::take_reported_edits)).
    pub(crate) reported: ReportDrain,
    /// What the ring refused, counted by the Processor.
    pub(crate) reports_dropped: Arc<AtomicU64>,
    _not_sync: PhantomData<Cell<()>>,
}

impl std::fmt::Debug for ClapController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClapController")
            .field("edits", &self.edits)
            .finish_non_exhaustive()
    }
}

impl ClapController {
    pub(crate) fn new(
        loaded: ClapLoaded,
        (edits, swept): (EditWriter, tutti_rt::SweptWatch),
        ranges: Publisher<RangeTable>,
        pools: Publisher<PoolCell>,
        (reported, reports_dropped): (ReportDrain, Arc<AtomicU64>),
    ) -> Self {
        Self {
            loaded: Some(loaded),
            edits,
            swept,
            ranges,
            pools,
            reported,
            reports_dropped,
            _not_sync: PhantomData,
        }
    }

    /// The loaded plugin, for the surface the [`Control`] impl
    /// and this type's own methods do not cover. The Controller does not
    /// `Deref` to it: a call that must also tell the
    /// Processor is a method here, not one reached through the loaded type.
    ///
    /// # Panics
    ///
    /// Never in use: the plugin is taken only when the Controller is consumed.
    #[must_use]
    pub fn loaded(&self) -> &ClapLoaded {
        self.loaded
            .as_ref()
            .expect("a Controller holds its plugin until it is consumed")
    }

    /// [`loaded`](Self::loaded), mutably. Prefer a method of the Controller
    /// where one exists (`set_thread_pool`).
    ///
    /// # Panics
    ///
    /// Never in use: the plugin is taken only when the Controller is consumed.
    pub fn loaded_mut(&mut self) -> &mut ClapLoaded {
        self.loaded
            .as_mut()
            .expect("a Controller holds its plugin until it is consumed")
    }

    /// Serves the plugin's `clap.thread-pool` requests on `pool` from the next
    /// block, or refuses them (`None`), and returns the pool it replaces. See
    /// [`ClapLoaded::set_thread_pool`], which the controller no longer
    /// reaches through `Deref`: this one tells the Processor too.
    ///
    /// Control thread. The Processor lets go of a replaced pool at its next block
    /// boundary and the controller frees it on a later tick, so dropping the
    /// pool never happens on the audio thread.
    pub fn set_thread_pool(
        &mut self,
        pool: Option<Arc<tutti_graph::WorkerPool>>,
    ) -> Option<Arc<tutti_clap_ffi::GraphPool>> {
        self.set_task_pool(pool.map(|pool| Arc::new(tutti_clap_ffi::GraphPool::new(pool))))
    }

    /// [`set_thread_pool`](Self::set_thread_pool) with the served pool built by
    /// the caller, for a test that gates a wrapped pool.
    #[doc(hidden)]
    pub fn set_task_pool(
        &mut self,
        pool: Option<Arc<tutti_clap_ffi::GraphPool>>,
    ) -> Option<Arc<tutti_clap_ffi::GraphPool>> {
        let old = self.loaded_mut().set_task_pool(pool);
        let snapshot = self.loaded().pool_snapshot();
        self.pools.publish(PoolCell(snapshot));
        old
    }
}

impl Drop for ClapController {
    fn drop(&mut self) {
        // The FFI plugin stops processing under the audio claim (waiting out a
        // `process` in flight), deactivates, and destroys on drop, so a
        // Processor that outlives this renders nothing. A drop off the main
        // thread has no token to deactivate with: the FFI plugin's drop hands
        // the teardown to the owner's main-thread collector instead.
        if let Some(mut loaded) = self.loaded.take()
            && let Some(main) = MainThreadMarker::new()
        {
            loaded.deactivate_plugin(main);
        }
    }
}

impl Controller for ClapController {
    type Loaded = ClapLoaded;
    type Processor = ClapProcessor;

    fn latency(&self, main: MainThreadMarker) -> Samples {
        self.loaded().latency_samples(main)
    }

    fn tail(&self, main: MainThreadMarker) -> PluginTail {
        self.loaded()
            .tail_samples(main)
            .map_or(PluginTail::Unknown, PluginTail::from_clap_samples)
    }
}

impl ClapController {
    /// Writes a parameter to the Processor's slot: applied at the head of its
    /// next block.
    pub(crate) fn queue_edit(&mut self, id: ParamId, value: Normalized) {
        // The write policy refused an id the plugin does not declare before
        // this call, so the slot exists.
        let written = self.edits.write(id, value);
        debug_assert!(written.is_ok(), "{written:?}");
    }

    /// Whether the Processor has taken every value [`queue_edit`](Self::queue_edit)
    /// stored: its drain's sweep has reached the writes made.
    pub(crate) fn queue_delivered(&self) -> bool {
        self.swept.get().has_reached(self.edits.written())
    }

    /// Makes every write made so far stale (see [`EditWriter::reset`]).
    pub(crate) fn reset_edits(&mut self) {
        self.edits.reset();
    }

    /// Lets the parameter ranges' and the pool's retired values go.
    pub(crate) fn reclaim(&mut self) {
        self.ranges.reclaim();
        self.pools.reclaim();
    }
}

impl ClapLoaded {
    /// Rejoins the halves of one activation: checks they are a pair, stops the
    /// Processor, drops it, and hands back the plugin deactivated.
    // The error carries the halves and the whole plugin back, which is the point.
    #[allow(clippy::result_large_err)]
    fn rejoin(
        main: MainThreadMarker,
        mut processor: ClapProcessor,
        mut controller: ClapController,
    ) -> Result<ClapLoaded, (ClapProcessor, ClapController, PluginError)> {
        if !controller.loaded().plugin.owns(processor.audio_side()) {
            return Err((
                processor,
                controller,
                PluginError::Other("the halves are of two activations".into()),
            ));
        }
        // The pair and nothing else: one share each.
        if controller.loaded().plugin.audio_sides() != 1 {
            return Err((
                processor,
                controller,
                PluginError::Other("the activation has a third handle".into()),
            ));
        }
        processor.stop();
        // Writes no block has taken would die with the slots: read each slot
        // once, nothing is rendering, so none is contended.
        let mut pending = Vec::new();
        processor.take_edits(&mut pending);
        drop(processor);
        let mut loaded = controller
            .loaded
            .take()
            .expect("a Controller holds its plugin until it is consumed");
        loaded.deactivate_plugin(main);
        // An inactive plugin takes a `params.flush` on the main thread, so the
        // writes land now and the next activation starts from them.
        for (id, value) in pending {
            // A write the plugin refuses (a parameter it no longer declares)
            // was refused at the call that queued it too, so none is owed.
            let _ = Control::set_parameter(&mut loaded, main, id, value);
        }
        Ok(loaded)
    }
}

impl Loaded for ClapLoaded {
    type Processor = ClapProcessor;
    type Controller = ClapController;

    /// CLAP restarts for latency and for the audio ports.
    const RESTARTS: RestartRules = RestartRules::CLAP;

    /// 64-bit when every audio port advertises it. The ports are read again
    /// (`clap_plugin_audio_ports.get` is `[main-thread]`, and a plugin
    /// re-declares its ports while deactivated). A port the host cannot read
    /// leaves 64-bit unsupported, so 32-bit is chosen, until a later read
    /// succeeds and sets it from the ports again.
    fn widths(&mut self, main: MainThreadMarker) -> Widths {
        if self.rescan_audio_ports(main).is_err() {
            self.audio.supports_f64 = false;
        }
        Widths::new(self.audio.supports_f64)
    }

    /// Splits the plugin into its two halves. A refusal is the configuration's
    /// end, not the plugin's: CLAP plugins routinely decline 64-bit audio or a
    /// rate, so [`Refused`] carries the unconsumed [`ClapLoaded`] back beside
    /// the reason, and a caller can retry against the same mapped library and
    /// instance. Both failure paths leave it intact: the 64-bit check returns
    /// before any FFI runs, and a plugin-side `activate` refusal leaves the
    /// instance inactive. The halves exist only on success.
    fn activate(
        self,
        main: MainThreadMarker,
        prepare: Prepare,
    ) -> Result<(ClapProcessor, ClapController), Refused<Self>> {
        self.activate_now(main, prepare)
    }

    fn deactivate(
        main: MainThreadMarker,
        processor: ClapProcessor,
        controller: ClapController,
    ) -> Result<Self, Stuck<ClapProcessor, ClapController>> {
        Self::rejoin(main, processor, controller).map_err(|(processor, controller, reason)| Stuck {
            processor,
            controller,
            reason,
        })
    }
}
