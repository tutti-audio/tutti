#![doc = include_str!("../README.md")]
//! MIDI **runtime state** for the Tutti audio engine: the graph nodes that carry
//! MIDI in, through and out of an audio graph, the lock-free rings MIDI crosses
//! threads on, and the engine's outbound clocks. `tutti::midi` names its
//! `MidiKeys` and `TimedMidiEvent` (feature `midi`).
//!
//! ## What this is
//!
//! Pure MIDI values live in `tutti-ump` and `tutti-midi-protocol`; OS ports live in
//! `tutti-midi-hardware`. This crate owns the runtime state that connects them:
//!
//! - **MIDI as graph nodes**, sending on `tutti-graph` event ports:
//!   [`MidiInputNode`] (a device's inlet in, one port per Group and channel), [`MidiClipNode`] and
//!   [`HarmonyNode`] (a timeline), [`SyncOut`] (Beat Clock, MTC and Machine Control). MIDI leaves the graph
//!   through an event tap (`tutti-graph`'s `EventTapReader`).
//! - [`ClockMap`]: the pure OS-time to graph-frame map (and back), built from
//!   readings of the OS `Instant` and the graph's `Frame`; it holds no clock.
//! - [`InputInlets`] and [`DeviceInput`]: one event inlet per hardware MIDI
//!   input. The device's thread places each arrival on a frame with a `ClockMap`
//!   fed the engine's block clock and pushes it into its inlet, so the audio thread
//!   converts no time; an input's input node follows its device through
//!   [`DeviceGeneration`].
//! - [`TempoFollower`]: what a transport that follows an outside tempo applies, as a pure
//!   value (a Group's Flex Set Tempo and Set Time Signature, M2-104 §7.5.3, §7.5.4); the
//!   session places the change.
//! - [`MidiKeys`]: the key helpers (`note_on`, `note_off`) over `tutti-graph`'s
//!   event inlet, which is how a control thread sends MIDI into the graph.
//! - [`SyncOut`] and [`JrStream`]: engine-produced MIDI *out* (the transport's Beat
//!   Clock, MTC, Machine Control and Flex Data, JR timestamps).
//! - [`MidiClockDecoder`] and [`MtcDecoder`]: the inbound half of
//!   sync (Beat Clock and MTC decoders), beside the master that sends them.
//! - [`BeatClockFollower`] and [`MtcFollower`]: what a transport that follows an outside
//!   Beat Clock or MTC does (tempo, play, stop, locate, lock), as pure state machines; the
//!   session places them.
//! - [`MpeTranslator`]: the inbound state (`live`) that rewrites classic-MPE channel
//!   spread into native MIDI 2.0 per-note messages, and [`MpeTransformNode`], the same
//!   as a node of one event stream.
//!
//! The MPE zone types (`MpeMode`, `MpeZoneConfig`, ...) are
//! `tutti-midi-protocol`'s; `MidiInputNode::mpe` takes a `MpeMode` from there.
//! So are the `SmpteFrameRate` and the `SmpteTimecode` the decoders assemble.
//!
//! ## What it does not own
//!
//! `tutti-ump` and `tutti-midi-protocol` are pure values with no state; this crate is state with **no
//! OS API**; `tutti-midi-hardware` is the OS edge. So a synth that plays a clip,
//! or an offline export, gets its MIDI without linking CoreMIDI or ALSA.
//!
//! - **No ports, and no `cfg(target_os)`.** Enumerating, opening and sending on a
//!   real endpoint belongs to `tutti-midi-hardware`.
//! - **No wire vocabulary.** `Ump`, `NoteId` and the MIDI 1↔2 scaling belong to
//!   `tutti-ump` and `tutti-midi-protocol` (MPE zone values included).
//! - **No files.** SMF and the path-level Clip File codec belong to
//!   `tutti-midi-file`.
//! - **No audio, and no ECS.** Nothing here touches a sample buffer.
//!
//! MPE lives here as an *input* transform, not as a per-synth state machine:
//! per M2-104, MPE is an input-edge concern, and synth voices track per-note
//! expression themselves. The transform is the one holder of the MPE zones and
//! the one receiver of the MPE Configuration Message: it also sends a Manager
//! Channel's zone-wide messages and its bend on each member channel of its zone,
//! so an instrument reads one channel at a time and holds no zone.
//! [`MidiInputNode`] runs it over a device's wire and [`MpeTransformNode`] over
//! any other event stream (a clip's).
//!
//! ## Routing is wiring
//!
//! A `MidiInputNode` sends each event out of the port of its Group and channel,
//! and a Group's System, SysEx and Flex messages out of that Group's channelless
//! port (`MidiInputPorts::channelless`). A Group is a channel space of its own
//! (M2-104 section 2.1.2, Appendix B.1), so each Group the node hears has seventeen
//! ports; the node hears the first Group unless built with
//! `MidiInputNode::hearing(MidiInputPorts::through(last))`, so a one-Group device is
//! seventeen ports and costs nothing more. A message with no Group
//! (Utility, UMP Stream) leaves the first Group's channelless port. Which node hears
//! which channel is which ports it takes event edges from: `MidiInputPorts::listening`
//! names them.
//!
//! ```rust
//! use tutti_graph::NodeKey;
//! use tutti_midi_runtime::MidiInputPorts;
//! use tutti_ump::{MidiChannel, MidiGroup};
//!
//! let input = NodeKey(1);
//! let ports = MidiInputPorts::FIRST_GROUP;
//! // A synth on channel 3 hears port 3 and the channelless port...
//! let on_three: Vec<_> = ports
//!     .listening(input, Some(MidiGroup::FIRST), Some(MidiChannel::of(3)))
//!     .collect();
//! let channel_three = ports.channel(input, MidiGroup::FIRST, MidiChannel::of(3));
//! let channelless = ports.channelless(input, MidiGroup::FIRST);
//! assert_eq!(on_three, [channel_three.unwrap(), channelless.unwrap()]);
//! // ...one on every channel hears all of them.
//! assert_eq!(ports.listening(input, None, None).count(), usize::from(ports.count()));
//! // Group 2's channel 3 is another port, on a node that hears two Groups.
//! let two = MidiInputPorts::through(MidiGroup::of(1));
//! let group2: Vec<_> = two
//!     .listening(input, Some(MidiGroup::of(1)), Some(MidiChannel::of(3)))
//!     .collect();
//! let channel = two.channel(input, MidiGroup::of(1), MidiChannel::of(3));
//! let channelless = two.channelless(input, MidiGroup::of(1));
//! assert_eq!(group2, [channel.unwrap(), channelless.unwrap()]);
//! ```
//!
//! A set of routing rules compiles to those port sets with `compile_routes`: each
//! rule's targets take its Group and channel's ports (several rules, the union), and an
//! optional fallback takes every port no rule covers.
//!
//! ## MIDI across a thread boundary
//!
//! Into the graph, a control thread pushes into an event inlet's writer
//! (`Graph::open_event_inlet`), held on the graph's clock until its frame, and
//! `MidiKeys` adds the key helpers. Nothing here allocates or locks on the audio
//! thread.
//!
//! ```rust
//! use tutti_graph::{EventInletNode, Stamp};
//! use tutti_midi_runtime::{LIVE_INLET_CAPACITY, MidiKeys};
//! use tutti_ump::{MidiChannel, MidiGroup, NoteNumber, NoteVelocity};
//!
//! let (_node, writer) = EventInletNode::pair(LIVE_INLET_CAPACITY);
//! let mut keys = MidiKeys::new(writer, MidiGroup::FIRST);
//! keys.note_on(Stamp::NextBlock, MidiChannel::FIRST, NoteNumber::of(60), NoteVelocity::new(0xC800))
//!     .unwrap();
//! ```
//!
//! ## Events are not lost silently
//!
//! An event port's capacity follows the block: [`CLIP_EVENT_DENSITY`],
//! [`HARMONY_EVENT_DENSITY`] and [`MIDI_INPUT_DENSITY`] are the events per frame a
//! node is sized for, and the graph multiplies by the block length. A port at
//! capacity refuses a new event, and the graph counts every refusal
//! (`Editor::take_event_losses`); a sync sender counts its own ([`SyncOutControls::losses`]).
//! A note-off has room past the capacity, and one the port still refuses is owed:
//! a clip's note ledger writes it at the first frame of a later block, before
//! anything else, so a note is never left sounding by a full port.
//!
//! ## MPE folded to native per-note messages
//!
//! `MpeTranslator` is the input edge: a member channel's bend is rewritten as a
//! *per-note* bend addressed at the note that channel holds, so no voice downstream
//! has to know what MPE is.
//!
//! ```rust
//! use tutti_midi_runtime::MpeTranslator;
//! use tutti_fixtures::midi::{self, lower_zone};
//! use tutti_graph::Offset;
//! use tutti_ump::{BendValue, Message, MidiChannel, NoteVelocity, U7, Voice2};
//!
//! let mut translator = MpeTranslator::new(lower_zone(7));
//!
//! // A note-on claims member channel 1; the map now resolves that channel.
//! // Each event becomes none, one, or (a bend range, a manager's pedal or bend)
//! // one per member channel, each handed to the closure.
//! let member = MidiChannel::of(1);
//! let mut sent = Vec::new();
//! let on = midi::note_on(member, 60, NoteVelocity::from(U7::of(100)).get());
//! translator.translate(&(Offset::ZERO, on), |e| sent.push(e));
//! assert_eq!(sent.len(), 1, "a note-on passes through");
//!
//! // A channel-wide bend on that member becomes a per-note bend on note 60.
//! let bend = midi::pitch_bend(member, 0xC000_0000);
//! sent.clear();
//! translator.translate(&(Offset::ZERO, bend), |e| sent.push(e));
//! let [(_, folded)] = sent[..] else { panic!("the channel holds a note") };
//! let Ok(Message::Midi2(_, Voice2::PerNotePitchBend { note, .. })) = folded.decode() else {
//!     panic!("a per-note bend")
//! };
//! assert_eq!(note.get(), 60);
//! ```
//!
//! ## Refusal is a value on the audio path
//!
//! What runs on or feeds the audio thread reports a refusal as a value: a full
//! mailbox drops and reports a count, and [`MpeTranslator::translate`] yields nothing for an
//! event it drops. A fallible control-side call returns its own error
//! ([`MpeFeaturesError`], [`OffsetOutOfRange`], [`NoTimeCode`]); the file codec's is
//! `tutti-midi-file`'s `ClipFileError`.
//!
//! ## Where it sits
//!
//! Builds on `tutti-ump`, `tutti-midi-protocol` and `tutti-graph`. Used by the
//! synth, SoundFont and plugin crates and by `tutti-midi-hardware`; re-exported by
//! `tutti`.
//!
//! ## Features
//!
//! None.
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod clock_map;
mod ledger;
mod live;
mod mpe_features;
mod mpe_zones;
mod outbound;
mod output_plan;
mod schedule;
mod sync;
mod taken;
mod tempo_follower;
#[cfg(test)]
mod test_support;
mod traits;

pub use clock_map::{ClockMap, ClockReading, GAIN, Observed, RESYNC};
pub use live::{
    ArrivalClock, DeviceGeneration, DeviceInput, DeviceWriters, GenerationWriter, InputId,
    InputInlets, InputReaders, InputWriters, LIVE_INLET_CAPACITY, MIDI_INPUT_DENSITY,
    MIDI_INPUT_EVENT_CAPACITY, MidiInputControls, MidiInputNode, MidiInputPorts, MidiKeys,
    MpeTransformControls, MpeTransformNode, MpeTranslator, RouteRule, SendUniversalError,
    compile_routes,
};
pub use mpe_features::{MpeFeaturesError, ask_mpe_features, mpe_features};
pub use mpe_zones::mpe_zones;
pub use outbound::{
    ACTIVE_SENSING_INTERVAL, ActiveSensingSender, JrClock, JrStream, MIDI_OUT_EVENT_CAPACITY,
    MachineControlSender, MidiOutNode, NoTimeCode, SYNC_OUT_EVENT_CAPACITY, SyncMessages, SyncOut,
    SyncOutControls,
};
pub use output_plan::{
    MAX_OUTPUT_AHEAD, MAX_OUTPUT_LATENCY, OffsetOutOfRange, Outgoing, OutputLatency, OutputOffset,
    OutputPlan, OutputTerms, Placement, SendAt,
};
pub use schedule::{
    CLIP_EVENT_DENSITY, ClipEvents, ClipEventsKind, HARMONY_EVENT_DENSITY, HarmonyChanges,
    HarmonyChangesError, HarmonyChangesKind, HarmonyControls, HarmonyNode, MidiClipControls,
    MidiClipNode, TimedHarmony, TimedMidiEvent,
};
pub use sync::{
    BeatClockEvent, BeatClockFollower, ClockTransportState, MachineControlEvent,
    MachineControlFollower, MidiClockDecoder, MtcCycle, MtcDecoder, MtcDirection, MtcEvent,
    MtcFollower, SegmentFault,
};
pub use taken::Taken;
pub use tempo_follower::{Followed, TempoFollower};
pub use traits::MidiOut;
// The counts a device's inlet keeps, as `DeviceInput::counts` returns them.
pub use tutti_graph::InletCounts;
