//! MIDI the engine *produces*, as opposed to plumbing it moves.
//!
//! - [`SyncOut`] sends the transport to other gear: Beat Clock and MTC from
//!   its `ClockMaster`, and Machine Control from a [`MachineControlSender`],
//!   as one graph node out of an event port; what it sends is one
//!   [`SyncMessages`] setting.
//! - [`MidiOutNode`] is the graph's end of a device output: what is wired to it
//!   is what the session's output port sends.
//! - [`ActiveSensingSender`] schedules Active Sensing for an output that goes
//!   quiet.
//! - [`jr_timestamp`] stamps outbound packets with JR Timestamps (M2-104 §7.6),
//!   so a receiver can reconstruct the sender's timing rather than inherit the
//!   transport's jitter.

pub(crate) mod active_sensing;
mod clock_master;
pub(crate) mod jr_timestamp;
pub(crate) mod machine_control_out;
pub(crate) mod out_node;
pub(crate) mod sync_messages;
pub(crate) mod sync_out;

pub use active_sensing::{ACTIVE_SENSING_INTERVAL, ActiveSensingSender};
pub use jr_timestamp::{JrClock, JrStream};
pub use machine_control_out::{MachineControlSender, NoTimeCode};
pub use out_node::{MIDI_OUT_EVENT_CAPACITY, MidiOutNode};
pub use sync_messages::SyncMessages;
pub use sync_out::{SYNC_OUT_EVENT_CAPACITY, SyncOut, SyncOutConfig, SyncOutControls};
