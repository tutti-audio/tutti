//! MIDI that arrives from a position on a timeline, rather than off a wire:
//! event source nodes walking each block's transport (the shared walk,
//! `walk.rs`).
//!
//! - [`MidiClipNode`] plays a clip's [`TimedMidiEvent`]s out of an event port.
//! - [`HarmonyNode`] sends a sequencer's chord and scale lanes.

pub(crate) mod clip_node;
pub(crate) mod harmony_node;
pub(crate) mod timed;
mod walk;

pub use clip_node::{
    CLIP_EVENT_DENSITY, ClipEvents, ClipEventsKind, ClipNote, MidiClipControls, MidiClipNode,
};
pub use harmony_node::{
    HARMONY_EVENT_DENSITY, HarmonyChanges, HarmonyChangesError, HarmonyChangesKind,
    HarmonyControls, HarmonyNode, TimedHarmony,
};
pub use timed::TimedMidiEvent;
