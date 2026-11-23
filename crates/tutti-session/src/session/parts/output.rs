//! [`OutputPart`]: the main output asked for, the policy, and the engine's output-side readings (the
//! heard tap, the load meter and its silenced count).

#![cfg_attr(
    not(feature = "device"),
    expect(dead_code, reason = "a device restart (feature `device`) is the user")
)]

use std::sync::Arc;

use tutti_buffers::Float;
use tutti_buffers::{ChannelLayout, ChannelTopology};
use tutti_engine::HeardTaps;
use tutti_engine::{BlockCounts, Load};
use tutti_graph::MainOutputError;
use tutti_rt::{Counter, Gauge};

use crate::graph::Graph;
use crate::value::Cell;
use crate::{OutputPolicy, SessionEvent};

/// The main output topology asked for, the output policy in force, and the engine's
/// output-side readings.
#[derive(Debug)]
pub(crate) struct OutputPart<F: Float> {
    /// The main output topology last asked for
    /// ([`Session::declare_main_output_topology`](crate::Session::declare_main_output_topology)),
    /// applied or not.
    pub(crate) main_output_request: Option<ChannelTopology>,
    /// Why `main_output_request` is not in force, if it is not.
    pub(crate) main_output_refusal: Option<MainOutputError>,
    /// Where the mix lands, as set ([`Session::policy`](crate::Session::policy)).
    pub(crate) policy: Arc<Cell<OutputPolicy>>,
    /// The engine's tap after the fader and the device width it was built
    /// for; `None` detached.
    pub(crate) taps: Option<(HeardTaps<F>, ChannelLayout)>,
    /// The engine's load and block counts, offline or on a device; `None`
    /// detached.
    pub(crate) readings: Option<(Gauge<Load>, Counter<BlockCounts>)>,
    /// [`BlockCounts::silenced`] as the last update read it: the next update reports the rise
    /// ([`SessionEvent::BlocksSilenced`]).
    silenced_seen: u64,
}

impl<F: Float> OutputPart<F> {
    pub(crate) fn new(
        main_output_request: Option<ChannelTopology>,
        policy: Arc<Cell<OutputPolicy>>,
    ) -> Self {
        Self {
            main_output_request,
            main_output_refusal: None,
            policy,
            taps: None,
            readings: None,
            silenced_seen: 0,
        }
    }

    /// Takes the refusal of a declaration that waited and could not be
    /// applied into `main_output_refusal`, where a device restart re-attempts it.
    pub(crate) fn sync_refusal(&mut self, graph: &mut Graph<F>) {
        if let Some(e) = graph.take_apply_refusal() {
            self.main_output_refusal = Some(e);
        }
    }

    /// Declares `main_output` on `graph` unless the same request is already in force.
    ///
    /// # Errors
    ///
    /// The graph's refusal, which is kept as `main_output_refusal`.
    pub(crate) fn declare(
        &mut self,
        graph: &mut Graph<F>,
        main_output: Option<ChannelTopology>,
    ) -> Result<(), MainOutputError> {
        self.sync_refusal(graph);
        // The same request again, in force, is nothing to do.
        if main_output == self.main_output_request && self.main_output_refusal.is_none() {
            return Ok(());
        }
        self.main_output_request = main_output;
        match graph.declare_main_output(main_output, false) {
            Ok(()) => self.main_output_refusal = None,
            Err(e) => {
                self.main_output_refusal = Some(e);
                return Err(e);
            }
        }
        Ok(())
    }

    /// A declaration a device refused is asked again, hard, of the device the
    /// graph is now placed on. Whether it is in force now, so the caller commits.
    pub(crate) fn reattempt(&mut self, graph: &mut Graph<F>) -> bool {
        if self.main_output_refusal.is_none() {
            return false;
        }
        match graph.declare_main_output(self.main_output_request, true) {
            Ok(()) => {
                self.main_output_refusal = None;
                true
            }
            Err(e) => {
                self.main_output_refusal = Some(e);
                false
            }
        }
    }

    /// The device width the heard tap opens at, after a device change.
    pub(crate) fn set_device_width(&mut self, width: ChannelLayout) {
        if let Some((_, layout)) = &mut self.taps {
            *layout = width;
        }
    }

    /// The blocks the engine silenced since the last update, as one
    /// [`SessionEvent::BlocksSilenced`]. The counter's tally is cumulative,
    /// so the rise is read against the last reading.
    pub(crate) fn report_silenced(&mut self, out: &mut Vec<SessionEvent>) {
        let Some((_, blocks)) = &self.readings else {
            return;
        };
        let now = blocks.get().silenced;
        let rise = now.saturating_sub(self.silenced_seen);
        self.silenced_seen = now;
        if rise > 0 {
            out.push(SessionEvent::BlocksSilenced(rise));
        }
    }

    /// Frees what the audio thread let go of in the heard tap, closes it if its
    /// reader was dropped, and pushes its pending install.
    pub(crate) fn reclaim(&mut self) {
        if let Some((taps, _)) = &mut self.taps {
            taps.reclaim();
        }
    }
}
