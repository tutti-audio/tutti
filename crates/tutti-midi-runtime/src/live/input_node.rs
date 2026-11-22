//! [`MidiInputNode`]: the graph side of the MIDI input edge, its events out of
//! one event port per Group and channel.
//!
//! A hardware device reaches the graph through an event inlet of its own
//! ([`InputInlets`](crate::InputInlets)), each event already on its frame
//! (the device's thread stamps it, `tutti-graph`'s event inlet holds it until
//! its block); the node has one event input, so what it reads is what the
//! inlet emitted. Each block it translates the device's events,
//! MIDI 1.0 to MIDI 2.0 ([`Midi2Translator`]: RPN and NRPN runs, Bank Select,
//! translating), and rewrites classic MPE into native per-note messages
//! ([`MpeTranslator`]), then sends every event out of the port for its address
//! ([`MidiInputPorts`]). **The translator and the MPE state are the
//! device's own**: a device that replaces another starts from none of the
//! one before's half-sent runs. A Group is a channel space of its own
//! (M2-104 section 2.1.2, Appendix B.1: a message in one Group does not apply
//! to Channels in another), so a Group has its own sixteen channel ports and
//! its own channelless port, which carries that Group's System, SysEx and Flex
//! messages. The node hears the first Group unless built to hear more
//! ([`MidiInputNode::hearing`]), so a one-Group device has [`MIDI_INPUT_PORTS`]
//! ports. A message with no Group (Utility, UMP Stream: they address
//! the Endpoint, not a Group) leaves the first Group's channelless port.
//! **Routing is wiring**: a synth listening to channel 3 of a Group takes event
//! edges from that Group's port 3 and its channelless port, one listening to a
//! whole Group from its seventeen ([`MidiInputPorts::listening`]); the nodes of
//! several devices merge in the listener's event fan-in.
//!
//! Translation runs on every event, wired or not: both stages are stateful
//! assemblers, and skipping an event corrupts the ones after it.
//!
//! **A source that ends leaves nothing held downstream.** The node keeps what
//! its device started and has not ended ([`Held`]: the keys down, the hold
//! pedals down and the bends off centre, as the device sent them), and
//! releases it as the messages that end it (Note Offs, then pedal-ups and
//! centred bends; `Held`'s docs say why not All Notes Off) through the
//! same translation as any other message, in three cases:
//!
//! - *The device changed or went.* Built [`following`](MidiInputNode::following)
//!   the [`DeviceGeneration`] that tracks its input's device, the node compares the
//!   generation (which the control side moves as each device arrives and as it
//!   departs) with the one it last saw every block. A change releases
//!   everything the old device held, at the block's first frame, then starts
//!   empty, translator and MPE state, so one device's half-sent run is not
//!   completed by the next; and an input whose device went hears nothing until
//!   another takes it. (The executor's edge close, `tutti-graph`'s
//!   `held.rs`, ends the same notes, pedals and bends when the device's inlet
//!   is taken out of the graph; this ends them as soon as the device lets go.)
//! - *Active Sensing lapsed* ([`ActiveSensing`], MIDI 1.0 Detailed
//!   Specification p. 32). Once a Group has sent an Active Sensing (FE), a
//!   message is expected from it at least every 330 ms; when none comes, the
//!   Group's held notes and controllers are released at the frame the time ran
//!   out. The watch counts the graph's frames, which are the arrival clock the
//!   inlets placed each event on, so the node reads no OS clock.
//! - *System Reset* (FF, p. 33). The Group's held state is released, its
//!   translator state and MPE state are reset, and its watch is dropped: voices
//!   off, controllers reset, running status cleared, back to the power-up
//!   condition.
//!
//! The three share one rule: a release the block has no room for stays held
//! and is owed, and goes out at the next block's first frame. A lapse releases
//! the Group's held state and leaves the translator and MPE state alone (the
//! specification asks only that voices go quiet); an engine stall does not
//! lapse a cable: what arrived during it is placed late, at the first frame of
//! the block that finds it, and clears the timer there. No input path keeps a byte parser (ALSA and
//! CoreMIDI deliver UMP), so no running status or SysEx reassembly is stranded
//! at this edge; Set Local On, Omni On or Poly, Song Position 0 and Stop belong
//! to the transport and the synth, and nothing here holds them.
//!
//! The FE and the FF are the cable's own: the node acts on them and sends
//! neither on (an FF is never echoed, p. 33).
//!
//! Its fork is silent: an offline render reads no wire.

use tutti_buffers::Float;
use tutti_graph::UmpAt;
use tutti_units::Losses;

use std::num::NonZeroU32;
use std::sync::Arc;
use tutti_graph::{
    Cx, Event, EventDensity, EventKind, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io,
    Node, NodeParts, Offset, Prepare, Shape, Status,
};
use tutti_midi_protocol::Midi2Translator;
use tutti_midi_protocol::{MpeMode, MpeZones};
use tutti_rt::{Counter, CounterWriter, Mutex, Publisher, Reader, counter};
use tutti_ump::{Held, Message, MidiChannel, MidiGroup, System, Ump};
use tutti_units::{Frame, SampleRate, Samples};

use super::mpe_translator::MpeTranslator;
use super::order::{BLANK, in_offset_order, offset_of};
use super::sensing::ActiveSensing;
use crate::live::device_generation::DeviceGeneration;

/// The port that carries a Group's events with no channel: after its sixteen
/// channel ports. For the first Group (and a node that hears only it); Group
/// `g`'s is [`MidiInputPorts::channelless`].
pub const CHANNELLESS_PORT: u16 = MidiChannel::COUNT as u16;

/// How many event outputs a [`MidiInputNode`] that hears one Group has: one
/// per channel and [`CHANNELLESS_PORT`]. Each further Group adds as many
/// ([`MidiInputPorts::count`]).
pub const MIDI_INPUT_PORTS: u16 = CHANNELLESS_PORT + 1;

/// The fewest events a [`MidiInputNode`] translates in one block, and each port's
/// declared event capacity at the shortest blocks.
pub const MIDI_INPUT_EVENT_CAPACITY: usize = 512;

/// [`MIDI_INPUT_EVENT_CAPACITY`] as a declared port capacity.
pub(super) const MIDI_INPUT_PORT_CAPACITY: NonZeroU32 =
    NonZeroU32::new(MIDI_INPUT_EVENT_CAPACITY as u32).unwrap();

/// The events per frame a [`MidiInputNode`] is sized for: one in eight
/// (6,000 a second at 48 kHz, where a 16-channel MPE controller sending a
/// bend, a pressure and a slide every 8 ms on each channel is 6,000), which
/// with its block is the node's poll and port capacity when that is more than
/// [`MIDI_INPUT_EVENT_CAPACITY`] (blocks past 4,096 frames). A hardware wire
/// carries far less than that, so this is headroom, not a forecast. The cost
/// is on each of the 17 ports the capacity and its note-off room at 32 bytes
/// an event: 0.3 MB in all at 512 events, 1.3 MB at 2,048 (a 16,384-frame
/// block); and for each port a PDC delay is on, 72 bytes an entry of `3.75 ×`
/// that (0.16 MB at 512, 0.62 MB at 2,048).
pub const MIDI_INPUT_DENSITY: EventDensity = EventDensity::per_kiloframe(128);

/// Which output port of a [`MidiInputNode`] carries what: the layout of the
/// Groups it hears.
///
/// Group `g` owns the ports `g * 17 ..= g * 17 + 16`: channel `c`'s is
/// `g * 17 + c`, and the seventeenth ([`channelless`](Self::channelless)) is
/// the Group's messages that have no channel. The Groups a node hears are the
/// first ones, `0` through the last it was built for, so the first Group's
/// ports are the same whatever follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MidiInputPorts {
    /// How many Groups, from the first: 1 to 16.
    groups: tutti_ump::GroupSpan,
}

impl MidiInputPorts {
    /// The ports of a node that hears the first Group alone: the default, and
    /// [`MIDI_INPUT_PORTS`] of them.
    pub const FIRST_GROUP: Self = Self::through(MidiGroup::FIRST);

    /// The ports of a node that hears every Group up to and including `last`.
    #[must_use]
    pub const fn through(last: MidiGroup) -> Self {
        Self {
            groups: tutti_ump::GroupSpan::of(last.get() + 1),
        }
    }

    /// The Groups the node hears, from the first.
    #[must_use]
    pub const fn groups(self) -> tutti_ump::GroupSpan {
        self.groups
    }

    /// How many event outputs the node has.
    #[must_use]
    pub const fn count(self) -> u16 {
        self.groups.get() as u16 * PORTS_PER_GROUP
    }

    /// The last Group the node hears.
    #[must_use]
    pub(crate) const fn last_group(self) -> MidiGroup {
        MidiGroup::masked(self.groups.get() - 1)
    }

    /// The port of `group`'s events with no channel (System, SysEx, Flex).
    /// `None` for a Group the node does not hear.
    #[must_use]
    pub const fn channelless(self, group: MidiGroup) -> Option<u16> {
        if group.get() < self.groups.get() {
            Some(group.get() as u16 * PORTS_PER_GROUP + CHANNELLESS_PORT)
        } else {
            None
        }
    }

    /// The port channel `channel` of `group` leaves from. `None` for a Group
    /// the node does not hear.
    #[must_use]
    pub const fn channel(self, group: MidiGroup, channel: MidiChannel) -> Option<u16> {
        if group.get() < self.groups.get() {
            Some(group.get() as u16 * PORTS_PER_GROUP + channel.get() as u16)
        } else {
            None
        }
    }

    /// The port an event leaves from: its channel's in its Group, a Group's
    /// channelless port for a message that has a Group but no channel, and the
    /// first Group's channelless port for one with no Group. `None` for an
    /// event of a Group the node does not hear: it was addressed to a
    /// Group nothing here listens to, so it is not delivered.
    #[must_use]
    pub fn port_of(self, event: &Ump) -> Option<u16> {
        match (event.address(), event.group()) {
            (Some(a), _) => self.channel(a.group, a.channel),
            (None, Some(group)) => self.channelless(group),
            (None, None) => self.channelless(MidiGroup::FIRST),
        }
    }

    /// The ports a listener takes edges from: the ports of `channel` in
    /// `group`, and that Group's channelless port. `None` for `channel` is
    /// every channel of the Group, and `None` for `group` is every Group the
    /// node hears (the first Group's channelless port then also carries
    /// what has no Group). A Group the node does not hear has no ports.
    pub fn listening(
        self,
        group: Option<MidiGroup>,
        channel: Option<MidiChannel>,
    ) -> impl Iterator<Item = u16> {
        let (from, to) = match group {
            Some(g) => (g.get(), g.get() + 1),
            None => (0, self.groups.get()),
        };
        (from..to.min(self.groups.get())).flat_map(move |g| {
            let base = u16::from(g) * PORTS_PER_GROUP;
            let (a, b) = match channel {
                Some(c) => (u16::from(c.get()), u16::from(c.get()) + 1),
                None => (0, CHANNELLESS_PORT),
            };
            (base + a..base + b).chain(std::iter::once(base + CHANNELLESS_PORT))
        })
    }
}

impl Default for MidiInputPorts {
    fn default() -> Self {
        Self::FIRST_GROUP
    }
}

/// A Group's ports: its sixteen channels and its channelless port.
const PORTS_PER_GROUP: u16 = MIDI_INPUT_PORTS;

/// The MIDI input edge as a graph node: one event input, for one device, and
/// [`MidiInputPorts::count`] event outputs ([`MIDI_INPUT_PORTS`] for the first
/// Group alone). See the module docs.
///
/// ```
/// use tutti_buffers::MaxBlock;
/// use tutti_units::{SampleRate, Samples};
/// use tutti_graph::NodeKey;
/// use tutti_graph::{Editor, Prepare};
/// use tutti_midi_protocol::MpeMode;
/// use tutti_midi_runtime::MidiInputNode;
/// use tutti_rt::Publisher;
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples::new(512))));
/// // The inlet is not wired yet.
/// let mode = Publisher::new(MpeMode::Disabled);
/// let node = MidiInputNode::new().mpe(&mode);
/// let controls = editor.insert(NodeKey(1), "midi in", node).controls;
/// controls.clear_mpe_profiles();
/// ```
pub struct MidiInputNode {
    /// The device's translation state.
    device: Device,
    /// The device's generation ([`MidiInputNode::following`]); `None` for a node
    /// that follows none, whose device is always present and never restarts.
    generation: Option<DeviceGeneration>,
    /// What the block sends: the translated events, which may outnumber the
    /// ones read (a member channel's bend range fans out over its zone).
    out: Box<[UmpAt]>,
    /// The Groups it hears, and so its ports.
    ports: MidiInputPorts,
    /// The session's MPE mode, read once per block; `None` for a node built
    /// over none, whose mode is [`MpeMode::Disabled`].
    mpe_mode: Option<Reader<MpeMode>>,
    /// What a host asked for, read once per block.
    mpe_request: Reader<Requests>,
    /// The cell `mpe_request` reads, which the controls publish to; the node
    /// never touches it.
    mpe_request_cell: Publisher<Requests>,
    /// Shared with the controls: serializes their edits of the requests.
    request_serial: Arc<Mutex<()>>,
    /// The Profile zones in force on the device's Groups: what the requests
    /// last read set.
    profiles: GroupProfiles,
    /// The publish number of the requests `profiles` was read from (0 for
    /// the cell's first value, which holds none).
    profiles_seq: u64,
    /// Translated events a block had no room for in `out`
    /// ([`MidiInputControls::losses`]).
    dropped: CounterWriter<Losses>,
    /// The reading end of `dropped`, given to the controls.
    dropped_reader: Counter<Losses>,
    /// The events a block translates and each port holds, from the block the
    /// node was prepared for.
    capacity: NonZeroU32,
}

/// The MPE Profile zones of each of a device's Groups.
type GroupProfiles = [Option<MpeZones>; MidiGroup::COUNT as usize];

/// What the control side asks of the node, published whole.
#[derive(Clone, Debug)]
struct Requests {
    /// The MPE Profile zones the device enabled.
    profiles: GroupProfiles,
}

impl Requests {
    fn new() -> Self {
        Self {
            profiles: NO_PROFILES,
        }
    }
}

const NO_PROFILES: GroupProfiles = [None; MidiGroup::COUNT as usize];

/// What the device keeps between blocks: its MIDI 1.0 assembler and its
/// classic-MPE state.
struct Device {
    translator: Midi2Translator,
    mpe: MpeTranslator,
    /// The MPE mode in force.
    mode: MpeMode,
    /// The generation this state was last reset at.
    generation: u64,
    /// Whether a device is there: an event of none is not read. Always for a
    /// node that follows no generation.
    present: bool,
    /// What the device started and has not ended, as it sent it.
    held: Held,
    /// The device's Groups that have sent an Active Sensing.
    sensing: ActiveSensing,
    /// The Groups (bit `g` for Group `g`) whose release did not fit the
    /// block's room: tried again at the next block's first frame.
    owing: u16,
}

impl Device {
    fn new(mode: MpeMode) -> Self {
        Self {
            translator: Midi2Translator::new(),
            mpe: MpeTranslator::new(mode),
            mode,
            generation: 0,
            present: true,
            held: Held::new(),
            // Built again for the real rate in `prepare`.
            sensing: ActiveSensing::at_rate(SampleRate::SR_48K),
            owing: 0,
        }
    }

    /// Adopt a requested MPE mode that differs from the one in force; `true`
    /// when it did, and the block then sends the new zones' default bend
    /// ranges first ([`MpeTranslator::zone_defaults`]: what an MPE Configuration
    /// Message resets a receiver to, as this node sends an MCM's).
    ///
    /// **The comparison is load-bearing**: the session's cell keeps the mode
    /// it was last given, so every block reads it, and `set_mode` rebuilds the
    /// MPE translator: re-adopting it every block would drop every sounding note's
    /// voice mapping, and re-send the ranges, every block.
    fn adopt(&mut self, requested: Option<MpeMode>) -> bool {
        let Some(mode) = requested else {
            return false;
        };
        if self.mode == mode {
            return false;
        }
        self.mode = mode;
        self.mpe.set_mode(mode);
        true
    }

    /// Empty, as for a device that has not sent anything. What the old device
    /// still holds is not forgotten: it was released first, and what did not
    /// fit is owed.
    fn restart(&mut self, generation: u64) {
        self.translator.reset();
        self.mpe = MpeTranslator::new(self.mode);
        self.sensing.forget_all();
        self.generation = generation;
        // A device is there while its generation is odd.
        self.present = generation % 2 == 1;
    }

    /// Releases what `group` (every Group for `None`) held, at `at`, through
    /// the translation as the device's own messages would go, into `staging`.
    /// What the block has no room for stays held and owed.
    fn release(&mut self, group: Option<MidiGroup>, at: Offset, staging: &mut Staging<'_>) {
        let Self {
            translator,
            mpe,
            held,
            owing,
            ..
        } = self;
        let all = held.release(group, |raw| {
            let Some(ev) = translator.translate(raw) else {
                return true;
            };
            staging.whole(ev.is_note_off(), |send| mpe.translate(&(at, ev), send))
        });
        let mask = group.map_or(u16::MAX, |g| 1 << g.get());
        if all {
            *owing &= !mask;
        } else {
            *owing |= mask;
        }
    }

    /// Ends what each Group that ran out of Active Sensing time by `now` held
    /// (see the module docs), at the frame it ran out, in the block that
    /// starts at `first` and is `block_len` long.
    fn lapse(&mut self, now: Frame, first: Frame, block_len: Samples, staging: &mut Staging<'_>) {
        while let Some((group, ran_out)) = self.sensing.next_lapse(now) {
            let at = ran_out.since(first).unwrap_or(Samples::ZERO);
            let at = Offset::new(at, block_len).unwrap_or(Offset::last(block_len));
            self.release(Some(group), at, staging);
        }
    }

    /// A System Reset on `group`'s cable at `at` (see the module docs).
    fn reset_group(&mut self, group: MidiGroup, at: Offset, staging: &mut Staging<'_>) {
        self.release(Some(group), at, staging);
        self.translator.reset_group(group);
        self.mpe.reset_group(group);
        self.sensing.forget(group);
    }

    /// The release of every Group that owes one.
    fn pay(&mut self, staging: &mut Staging<'_>) {
        for group in MidiGroup::all() {
            if self.owing & (1 << group.get()) != 0 {
                self.release(Some(group), Offset::ZERO, staging);
            }
        }
    }
}

impl std::fmt::Debug for MidiInputNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiInputNode")
            .field("mpe_mode", &self.device.mode)
            .field("mpe_request", &self.mpe_request)
            .field("profiles", &self.profiles)
            .field("dropped", &self.dropped_reader)
            .finish_non_exhaustive()
    }
}

impl Default for MidiInputNode {
    fn default() -> Self {
        Self::new()
    }
}

impl MidiInputNode {
    /// A node with its one event input, with MPE translation off.
    #[must_use]
    pub fn new() -> Self {
        let cell = Publisher::new(Requests::new());
        let (dropped, dropped_reader) = counter();
        Self {
            mpe_mode: None,
            mpe_request: cell.reader(),
            request_serial: Arc::default(),
            mpe_request_cell: cell,
            device: Device::new(MpeMode::Disabled),
            profiles: NO_PROFILES,
            profiles_seq: 0,
            generation: None,
            out: vec![BLANK; tutti_graph::note_off_room(MIDI_INPUT_EVENT_CAPACITY)]
                .into_boxed_slice(),
            ports: MidiInputPorts::FIRST_GROUP,
            dropped,
            dropped_reader,
            capacity: MIDI_INPUT_PORT_CAPACITY,
        }
    }

    /// Follows the device `generation` says is behind the node's event input: each
    /// block the node reads the generation (an acquire load of a cell the control
    /// side stores to when a device comes or goes), and when it changed it
    /// releases what the device held, starts over, and hears nothing while no
    /// device is there. Without it a device is there from the first block. The
    /// node starts at the generation stands at now, whatever it is, so a node
    /// built before any device has arrived treats its device as present until
    /// the first change (it has sent nothing to release), and one built while a
    /// device is there releases nothing at its first block.
    #[must_use]
    pub fn following(mut self, generation: &DeviceGeneration) -> Self {
        self.device.generation = generation.generation();
        self.generation = Some(generation.clone());
        self
    }

    /// Hear the Groups of `ports` (the first Group alone by default): each
    /// has its own channel ports and channelless port, and an event of a Group
    /// past them is not delivered ([`MidiInputPorts::port_of`]). The node's
    /// event outputs are [`MidiInputPorts::count`].
    #[must_use]
    pub fn hearing(mut self, ports: MidiInputPorts) -> Self {
        self.ports = ports;
        self
    }

    /// Translate classic MPE in the mode `mode` holds from the first block, on
    /// every Group, and follow it: a publish to `mode` is adopted at the next
    /// block. The reader is registered here, so build the node on the control
    /// side.
    ///
    /// A new mode stops and resets the channels that enter or leave MPE
    /// control (M1-100 section 2.2.3: All Sound Off and Reset All Controllers,
    /// sent ahead of the new zones' default bend ranges); the others keep
    /// their notes. Publishing the mode in force changes nothing.
    #[must_use]
    pub fn mpe(mut self, mode: &Publisher<MpeMode>) -> Self {
        let initial = *mode.current();
        self.device.mode = initial;
        self.device.mpe = MpeTranslator::new(initial);
        self.mpe_mode = Some(mode.reader());
        self
    }
}

/// The host's handle on a [`MidiInputNode`]: the MPE Profile zones of its
/// device. Cheap to clone.
#[derive(Clone, Debug)]
pub struct MidiInputControls {
    /// What the node reads; the controls publish to it and read it back with
    /// `Publisher::current`.
    request: Publisher<Requests>,
    /// Serializes a read-modify-publish of `request` (`Publisher` has no
    /// compare-and-publish), so two control threads cannot lose an edit.
    serial: Arc<Mutex<()>>,
    dropped: Counter<Losses>,
}

impl MidiInputControls {
    /// The MIDI-CI MPE Profile zones (M2-120) the device enabled on `group`: from the next block, what the device sends on
    /// that Group follows the zones' rules instead of the mode's, until `zones`
    /// is `None` ([`MpeTranslator::set_profile`] says what the change stops and
    /// resets). Control thread. Setting what is set changes nothing.
    ///
    /// The zones are the device's alone: the session's mode is not changed. A device that
    /// takes the input starts with none, but the control side's record stays
    /// until it is set: clear it when the device goes
    /// ([`clear_mpe_profiles`](Self::clear_mpe_profiles)).
    pub fn set_mpe_profile(&self, group: MidiGroup, zones: Option<MpeZones>) {
        self.edit_profiles(|held| {
            let changed = held[group.index()] != zones;
            held[group.index()] = zones;
            changed
        });
    }

    /// [`set_mpe_profile`](Self::set_mpe_profile) with none, on every Group.
    pub fn clear_mpe_profiles(&self) {
        self.edit_profiles(|held| {
            let changed = held.iter().any(Option::is_some);
            *held = [None; MidiGroup::COUNT as usize];
            changed
        });
    }

    /// Publishes the requests with `edit` applied to the device's zones, when it
    /// says it changed them.
    fn edit_profiles(&self, edit: impl FnOnce(&mut GroupProfiles) -> bool) {
        let _serial = self.serial.lock();
        let mut request = (*self.request.current()).clone();
        if edit(&mut request.profiles) {
            self.request.publish(request);
        }
    }

    /// What the node has lost since it was built, in events: all of it
    /// `refused`, the translated events that found no room past the node's
    /// capacity in one block. An event's
    /// translation (a bend range or an MPE reset fans out over a zone) is
    /// dropped whole rather than cut partway. What a device's inlet could not
    /// hold is counted at the inlet, not here
    /// ([`InputInlets`](crate::InputInlets)).
    pub fn losses(&self) -> Losses {
        self.dropped.get()
    }
}

impl<F: Float> Node<F> for MidiInputNode {
    /// No audio; one event input per attached input, [`MidiInputPorts::count`]
    /// event outputs ([`MIDI_INPUT_PORTS`] for the first Group alone) of the
    /// prepared capacity ([`MIDI_INPUT_EVENT_CAPACITY`] at least, and
    /// [`MIDI_INPUT_DENSITY`] per frame of a long block).
    fn shape(&self) -> Shape {
        Shape::EMPTY
            .with_events(1, self.ports.count())
            .with_event_capacity(self.capacity)
            // Event-only: a translation per event, nothing per frame.
            .with_cost(tutti_graph::Cost::Light)
    }

    fn prepare(&mut self, p: &Prepare) {
        // The limit is frames of this rate; a watch counted at another is
        // dropped, and the device's next Active Sensing starts it again.
        self.device.sensing = ActiveSensing::at_rate(p.sample_rate());
        let capacity = MIDI_INPUT_DENSITY.capacity(p).max(MIDI_INPUT_PORT_CAPACITY);
        if capacity != self.capacity {
            self.capacity = capacity;
            self.out =
                vec![BLANK; tutti_graph::note_off_room(capacity.get() as usize)].into_boxed_slice();
        }
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_, F>) -> Status {
        // One lend, before any event is translated, stated or not: the mode
        // and the Profile zones are read from the same requests.
        let requests = self.mpe_request.lend();
        let mode = self.mpe_mode.as_mut().map(|reader| *reader.lend());
        let reset = self.device.adopt(mode);
        let block_len = cx.env.block_len;
        let first = cx.env.frame;
        // An event's translation goes in whole or not at all: a fan-out cut
        // partway would leave some of a zone's channels at another range.
        // A note-off may use the room past the capacity (the port's own
        // `note_off_room`), so a block full of other messages (a bend range
        // fanned out over a zone) cannot keep it out.
        let mut staging = Staging::new(&mut self.out, self.capacity.get() as usize);
        // A host-set mode's ranges, at frame 0, ahead of the block's events
        // (the sort below is stable): the receiver's, not the device's.
        if reset {
            for group in 0..self.ports.last_group().get() + 1 {
                staging.whole(false, |send| {
                    self.device
                        .mpe
                        .zone_defaults(MidiGroup::masked(group), Offset::ZERO, send);
                });
            }
        }
        let device = &mut self.device;
        // The Profile zones a host set, at frame 0 too: the device's change
        // stops and resets the channels it moves, and starts the zones at their
        // bend ranges.
        if requests.seq() != self.profiles_seq {
            self.profiles_seq = requests.seq();
            for group in MidiGroup::all() {
                let want = requests.profiles[group.index()];
                let held = &mut self.profiles[group.index()];
                if want != *held {
                    *held = want;
                    staging.whole(false, |send| {
                        device.mpe.set_profile(group, want, Offset::ZERO, send);
                    });
                }
            }
        }
        // A device that arrived or left: what the one before held is ended
        // before anything else of the input is read.
        if let Some(cell) = &self.generation {
            let generation = cell.generation();
            if generation != device.generation {
                device.release(None, Offset::ZERO, &mut staging);
                device.restart(generation);
                // The new device has enabled no Profile.
                self.profiles = NO_PROFILES;
            }
        }
        device.pay(&mut staging);
        if device.present {
            for event in io.events(0) {
                let EventKind::Midi(raw) = event.kind else {
                    continue;
                };
                let at = event.offset;
                let frame = cx.env.frame_at(at);
                // A cable that ran out of time before this message ended its
                // notes then, not now.
                device.lapse(frame, first, block_len, &mut staging);
                let message = raw.decode().ok();
                if let Some(group) = raw.group() {
                    let sensing =
                        matches!(message, Some(Message::System(_, System::ActiveSensing)));
                    device.sensing.heard(group, frame, sensing);
                }
                match message {
                    // The cable's own: acted on here, sent on to no one.
                    Some(Message::System(_, System::ActiveSensing)) => continue,
                    Some(Message::System(group, System::Reset)) => {
                        device.reset_group(group, at, &mut staging);
                        continue;
                    }
                    _ => {}
                }
                device.held.observe(raw);
                // The translator ignores a MIDI 2.0 Control Change of CC 0, 6,
                // 32, 38 and 98 to 101, as a MIDI 2.0 receiver does (M2-104
                // section 7.4.6): `None` here is that rule, not a lost event.
                if let Some(ev) = device.translator.translate(raw) {
                    let mpe = &mut device.mpe;
                    staging.whole(ev.is_note_off(), |send| mpe.translate(&(at, ev), send));
                }
            }
            device.lapse(first + block_len, first, block_len, &mut staging);
        }
        // What the control side may wait for, stored once this block's events are
        // pushed: nothing is owed. A release that did not fit the block stays
        // owed (`Staging::whole` refuses it whole) and a push a port refuses is
        // counted by the executor, so a generation is handled only for what was sent or
        // counted.
        let handled_generation = self
            .generation
            .as_ref()
            .filter(|_| device.owing == 0)
            .map(|_| device.generation);
        let Staging { kept, dropped, .. } = staging;
        if dropped > 0 {
            self.dropped.add(Losses::refused(dropped));
            // The executor's too, so an export watching its count sees it.
            io.count_lost(dropped);
        }
        let events = &mut self.out[..kept];
        in_offset_order(events);
        for ev in events.iter() {
            // A Group the node does not hear has no port.
            let Some(port) = self.ports.port_of(&ev.1) else {
                continue;
            };
            // Refused past the capacity: counted by the executor.
            let _ = io
                .event_out(port)
                .push(Event::midi(offset_of(ev, block_len), ev.1));
        }
        if let (Some(cell), Some(handled)) = (&self.generation, handled_generation) {
            cell.mark_handled(handled);
        }
        Status::Modified
    }

    /// The device state of a freshly prepared node: the MIDI 1 translator and the MPE
    /// translator start empty (the MPE mode in force stays), nothing is held or
    /// owed, no Group has sent an Active Sensing, and the Profile zones in
    /// force are read again from the requests. What the node held is not
    /// released: `reset` has no output to release into. The executor, when it
    /// parks a bypassed node, ends the keys, hold pedals and bends the node held
    /// on its event outputs.
    fn reset(&mut self) {
        self.device.translator.reset();
        self.device.mpe = MpeTranslator::new(self.device.mode);
        self.device.held.clear();
        self.device.sensing.forget_all();
        self.device.owing = 0;
        self.profiles = NO_PROFILES;
        // The cell's first value holds no zones, so number 0 reads the requests
        // again whenever one was published.
        self.profiles_seq = 0;
    }
}

/// The block's translated events, as they are made.
pub(super) struct Staging<'a> {
    out: &'a mut [UmpAt],
    capacity: usize,
    /// How many of `out` are kept.
    pub(super) kept: usize,
    /// How many events were dropped for want of room.
    pub(super) dropped: u64,
}

impl<'a> Staging<'a> {
    /// Staging into `out`, of which a note-off may use all and any other
    /// event the first `capacity`.
    pub(super) fn new(out: &'a mut [UmpAt], capacity: usize) -> Self {
        Self {
            out,
            capacity,
            kept: 0,
            dropped: 0,
        }
    }
}

impl Staging<'_> {
    /// Keeps everything `make` sends, or, if it does not all fit, none of it
    /// and counts it dropped; whether it all fit. A `note_off`'s may use the
    /// room past the capacity.
    pub(super) fn whole(
        &mut self,
        note_off: bool,
        make: impl FnOnce(&mut dyn FnMut(UmpAt)),
    ) -> bool {
        let limit = if note_off {
            self.out.len()
        } else {
            self.capacity.min(self.out.len())
        };
        let start = self.kept;
        let mut sent = 0u64;
        let mut fits = true;
        make(&mut |ev| {
            sent += 1;
            if fits && self.kept < limit {
                self.out[self.kept] = ev;
                self.kept += 1;
            } else {
                fits = false;
            }
        });
        if !fits {
            self.kept = start;
            self.dropped += sent;
        }
        fits
    }
}

/// A [`MidiInputNode`]'s fork: the same ports, nothing to read.
struct SilentInput {
    ports: MidiInputPorts,
}

impl<F: Float> ForkSource<F> for SilentInput {
    fn fork(&self, _mode: ForkMode) -> Result<Forked<F>, ForkCause> {
        Ok(Forked::new(Box::new(
            MidiInputNode::new().hearing(self.ports),
        )))
    }
}

impl<F: Float> IntoNode<F> for MidiInputNode {
    type Controls = MidiInputControls;

    fn into_parts(self) -> NodeParts<MidiInputControls, F> {
        let fork = SilentInput { ports: self.ports };
        let controls = MidiInputControls {
            request: self.mpe_request_cell.clone(),
            serial: Arc::clone(&self.request_serial),
            dropped: self.dropped_reader.clone(),
        };
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}

/// Frees what the control side retired and no block holds, without publishing
/// (the MPE requests a replacement left behind); control thread, from the owner's tick.
impl tutti_rt::Reclaim for MidiInputControls {
    fn reclaim(&self) -> usize {
        self.request.reclaim()
    }
}

#[cfg(test)]
mod tests;
