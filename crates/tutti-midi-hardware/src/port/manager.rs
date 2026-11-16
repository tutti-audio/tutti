//! [`HardwareMidiInputs`]: every open input port, and the event inlet each
//! one pushes into.
//!
//! The list is control-side state: the audio thread reads nothing here. A port
//! is an inlet made by the shared [`InputInlets`], named by its [`InputId`]
//! (minted once per port, so a stale id names nothing); opening one returns the
//! [`InputProducerHandle`] the driver thread pushes through and the inlet node
//! the graph's owner adds. Removing the port retires its entry in this list.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use tutti_graph::{EventInletNode, Losses};
use tutti_midi_runtime::{InputId, InputInlets};
use tutti_rt::{Counter, Gauge};
use tutti_types::{Resyncs, Seconds};

use super::async_port::{HardwareMidiInput, InputProducerHandle, StreamDrain};
use crate::InputTiming;

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
    /// What the port's inlet has lost since the port opened, in events:
    /// `lost` for want of room in its ring, `evicted` from its waiting list,
    /// `refused` by the graph's port, and `late`: events that reached the graph
    /// after their frame had begun and sounded at the first frame of the block
    /// that found them (late, not lost; the lead the inlets place arrivals with
    /// ([`InputInlets`]) is what prevents it).
    pub losses: Losses,
    /// How many UMP Stream messages the port's stream ring had no room for,
    /// since the port opened.
    pub stream_lost: u64,
    /// How many JR Clock messages restarted the sender's clock, since the port
    /// opened: each is a jump the clock could not follow, or a silence of a
    /// second. Counted only while the port is timed by the device's JR
    /// Timestamps.
    pub jr_restarted: u64,
    /// How many JR Clock messages were ignored as late (reordered, repeated, or
    /// from a clock that stepped back), under the same condition.
    pub jr_late: u64,
}

impl PortInfo {
    fn of(id: InputId, port: &HardwareMidiInput) -> Self {
        Self {
            id,
            name: port.name().to_string(),
            active: port.is_active(),
            losses: port.losses(),
            stream_lost: port.stream_lost(),
            jr_restarted: port.jr_restarted(),
            jr_late: port.jr_late(),
        }
    }
}

/// Every open hardware MIDI input.
///
/// A backend creates one port per connection with
/// [`open_input_port`](Self::open_input_port) and pushes into the
/// [`InputProducerHandle`] it returns from the driver's own thread; each port
/// is an event inlet of the [`InputInlets`] the list was built over, whose
/// node the graph's owner adds and wires to the input's ingest node. When the
/// connection closes, `remove_input_port` retires the port.
///
/// Cheap to share: hold it in an `Arc` and hand clones to the session.
pub struct HardwareMidiInputs {
    inlets: Arc<InputInlets>,
    ports: Mutex<Ports>,
}

/// The port list, keyed by each port's id (in the order they opened), and the
/// losses of the ports removed so far. They share one lock so a port moves
/// from the one to the other in one step, and a [`losses`](HardwareMidiInputs::losses)
/// read never counts it twice or not at all.
#[derive(Default)]
struct Ports {
    live: BTreeMap<InputId, Arc<HardwareMidiInput>>,
    /// A retired port's counts stay in [`losses`](HardwareMidiInputs::losses).
    retired: Losses,
}

impl HardwareMidiInputs {
    /// A list with no ports, each port an inlet of `inlets`.
    #[must_use]
    pub fn new(inlets: Arc<InputInlets>) -> Self {
        Self {
            inlets,
            ports: Mutex::new(Ports::default()),
        }
    }

    /// The inlets the ports are made by: the owner of the graph sets their
    /// clock and lead.
    #[must_use]
    pub fn inlets(&self) -> &Arc<InputInlets> {
        &self.inlets
    }

    fn ports(&self) -> std::sync::MutexGuard<'_, Ports> {
        // A poisoned lock guards nothing inconsistent: every change is whole.
        self.ports.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the ports' inlets have lost, summed over all ports, current and
    /// removed, since this list was built (see [`PortInfo::losses`]). An inlet
    /// fills only when events arrive faster than blocks take them for long
    /// enough to fill it. Control side.
    #[must_use]
    pub fn losses(&self) -> Losses {
        let ports = self.ports();
        let live: Losses = ports.live.values().map(|p| p.losses()).sum();
        live + ports.retired
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
        let name = name.into();
        let mut ports = self.ports();
        let (input, node) = self.inlets.open();
        let id = input.id();
        let (port, handle) = HardwareMidiInput::new(name, input);
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
        match ports.live.remove(&id) {
            Some(port) => {
                ports.retired += port.losses();
                true
            }
            None => false,
        }
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

    /// The readers of the clock map of the port `id` names: its latest
    /// correction (signed, positive when a reading came late) and how many
    /// times it re-anchored. `None` for a stale id. Readable from any thread;
    /// they read what the port's last value was once it is retired.
    #[must_use]
    pub fn clock_readings(&self, id: InputId) -> Option<(Gauge<Seconds>, Counter<Resyncs>)> {
        self.port(id).map(|port| port.clock_readings())
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

    /// Sets whether the port `id` names delivers. Returns `false`, and changes
    /// nothing, when `id` names no live port.
    ///
    /// An inactive port discards what the driver pushes: nothing is queued
    /// while it is paused, so there is no burst when it is active again.
    pub fn set_port_active(&self, id: InputId, active: bool) -> bool {
        match self.port(id) {
            Some(port) => {
                port.set_active(active);
                true
            }
            None => false,
        }
    }

    /// Sets where the event times of the port `id` names come from:
    /// [`InputTiming::JitterReduction`] places its events by the JR Timestamps
    /// the device sends, [`InputTiming::Arrival`] by the OS's arrival stamp
    /// (the port's state until this is called). A [`MidiSession`](crate::MidiSession)
    /// sets it as an input's device settles; a port fed without a session sets
    /// it itself. `false` for a stale id.
    pub fn set_input_timing(&self, id: InputId, timing: InputTiming) -> bool {
        match self.port(id) {
            Some(port) => {
                port.set_timing(timing);
                true
            }
            None => false,
        }
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
