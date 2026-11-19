//! [`Hardware`] without the `midi-hardware` feature: no OS MIDI layer is open. `hardware` is
//! this module's twin when the layer is built.

#![cfg_attr(
    not(feature = "device"),
    expect(dead_code, reason = "a device restart (feature `device`) is the user")
)]

use tutti_platform::Instant;

use tutti_buffers::Float;
use tutti_units::{SampleRate, Samples};

use super::MidiNodes;
use crate::SessionEvent;
use crate::builder::Parts;
use crate::graph::Graph;
use crate::midi::MidiState;
use crate::value::ValueTable;

/// No OS MIDI layer.
#[derive(Debug)]
pub(crate) struct Hardware;

impl Hardware {
    pub(crate) fn new<F: Float>(_: &mut Parts<F>) -> (Self, MidiState) {
        (Self, MidiState::default())
    }

    /// No policy cell.
    pub(crate) fn provide_values(&self, _: &ValueTable) {}

    /// No inlet to scan.
    pub(crate) fn reclaim(&self) -> usize {
        0
    }

    /// No device to step.
    pub(crate) fn poll<F: Float>(
        &mut self,
        _: &mut MidiState,
        _: Option<&MidiNodes>,
        _: &mut Graph<F>,
        _: Instant,
        _: &mut Vec<SessionEvent>,
    ) -> bool {
        false
    }

    /// No inlet to place.
    pub(crate) fn set_block(&self, _: Samples, _: SampleRate) {}
}
