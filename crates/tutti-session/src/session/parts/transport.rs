//! [`TransportPart`]: the session's transport, the handle that drives it, and the metronome
//! that follows it.

#![cfg_attr(
    not(feature = "device"),
    expect(dead_code, reason = "a device restart (feature `device`) is the user")
)]

use tutti_graph::NodeKey;
use tutti_timeline::{MotionFsm, Transport, TransportSettings};
use tutti_units::SampleRate;

use crate::TransportHandle;

/// The transport, the handle with its verbs, and the click that follows the transport.
#[derive(Debug)]
pub(crate) struct TransportPart {
    transport: Transport,
    /// The transport's verbs and the engine's block clock behind
    /// [`Session::playhead`](crate::Session::playhead); the clock is `None`
    /// detached, where nothing renders.
    pub(crate) handle: TransportHandle,
    /// The metronome's node, when the builder inserted one.
    pub(crate) click: Option<NodeKey>,
}

impl TransportPart {
    pub(crate) fn new(transport: Transport, click: Option<NodeKey>) -> Self {
        Self {
            handle: TransportHandle::new(transport.clone()),
            transport,
            click,
        }
    }

    /// The transport.
    pub(crate) const fn transport(&self) -> &Transport {
        &self.transport
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
}
