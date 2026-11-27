//! [`HardwareMidiInputs`]: every open input port, and the event inlet each
//! one pushes into.
//!
//! The list is control-side state: the audio thread reads nothing here. A port
//! is an inlet made by the shared [`InputInlets`], named by its [`InputId`]
//! (minted once per port, so a stale id names nothing); opening one returns the
//! [`InputProducerHandle`] the driver thread pushes through and the inlet node
//! the graph's owner adds. Removing the port retires its entry in this list.

use std::collections::BTreeMap;
use std::sync::Arc;
use tutti_rt::{Bell, Mutex};

use tutti_graph::EventInletNode;
use tutti_midi_runtime::{DeviceWriters, InputId, InputInlets};
use tutti_units::Losses;
use tutti_units::Resyncs;

use super::async_port::{HardwareMidiInput, InputProducerHandle, StreamDrain};
use crate::InputTiming;

/// An [`InputId`] that names no open port: the port closed, or the id came from
/// another list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("input port {0:?} is not open")]
pub struct StaleInput(pub InputId);

/// A snapshot view of one port, computed on demand from the underlying
/// `HardwareMidiInput`. The port is the single source of truth for `name` and
/// `active` — `PortInfo` just bundles them with the port's id/type for
/// listing. `active` is a point-in-time value, not a live handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortInfo {
    /// The port's identity, and the key every other method on
    /// [`HardwareMidiInputs`] takes.
    pub id: InputId,
    /// The endpoint name the port was created with — for an open connection,
    /// the name the OS reported for that device.
    pub name: String,
    /// Whether the port delivers. An inactive port's events are discarded at
    /// the driver's thread, not queued.
    pub active: bool,
    /// What the port lost since it opened: `lost` counts the UMP Stream
    /// messages its stream ring had no room for and the events the driver
    /// delivered that could not be read
    /// ([`InputProducerHandle::count_lost`](crate::InputProducerHandle::count_lost)), and `late` the JR Clock
    /// messages ignored as late (reordered, repeated, or from a clock that
    /// stepped back), counted only while the port is timed by the device's JR
    /// Timestamps.
    pub losses: Losses,
    /// How many JR Clock messages restarted the sender's clock, since the port
    /// opened: each is a jump the clock could not follow, or a silence of a
    /// second. Counted only while the port is timed by the device's JR
    /// Timestamps.
    pub jr_restarted: Resyncs,
}

impl PortInfo {
    fn of(id: InputId, port: &HardwareMidiInput) -> Self {
        Self {
            id,
            name: port.name().to_string(),
            active: port.is_active(),
            losses: port.losses(),
            jr_restarted: port.jr_restarted(),
        }
    }
}

/// Every open hardware MIDI input.
///
/// A backend creates one port per connection with
/// [`open_input_port`](Self::open_input_port) and pushes into the
/// [`InputProducerHandle`] it returns from the driver's own thread; each port
/// is an event inlet of the [`InputInlets`] the list was built over, whose
/// node the graph's owner adds and wires to the input's input node. When the
/// connection closes, `remove_input_port` retires the port.
///
/// Cheap to share: hold it in an `Arc` and hand clones to the session.
pub struct HardwareMidiInputs {
    inlets: Arc<InputInlets>,
    ports: Mutex<Ports>,
    /// Rung by each port's reader after it writes the stream ring.
    bell: Option<Bell>,
}

/// The port list, keyed by each port's id (in the order they opened).
#[derive(Default)]
struct Ports {
    live: BTreeMap<InputId, Arc<HardwareMidiInput>>,
}

impl HardwareMidiInputs {
    /// A list with no ports, each port an inlet of `inlets`.
    #[must_use]
    pub fn new(inlets: Arc<InputInlets>) -> Self {
        Self {
            inlets,
            ports: Mutex::new(Ports::default()),
            bell: None,
        }
    }

    /// This list with every port's reader ringing `bell` after it writes what
    /// [`MidiSession::poll`](crate::MidiSession::poll) reads (a UMP Stream
    /// message, or a copy of a SysEx7 packet): the reader may be an OS callback
    /// thread, and [`Bell::ring`] never allocates, locks or waits.
    #[must_use]
    pub fn ringing(mut self, bell: Bell) -> Self {
        self.bell = Some(bell);
        self
    }

    /// The inlets the ports are made by: the owner of the graph sets their
    /// clock and lead.
    #[must_use]
    pub fn inlets(&self) -> &Arc<InputInlets> {
        &self.inlets
    }

    /// Rings the bell, if the list has one: whoever waits on it polls the session.
    pub(crate) fn ring(&self) {
        if let Some(bell) = &self.bell {
            bell.ring();
        }
    }

    fn ports(&self) -> std::sync::MutexGuard<'_, Ports> {
        // A poisoned lock guards nothing inconsistent: every change is whole.
        self.ports.lock()
    }

    /// Creates an input port named `name` and returns the **only** producer
    /// handle for it, with the inlet node the graph's owner adds;
    /// [`InputProducerHandle::port`] is the port's identity.
    ///
    /// The port starts **active**. Control side: it allocates the inlet. Safe to
    /// call from several threads at once.
    ///
    /// There is no other way to reach a port's producer: give the handle to the
    /// one thread that pushes. Dropping it leaves the port's entry in the list
    /// until `remove_input_port` retires it, and the entry then reads as a port
    /// fed by nothing.
    #[must_use = "the handle is the port's only producer, and the node its graph side: dropping either leaves the port fed by nothing"]
    pub fn open_input_port(
        &self,
        name: impl Into<String>,
    ) -> (InputProducerHandle, EventInletNode) {
        self.open_input_port_with(name, DeviceWriters::unread())
    }

    /// [`open_input_port`](Self::open_input_port), the port's clock map
    /// reporting to `writers` (see [`DeviceWriters`]).
    #[must_use = "the handle is the port's only producer, and the node its graph side: dropping either leaves the port fed by nothing"]
    pub fn open_input_port_with(
        &self,
        name: impl Into<String>,
        writers: DeviceWriters,
    ) -> (InputProducerHandle, EventInletNode) {
        let name = name.into();
        let mut ports = self.ports();
        let (input, node) = self.inlets.open_with(writers);
        let id = input.id();
        let (port, handle) = HardwareMidiInput::new(name, input, self.bell.clone());
        ports.live.insert(id, Arc::new(port));
        (handle, node)
    }

    /// Retires the port `id` names. Returns `false`, and changes nothing, when
    /// `id` names no live port: one already removed.
    ///
    /// Events still in its inlet are discarded with it. Close the port's driver
    /// connection first (drop its [`InputProducerHandle`]), so nothing is still
    /// pushing; a handle that outlives the port is
    /// harmless, and whatever it pushes is lost. Control side.
    pub(crate) fn remove_input_port(&self, id: InputId) -> bool {
        let mut ports = self.ports();
        ports.live.remove(&id).is_some()
    }

    /// How many ports the list holds.
    #[cfg(test)]
    pub(crate) fn port_count(&self) -> usize {
        self.ports().live.len()
    }

    /// The live port `id` names, if any: a stale id names none.
    fn port(&self, id: InputId) -> Option<Arc<HardwareMidiInput>> {
        self.ports().live.get(&id).cloned()
    }

    /// A snapshot of one port, or `None` when `id` names no live port.
    pub fn port_info(&self, id: InputId) -> Option<PortInfo> {
        self.port(id).map(|port| PortInfo::of(id, &port))
    }

    /// Every input port, in index order.
    pub fn list_input_ports(&self) -> Vec<PortInfo> {
        self.ports()
            .live
            .iter()
            .map(|(id, port)| PortInfo::of(*id, port))
            .collect()
    }

    /// The UMP Stream messages the port `id` names has held since the last
    /// read, oldest first, taken as the iterator is read: what the device said
    /// about itself (`InputProducerHandle::push` keeps them out of the inlet).
    /// Nothing for a stale id. **No lock is held between steps**, so the
    /// caller may do what it likes with each message. Control side, one reader
    /// at a time.
    pub(crate) fn drain_stream(&self, id: InputId) -> StreamDrain {
        StreamDrain::new(self.port(id))
    }

    /// Sets whether the port `id` names delivers.
    ///
    /// # Errors
    ///
    /// [`StaleInput`], changing nothing, when `id` names no live port.
    ///
    /// An inactive port discards what the driver pushes: nothing is queued
    /// while it is paused, so there is no burst when it is active again.
    pub fn set_port_active(&self, id: InputId, active: bool) -> Result<(), StaleInput> {
        self.port(id).ok_or(StaleInput(id))?.set_active(active);
        Ok(())
    }

    /// Sets where the event times of the port `id` names come from:
    /// [`InputTiming::JitterReduction`] places its events by the JR Timestamps
    /// the device sends, [`InputTiming::Arrival`] by the OS's arrival stamp
    /// (the port's state until this is called). A [`MidiSession`](crate::MidiSession)
    /// sets it as an input's device settles; a port fed without a session sets
    /// it itself.
    ///
    /// # Errors
    ///
    /// [`StaleInput`], changing nothing, when `id` names no live port.
    pub fn set_input_timing(&self, id: InputId, timing: InputTiming) -> Result<(), StaleInput> {
        self.port(id).ok_or(StaleInput(id))?.set_timing(timing);
        Ok(())
    }

    /// Whether the stream ring of the port `id` names also takes a copy of the
    /// SysEx7 packets the device sends (`drain_stream` then yields them beside
    /// the Stream messages), for a control side that is waiting for an Identity
    /// Reply. The packets still reach the graph. `false` for a stale id.
    pub(crate) fn hear_sysex(&self, id: InputId, hear: bool) -> bool {
        match self.port(id) {
            Some(port) => {
                port.set_hear_sysex(hear);
                true
            }
            None => false,
        }
    }
}

impl core::fmt::Debug for HardwareMidiInputs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("HardwareMidiInputs")
            .field("num_input_ports", &self.ports().live.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;
