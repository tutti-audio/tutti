//! MIDI crossing the graph's edge into it, from a wire or a control thread (out
//! of it is an event tap, `tutti-graph`'s). Inside the graph MIDI travels on event
//! ports; these are the nodes at its border.
//!
//! - [`MidiInputNode`] reads the event inlets of the hardware devices
//!   ([`InputInlets`]), translates, and sends each event out of its channel's
//!   port: routing by channel is which ports a listener is wired to.
//! - [`MpeTransformNode`] is the same transform as a node of one event stream
//!   (a clip's), the one holder of the zones there.
//! - [`MidiKeys`] is the key helpers over `tutti-graph`'s event inlet, which
//!   is how a control thread sends into the graph (a keyboard, a preview).
//! - [`MpeTranslator`] is the inbound state that rewrites classic-MPE channel-spread
//!   into native MIDI-2 per-note messages ([`MidiInputNode`] is its caller).
//! - [`compile_routes`] turns inbound routing rules into which of the input
//!   node's ports each listener takes.

pub(crate) mod device_generation;
pub(crate) mod input_inlets;
pub(crate) mod input_node;
pub(crate) mod keys;
pub(crate) mod mpe_translator;
pub(crate) mod order;
mod route;
mod sensing;
pub(crate) mod transform_node;

pub use device_generation::{DeviceGeneration, GenerationWriter};
pub use input_inlets::{
    ArrivalClock, DeviceInput, DeviceWriters, InputId, InputInlets, InputReaders, InputWriters,
};
pub use input_node::{
    MIDI_INPUT_DENSITY, MIDI_INPUT_EVENT_CAPACITY, MidiInputControls, MidiInputNode, MidiInputPorts,
};
pub use keys::{LIVE_INLET_CAPACITY, MidiKeys};
pub use mpe_translator::MpeTranslator;
pub use route::{RouteRule, compile_routes};
pub use transform_node::{MpeTransformControls, MpeTransformNode};
