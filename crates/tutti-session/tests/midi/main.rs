//! MIDI through the session: clips, routes, held keys, the outputs and the SoundFont unit.

/// Inert outside `assert_no_alloc`, so every test here may also gate a closure on
/// allocating nothing.
#[global_allocator]
static A: tutti_fixtures::RtAllocCheck = tutti_fixtures::RtAllocCheck;

// A held key across a retarget and a re-route.
#[cfg(feature = "midi")]
mod common;
#[cfg(feature = "midi")]
mod held_keys;
#[cfg(feature = "midi-hardware")]
#[path = "../support/tempo.rs"]
mod tempo;
// Clips, live inputs, routes and the outputs (`Session::midi`).
#[cfg(feature = "midi")]
mod midi;
// A request to a MIDI-CI peer is a future: a Profile switch, a Message Report.
#[cfg(feature = "midi-hardware")]
mod midi_ci_requests;
// Property Exchange with a MIDI-CI peer as futures: a Get and a Set of a listed Resource.
#[cfg(feature = "midi-hardware")]
mod midi_ci_property;
// A build that fails after the session's executor exists leaves nothing of it.
#[cfg(feature = "midi-hardware")]
mod build_failure;
#[cfg(all(feature = "midi-hardware", target_os = "linux"))]
mod build_failure_threads;
// MIDI ports: absent, replugged, ambiguous, released, per-port input node.
#[cfg(feature = "midi-hardware")]
mod midi_ports;
// A MIDI input handle reads the timing of its clock map and its resyncs.
#[cfg(feature = "midi-hardware")]
mod midi_input_timing;
// A MIDI input handle reads what its port lost: the inlet, across a replug.
#[cfg(feature = "midi-hardware")]
mod midi_input_losses;
// Who the session is on the wire (`MidiPolicy`): its Endpoint and MIDI-CI identity.
#[cfg(feature = "midi-hardware")]
mod midi_policy;
// Route rules: the union of two on one target, and a disabled one.
#[cfg(feature = "midi-hardware")]
mod midi_routes;
// A clip's velocity keeps its 16 bits to the event input.
#[cfg(feature = "midi")]
mod midi_velocity;
// MIDI into a SoundFont unit, rendered: a clip on its beat, a live preview
// beside it, a routed hardware note.
#[cfg(feature = "soundfont")]
mod midi_soundfont;
// The General MIDI setup of a SoundFont unit (`Session::add_general_midi`).
#[cfg(feature = "soundfont")]
mod general_midi;
// A sink as an output node read through an event tap.
#[cfg(feature = "midi")]
mod midi_out_tap;
// The session's MIDI front: hardware through the view, the session's MPE mode,
// a recorded take and its placement, a saved route rule.
#[cfg(feature = "midi")]
mod midi_front;
// A clip's events and a harmony node's changes as values: written, equal, removed.
#[cfg(feature = "midi")]
mod midi_value_targets;
// File loads as jobs: a MIDI file and a SoundFont land as events in `update()`.
#[cfg(feature = "midi")]
mod loads;
// The session follows a live Flex Set Tempo and Set Time Signature.
#[cfg(feature = "midi-hardware")]
mod midi_follow;
// A map published with its step behind the playhead moves no beat.
#[cfg(feature = "midi")]
mod follow_late_publish;
// The session follows a MIDI Beat Clock or MIDI Time Code from the input.
#[cfg(feature = "midi-hardware")]
mod midi_clock_follow;
// The session follows MIDI Machine Control from the input and sends it from the transport.
#[cfg(feature = "midi-hardware")]
mod midi_machine_control;
// A sink on the system clock: no device, the timer's blocks.
#[cfg(feature = "midi")]
mod system_clock;
// A session's input and output ports render a block without allocating.
#[cfg(feature = "midi-hardware")]
mod ports_no_alloc;
