//! [`MidiPart`] without the `midi` feature: the session declares no MIDI, so each step over it
//! does nothing. `midi` is this module's twin when MIDI is built.

#![cfg_attr(
    not(feature = "device"),
    expect(dead_code, reason = "a device restart (feature `device`) is the user")
)]

use tutti_platform::Instant;

use tutti_buffers::Float;
use tutti_graph::NodeKey;
use tutti_units::{SampleRate, Samples};

use super::TransportPart;
use crate::builder::{Options, Parts};
use crate::graph::Graph;
use crate::value::ValueTable;
use crate::{Playhead, SessionEvent, TransportHandle};

/// No MIDI.
#[derive(Debug)]
pub(crate) struct MidiPart;

/// Nothing was followed.
#[derive(Debug, Default)]
pub(crate) struct Followed;

impl Followed {
    /// The graph was not edited.
    pub(crate) fn edited(&self) -> bool {
        false
    }
}

impl MidiPart {
    pub(crate) fn new<F: Float>(_: &mut Parts<F>, _: &Options) -> Self {
        Self
    }

    /// No MIDI cell to register.
    pub(crate) fn provide_values(&self, _: &ValueTable) {}

    /// No cell published.
    pub(crate) fn reclaim(&self) -> usize {
        0
    }

    /// The session holds nothing of `node`.
    pub(crate) fn forget_node(&mut self, _: NodeKey) -> Option<NodeKey> {
        None
    }

    /// No output to drain.
    pub(crate) fn out_phase<F: Float>(&mut self, _: &mut Graph<F>, _: Instant) -> bool {
        false
    }

    /// No device to step.
    pub(crate) fn poll_devices<F: Float>(
        &mut self,
        _: &mut Graph<F>,
        _: Instant,
        _: &mut Vec<SessionEvent>,
    ) -> bool {
        false
    }

    /// Nothing is followed.
    pub(crate) fn poll_follow<F: Float>(
        &mut self,
        _: &TransportPart,
        _: Playhead,
        _: &mut Graph<F>,
        _: &mut Vec<SessionEvent>,
    ) -> Followed {
        Followed
    }

    /// Nothing was asked of the transport.
    pub(crate) fn send_follow(_: Followed, _: &TransportHandle, _: &mut Vec<SessionEvent>) {}

    /// No inlet to place.
    pub(crate) fn set_block(&self, _: Samples, _: SampleRate) {}

    /// No tap to reopen.
    pub(crate) fn follow_rate<F: Float>(&mut self, _: &mut Graph<F>, _: Instant) -> bool {
        false
    }
}
