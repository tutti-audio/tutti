//! The restart path: stop a stream, move it to another driver or rate, start it.
//!
//! A device that comes back at another rate needs the graph re-rated, which a
//! [`StreamHost`] cannot do (it holds only the graph's audio side):
//! [`restart_with`](StreamHost::restart_with) runs the host's hook between the
//! stop and the start.

use std::sync::Arc;

use tutti_buffers::Float;
use tutti_units::SampleRate;

use crate::callback::AudioCallbackState;
use crate::stream::{
    OutputQuantum, OutputSpec, OutputStream, QuantumOutOfRange, StartError, StreamDriver,
};

/// Access to the engine while its stream is stopped, handed to a restart hook.
///
/// Only [`StreamHost::restart_with`] creates one, and only for the duration of
/// the hook, so what it reaches of the audio thread's state is reached only
/// while no callback can run.
#[derive(Debug)]
pub struct Stopped<'a, F: Float = f32> {
    state: &'a AudioCallbackState<F>,
}

impl<F: Float> Stopped<'_, F> {
    /// Installs every commit the graph's editor has sent, moving the engine's
    /// clock to a re-prepare's rate
    /// ([`Engine::apply_pending`](crate::Engine::apply_pending)). Returns
    /// whether the graph now runs a plan with no re-prepare pending (`false`
    /// while a re-prepare is half done). It is the engine's call, not
    /// `Executor::apply_pending`, which drains only the commit queue.
    ///
    /// A host drives a re-prepare's two halves through it: settle (the
    /// executor checks its units out), let the editor collect and resume,
    /// settle again (the executor adopts the resumed plan), so the first
    /// block at the new rate renders the re-prepared graph instead of the
    /// silent block a re-prepare otherwise costs.
    pub fn apply_pending(&self) -> bool {
        // `Engine::apply_pending` panics if a callback renders at the same
        // moment. None can: a `Stopped` exists only inside a restart hook, after
        // the stream was stopped and before the next opens, and
        // `StreamHost::new` makes the host's streams the state's only renderer.
        self.state.engine.apply_pending()
    }
}

/// The output stream lifecycle a host holds: start, restart on another driver
/// or rate, stop.
///
/// Built with [`new`](Self::new) from an [`OutputStream`] and the
/// [`AudioCallbackState`] its callback renders. Reads (spec, running state,
/// faults) go through [`stream`](Self::stream). Every method runs on the
/// control thread and there is no internal lock.
pub struct StreamHost<F: Float = f32> {
    stream: OutputStream,
    callback_state: Arc<AudioCallbackState<F>>,
    /// The rate the graph runs at: the stream's at build, then only what a
    /// restart hook returned `Ok` for. Kept apart from the spec, which a
    /// restart resolves to the new device's config before its hook runs;
    /// comparing against that would let a second plain restart onto the
    /// same new-rate device find "no change" and start over the old graph.
    graph_rate: SampleRate,
}

// Hand-rolled: `AudioCallbackState` holds the RT-side graph handles and is not
// `Debug`. Forwards the stream, which carries the configuration a host logs.
impl<F: Float> std::fmt::Debug for StreamHost<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamHost")
            .field("stream", &self.stream)
            .finish_non_exhaustive()
    }
}

impl<F: Float> StreamHost<F> {
    /// Creates a host from a stream and the state its callback will read. Does
    /// not start or stop anything.
    ///
    /// From here on the host's streams are the only thing that renders
    /// `callback_state`: a caller must not also call
    /// [`process_audio`](crate::process_audio) on a clone of it. A restart
    /// relies on that: its hook reaches the engine from the control side
    /// ([`Stopped::apply_pending`]) because the stream it stopped was the only
    /// renderer.
    pub fn new(stream: OutputStream, callback_state: Arc<AudioCallbackState<F>>) -> Self {
        Self {
            graph_rate: stream.sample_rate(),
            stream,
            callback_state,
        }
    }

    /// Returns the rate the graph runs at, as far as this host knows.
    ///
    /// This is the stream's rate at [`new`](Self::new), then the rate of the
    /// last restart whose hook returned `Ok`.
    pub fn graph_rate(&self) -> SampleRate {
        self.graph_rate
    }

    /// Says the graph is prepared as `prepared` says: the stream's rate and
    /// callback size ([`quantum`](tutti_graph::Prepare::quantum)) and
    /// [`graph_rate`](Self::graph_rate) take it. For a caller whose restart
    /// hook failed part way and left the graph where it is, so that what the
    /// host reports is what the graph is prepared at. It takes the graph's own
    /// values whole, so it cannot be told anything else.
    ///
    /// # Errors
    /// [`QuantumOutOfRange`], when the graph's callback size is past
    /// [`MAX_FRAMES`](crate::MAX_FRAMES); nothing changes.
    pub fn sync_to_graph(
        &mut self,
        prepared: &tutti_graph::Prepare,
    ) -> Result<(), QuantumOutOfRange> {
        let quantum = prepared
            .quantum()
            .map(OutputQuantum::try_from)
            .transpose()?;
        self.graph_rate = prepared.sample_rate();
        let mut spec = self.stream.spec().clone();
        spec.sample_rate = prepared.sample_rate();
        spec.quantum = quantum;
        self.stream.set_spec(spec);
        Ok(())
    }

    /// The stream this host runs, for reads: [`spec`](OutputStream::spec),
    /// [`is_running`](OutputStream::is_running), [`faults`](OutputStream::faults),
    /// [`latency`](OutputStream::latency).
    #[must_use]
    pub fn stream(&self) -> &OutputStream {
        &self.stream
    }

    /// Stops any running stream and starts a new one on `driver`, at the
    /// current [`spec`](OutputStream::spec).
    ///
    /// # Errors
    /// As [`OutputStream::start_with`].
    pub fn start_with<D: StreamDriver<F>>(
        &mut self,
        driver: D,
    ) -> Result<(), StartError<D::Error>> {
        self.stream.stop();
        self.stream
            .start_with(Arc::clone(&self.callback_state), driver)
    }

    /// Stops the stream. Idempotent.
    pub fn stop(&mut self) {
        self.stream.stop();
    }

    /// Restarts the stream: stops it, asks `resolve` for the spec and driver
    /// the new stream runs at, runs `rerate` with that spec, and starts the
    /// driver.
    ///
    /// `rerate` is where a host re-rates what the stream does not own (the
    /// graph's control side, the transport's rate, the device config it
    /// publishes) while no callback runs: the stream starts only once it has
    /// returned `Ok`, so the first block at the new rate renders the re-rated
    /// graph. A hook that fails leaves the stream **stopped** and its error
    /// returned: nothing plays at a rate the graph was not moved to. Its `Ok`
    /// is the claim that the graph now runs at the spec's rate
    /// ([`graph_rate`](Self::graph_rate)); after a failure the host keeps the
    /// previous rate and spec. The hook is handed a [`Stopped`]: the engine,
    /// reachable from the control side for as long as the hook runs and no
    /// callback can, so the host can finish a graph re-prepare before the first
    /// block ([`Stopped::apply_pending`]).
    ///
    /// # Errors
    /// What `resolve` or the hook report, or what the driver's
    /// [`open`](StreamDriver::open) reports (as `E`).
    pub fn restart_with<D: StreamDriver<F>, E: From<StartError<D::Error>>>(
        &mut self,
        resolve: impl FnOnce() -> Result<(OutputSpec, D), E>,
        rerate: impl FnOnce(&OutputSpec, &Stopped<'_, F>) -> Result<(), E>,
    ) -> Result<(), E> {
        self.stream.stop();
        let old = self.stream.spec().clone();
        let (spec, driver) = resolve()?;
        self.stream.set_spec(spec);
        let stopped = Stopped {
            state: &self.callback_state,
        };
        match rerate(self.stream.spec(), &stopped) {
            Ok(()) => self.graph_rate = self.stream.spec().sample_rate,
            Err(e) => {
                self.stream.set_spec(old);
                return Err(e);
            }
        }
        self.stream
            .start_with(Arc::clone(&self.callback_state), driver)?;
        Ok(())
    }
}
