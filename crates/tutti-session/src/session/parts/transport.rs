//! [`TransportPart`]: the session's transport, the handle that drives it, and what the
//! last plan sent says of the delay it adds.

#![cfg_attr(
    not(feature = "device"),
    expect(dead_code, reason = "a device restart (feature `device`) is the user")
)]

use std::sync::Arc;

use tutti_graph::{Compensation, NodeKey, PrerollTable};
use tutti_transport::{MotionFsm, Transport, TransportSettings};
use tutti_units::{SampleRate, Samples};

use crate::TransportHandle;

/// The transport, its verbs and the figures of the plan sent last.
#[derive(Debug)]
pub(crate) struct TransportPart {
    transport: Transport,
    /// The transport's verbs and the engine's block clock behind
    /// [`Session::playhead`](crate::Session::playhead); the clock is `None`
    /// detached, where nothing renders.
    pub(crate) handle: TransportHandle,
    /// The metronome's node, when the builder inserted one.
    pub(crate) click: Option<NodeKey>,
    /// The per-channel pre-roll of the plan sent last, for sources outside
    /// the graph (a disk stream), republished with every plan.
    pub(crate) compensation: Arc<PrerollTable>,
    /// The total latency of the plan sent last.
    pub(crate) latency: Samples,
}

impl TransportPart {
    pub(crate) fn new(
        transport: Transport,
        click: Option<NodeKey>,
        compensation: Arc<PrerollTable>,
    ) -> Self {
        Self {
            handle: TransportHandle::new(transport.clone()),
            transport,
            click,
            compensation,
            latency: Samples(0),
        }
    }

    /// The transport's settings: the tempo map and the project meter.
    pub(crate) fn settings(&self) -> &TransportSettings {
        self.transport.settings()
    }

    /// The transport's motion state machine.
    pub(crate) fn motion(&self) -> &MotionFsm {
        self.transport.motion()
    }

    /// The rate the transport runs at.
    pub(crate) fn sample_rate(&self) -> SampleRate {
        self.transport.sample_rate()
    }

    /// Moves the transport to `rate`.
    pub(crate) fn set_sample_rate(&self, rate: SampleRate) {
        self.transport.set_sample_rate(rate);
    }

    /// Publishes the compensation of the plan sent last: the per-channel
    /// pre-roll and the total latency. Nothing before the first plan.
    pub(crate) fn publish(&mut self, sent: Option<Compensation>) {
        if let Some(sent) = sent {
            self.latency = sent.total();
            self.compensation.set(sent.channels());
        }
    }
}
