//! MIDI in the session (feature `midi`): a live
//! input per target, one clip per target, the hardware input's route rules,
//! and the outputs, each a [`MidiOutNode`](tutti_midi_runtime::MidiOutNode)
//! whose event tap [`Phase::MidiOut`] drains to a device's output
//! (`MidiOutput`, feature `midi-hardware`) or a host's own sink ([`MidiSink`](crate::MidiSink)).
//!
//! Every piece is wiring. A clip is a `MidiClipNode`, a live input a
//! an event inlet, each the session's own node fed into its target's event
//! input through the session's feeds ([`Graph::set_feed`]); a route rule
//! gives a target ports of the hardware input node. A clip or a live input
//! outlives its target: a host that re-binds a target to a new node
//! re-aims them ([`Midi::retarget`]), and removes one with
//! [`Session::remove`](crate::Session::remove).
//!
//! [`Phase::MidiOut`]: crate::Phase::MidiOut

use std::collections::{BTreeMap, BTreeSet};
use tutti_buffers::Float;
use tutti_graph::UmpAt;
use tutti_platform::Instant;

use std::sync::{Arc, Weak};
use tutti_graph::{EventTapOptions, EventTapReader, NodeKey, Offset, TapEvent};
use tutti_midi_protocol::MpeMode;
use tutti_midi_runtime::{
    ClipEvents, ClockMap, DeviceGeneration, GenerationWriter, MidiClipNode, MidiInputPorts,
    MidiKeys, MpeTransformNode, Outgoing, OutputLatency, OutputPlan, OutputTerms, Placement,
    RouteRule, SendAt, TimedMidiEvent, compile_routes,
};
use tutti_music::MeterMap;
use tutti_rt::Mint;
use tutti_tempo::TempoMap;
use tutti_ump::{MidiChannel, MidiGroup, Ump};
use tutti_units::{Beat, BeatDuration, Frame, Samples};

use crate::RouteDeclarer;
use crate::graph::{Feeder, Graph};
use crate::instrument::open_keys;
use crate::midi_follow::{
    CLOCK_TAP_CAPACITY, ClockAction, ClockFollow, FOLLOW_TAP_CAPACITY, Follow, FollowCx,
    MACHINE_TAP_CAPACITY, MachineFollow, Placed,
};
use crate::{EventOutPort, Key, MidiError, MidiNodes, MidiReport};
use crate::{FollowClock, FollowMachineControl, FollowTempo};

/// What a [`Midi`] call returns: the area's one error.
type Result<T> = std::result::Result<T, MidiError>;

/// Max events drained per chunk into the stack buffer: one frame of clock at
/// any sane tempo is a handful of events, so this is generous headroom.
const DRAIN_CHUNK: usize = 256;

/// Events one input port's inlet holds: the ring its thread pushes into,
/// and as many waiting for their frame. It is the one ring of the hardware
/// input path (`DRAIN_CHUNK` above is the clock's drain, and the 256 of
/// `SessionEvent::EventsDropped` a log of losses, neither a MIDI input ring).
///
/// **The figure is one second of a saturated MIDI 1.0 cable.** The hardware
/// interface sends a byte every 320 microseconds (MIDI 1.0 Detailed
/// Specification, Hardware: 31.25 kbaud, a start bit, 8 data bits and a stop
/// bit), so the shortest message there is, a two-byte one under running
/// status, comes at most every 640 microseconds: 1,563 a second. 2,048 holds
/// that for 1.3 s, where an event waits for its frame only the lead (one block,
/// [`input_lead`]) and a ring is emptied every block, so a full ring means a
/// stalled graph, which the inlet counts as lost
/// ([`MidiInput::losses`](crate::MidiInput::losses)) and does not block on. A device faster than a
/// DIN cable (USB) fills it sooner, at its own rate, for the same stall.
#[cfg(feature = "midi-hardware")]
pub(crate) const MIDI_INPUT_INLET_CAPACITY: tutti_graph::EventCapacity =
    tutti_graph::EventCapacity::of(2048);

/// How long after an event arrives the hardware inlets place it: one block of
/// the device's callback (its `quantum` when known, else the largest block), so
/// an event that arrived between two blocks is on a frame of the next and is
/// not late. The spacing between events is kept; the
/// whole is that much later than the OS delivered it.
#[cfg(feature = "midi-hardware")]
pub(crate) fn input_lead(block: Samples, rate: tutti_units::SampleRate) -> std::time::Duration {
    std::time::Duration::from_secs_f64(block.to_seconds(rate).get())
}

/// The engine's block clock as the device threads read it
/// ([`tutti_midi_runtime::ArrivalClock`]). The engine stamps with
/// [`tutti_platform::monotonic_ns`]; the backends stamp arrivals with `Instant`,
/// the same clock on a native target. One pairing of the two, read once per
/// process, relates them, so there is one timebase end to end and the
/// conversion is an offset, never a second clock.
#[cfg(feature = "midi-hardware")]
pub(crate) fn arrival_clock(
    clock: tutti_engine::BlockClockReader,
) -> tutti_midi_runtime::ArrivalClock {
    std::sync::Arc::new(move || {
        let reading = clock.read()?;
        Some((
            crate::os_clock::os_instant(reading.os_ns)?,
            tutti_units::Moment::new(reading.frame, reading.rate),
        ))
    })
}

/// What the session reads of the engine to time the outputs: its block
/// clock and the audio output's latency (the stream's cell, which the device's
/// callback writes, `None` until it reports, and a restart resets in place).
#[derive(Clone, Debug)]
pub(crate) struct OutputClockSource {
    clock: tutti_engine::BlockClockReader,
    audio_latency: std::sync::Arc<tutti_engine::StreamLatency>,
}

impl OutputClockSource {
    pub(crate) fn new(
        clock: tutti_engine::BlockClockReader,
        audio_latency: std::sync::Arc<tutti_engine::StreamLatency>,
    ) -> Self {
        Self {
            clock,
            audio_latency,
        }
    }

    /// The audio output's latency as the device reports it now: read on every
    /// drain, for it can arrive or change after a restart. The OS reports a
    /// `Duration`; this is the one place it becomes `Seconds`.
    fn audio_output(&self) -> Option<tutti_units::Seconds> {
        self.audio_latency
            .get()
            .map(|latency| tutti_units::Seconds::new(latency.as_secs_f64()))
    }

    /// The timing of a drain at `now`: the latest block's map, `now` and the
    /// terms as the device reports them.
    fn timing(&self, latency: OutputLatency, now: Instant) -> OutputTiming {
        OutputTiming {
            map: self.map(),
            now,
            terms: OutputTerms {
                audio_output: self.audio_output(),
                latency,
            },
        }
    }

    /// The latest block reading as a map from frames to instants: `None`
    /// before the first block.
    fn map(&self) -> Option<ClockMap> {
        let reading = self.clock.read()?;
        Some(ClockMap::new(
            reading.rate,
            tutti_midi_runtime::ClockReading::new(
                crate::os_clock::os_instant(reading.os_ns)?,
                reading.frame,
            ),
        ))
    }
}

/// What [`UmpOut::send_frames`] times a batch by: the frames' map to the OS
/// clock, the caller's reading of that clock and every other term of the send
/// time ([`OutputPlan`]).
#[derive(Clone, Copy, Debug)]
pub(crate) struct OutputTiming {
    /// Places a frame on the OS clock; `None` before the engine's first block,
    /// when every frame is sent at once.
    pub(crate) map: Option<ClockMap>,
    /// The OS clock now: what a send time is compared with.
    pub(crate) now: Instant,
    /// The audio output's latency and the output's own.
    pub(crate) terms: OutputTerms,
}

/// An output node's tap, in events: the ring its output is copied into between
/// two updates (a note-off is kept up to
/// [`note_off_room`](tutti_graph::note_off_room) of it).
pub(crate) const MIDI_OUT_TAP_CAPACITY: tutti_graph::EventCapacity =
    tutti_graph::EventCapacity::of(1024);

/// Whether a host's own sink ([`MidiSink`](crate::MidiSink)) is sent JR
/// Timestamps (M2-104-UM §7.2.2.2). It gates timestamps, not delivery: with it
/// off the events still go out, unstamped. A device's output has no such choice:
/// it stamps iff its negotiated configuration receives JR Timestamps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum Stamping {
    /// Events are sent as they are.
    #[default]
    Off,
    /// Each event is sent with the JR Timestamp that keeps its spacing, and the
    /// stream carries the JR Clock (§7.2.2.1) a receiver needs to use them.
    Jr,
}

/// A UMP MIDI output and its JR stamp stream: where one output node's events
/// go, an output port's device or a sink's [`MidiOut`](tutti_midi_runtime::MidiOut).
///
/// The sink carries UMP words to the wire, so JR Timestamps and other
/// MIDI-2-only messages survive, which is the whole reason this type exists:
/// a device's output puts them in its device's wire form (`OpenOutput`), and
/// stamps only when the device receives them.
///
/// The sink is a value this type holds: a host's own `MidiOut`
/// ([`new`](Self::new)), or, with the `midi-hardware` feature, the
/// `OpenOutput` of a device. Sends take `&mut self`.
///
/// The [`JrStream`](tutti_midi_runtime::JrStream) is the whole endpoint's,
/// not one drain's: a per-drain origin would restart each at zero and
/// interleave stamps that walk backwards.
///
/// A device's output is erased behind the backend (`OpenOutput`), so there is
/// one path and no `#[cfg]` per platform: a Linux ALSA seq-UMP output stamps
/// exactly as a CoreMIDI one does.
pub(crate) struct UmpOut {
    sink: Sink,
    stream: tutti_midi_runtime::JrStream,
    /// Whether a host's own sink is JR-stamped. Separate from "is there an
    /// output" on purpose: stamping off must still *send*, just without
    /// timestamps. Folding the two together would make disabling the stamper
    /// silently mute MIDI out. A device's output says for itself
    /// ([`is_stamping`](Self::is_stamping)).
    stamping: Stamping,
    /// Scratch for the stamped block, reused across sends: stamping runs
    /// once per block and the result is only borrowed, straight to
    /// `MidiOut::queue`. `JrStream::stamp` clears it on entry, so nothing
    /// carries between blocks.
    stamped: Vec<UmpAt>,
    /// Scratch for a batch of frames rebased onto offsets, reused likewise.
    rebased: Vec<UmpAt>,
    /// Scratch: when each offset of the batch is sent, by offset.
    sends: Vec<(Offset, SendAt)>,
    /// Scratch for the batch as the sink takes it, with its send times.
    outgoing: Vec<Outgoing>,
    /// Scratch a drain collects its whole run of events into
    /// ([`route_drain`]), so one drain is one batch.
    drained: Vec<(Frame, Ump)>,
}

/// What an [`UmpOut`] sends into.
enum Sink {
    /// A host's own sink.
    Host(Arc<dyn tutti_midi_runtime::MidiOut>),
    /// A device's output, in its wire form.
    #[cfg(feature = "midi-hardware")]
    Device(Box<tutti_midi_hardware::OpenOutput>),
}

impl Sink {
    /// Queues `events`; returns how many events the host's sink refused, not
    /// counting the JR stamps among them. A device's output counts its own
    /// refusals in its losses.
    fn queue_timed(&mut self, events: &[Outgoing]) -> usize {
        match self {
            Self::Host(sink) => {
                let taken = sink.queue_timed(events).packets();
                events[taken..]
                    .iter()
                    .filter(|event| !event.ump.is_jr())
                    .count()
            }
            #[cfg(feature = "midi-hardware")]
            Self::Device(out) => {
                let _ = out.queue_timed(events);
                0
            }
        }
    }
}

impl core::fmt::Debug for UmpOut {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UmpOut")
            .field("origin", &self.stream.origin())
            .field("stamping", &self.is_stamping())
            .finish_non_exhaustive()
    }
}

impl UmpOut {
    /// Wraps a host's UMP output sink, stamping *and clocking* at
    /// `sample_rate`, with stamping off ([`set_stamping`](Self::set_stamping)).
    ///
    /// No group: JR Timestamps are groupless utility messages (M2-104-UM
    /// §2.1.2), so a stamp applies to the whole stream.
    ///
    /// This is a UMP-transport sender, so it owns the JR Clock cadence
    /// (§7.2.2.1). Without it the stamps this type exists to emit are inert:
    /// §7.2.2.3 has a receiver that has seen no JR Clock render messages "as
    /// soon as possible", discarding the timing entirely.
    pub(crate) fn new(
        sink: Arc<dyn tutti_midi_runtime::MidiOut>,
        sample_rate: tutti_units::SampleRate,
    ) -> Self {
        Self::over(Sink::Host(sink), sample_rate)
    }

    fn over(sink: Sink, sample_rate: tutti_units::SampleRate) -> Self {
        let stream = tutti_midi_runtime::JrStream::new(sample_rate).clock(sample_rate);
        Self::with_stream(sink, stream)
    }

    fn with_stream(sink: Sink, stream: tutti_midi_runtime::JrStream) -> Self {
        Self {
            sink,
            stream,
            stamping: Stamping::Off,
            stamped: Vec::new(),
            rebased: Vec::new(),
            sends: Vec::new(),
            outgoing: Vec::new(),
            drained: Vec::new(),
        }
    }

    /// Sets whether a host's own sink is JR-stamped. A device's output stamps
    /// iff its configuration receives JR Timestamps, whatever this says.
    pub(crate) fn set_stamping(&mut self, stamping: Stamping) {
        self.stamping = stamping;
    }

    /// Moves the stamp stream to `rate`, the graph's after a re-prepare: JR
    /// time carries on from where it stood ([`JrStream::at_rate`](tutti_midi_runtime::JrStream::at_rate)).
    pub(crate) fn set_rate(&mut self, rate: tutti_units::SampleRate) {
        self.stream = self.stream.at_rate(rate);
    }

    /// Whether it JR-stamps.
    #[must_use]
    pub(crate) fn is_stamping(&self) -> bool {
        match &self.sink {
            Sink::Host(_) => self.stamping == Stamping::Jr,
            #[cfg(feature = "midi-hardware")]
            Sink::Device(out) => out.config().rx_jr,
        }
    }

    /// The device output this sends into, if it is one.
    #[cfg(feature = "midi-hardware")]
    #[must_use]
    pub(crate) fn device(&self) -> Option<&tutti_midi_hardware::OpenOutput> {
        match &self.sink {
            Sink::Device(out) => Some(out),
            Sink::Host(_) => None,
        }
    }

    /// The device output this sends into, if it is one, to follow what its
    /// device settled.
    #[cfg(feature = "midi-hardware")]
    pub(crate) fn device_mut(&mut self) -> Option<&mut tutti_midi_hardware::OpenOutput> {
        match &mut self.sink {
            Sink::Device(out) => Some(out),
            Sink::Host(_) => None,
        }
    }

    /// An output over `output`, stamping and clocking at `sample_rate`.
    #[cfg(feature = "midi-hardware")]
    pub(crate) fn of_device(
        output: tutti_midi_hardware::OpenOutput,
        sample_rate: tutti_units::SampleRate,
    ) -> Self {
        Self::over(Sink::Device(Box::new(output)), sample_rate)
    }

    /// An output over `output` that carries on `stream`, the stamp stream of
    /// an output that was closed ([`into_stream`](Self::into_stream)).
    #[cfg(feature = "midi-hardware")]
    pub(crate) fn resumed(
        output: tutti_midi_hardware::OpenOutput,
        stream: tutti_midi_runtime::JrStream,
    ) -> Self {
        Self::with_stream(Sink::Device(Box::new(output)), stream)
    }

    /// The stamp stream, which outlives the sink it stamped for.
    #[cfg(feature = "midi-hardware")]
    pub(crate) fn into_stream(self) -> tutti_midi_runtime::JrStream {
        self.stream
    }

    /// Sends a batch of events, each on its frame on the graph's clock. The
    /// batch is one block: its first frame (the earliest) is its start, and
    /// its span runs to the last frame inclusive, so what the stamper reads
    /// is the events' spacing and the JR Clock advances by the span. An empty
    /// batch sends nothing.
    ///
    /// With `timing`, each event is sent at the time its frame has on the OS
    /// clock ([`OutputPlan`]: the frame's instant, plus the audio output's
    /// latency, less the device's, plus the offset), and the sink schedules it;
    /// without, every event goes at once. An event whose time is more than
    /// [`MAX_OUTPUT_AHEAD`](tutti_midi_runtime::MAX_OUTPUT_AHEAD) ahead is not
    /// sent, and the call returns how many events were refused, those a host's
    /// sink refused with them; a batch whose span no block on this target can
    /// hold is refused whole. The JR stamps keep the events' spacing whether or
    /// not they are scheduled.
    pub(crate) fn send_frames(
        &mut self,
        events: &[(Frame, Ump)],
        timing: Option<&OutputTiming>,
    ) -> usize {
        let (Some(first), Some(last)) = (
            events.iter().map(|e| e.0).min(),
            events.iter().map(|e| e.0).max(),
        ) else {
            return 0;
        };
        // `None` only for a span no block on this target can hold.
        let Some(span) = last.since(first).map(|d| d + Samples::new(1)) else {
            return events.len();
        };
        let plan = timing.map(|t| OutputPlan::new(t.terms));
        let mut batch = core::mem::take(&mut self.rebased);
        let mut sends = core::mem::take(&mut self.sends);
        batch.clear();
        sends.clear();
        let mut refused = 0;
        for &(frame, ump) in events {
            let send = match (plan.as_ref(), timing) {
                (Some(plan), Some(t)) => match plan.place(frame, t.map.as_ref(), t.now) {
                    Placement::Send(send) => send,
                    Placement::TooFar => {
                        refused += 1;
                        continue;
                    }
                },
                _ => SendAt::Now,
            };
            // Always `Some`: every rebased frame is below the span.
            if let Some(at) = frame.since(first).and_then(|d| Offset::new(d, span)) {
                batch.push((at, ump));
                sends.push((at, send));
            }
        }
        sends.sort_unstable_by_key(|&(at, _)| at);
        self.sends = sends;
        let sink_refused = self.send_batch(&batch);
        self.sends.clear();
        self.rebased = batch;
        refused + sink_refused
    }

    /// Stamps `events` (when stamping) and sends them, each at the time
    /// `self.sends` has for its offset, else at once. The JR Clock cadence
    /// advances only by the events handed in, so a silent stream never clocks.
    fn send_batch(&mut self, events: &[UmpAt]) -> usize {
        if !self.is_stamping() {
            return self.send_all(events);
        }
        // Taken out and put back so `stream` and the buffer can be borrowed
        // mutably at once; `stamp` clears it, so the swap carries no state.
        let mut buf = core::mem::take(&mut self.stamped);
        self.stream.stamp(events, &mut buf);
        let refused = self.send_all(&buf);
        self.stamped = buf;
        refused
    }

    fn send_all(&mut self, events: &[UmpAt]) -> usize {
        let mut out = core::mem::take(&mut self.outgoing);
        out.clear();
        // A JR stamp is at its event's offset, so it is sent with it. An offset
        // is its frame less the batch's first and a send time is a function of
        // the frame, so events on one offset share one send time and any
        // match of the search is the right one.
        out.extend(events.iter().map(|&(at, ump)| {
            Outgoing::new(
                at,
                self.sends
                    .binary_search_by_key(&at, |&(o, _)| o)
                    .map_or(SendAt::Now, |i| self.sends[i].1),
                ump,
            )
        }));
        // One `queue_timed` for the batch: a backend may pack several UMP
        // messages into one packet.
        let refused = self.sink.queue_timed(&out);
        self.outgoing = out;
        refused
    }

    /// The absolute frame the next stamp starts from.
    #[cfg(test)]
    pub(crate) fn origin(&self) -> Frame {
        self.stream.origin()
    }
}

/// The writing half of an output's cumulative losses: held by the session beside
/// the output, written on the session's thread.
pub(crate) struct LossWriters {
    losses: tutti_rt::CounterWriter<tutti_graph::Losses>,
}

/// The reading half of an output's losses, shared with its handle.
#[derive(Clone, Debug)]
pub(crate) struct LossReaders {
    losses: tutti_rt::Counter<tutti_graph::Losses>,
}

/// A fresh pair of loss cells, at zero.
pub(crate) fn loss_cells() -> (LossWriters, LossReaders) {
    let (writer, losses) = tutti_rt::counter();
    (LossWriters { losses: writer }, LossReaders { losses })
}

impl LossWriters {
    /// Counts `n` events its node's tap had no room for.
    pub(crate) fn lost(&mut self, n: u64) {
        self.losses.add(tutti_graph::Losses::lost(n));
    }

    /// Counts `n` events that did not go: refused by the output, scheduled too
    /// far ahead, or with no output to send to.
    pub(crate) fn refused(&mut self, n: u64) {
        self.losses.add(tutti_graph::Losses::refused(n));
    }
}

impl LossReaders {
    /// The losses so far: events the tap lost, events that did not go.
    pub(crate) fn get(&self) -> tutti_graph::Losses {
        self.losses.get()
    }
}

/// Routes one drain as one batch to `out`: `fill` hands every event of the
/// drain to the callback it is given, and they are then sent together
/// ([`UmpOut::send_frames`]), so the spacing of the whole drain, not of a
/// chunk of it, is what the stamps and the send times keep. With no output the
/// events are counted as refused in `losses`. Returns how many events did not go.
fn route_drain(
    out: Option<&mut UmpOut>,
    losses: &mut LossWriters,
    timing: Option<&OutputTiming>,
    fill: impl FnOnce(&mut dyn FnMut(Frame, Ump)),
) -> u64 {
    match out {
        Some(out) => {
            let mut batch = core::mem::take(&mut out.drained);
            batch.clear();
            fill(&mut |frame, ump| batch.push((frame, ump)));
            let refused = out.send_frames(&batch, timing) as u64;
            out.drained = batch;
            losses.refused(refused);
            refused
        }
        None => {
            let mut n = 0;
            fill(&mut |_, _| n += 1);
            losses.refused(n);
            n
        }
    }
}

/// What the handles of an output port, an input port or a sink share: its
/// record and its node. Clones of a handle share one claim.
#[derive(Debug)]
pub(crate) struct Claim<S> {
    pub(crate) shared: Arc<S>,
    pub(crate) node: NodeKey,
}

/// A port or sink that retires when its handles are all dropped.
pub(crate) trait Sweepable {
    /// Whether a handle lives.
    fn is_held(&self) -> bool;
    /// The output node.
    fn node(&self) -> NodeKey;
    /// Moves the node's generation on, the drop that the node acknowledges once
    /// the graph has rendered past it; the generation to wait on.
    fn depart(&mut self) -> DeviceGeneration;
}

/// What an input port, an output port or a sink left behind when its handles
/// were all dropped, and waits for the node's generation to go idle. The port
/// closes only after the graph rendered past the drop and the tap was drained:
/// `going` is dropped when [`settle`] takes the node out, and a device or sink
/// closes with it.
pub(crate) struct Retiring<G> {
    pub(crate) node: NodeKey,
    pub(crate) going: G,
    pub(crate) generation: DeviceGeneration,
}

/// The control side of an output node's generation: the node's half follows it
/// ([`MidiOutNode::following`](tutti_midi_runtime::MidiOutNode::following)), and
/// the drop of the last handle moves it on.
#[derive(Debug)]
pub(crate) struct Departure {
    writer: GenerationWriter,
    generation: DeviceGeneration,
}

impl Departure {
    /// A generation with its node present, and the node's half.
    pub(crate) fn new() -> (Self, DeviceGeneration) {
        let (mut writer, generation) = DeviceGeneration::new();
        writer.arrive();
        let node = generation.clone();
        (Self { writer, generation }, node)
    }

    /// The node's generation moved on, to wait on.
    pub(crate) fn depart(&mut self) -> DeviceGeneration {
        self.writer.depart();
        self.generation.clone()
    }
}

/// Moves the `ports` whose handles were all dropped to `retiring`, each after
/// `leave` has run on it (what ends at the drop itself), as `G`. Whether any
/// moved.
pub(crate) fn sweep_dropped<P: Sweepable, G: From<P>>(
    ports: &mut Vec<P>,
    retiring: &mut Vec<Retiring<G>>,
    mut leave: impl FnMut(&mut P),
) -> bool {
    let mut moved = false;
    let mut at = 0;
    while at < ports.len() {
        if ports[at].is_held() {
            at += 1;
            continue;
        }
        let mut port = ports.remove(at);
        leave(&mut port);
        retiring.push(Retiring {
            node: port.node(),
            generation: port.depart(),
            going: G::from(port),
        });
        moved = true;
    }
    moved
}

/// Takes out the nodes of the `retiring` the graph has rendered past the drop
/// of (or a host removed itself), after `drain` has read what their taps hold a
/// last time; the rest are drained and stay. A retiring one `force` names is
/// taken out at once. A port closes with its node, after its last drain: the
/// one place the order is written. Whether the graph was edited.
pub(crate) fn settle<G, F: Float>(
    retiring: &mut Vec<Retiring<G>>,
    graph: &mut Graph<F>,
    mut drain: impl FnMut(&mut G),
    mut force: impl FnMut(&G) -> bool,
) -> bool {
    let mut edited = false;
    retiring.retain_mut(|going| {
        let done = going.generation.is_idle() || !graph.contains(going.node) || force(&going.going);
        drain(&mut going.going);
        if done {
            // A host that removed the node itself leaves nothing to take.
            let _ = graph.remove(going.node);
            edited = true;
        }
        !done
    });
    edited
}

/// The session's side of a [`Claim`]: a weak reference, so a port or sink is
/// held exactly as long as some handle of it lives.
#[derive(Debug)]
pub(crate) struct Held<S>(Weak<Claim<S>>);

impl<S> Held<S> {
    /// The new claim on `shared` and `node`, and the session's side of it.
    pub(crate) fn new(shared: Arc<S>, node: NodeKey) -> (Arc<Claim<S>>, Self) {
        let claim = Arc::new(Claim { shared, node });
        let held = Self(Arc::downgrade(&claim));
        (claim, held)
    }

    /// Whether a handle lives.
    pub(crate) fn is_held(&self) -> bool {
        self.0.strong_count() > 0
    }
}

tutti_rt::id! {
    /// An id [`Midi::route`] returns, for [`Midi::unroute`].
    pub struct MidiRouteId;
}

/// One inbound routing rule: which Group and channel of the hardware input
/// reach which targets, keyed by `T` (the session's own rules name nodes, a
/// host's its own keys: [`Midi::set_routes`]).
///
/// A channel is the channel of one Group (M2-104 Appendix B.1): the rules
/// below match the first Group unless told otherwise
/// ([`in_group`](MidiRouteRule::in_group),
/// [`in_any_group`](MidiRouteRule::in_any_group)).
///
/// Several rules may name the same channel; every matching rule's targets
/// receive the event.
///
/// **Persisting a rule** (feature `serde`): a [`NodeKey`] names a node of one
/// session and means nothing in the next, so a rule that is saved is keyed by
/// the app's own stable id, `MidiRouteRule<AppId>` (`AppId: Serialize`), and
/// the app maps it to a key when it loads: [`set_routes`](Midi::set_routes)
/// takes the rules keyed by `T` and a `resolve` from `T` to a node, which is
/// where the app's table lives. `MidiRouteRule<NodeKey>` itself has no serde.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MidiRouteRule<T = NodeKey> {
    /// Group filter: `None` matches every Group the input hears, `Some(g)`
    /// only Group `g`.
    pub group: Option<MidiGroup>,
    /// Channel filter: `None` matches any channel, `Some(n)` only channel
    /// `n`.
    pub channel: Option<MidiChannel>,
    /// The targets this rule feeds.
    pub targets: Vec<T>,
    /// A disabled rule stays declared but routes nothing: a mute that does
    /// not lose the rule.
    pub enabled: bool,
}

impl<T> MidiRouteRule<T> {
    /// A rule matching every channel. Add targets with [`to`](Self::to).
    pub fn any_channel() -> Self {
        Self {
            group: Some(MidiGroup::FIRST),
            channel: None,
            targets: Vec::new(),
            enabled: true,
        }
    }

    /// A rule matching one channel.
    pub fn for_channel(channel: MidiChannel) -> Self {
        Self {
            group: Some(MidiGroup::FIRST),
            channel: Some(channel),
            targets: Vec::new(),
            enabled: true,
        }
    }

    /// Matches `group` instead of the first.
    #[must_use]
    pub fn in_group(mut self, group: MidiGroup) -> Self {
        self.group = Some(group);
        self
    }

    /// Matches the channel in every Group the input hears.
    #[must_use]
    pub fn in_any_group(mut self) -> Self {
        self.group = None;
        self
    }

    /// Adds a destination: a `T`, or anything that converts into one (a
    /// session's own rule takes a graph [`Key`] as it takes a
    /// [`NodeKey`]).
    #[must_use]
    pub fn to(mut self, target: impl Into<T>) -> Self {
        self.targets.push(target.into());
        self
    }

    /// Declares the rule without arming it.
    #[must_use]
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }
}

/// One clip: the MPE transform between it and its target, and the target it
/// feeds. The target is fed by the transform, which the clip feeds.
#[derive(Debug)]
struct Clip {
    /// The transform node, which is part of the clip's wiring: it goes with
    /// the clip.
    transform: NodeKey,
    target: NodeKey,
}

/// One live input: the target it feeds, and its keys until [`Midi::input`]
/// hands them out (a take can open the inlet before anyone plays it).
struct Live {
    keys: Option<MidiKeys>,
    target: NodeKey,
}

/// The outputs' latency and offset, a cell the value table names.
struct LatencyCell(Arc<crate::value::Cell<OutputLatency>>);

impl Default for LatencyCell {
    fn default() -> Self {
        Self(crate::value::Cell::new(
            OutputLatency::default(),
            |_| Ok(()),
        ))
    }
}

/// The session's MIDI state.
#[derive(Default)]
pub(crate) struct MidiState {
    clips: BTreeMap<NodeKey, Clip>,
    live: BTreeMap<NodeKey, Live>,
    /// The session's own route rules.
    rules: BTreeMap<MidiRouteId, MidiRouteRule>,
    fallback: Option<NodeKey>,
    /// Names the session's own route rules.
    minted: Mint,
    /// The sinks the last route compile fed.
    routed: BTreeSet<NodeKey>,
    /// Whether a host's last `set_routes` declared any rule or fallback:
    /// the routes are then the host's, and the session's own are refused.
    host_routes: bool,
    /// The sinks opened by `open_midi_sink`.
    pub(crate) sinks: crate::midi_sink::Sinks,
    /// The ports begun by `open_midi_input` and `open_midi_output` and installed by `apply`.
    pub(crate) ports: crate::midi_port::Ports,
    /// The session's one MPE mode ([`MpeModeTarget`](crate::MpeModeTarget)),
    /// the cell every input and clip transform reads.
    mpe: tutti_rt::Publisher<MpeMode>,
    /// The tempo the session follows, if it does ([`Midi::follow_tempo`]).
    follow: Option<Follow>,
    /// Whether a live tempo owns the map, as `follow` and `clock` left it:
    /// the tempo target's refusal reads it ([`is_following`](Self::is_following)).
    following: Arc<std::sync::atomic::AtomicBool>,
    /// The MIDI clock the session follows, if it does ([`Midi::follow_clock`]).
    clock: Option<ClockFollow>,
    /// The Machine Control the session follows, if it does
    /// ([`Midi::follow_machine_control`]).
    machine: Option<MachineFollow>,
    /// Events [`Midi::unfollow_tempo`] and [`Midi::unfollow_clock`] queued for the next `Poll` phase, which
    /// reports them.
    follow_events: Vec<MidiReport>,
    /// What the engine gives the outputs' timing: its block clock and the
    /// audio output's latency. `None` offline, where every event
    /// is sent at once.
    output_clock: Option<OutputClockSource>,
    /// The `now` of the last [`midi_out_phase`](Self::midi_out_phase): the time
    /// an open's final drain of a dropped output is timed by.
    last_now: Option<Instant>,
    /// The outputs' own latency and offset ([`Midi::output_latency`]).
    output_latency: LatencyCell,
    /// The session's subscription to the device list: `Phase::Poll` reads it
    /// and nothing else does.
    #[cfg(feature = "midi-hardware")]
    device_events: Option<tutti_midi_hardware::DeviceEvents>,
    /// Whether the device watch's failure has been reported.
    #[cfg(feature = "midi-hardware")]
    watch_failure_reported: bool,
}

impl std::fmt::Debug for MidiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiState")
            .field("clips", &self.clips)
            .field("rules", &self.rules)
            .field("fallback", &self.fallback)
            .field("minted", &self.minted)
            .field("routed", &self.routed)
            .field("host_routes", &self.host_routes)
            .finish_non_exhaustive()
    }
}

impl MidiState {
    /// A state whose `Phase::Poll` reads `device_events`.
    #[cfg(feature = "midi-hardware")]
    pub(crate) fn with_device_events(
        device_events: Option<tutti_midi_hardware::DeviceEvents>,
    ) -> Self {
        Self {
            device_events,
            ..Self::default()
        }
    }

    /// The outputs are timed by `clock` from now on (a session with an
    /// audio device, or the system clock's zero-latency output).
    pub(crate) fn set_output_clock(&mut self, clock: OutputClockSource) {
        self.output_clock = Some(clock);
    }

    /// The state with the session's MPE mode as the builder declared it.
    pub(crate) fn with_mpe(mut self, mpe: MpeMode) -> Self {
        self.mpe = tutti_rt::Publisher::new(mpe);
        self
    }

    /// The cell holding the session's MPE mode.
    pub(crate) fn mpe(&self) -> &tutti_rt::Publisher<MpeMode> {
        &self.mpe
    }

    /// `node` left the graph: a clip or live input of the session's own is
    /// forgotten (the graph took it out of every feed), and a routed sink
    /// is no longer one. A clip and its transform go together: the other node
    /// of the pair, which the caller takes out of the graph too.
    #[must_use = "the clip's other node is still in the graph"]
    pub(crate) fn forget_node(&mut self, node: NodeKey) -> Option<NodeKey> {
        self.live.remove(&node);
        self.routed.remove(&node);
        if let Some(clip) = self.clips.remove(&node) {
            return Some(clip.transform);
        }
        let clip = self
            .clips
            .iter()
            .find_map(|(&clip, c)| (c.transform == node).then_some(clip))?;
        self.clips.remove(&clip);
        Some(clip)
    }

    /// `Phase::Poll`, for the ports: retires those whose handles were all dropped, takes in
    /// the devices that came and went (each a [`MidiReport`], and a port that
    /// held one reads it as its [`Presence`](crate::Presence)), sends what the
    /// handles asked of the MIDI-CI peers, steps the UMP Stream negotiations and
    /// the MIDI-CI of the open inputs. Whether the graph was edited.
    #[cfg(feature = "midi-hardware")]
    pub(crate) fn midi_input_phase<F: Float>(
        &mut self,
        io: Option<&mut tutti_midi_hardware::MidiSession>,
        nodes: Option<&MidiNodes>,
        graph: &mut Graph<F>,
        now: Instant,
        out: &mut Vec<crate::SessionEvent>,
    ) -> bool {
        let Some(io) = io else { return false };
        // Drained first: reopening a device takes the state mutably.
        let pending: Vec<_> = self
            .device_events
            .iter()
            .flat_map(tutti_midi_hardware::DeviceEvents::try_iter)
            .collect();
        let rate = graph.prepared().sample_rate();
        let edits = self.ports.poll(io, graph, rate, pending, now, out);
        if edits.routes && !self.host_routes {
            let layout = nodes.map_or(MidiInputPorts::FIRST_GROUP, |n| n.input_ports);
            self.compile_session_feeds(graph, layout);
        }
        if !self.watch_failure_reported
            && let Some(why) = self
                .device_events
                .as_ref()
                .and_then(tutti_midi_hardware::DeviceEvents::failure)
        {
            self.watch_failure_reported = true;
            out.push(MidiReport::WatchFailed(why).into());
        }
        edits.graph || edits.routes
    }

    /// Compiles the session's own route rules onto every input port's input node
    /// node.
    pub(crate) fn compile_session_feeds<F: Float>(
        &mut self,
        graph: &mut Graph<F>,
        layout: MidiInputPorts,
    ) {
        let inputs = self.input_nodes();
        let rules = std::mem::take(&mut self.rules);
        let fallback = self.fallback;
        compile_feeds(
            graph,
            &mut self.routed,
            layout,
            &inputs,
            rules.values(),
            fallback,
            Some,
        );
        self.rules = rules;
    }

    /// The input nodes of the input ports: none on a session without the OS
    /// MIDI layer.
    pub(crate) fn input_nodes(&self) -> Vec<NodeKey> {
        self.ports.input_nodes()
    }

    /// Whether `node` is the input node of an input port.
    pub(crate) fn is_input_node(&self, node: NodeKey) -> bool {
        self.ports.is_input_node(node)
    }

    /// The timing of a drain at `now`, from the engine's block clock: `None`
    /// offline.
    pub(crate) fn output_timing(&self, now: Instant) -> Option<OutputTiming> {
        self.output_clock
            .as_ref()
            .map(|source| source.timing(self.output_latency.0.current(), now))
    }

    /// Whether the session follows a live tempo now.
    pub(crate) fn is_following(&self) -> bool {
        self.follow.is_some() || self.clock.as_ref().is_some_and(ClockFollow::owns_tempo)
    }

    /// The cell of the outputs' latency, for the value table.
    pub(crate) fn output_latency_cell(&self) -> Arc<crate::value::Cell<OutputLatency>> {
        Arc::clone(&self.output_latency.0)
    }

    /// The flag the tempo target reads, shared with this state.
    pub(crate) fn following_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.following)
    }

    /// Publishes [`is_following`](Self::is_following) to the tempo target,
    /// after a follower is set or dropped.
    fn sync_following(&self) {
        self.following
            .store(self.is_following(), std::sync::atomic::Ordering::Release);
    }

    /// `Phase::Poll`, for the MIDI clock the session follows: reads its tap,
    /// publishes the tempo it fit and reports its lock into `out`. What it asks
    /// of the transport, in order, for the caller to send; and whether the graph
    /// was edited, which the caller commits.
    pub(crate) fn clock_phase<F: Float>(
        &mut self,
        cx: &FollowCx<'_>,
        graph: &mut Graph<F>,
        out: &mut Vec<crate::SessionEvent>,
    ) -> (Vec<Placed>, bool) {
        let Some(clock) = self.clock.as_mut() else {
            return (Vec::new(), false);
        };
        let actions = clock.heard(cx, out);
        let mut edited = false;
        if clock.tap_rate() != graph.prepared().sample_rate() {
            edited = true;
            if let Some(tap) = graph.taps().reopen_event(clock.tap_id()) {
                clock.set_tap(tap);
            }
        }
        (actions, edited)
    }

    /// `Phase::Poll`, for the Machine Control the session follows: reads its
    /// tap, reports what it did not act on into `out`. What it asks of the
    /// transport, in order, for the caller to send; and whether the graph was
    /// edited, which the caller commits.
    pub(crate) fn machine_phase<F: Float>(
        &mut self,
        cx: &FollowCx<'_>,
        graph: &mut Graph<F>,
        out: &mut Vec<crate::SessionEvent>,
    ) -> (Vec<Placed>, bool) {
        let Some(machine) = self.machine.as_mut() else {
            return (Vec::new(), false);
        };
        let actions = machine
            .heard(cx, out)
            .into_iter()
            .map(ClockAction::at_next_block)
            .collect();
        let mut edited = false;
        if machine.tap_rate() != graph.prepared().sample_rate() {
            edited = true;
            if let Some(tap) = graph.taps().reopen_event(machine.tap_id()) {
                machine.set_tap(tap);
            }
        }
        (actions, edited)
    }

    /// `Phase::Poll`, for the tempo the session follows: reports what
    /// `unfollow_tempo` queued, reads the follower's tap and applies what it
    /// heard (the project's tempo map and meter, overlaid at the next block
    /// start; see `midi_follow`). Whether the graph was edited, which the
    /// caller commits.
    pub(crate) fn follow_phase<F: Float>(
        &mut self,
        cx: &FollowCx<'_>,
        graph: &mut Graph<F>,
        out: &mut Vec<crate::SessionEvent>,
    ) -> bool {
        out.extend(self.follow_events.drain(..).map(Into::into));
        let Some(follow) = self.follow.as_mut() else {
            return false;
        };
        // What the tap holds is read at the scale it was opened at, then the
        // tap is reopened if the graph has been re-prepared at another rate.
        let heard = follow.heard();
        let mut edited = false;
        if follow.tap_rate() != graph.prepared().sample_rate() {
            edited = true;
            if let Some(tap) = graph.taps().reopen_event(follow.tap_id()) {
                follow.set_tap(tap);
            }
        }
        follow.apply(&heard, cx, out);
        edited
    }

    /// The outputs' taps are on one scale of frames, the rate they were opened
    /// at: when the graph has been re-prepared at another, drains the old
    /// readers and reopens those taps (and no other: a caller's own are the
    /// caller's). Whether any reopened; the graph is then edited and the next
    /// commit compiles the new taps.
    pub(crate) fn follow_rate<F: Float>(&mut self, graph: &mut Graph<F>, now: Instant) -> bool {
        let timing = self.output_timing(now);
        let sinks = self.sinks.reopen(graph, timing.as_ref());
        let ports = self.ports.reopen(graph, timing.as_ref());
        sinks || ports
    }

    /// How a sweep outside an update times what it sends: by the `now` of the
    /// last update, so it is placed as that update's drain would have been;
    /// `None` before any update, and what is sent then goes at once.
    pub(crate) fn sweep_timing(&self) -> Option<OutputTiming> {
        self.last_now.and_then(|now| self.output_timing(now))
    }

    /// `Phase::MidiOut`: drains every output's tap to its device or sink.
    /// Whether the graph was edited.
    pub(crate) fn midi_out_phase<F: Float>(&mut self, graph: &mut Graph<F>, now: Instant) -> bool {
        // A graph re-rated outside the device hook (`Graph::set_sample_rate`)
        // is followed here, before this update's events are read.
        self.last_now = Some(now);
        let mut edited = self.follow_rate(graph, now);
        let timing = self.output_timing(now);
        edited |= self.sinks.drain(graph, timing.as_ref());
        edited |= self.ports.drain_outputs(graph, timing.as_ref());
        edited
    }
}

/// Reads `tap` dry and routes what it held to `out` (with none, the events are
/// counted as refused), each event on its frame. The events the tap had no room
/// for ([`EventTapReader::take_lost`]) are counted lost, and those that did not
/// go refused, in `losses`.
pub(crate) fn drain_events(
    tap: &mut EventTapReader,
    out: Option<&mut UmpOut>,
    losses: &mut LossWriters,
    timing: Option<&OutputTiming>,
) {
    let mut events = [TapEvent {
        frame: Frame::ZERO,
        ump: Ump([0; 4]),
    }; DRAIN_CHUNK];
    route_drain(out, losses, timing, |push| {
        loop {
            let n = tap.pop_into(&mut events);
            for e in &events[..n] {
                push(e.frame, e.ump);
            }
            if n < DRAIN_CHUNK {
                break;
            }
        }
    });
    losses.lost(tap.take_lost());
}

/// Wires `inputs`' ports as `rules` route them (`tutti_midi_runtime::compile_routes`)
/// to the sinks `resolve` names, replacing every route before: the ports of
/// every input node feed each sink their rule gives ports, so devices merge in
/// the sink's event fan-in. A target `resolve` maps to no node, or to one not in
/// the graph, takes nothing (and still covers its rule's ports): a host's table
/// may name a target it has not made yet or has removed, so this is the one
/// place a missing target is skipped, not refused. A sink fed
/// before and not now is unwired. `routed` holds the sinks the last compile fed.
fn compile_feeds<'r, T: Copy + Ord + 'r, F: Float>(
    graph: &mut Graph<F>,
    routed: &mut BTreeSet<NodeKey>,
    layout: MidiInputPorts,
    inputs: &[NodeKey],
    rules: impl IntoIterator<Item = &'r MidiRouteRule<T>>,
    fallback: Option<T>,
    resolve: impl Fn(T) -> Option<NodeKey>,
) {
    let ports = compile_routes(
        layout,
        rules.into_iter().filter(|r| r.enabled).map(|r| RouteRule {
            group: r.group,
            channel: r.channel,
            targets: &r.targets,
        }),
        fallback,
    );
    let mut wanted: BTreeMap<NodeKey, Vec<EventOutPort>> = BTreeMap::new();
    for (target, ports) in ports {
        let Some(sink) = resolve(target).filter(|&n| graph.contains(n)) else {
            continue;
        };
        wanted.entry(sink).or_default().extend(
            inputs
                .iter()
                .flat_map(|&input| ports.iter().map(move |&p| EventOutPort::new(input, p))),
        );
    }
    for &fed in routed.iter() {
        if !wanted.contains_key(&fed) {
            graph.set_feed(fed, Feeder::Routes, Vec::new());
        }
    }
    *routed = wanted.keys().copied().collect();
    for (sink, mut sources) in wanted {
        sources.sort();
        sources.dedup();
        graph.set_feed(sink, Feeder::Routes, sources);
    }
}

/// The session's MIDI: live inputs, clips, the hardware
/// input's routes and the outputs. [`Session::midi`](crate::Session::midi)
/// hands it out; what it edits goes out with the next update's commit.
#[derive(Debug)]
pub struct Midi<'a, F: Float = f32> {
    pub(crate) graph: &'a mut Graph<F>,
    pub(crate) state: &'a mut MidiState,
    pub(crate) nodes: Option<&'a MidiNodes>,
    /// The transport's settings (the tempo map a follower overlays).
    pub(crate) settings: &'a tutti_timeline::TransportSettings,
    /// The project meter a follower overlays.
    pub(crate) meter: tutti_timeline::ProjectMeter,
    /// The OS MIDI layer, when the builder opened it.
    #[cfg(feature = "midi-hardware")]
    pub(crate) io: Option<&'a mut tutti_midi_hardware::MidiSession>,
}

impl<F: Float> Midi<'_, F> {
    /// Retires the ports and sinks whose handles were all dropped, and ends the
    /// ones the graph has rendered past. An input leaves through
    /// [`Ports::retire`](crate::midi_port::Ports), which `update`'s poll runs as
    /// well; an output or a sink through the sweep `update`'s `Phase::MidiOut`
    /// runs as well. Each stays, its tap drained into its device or sink, until
    /// the graph has rendered past the drop, so what was queued before the drop
    /// is sent; then its node is taken out and its device or sink closes. A
    /// device an output of the session still holds is the exception: opening it
    /// again ends that output at once (what the graph has not rendered is lost).
    /// A sink has no such release.
    ///
    /// **A session that renders no block keeps what it retires**: the node
    /// stays, with the device connection or the sink's `Arc<dyn MidiOut>`,
    /// until blocks render, the device is opened again, or the session drops.
    /// The drain is timed by the
    /// `now` of the last `update`, so it is placed as that update's drain would
    /// have been; before any `update` there is no output clock and it is sent at
    /// once.
    ///
    /// The graph edits go uncommitted here: the next `update` commits them.
    pub(crate) fn close_dropped(&mut self) {
        let timing = self.state.sweep_timing();
        // The sweeps' `edited` results are not read: the graph is edited in
        // place, and the next `update` commits it.
        let _ = self.state.sinks.close_dropped(self.graph, timing.as_ref());
        let _ = self
            .state
            .ports
            .close_dropped_outputs(self.graph, timing.as_ref());
        #[cfg(feature = "midi-hardware")]
        if let Some(io) = self.io.as_deref_mut() {
            let edits = self.state.ports.retire(io, self.graph);
            if edits.routes {
                self.compile_session_routes();
            }
        }
    }

    /// `target`, if it takes MIDI: it is in the graph with an event input.
    fn takes_midi(&self, target: NodeKey) -> Result<()> {
        if self.graph.node_event_inputs(target).is_ok_and(|n| n > 0) {
            Ok(())
        } else {
            Err(MidiError::NoEventInput(target))
        }
    }

    // --- Live input ---

    /// Returns the keys of `target`'s live input: an event inlet fed into the
    /// main event input of **any node that takes MIDI** that is not an
    /// instrument of the session (a plugin effect, a modulator, your own
    /// node), for a keyboard, a preview or an all-notes-off the host sends.
    /// What is sent plays from the next block
    /// ([`Stamp::NextBlock`](tutti_graph::Stamp::NextBlock)) or on a frame of
    /// the engine's clock.
    ///
    /// **An instrument has its keys already**: one made by
    /// [`Session::add_instrument`](crate::Session::add_instrument) or
    /// [`Session::instrument`](crate::Session::instrument) is played through
    /// [`InstrumentHandle::keys_mut`](crate::InstrumentHandle::keys_mut), and
    /// asking for its input here is refused, so an instrument never has two
    /// live inlets.
    ///
    /// Like the handle's, these keys are a performance: they play in the live
    /// graph and reach the device output, and a bounce does not contain them
    /// (an event inlet's fork is silent). Notes an export must hold are a
    /// clip ([`play_clip`](Self::play_clip)).
    ///
    /// A target has one live input and its keys are the inlet's one
    /// producer: they are handed out once. [`tap`](Self::tap) opens the
    /// inlet too, and leaves the keys for this call.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoEventInput`] when `target` is not in the graph or takes
    /// no MIDI; [`MidiError::InstrumentKeys`] when `target` is an instrument
    /// with a handle; [`MidiError::InputTaken`] when the keys were handed out
    /// already.
    pub fn input(&mut self, target: impl Into<NodeKey>) -> Result<MidiKeys> {
        let target = target.into();
        let node = self.live_inlet(target)?;
        self.state
            .live
            .get_mut(&node)
            .and_then(|live| live.keys.take())
            .ok_or(MidiError::InputTaken(target))
    }

    /// The queue the handle of instrument `target` plays through, if it has a
    /// handle.
    fn instrument_queue(&self, target: NodeKey) -> Option<NodeKey> {
        self.graph
            .feed(target, Feeder::Instrument)
            .first()
            .map(|port| port.node())
    }

    /// The node of `target`'s live input, opened with its keys held if it has
    /// none.
    fn live_inlet(&mut self, target: NodeKey) -> Result<NodeKey> {
        self.takes_midi(target)?;
        if let Some((node, _)) = self.fed_by(target, Feeder::LiveInput, &self.state.live) {
            return Ok(node);
        }
        if self.instrument_queue(target).is_some() {
            return Err(MidiError::InstrumentKeys(target));
        }
        let (node, keys) = open_keys(self.graph);
        let node = node.erase();
        self.graph
            .set_feed(target, Feeder::LiveInput, vec![EventOutPort::new(node, 0)]);
        self.state.live.insert(
            node,
            Live {
                keys: Some(keys),
                target,
            },
        );
        Ok(node)
    }

    // --- Clips ---

    /// Plays `events` on `target` through its clip, `at` the beat the
    /// events' beat 0 lands on: the clip it has, its events replaced in place
    /// (which ends the notes the old events left sounding), or a new
    /// [`MidiClipNode`] fed into its event input. A target has one clip; it
    /// is found again with [`clip`](Self::clip) and emptied with
    /// [`stop_clip`](Self::stop_clip).
    ///
    /// The events are [`TimedMidiEvent`]s, positioned in beats from the
    /// clip's own start, in any order; the `packets` of a file's
    /// [`MetricalView::timeline`](tutti_midi_file::MetricalView::timeline) are
    /// the `(Beat, Ump)` pairs they are built from, and
    /// a loaded file's tracks are played with [`play_track`](Self::play_track). The clip reads its
    /// block's `Env`, so a note lands on its frame through seeks and loop
    /// wraps, and **a bounce contains it**: an export forks the clip with its
    /// target, where the keys of a live input are not in one.
    ///
    /// **A clip does not loop itself**: it plays on the transport's timeline,
    /// so a repeat is the transport's loop
    /// (`session.transport_handle().looping()`) over the beats it
    /// occupies.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // The melody's beats 0..4, played from bar 3 of 4/4.
    /// session.midi().play_clip(synth.node(), Beat::new(8.0), melody)?;
    /// ```
    ///
    /// # Errors
    ///
    /// [`MidiError::NoEventInput`] when `target` is not in the graph or takes
    /// no MIDI; [`MidiError::BeatNotFinite`] when `at`, or an event moved by
    /// it, is not a finite beat (nothing changes); [`MidiError::ClipEvents`]
    /// when the target's clip cannot be written (nothing changes).
    pub fn play_clip(
        &mut self,
        target: impl Into<NodeKey>,
        at: Beat,
        events: impl IntoIterator<Item = TimedMidiEvent>,
    ) -> Result<()> {
        let target = target.into();
        self.takes_midi(target)?;
        if !at.get().is_finite() {
            return Err(MidiError::BeatNotFinite(at));
        }
        let by = BeatDuration::new(at.get());
        let events = events
            .into_iter()
            .map(|e| e.shifted(by).ok_or(MidiError::BeatNotFinite(at)))
            .collect::<Result<Vec<_>>>()?;
        let events: Arc<ClipEvents> = Arc::new(events.into_iter().collect());
        self.install_clip(target, events)
    }

    /// Plays the clip `events` on `target`: the clip it has, replaced in
    /// place, or a new one fed into its event input.
    fn install_clip(&mut self, target: NodeKey, events: Arc<ClipEvents>) -> Result<()> {
        let clip = match self.clip(target) {
            Some(clip) => {
                self.publish_clip(clip, events)?;
                clip.erase()
            }
            None => {
                let (clip, _) = self.graph.add(MidiClipNode::new([]));
                // A clip that cannot take its events is not left in the
                // graph.
                if let Err(refused) = self.publish_clip(clip, events) {
                    // `remove` fails only for a key not in the graph, and
                    // this one was added above.
                    let removed = self.graph.remove(clip);
                    debug_assert!(removed.is_ok(), "the clip just added is in the graph");
                    return Err(refused);
                }
                let clip = clip.erase();
                // The clip's MIDI is classic MPE until the transform says
                // otherwise: it is the one holder of the zones.
                let (transform, _) = self.graph.add(MpeTransformNode::new(self.state.mpe()));
                let transform = transform.erase();
                self.graph.set_feed(
                    transform,
                    Feeder::Sequencer,
                    vec![EventOutPort::new(clip, 0)],
                );
                self.state.clips.insert(clip, Clip { transform, target });
                clip
            }
        };
        // Set on every call: `set_feed` leaves an unchanged feed (and the
        // edit flag) alone.
        let transform = self.state.clips[&clip].transform;
        self.graph.set_feed(
            target,
            Feeder::Sequencer,
            vec![EventOutPort::new(transform, 0)],
        );
        Ok(())
    }

    /// Plays a loaded track on `target` through its clip: its events from
    /// the transport's beat 0, with [`play_clip`](Self::play_clip)'s clip,
    /// looping and bounce behaviour. The clip starts at its own beats; a
    /// caller who wants an offset rebuilds the events and uses `play_clip`. The track is the one
    /// [`load_midi`](Self::load_midi) returned
    /// ([`LoadedMidi::track`](crate::LoadedMidi::track)); its events are built
    /// already, so this publishes a shared value and converts nothing.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoEventInput`] when `target` is not in the graph or takes
    /// no MIDI; [`MidiError::ClipEvents`] when the target's clip cannot be
    /// written (nothing changes).
    pub fn play_track(
        &mut self,
        target: impl Into<NodeKey>,
        track: &crate::LoadedTrack,
    ) -> Result<()> {
        let target = target.into();
        self.takes_midi(target)?;
        self.install_clip(target, Arc::clone(&track.events))
    }

    /// Empties `target`'s clip, ending the notes it left sounding. The clip
    /// stays, to play the next [`play_clip`](Self::play_clip).
    ///
    /// # Errors
    ///
    /// [`MidiError::NoClip`] when `target` has none;
    /// [`MidiError::ClipEvents`] when the clip's events cannot be written.
    #[must_use = "a target with no clip is an error"]
    pub fn stop_clip(&mut self, target: impl Into<NodeKey>) -> Result<()> {
        let target = target.into();
        match self.clip(target) {
            Some(clip) => self.publish_clip(clip, ClipEvents::default()),
            None => Err(MidiError::NoClip(target)),
        }
    }

    /// Writes `events` as the events of `clip`, through the clip's target.
    fn publish_clip(
        &self,
        clip: Key<MidiClipNode>,
        events: impl Into<Arc<ClipEvents>>,
    ) -> Result<()> {
        self.graph
            .value_table()
            .write(clip.events().to(events.into()))
            .map_err(MidiError::ClipEvents)
    }

    /// Returns `target`'s clip node, if it has one.
    #[must_use]
    pub fn clip(&self, target: impl Into<NodeKey>) -> Option<Key<MidiClipNode>> {
        let target = target.into();
        self.graph
            .feed(target, Feeder::Sequencer)
            .iter()
            .find_map(|s| {
                self.state
                    .clips
                    .iter()
                    .find_map(|(&clip, c)| (c.transform == s.node()).then_some(clip))
            })
            .map(Key::new)
    }

    /// The node of `ours` that `feeder` feeds `target` with: the feed is
    /// the truth (a host's `Graph::move_feeds` moves it), each entry's
    /// `target` only what [`retarget`](Self::retarget) unfeeds.
    fn fed_by<'s, V>(
        &self,
        target: NodeKey,
        feeder: Feeder,
        ours: &'s BTreeMap<NodeKey, V>,
    ) -> Option<(NodeKey, &'s V)> {
        self.graph
            .feed(target, feeder)
            .iter()
            .find_map(|s| ours.get_key_value(&s.node()))
            .map(|(&node, v)| (node, v))
    }

    /// Feeds clip `node` (one this session made) into `target` in place of
    /// the target it fed: a host whose target was re-bound to a new node keeps
    /// its clip. A target takes one clip: one it had is no longer fed. A live
    /// input moves with [`retarget_input`](Self::retarget_input).
    ///
    /// # Errors
    ///
    /// [`MidiError::NoEventInput`] when `target` is not in the graph or takes
    /// no MIDI; [`MidiError::NotSessionMidi`] when `node` is not a clip of
    /// this session's.
    pub fn retarget(&mut self, node: impl Into<NodeKey>, target: impl Into<NodeKey>) -> Result<()> {
        let node = node.into();
        let target = target.into();
        self.takes_midi(target)?;
        let Some(clip) = self.state.clips.get_mut(&node) else {
            return Err(MidiError::NotSessionMidi(node));
        };
        let old = std::mem::replace(&mut clip.target, target);
        let transform = clip.transform;
        self.refeed(transform, old, target, Feeder::Sequencer);
        Ok(())
    }

    /// Feeds the live input of `from` into `to` in place of it, a host's
    /// re-bind of the target (`from` may be gone from the graph already): the
    /// same inlet and keys, now playing `to`. A target takes one live input:
    /// one `to` had is no longer fed.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoEventInput`] when `to` is not in the graph or takes no
    /// MIDI; [`MidiError::NoLiveInput`] when `from` has no live input.
    pub fn retarget_input(
        &mut self,
        from: impl Into<NodeKey>,
        to: impl Into<NodeKey>,
    ) -> Result<()> {
        let from = from.into();
        let to = to.into();
        self.takes_midi(to)?;
        let Some((&node, live)) = self.state.live.iter_mut().find(|(_, l)| l.target == from) else {
            return Err(MidiError::NoLiveInput(from));
        };
        live.target = to;
        self.refeed(node, from, to, Feeder::LiveInput);
        Ok(())
    }

    /// Moves the feed of `node`'s event output from `old` to `target`.
    fn refeed(&mut self, node: NodeKey, old: NodeKey, target: NodeKey, feeder: Feeder) {
        if old != target
            && self
                .graph
                .feed(old, feeder)
                .contains(&EventOutPort::new(node, 0))
        {
            self.graph.set_feed(old, feeder, Vec::new());
        }
        self.graph
            .set_feed(target, feeder, vec![EventOutPort::new(node, 0)]);
    }

    // --- Routes ---

    /// Adds a rule routing the hardware input to its targets. Returns its
    /// id, for [`unroute`](Self::unroute).
    ///
    /// The routes have one declarer: these rules and
    /// [`set_route_fallback`](Self::set_route_fallback), or a host's
    /// [`set_routes`](Self::set_routes).
    ///
    /// # Errors
    ///
    /// [`MidiError::TwoRouteDeclarers`] while a host's `set_routes` declares
    /// the routes, and [`MidiError::NoEventInput`] naming a target that is not
    /// in the graph or takes no MIDI. Nothing changes.
    pub fn route(&mut self, rule: MidiRouteRule) -> Result<MidiRouteId> {
        self.own_may_declare()?;
        for &target in &rule.targets {
            self.takes_midi(target)?;
        }
        let id = MidiRouteId::mint(&self.state.minted);
        self.state.rules.insert(id, rule);
        self.compile_session_routes();
        Ok(id)
    }

    /// Removes a rule [`route`](Self::route) added.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoSuchRoute`] when there is no rule `id` (removed already, or
    /// never added); nothing changes.
    pub fn unroute(&mut self, id: MidiRouteId) -> Result<()> {
        if self.state.rules.remove(&id).is_none() {
            return Err(MidiError::NoSuchRoute(id));
        }
        self.compile_session_routes();
        Ok(())
    }

    /// Where an event matching no rule goes, if anywhere: every port no
    /// rule covers. `None` (the default) drops unmatched events.
    ///
    /// # Errors
    ///
    /// [`MidiError::TwoRouteDeclarers`] while a host's
    /// [`set_routes`](Self::set_routes) declares the routes, and
    /// [`MidiError::NoEventInput`] naming a fallback that is not in the graph
    /// or takes no MIDI. Nothing changes.
    pub fn set_route_fallback(&mut self, fallback: Option<impl Into<NodeKey>>) -> Result<()> {
        self.own_may_declare()?;
        let fallback = fallback.map(Into::into);
        if let Some(target) = fallback {
            self.takes_midi(target)?;
        }
        self.state.fallback = fallback;
        self.compile_session_routes();
        Ok(())
    }

    fn own_may_declare(&self) -> Result<()> {
        if self.state.host_routes {
            Err(MidiError::TwoRouteDeclarers(RouteDeclarer::Host))
        } else {
            Ok(())
        }
    }

    /// The layout of the input ports: the first Group's with no MIDI nodes,
    /// where nothing is compiled.
    fn input_ports(&self) -> MidiInputPorts {
        self.nodes
            .map_or(MidiInputPorts::FIRST_GROUP, |n| n.input_ports)
    }

    /// Compiles the session's own rules onto every input port.
    pub(crate) fn compile_session_routes(&mut self) {
        if self.state.host_routes {
            return;
        }
        let layout = self.input_ports();
        self.state.compile_session_feeds(self.graph, layout);
    }

    /// Routes `input` (a `MidiInputNode`: one event output per channel and
    /// one for channelless messages, in each Group it hears) by `rules`, keyed by the host's own
    /// `T` and resolved to nodes through `resolve`, replacing every route
    /// before (the session's own rules included).
    ///
    /// A channel rule gives its targets that channel's port in its Group and
    /// the Group's channelless one, an any-channel rule all of them, several rules the
    /// union (`tutti_midi_runtime::compile_routes`); the fallback takes
    /// every port no armed rule covers. A target `resolve` maps to no node,
    /// or to one not in the graph, takes nothing (and still covers its
    /// rule's ports); one fed before and not now is unwired.
    ///
    /// The routes have one declarer: while this declares any rule or a
    /// fallback, the session's own [`route`](Self::route) and
    /// [`set_route_fallback`](Self::set_route_fallback) are refused; a call
    /// with neither hands the routes back.
    ///
    /// # Errors
    ///
    /// [`MidiError::TwoRouteDeclarers`] while the session's own rules or
    /// fallback are declared ([`unroute`](Self::unroute) them first), and
    /// [`MidiError::NotAnInput`] when `input` is not in the graph or has no event
    /// output. Nothing is changed. Those are the only refusals: any node with event
    /// outputs may be the source (a host's own `MidiInputNode`, or a stand-in
    /// that sends on the input layout's ports), because a host routes from
    /// sources the session did not make, so a port's input node is not required.
    pub fn set_routes<'r, T: Copy + Ord + 'r>(
        &mut self,
        input: impl Into<NodeKey>,
        rules: impl IntoIterator<Item = &'r MidiRouteRule<T>>,
        fallback: Option<T>,
        resolve: impl Fn(T) -> Option<NodeKey>,
    ) -> Result<()> {
        let input = input.into();
        if !self
            .graph
            .shape(input)
            .is_some_and(|shape| shape.event_out > 0)
        {
            return Err(MidiError::NotAnInput(input));
        }
        if !self.state.rules.is_empty() || self.state.fallback.is_some() {
            return Err(MidiError::TwoRouteDeclarers(RouteDeclarer::Session));
        }
        let rules: Vec<&MidiRouteRule<T>> = rules.into_iter().collect();
        self.state.host_routes = !rules.is_empty() || fallback.is_some();
        self.compile_routes(input, rules, fallback, resolve);
        Ok(())
    }

    /// `rules` onto `input`'s ports, replacing every route before (the
    /// session's own rules included).
    fn compile_routes<'r, T: Copy + Ord + 'r>(
        &mut self,
        input: NodeKey,
        rules: impl IntoIterator<Item = &'r MidiRouteRule<T>>,
        fallback: Option<T>,
        resolve: impl Fn(T) -> Option<NodeKey>,
    ) {
        let layout = self.input_ports();
        compile_feeds(
            self.graph,
            &mut self.state.routed,
            layout,
            &[input],
            rules,
            fallback,
            resolve,
        );
    }

    // --- Tapping what is played ---

    /// Taps what is played into `target`: an event tap on the event output
    /// of the inlet that feeds it, which every note sent through its keys
    /// passes. For an instrument with a handle
    /// ([`Session::add_instrument`](crate::Session::add_instrument),
    /// [`Session::instrument`](crate::Session::instrument)) that inlet is the
    /// handle's own queue, so **what the handle's
    /// [`keys`](crate::InstrumentHandle::keys_mut) play is recorded**; any
    /// other target is tapped on its live input ([`input`](Self::input),
    /// opened when it has none). A reader of
    /// `capacity` events (a non-note-off is refused once that many are held, a
    /// note-off is kept up to [`note_off_room`](tutti_graph::note_off_room) of
    /// it; both counted by [`EventTapReader::take_lost`]).
    ///
    /// Read it with [`EventTapReader::drain`] from a thread of your own. Each
    /// event's [`frame`](TapEvent::frame) is the frame of the graph's clock it
    /// was due on, as it left the inlet and before the target heard it; where
    /// a take places it on the timeline is
    /// [`TakeLatency::place`](crate::TakeLatency::place) (the frame minus the input and
    /// output latency and the inlet's lead, plus the user's trim).
    ///
    /// The tap is on **the keys' inlet only**: a clip ([`play_clip`](Self::play_clip))
    /// is already on the timeline, and notes a hardware input routes straight to
    /// `target` ([`route`](Self::route)) do not pass it. To record a
    /// hardware player, tap the input node's port with `Graph::taps`, or
    /// send what its device pushes through the keys.
    ///
    /// The tap is closed when its reader is dropped (the next
    /// [`Reclaim`](crate::Phase::Reclaim) phase) or with
    /// `Graph::taps().close_event`. A re-prepare at another rate reopens the
    /// session's own taps only (the outputs'); reopen this one with
    /// `Graph::taps().reopen_event`.
    ///
    /// # Errors
    ///
    /// [`MidiError::NoEventInput`] when `target` is not in the graph or takes no MIDI,
    /// and [`MidiError::Tap`] when the live input's port is gone from the graph.
    pub fn tap(
        &mut self,
        target: impl Into<NodeKey>,
        capacity: tutti_graph::EventCapacity,
    ) -> Result<EventTapReader> {
        let target = target.into();
        let node = if let Some(queue) = self.instrument_queue(target) {
            self.takes_midi(target)?;
            queue
        } else {
            self.live_inlet(target)?
        };
        let source = EventOutPort::new(node, 0);
        self.graph
            .taps()
            .open_event(
                source,
                EventTapOptions::new(capacity).align(tutti_graph::TapAlign::Raw),
            )
            .map_err(MidiError::Tap)
    }

    // --- Following an outside tempo ---

    /// A tap on the channelless port of `group` of the input port `input`, for a
    /// follower to read.
    fn follow_tap(
        &mut self,
        input: NodeKey,
        group: MidiGroup,
        capacity: tutti_graph::EventCapacity,
    ) -> Result<EventTapReader> {
        if !self.state.is_input_node(input) {
            return Err(MidiError::NotAnInput(input));
        }
        let port = self
            .input_ports()
            .channelless(group)
            .ok_or(MidiError::GroupNotHeard(group))?;
        self.graph
            .taps()
            .open_event(
                EventOutPort::new(input, port),
                EventTapOptions::new(capacity).align(tutti_graph::TapAlign::Raw),
            )
            .map_err(MidiError::Tap)
    }

    /// Follows the tempo (and, if `follow.meter`, the time signature) the session's MIDI
    /// input's `follow.group` sends as Flex Data Set Tempo and Set Time Signature
    /// (M2-104 §7.5.3, §7.5.4): the session reads them from an event tap on
    /// that Group's channelless port in each
    /// [`Poll`](crate::Phase::Poll) phase and publishes them to the transport,
    /// reported as [`MidiReport::TempoFollowed`] and [`MidiReport::MeterFollowed`].
    /// **Off by default**: until this is called, a Flex tempo changes nothing.
    ///
    /// The source is Flex Set Tempo on one Group; MIDI Beat Clock and MIDI Time
    /// Code are followed by [`follow_clock`](Self::follow_clock).
    /// **It overlays the project's maps; it does not replace them.** The tempo
    /// map and meter the project has now are kept; from the beat of each change,
    /// the published map is the project's up to there and the live steps after,
    /// so a project point or ramp ahead of the playhead is not played while
    /// following (a ramp a step lands in is cut at its beat). The session has no
    /// project tempo apart from the published map, so **the map it finds now is
    /// the one [`unfollow_tempo`](Self::unfollow_tempo) restores**. While
    /// following, a write to the tempo target refuses with
    /// [`SetRefusal::Following`](crate::SetRefusal::Following), so nothing else
    /// publishes in between.
    ///
    /// A change takes effect at the start of the next block (a meter at the next
    /// bar line at or after it), at the latest one block and one update after it
    /// was sent; a device that needs a tempo change on a beat sends it ahead,
    /// which Flex allows (a Flex message carries no time of its own). The
    /// transport's beat stays continuous when a publish lands late.
    ///
    /// **One follower at a time**: following again replaces the first. The
    /// project's maps stay the ones found by the first call, and the tempo in
    /// force stays until the new source sends one.
    ///
    /// # Errors
    ///
    /// [`MidiError::NotAnInput`] when `follow.input` is not an input port's input node,
    /// [`MidiError::GroupNotHeard`] for a Group the input does not hear,
    /// [`MidiError::Tap`] when the tap cannot open because the host removed the node, and
    /// [`MidiError::TempoSourceTaken`] while a Beat Clock
    /// [`follow_clock`](Self::follow_clock) publishes the tempo. Nothing changes.
    pub fn follow_tempo(&mut self, follow: FollowTempo) -> Result<()> {
        if self
            .state
            .clock
            .as_ref()
            .is_some_and(ClockFollow::owns_tempo)
        {
            return Err(MidiError::TempoSourceTaken);
        }
        let tap = self.follow_tap(follow.input, follow.group, FOLLOW_TAP_CAPACITY)?;
        let core = tutti_midi_runtime::TempoFollower::new(
            tutti_midi_runtime::TempoSource::Flex(follow.group),
            follow.meter,
        );
        self.state.follow = Some(match self.state.follow.take() {
            Some(first) => first.replaced_by(core, tap),
            None => Follow::new(core, tap, self.settings.tempo_map(), self.meter.current()),
        });
        self.state.sync_following();
        self.discard_staged_maps(follow.meter);
        Ok(())
    }

    /// Drops the tempo (and, with `meter`, the meter) a write staged and no
    /// update has published: a follower owns the maps from here, and the
    /// project's are the ones in force, so the staged ones must not publish
    /// over what an unfollow restores.
    fn discard_staged_maps(&self, meter: bool) {
        use crate::value::SlotId;
        let ids: &[SlotId] = if meter {
            &[SlotId::Tempo, SlotId::Meter]
        } else {
            &[SlotId::Tempo]
        };
        self.graph.value_table().discard_pending(ids);
    }

    /// Stops following ([`follow_tempo`](Self::follow_tempo)): publishes the
    /// project's tempo map and meter again, for the ones the follower changed.
    /// The transport takes them at the next block at its current beat, so the
    /// playhead does not jump, and the tempo from there on is the project's.
    /// Reported as [`MidiReport::TempoUnfollowed`] by the next update.
    ///
    /// # Errors
    ///
    /// [`MidiError::NotFollowing`] when nothing is followed.
    pub fn unfollow_tempo(&mut self) -> Result<()> {
        let follow = self.state.follow.take().ok_or(MidiError::NotFollowing)?;
        self.state.sync_following();
        let (tempo, meter) = follow.restore();
        if let Some(tempo) = tempo {
            self.settings.set_tempo_map(tempo);
        }
        if let Some(meter) = meter {
            self.meter.set_meter(meter);
        }
        self.state.follow_events.push(MidiReport::TempoUnfollowed);
        Ok(())
    }

    // --- Following an outside clock ---

    /// Follows the MIDI clock `follow.kind` that the session's MIDI input's
    /// `follow.group` sends, and drives the transport with it. The session reads
    /// it from an event tap on that Group's channelless port in each
    /// [`Poll`](crate::Phase::Poll) phase. **Off by default**: until this is
    /// called, a Timing Clock or a Quarter Frame changes nothing.
    ///
    /// **[`ClockKind::BeatClock`](tutti_midi_runtime::ClockKind::BeatClock)**:
    /// the tempo fit from the Timing Clocks' spacing is published into the
    /// tempo map as [`follow_tempo`](Self::follow_tempo)'s is (and reported as
    /// [`MidiReport::TempoFollowed`]), Start plays from beat 0, Continue plays
    /// from where the source counted to, Stop stops, and a Song Position
    /// Pointer locates to its sixteenth notes (M1 pp.27 to 31).
    ///
    /// **[`ClockKind::Mtc`](tutti_midi_runtime::ClockKind::Mtc)**: the position
    /// Quarter Frames spell drives the playhead, `00:00:00:00` being beat 0 on
    /// the tempo map in force. The transport plays once the follower locks,
    /// locates when the source jumps, and a Full Message locates.
    ///
    /// **Lock** is reported as [`MidiReport::ClockLocked`] and
    /// [`MidiReport::ClockUnlocked`]; the rules and constants are those of
    /// `tutti_midi_runtime::ClockFollower`. A source silent for its dropout
    /// unlocks and stops the transport it was rolling.
    ///
    /// A command is placed on the frame its message arrived on, 20 ms on, and
    /// never sooner than a declick past the next block (`At::Frame`), with the source's
    /// position advanced by the time since the message arrived, so a source that
    /// kept rolling is not trailed. **A Beat
    /// Clock follower owns the tempo map** as `follow_tempo` does: the map found
    /// now is the one [`unfollow_clock`](Self::unfollow_clock) restores, and
    /// `Session::set_tempo` refuses while it follows.
    ///
    /// **Drift is corrected.** In each poll while the transport rolls, the
    /// beat the source counts is compared with the beat the transport reaches
    /// at the next block ([`clock_error`](Self::clock_error)), and the tempo
    /// published is the fit trimmed to close the error in `ClockFollower::CATCH_UP`
    /// (4 seconds), by at most 2% of the fit. A trim goes into the map as any
    /// tempo does, a step of at least `ClockFollower::TEMPO_STEP` and one point
    /// a beat at most, so an error stays within `TEMPO_STEP * CATCH_UP / 60`
    /// (0.0067 beat) and the fit's noise. An error past `ClockFollower::SLIP` (a
    /// quarter of a beat) locates to the source's beat once and is reported as
    /// [`MidiReport::ClockSlipped`]. MTC has no tempo to trim, and the same slip rule
    /// reads its time against the transport's at `ClockFollower::MTC_SLIP_FRAMES` (2
    /// frames). A source off nominal speed is followed by those locates alone; MTC
    /// running backward stops the transport ([`MidiReport::ClockReversed`]).
    ///
    /// **One clock at a time**: following again replaces the first.
    ///
    /// # Errors
    ///
    /// [`MidiError::NotAnInput`] when `follow.input` is not an input port's input node,
    /// [`MidiError::GroupNotHeard`] for a Group the input does not hear,
    /// [`MidiError::Tap`] when the tap cannot open because the host removed the node, and
    /// [`MidiError::TempoSourceTaken`] for a Beat Clock follow while
    /// [`follow_tempo`](Self::follow_tempo) is active. Nothing changes.
    pub fn follow_clock(&mut self, follow: FollowClock) -> Result<()> {
        if follow.kind == tutti_midi_runtime::ClockKind::BeatClock && self.state.follow.is_some() {
            return Err(MidiError::TempoSourceTaken);
        }
        let tap = self.follow_tap(follow.input, follow.group, CLOCK_TAP_CAPACITY)?;
        let core = tutti_midi_runtime::ClockFollower::new(follow.kind);
        // A second follow replaces the first; the maps the first overlaid are
        // the project's again before the second takes them.
        if let Some(first) = self.state.clock.take() {
            self.restore_maps(first.restore());
        }
        self.state.clock = Some(ClockFollow::new(
            core,
            tap,
            self.settings.tempo_map(),
            self.meter.current(),
        ));
        self.state.sync_following();
        if follow.kind == tutti_midi_runtime::ClockKind::BeatClock {
            self.discard_staged_maps(true);
        }
        Ok(())
    }

    /// The error of the clock the session follows: the source's position less
    /// the transport's, read at the last [`Poll`](crate::Phase::Poll) phase
    /// (positive when the transport is behind), in beats for Beat Clock and in
    /// seconds for MTC. `None` when no clock is followed, or the follower could
    /// not read one: not locked, not rolling, or a locate that has not landed
    /// yet.
    #[must_use]
    pub fn clock_error(&self) -> Option<tutti_midi_runtime::ClockOffset> {
        self.state.clock.as_ref().and_then(ClockFollow::error)
    }

    /// Stops following ([`follow_clock`](Self::follow_clock)): the transport
    /// keeps the motion it had, and the project's tempo map and meter are
    /// published again for the ones a Beat Clock follower changed. Reported as
    /// [`MidiReport::ClockUnfollowed`] by the next update.
    ///
    /// # Errors
    ///
    /// [`MidiError::NotFollowingClock`] when no clock is followed.
    pub fn unfollow_clock(&mut self) -> Result<()> {
        let clock = self
            .state
            .clock
            .take()
            .ok_or(MidiError::NotFollowingClock)?;
        self.state.sync_following();
        self.restore_maps(clock.restore());
        self.state.follow_events.push(MidiReport::ClockUnfollowed);
        Ok(())
    }

    /// Publishes the maps a follower changed, as the project had them.
    fn restore_maps(&self, (tempo, meter): (Option<Arc<TempoMap>>, Option<Arc<MeterMap>>)) {
        if let Some(tempo) = tempo {
            self.settings.set_tempo_map(tempo);
        }
        if let Some(meter) = meter {
            self.meter.set_meter(meter);
        }
    }

    // --- Machine Control ---

    /// Follows the MIDI Machine Control (RP-013) commands the session's MIDI
    /// input's `follow.group` sends to `follow.device`, to a group it was
    /// assigned to or to the all-call, and drives the transport with them. The
    /// session reads them from an event tap on that Group's channelless port in
    /// each [`Poll`](crate::Phase::Poll) phase. **Off by default**: until this
    /// is called, a Machine Control command changes nothing.
    ///
    /// **A separate follower from [`follow_clock`](Self::follow_clock)**, not a
    /// kind of clock: MMC says what to do and carries neither a tempo nor a
    /// running position, so it owns no part of the tempo map and composes with
    /// an MTC or Beat Clock follower (MTC for the position, MMC for the
    /// buttons, as RP-013 p.76 describes). Each reads its own tap.
    ///
    /// STOP and PAUSE stop the transport, PLAY and DEFERRED PLAY play it, and
    /// LOCATE locates to its time code (`00:00:00:00` is beat 0 on the tempo map
    /// in force, as for MTC, and a drop-frame label is counted as SMPTE counts
    /// it). A LOCATE and a play in one message locate and then play. Every
    /// command is placed at the next block (`At::NextBlock`). A command the
    /// transport has no counterpart for is reported as
    /// [`MidiReport::MachineControlIgnored`]; no response is sent. COMMAND SEGMENT
    /// strings are reassembled first, and a segment that does not belong is
    /// [`MidiReport::MachineControlSegmentation`].
    ///
    /// **One at a time**: following again replaces the first, forgetting the
    /// groups it was assigned to.
    ///
    /// # Errors
    ///
    /// [`MidiError::NotAnInput`] when `follow.input` is not an input port's input node and
    /// [`MidiError::GroupNotHeard`] for a Group the input does not hear, and
    /// [`MidiError::Tap`] when the tap cannot open because the host removed the node.
    /// Nothing changes.
    pub fn follow_machine_control(&mut self, follow: FollowMachineControl) -> Result<()> {
        let tap = self.follow_tap(follow.input, follow.group, MACHINE_TAP_CAPACITY)?;
        self.state.machine = Some(MachineFollow::new(
            tutti_midi_runtime::MachineControlFollower::new(follow.device),
            tap,
        ));
        Ok(())
    }

    /// Stops following Machine Control ([`follow_machine_control`](Self::follow_machine_control)):
    /// the transport keeps the motion it had. Reported as
    /// [`MidiReport::MachineControlUnfollowed`] by the next update.
    ///
    /// # Errors
    ///
    /// [`MidiError::NotFollowingMachineControl`] when nothing is followed.
    pub fn unfollow_machine_control(&mut self) -> Result<()> {
        self.state
            .machine
            .take()
            .ok_or(MidiError::NotFollowingMachineControl)?;
        self.state
            .follow_events
            .push(MidiReport::MachineControlUnfollowed);
        Ok(())
    }

    // --- Outputs ---

    /// Adds an output node to the graph and opens its tap: the node is fed by
    /// the session's sync node (every output carries it), and the tap is aligned to
    /// the outputs' time so MIDI leaves with the audio. What a host wires to
    /// the node's event input is read from the tap. The departure moves the node's
    /// generation on when its last handle drops.
    pub(crate) fn add_output_node(&mut self) -> (NodeKey, EventTapReader, Departure) {
        let (departure, generation) = Departure::new();
        let (key, _) = self.graph.add(tutti_graph::ForkByClone(
            tutti_midi_runtime::MidiOutNode::new().following(&generation),
        ));
        let node = key.erase();
        if let Some(nodes) = self.nodes {
            self.graph
                .set_feed(node, Feeder::Clock, vec![EventOutPort::new(nodes.sync, 0)]);
        }
        let tap = self
            .graph
            .taps()
            .open_event(
                EventOutPort::new(node, 0),
                EventTapOptions::new(MIDI_OUT_TAP_CAPACITY),
            )
            .expect("the output node is in the graph with its event output");
        (node, tap, departure)
    }

    /// The outputs' own latency and offset, a target of
    /// [`Session::set`](crate::Session::set): what a MIDI device takes to
    /// sound an event (it is sent that much earlier) and a trim on top
    /// (positive sends later), added to the terms every event is timed by
    /// ([`output_terms`](Self::output_terms)). A write applies from the next
    /// drain, to every sink and output port: it is one setting
    /// of the session, not an output's.
    #[must_use]
    pub const fn output_latency(&self) -> crate::OutputLatencyTarget {
        crate::OutputLatencyTarget::new()
    }

    /// The terms every output's events are timed by now: the audio
    /// output's latency as the device reports it (zero on a system clock, which
    /// has no device), and the output's own
    /// ([`output_latency`](Self::output_latency)). `None` on an offline
    /// session, which has no clock of its own and sends every event at once.
    ///
    /// **While [`is_complete`](OutputTerms::is_complete) is false the audio
    /// output's latency is not reported yet** (before the stream's first
    /// callback, and again after a restart): events are timed with the terms
    /// known, so they leave early by that latency.
    #[must_use]
    pub fn output_terms(&self) -> Option<OutputTerms> {
        let source = self.state.output_clock.as_ref()?;
        Some(OutputTerms {
            audio_output: source.audio_output(),
            latency: self.state.output_latency.0.current(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tutti_fixtures::midi;
    use tutti_units::SampleRate;

    /// **A device's inlet holds at least a second of a saturated MIDI 1.0
    /// cable** (MIDI 1.0 Detailed Specification, Hardware: a byte every 320
    /// microseconds, so a two-byte message every 640 at best): the figure of
    /// [`MIDI_INPUT_INLET_CAPACITY`], derived here from the spec's timing.
    /// An output's one cell keeps tap losses and refusals apart: the reader
    /// answers both, each in its own count.
    #[test]
    fn an_outputs_lost_and_refused_events_read_back_as_their_own_counts() {
        let (mut writers, readers) = loss_cells();
        writers.lost(3);
        writers.refused(2);
        writers.refused(1);
        assert_eq!(
            readers.get(),
            tutti_graph::Losses::lost(3) + tutti_graph::Losses::refused(3)
        );
    }

    #[cfg(feature = "midi-hardware")]
    #[test]
    fn the_inlet_holds_a_second_of_a_saturated_midi1_cable() {
        let shortest_message = Duration::from_micros(2 * 320);
        // 1,563 messages.
        let in_a_second = Duration::from_secs(1).as_micros() / shortest_message.as_micros() + 1;
        assert!(u128::from(MIDI_INPUT_INLET_CAPACITY.get()) >= in_a_second);
    }

    /// Counts what reached the wire. Stands in for a real OS endpoint so
    /// these tests run on every platform.
    struct CountingSink(Arc<AtomicUsize>);
    impl tutti_midi_runtime::MidiOut for CountingSink {
        fn queue_timed(&self, batch: &[tutti_midi_runtime::Outgoing]) -> tutti_midi_runtime::Taken {
            let events: Vec<tutti_graph::UmpAt> = batch.iter().map(|e| (e.at, e.ump)).collect();
            self.0.fetch_add(events.len(), Ordering::SeqCst);
            tutti_midi_runtime::Taken::within(batch, batch.len())
        }
    }

    fn out_with_counter(stamping: Stamping) -> (UmpOut, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let mut out = UmpOut::new(Arc::new(CountingSink(count.clone())), SampleRate::SR_48K);
        out.set_stamping(stamping);
        (out, count)
    }

    fn note(offset: usize) -> UmpAt {
        (
            Offset::of(Samples::new(offset), Samples::new(512)),
            midi::note_on(MidiChannel::FIRST, 60, 0x8000),
        )
    }

    /// Both drains stamp through one stream, so the origin keeps climbing
    /// across them.
    #[test]
    fn both_producers_advance_one_origin() {
        let (mut out, _) = out_with_counter(Stamping::Jr);

        out.send_batch(&[note(511)]);
        assert_eq!(out.origin(), Frame::new(512), "one 512-frame block");

        out.send_batch(&[note(511)]);
        assert_eq!(
            out.origin(),
            Frame::new(1024),
            "the second producer continues from the first, it does not restart"
        );
    }

    /// **Stamping off must still send.** Stamping gates timestamps, not
    /// delivery: there is no second, unstamped transport to fall through
    /// to.
    #[test]
    fn stamping_disabled_still_delivers_events() {
        let (mut out, count) = out_with_counter(Stamping::Off);
        out.send_batch(&[note(0), note(1)]);
        assert_eq!(
            count.load(Ordering::SeqCst),
            2,
            "events must reach the sink with stamping off"
        );
        assert_eq!(
            out.origin(),
            Frame::new(0),
            "and the stream must not advance when it did not stamp"
        );
    }

    /// With stamping on, more messages reach the wire than were handed in
    /// (the JR Timestamp prefixes): what makes the test above non-vacuous.
    #[test]
    fn stamping_enabled_adds_timestamp_messages() {
        let (mut out, count) = out_with_counter(Stamping::Jr);
        out.send_batch(&[note(0)]);
        assert!(
            count.load(Ordering::SeqCst) > 1,
            "a stamped send carries its JR Timestamp prefix too, got {}",
            count.load(Ordering::SeqCst)
        );
    }

    /// An open output takes the batch whole: both events reach the sink and
    /// none is refused.
    #[test]
    fn sending_to_an_open_output_refuses_nothing() {
        let (mut out, count) = out_with_counter(Stamping::Off);
        let refused = out.send_frames(&[at(0), at(1)], None);
        assert_eq!(count.load(Ordering::SeqCst), 2, "both reached the sink");
        assert_eq!(refused, 0, "nothing was dropped");
    }

    /// An event on a frame of the graph's clock.
    fn at(frame: u64) -> (Frame, Ump) {
        (Frame::new(frame), note(0).1)
    }

    /// Keeps every batch it is handed.
    struct Recording(Arc<std::sync::Mutex<Vec<Vec<UmpAt>>>>);
    impl tutti_midi_runtime::MidiOut for Recording {
        fn queue_timed(&self, batch: &[tutti_midi_runtime::Outgoing]) -> tutti_midi_runtime::Taken {
            let events: Vec<tutti_graph::UmpAt> = batch.iter().map(|e| (e.at, e.ump)).collect();
            self.0.lock().unwrap().push(events.to_vec());
            tutti_midi_runtime::Taken::within(batch, batch.len())
        }
    }

    /// **A batch is read as a block from its earliest frame to its last,
    /// inclusive.** Frames 1 000, 1 250 and 1 999 (given out of order) reach
    /// the sink as offsets 250, 0 and 999 of a 1 000-frame span, and a
    /// stamping output advances its origin by that span: 1 000 frames.
    #[test]
    fn a_batch_of_frames_is_a_block_from_its_first_frame() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut out = UmpOut::new(Arc::new(Recording(log.clone())), SampleRate::SR_48K);
        out.send_frames(&[at(1_250), at(1_000), at(1_999)], None);
        let sent = log.lock().unwrap().remove(0);
        let span = Samples::new(1_000);
        let offsets: Vec<_> = sent.iter().map(|e| e.0).collect();
        assert_eq!(
            offsets,
            [250, 0, 999].map(|o| Offset::of(Samples::new(o), span)),
            "each event's spacing from the earliest"
        );
        out.set_stamping(Stamping::Jr);
        let before = out.origin();
        out.send_frames(&[at(1_250), at(1_000), at(1_999)], None);
        assert_eq!(
            out.origin().since(before),
            Some(Samples::new(1_000)),
            "the span"
        );
    }

    /// **Events sharing a frame are sent at that frame's time, whichever
    /// event of the frame a send is looked up by.** The send time is a pure
    /// function of the frame, and an offset is the frame less the batch's
    /// first, so one offset names one frame and one time: three events on two
    /// frames, with stamping on, each go out at the time its own frame has.
    #[test]
    fn events_on_one_frame_share_that_frames_send_time() {
        let t0 = Instant::now() + Duration::from_secs(60);
        let (mut out, log) = timed_out();
        out.set_stamping(Stamping::Jr);
        let timing = timing_at(t0, OutputTerms::default());
        let map = timing.map.unwrap();
        out.send_frames(&[at(1_000), at(2_000), at(1_000)], Some(&timing));
        let sent = taken(&log);
        let notes: Vec<_> = sent.iter().filter(|e| e.ump == note(0).1).collect();
        assert_eq!(notes.len(), 3);
        for (note, frame) in notes.iter().zip([1_000, 2_000, 1_000]) {
            assert_eq!(
                note.send,
                SendAt::At(map.os_at(Frame::new(frame)).unwrap()),
                "frame {frame}"
            );
        }
    }

    /// Keeps every batch's events with the time each was sent at.
    struct Timed(Arc<std::sync::Mutex<Vec<Outgoing>>>);
    impl tutti_midi_runtime::MidiOut for Timed {
        fn queue_timed(&self, events: &[Outgoing]) -> tutti_midi_runtime::Taken {
            self.0.lock().unwrap().extend_from_slice(events);
            tutti_midi_runtime::Taken::within(events, events.len())
        }
    }

    fn timing_at(t0: Instant, terms: OutputTerms) -> OutputTiming {
        OutputTiming {
            map: Some(ClockMap::new(
                SampleRate::SR_48K,
                tutti_midi_runtime::ClockReading::new(t0, Frame::ZERO),
            )),
            now: t0 - Duration::from_millis(10),
            terms,
        }
    }

    fn taken(log: &Arc<std::sync::Mutex<Vec<Outgoing>>>) -> Vec<Outgoing> {
        log.lock().unwrap().clone()
    }

    fn timed_out() -> (UmpOut, Arc<std::sync::Mutex<Vec<Outgoing>>>) {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let out = UmpOut::new(Arc::new(Timed(log.clone())), SampleRate::SR_48K);
        (out, log)
    }

    /// **Events are sent at the instants their frames have, spaced as the
    /// frames are.** Ticks 1 000 frames apart at 48 kHz are 20.833 ms apart in
    /// `SendAt::At`, and the first is where the clock puts frame 1 000.
    #[test]
    fn frames_are_sent_at_their_instants_spaced_by_the_rate() {
        let t0 = Instant::now() + Duration::from_secs(60);
        let (mut out, log) = timed_out();
        let timing = timing_at(t0, OutputTerms::default());
        let refused = out.send_frames(&[at(1_000), at(2_000), at(3_000)], Some(&timing));
        assert_eq!(refused, 0);
        let sends: Vec<SendAt> = log.lock().unwrap().iter().map(|e| e.send).collect();
        let SendAt::At(first) = sends[0] else {
            panic!("not scheduled: {sends:?}")
        };
        assert_eq!(Some(first), timing.map.unwrap().os_at(Frame::new(1_000)));
        // 1,000 frames at 48 kHz are 20.833 ms.
        let spacing = Duration::from_secs_f64(1_000.0 / 48_000.0);
        for pair in sends.windows(2) {
            let (SendAt::At(a), SendAt::At(b)) = (pair[0], pair[1]) else {
                panic!("not scheduled: {pair:?}")
            };
            let off = (b - a).abs_diff(spacing);
            assert!(off < Duration::from_micros(1), "{:?} off", off);
        }
    }

    /// **An output's offset shifts every event by exactly it**, and the audio
    /// latency and the device's go in as the plan says.
    #[test]
    fn the_terms_shift_every_event() {
        let t0 = Instant::now() + Duration::from_secs(60);
        let sent_with = |terms: OutputTerms| {
            let (mut out, log) = timed_out();
            out.send_frames(&[at(1_000), at(2_000)], Some(&timing_at(t0, terms)));
            taken(&log)
        };
        let base = sent_with(OutputTerms::default());
        let terms = OutputTerms {
            audio_output: Some(tutti_units::Seconds::new(0.03)),
            latency: OutputLatency::new(
                tutti_units::Seconds::new(0.005),
                tutti_midi_runtime::OutputOffset::new(tutti_units::Seconds::new(0.25)).unwrap(),
            )
            .unwrap(),
        };
        let shifted = sent_with(terms);
        for (a, b) in base.iter().zip(&shifted) {
            let (SendAt::At(a), SendAt::At(b)) = (a.send, b.send) else {
                panic!("not scheduled")
            };
            assert_eq!(b, a + Duration::from_millis(30 - 5 + 250));
        }
    }

    /// **A JR stamp is sent with its event**, so a stamping output schedules
    /// the pair together and the stamps keep the spacing.
    #[test]
    fn a_jr_stamp_is_sent_at_its_events_time() {
        let t0 = Instant::now() + Duration::from_secs(60);
        let (mut out, log) = timed_out();
        out.set_stamping(Stamping::Jr);
        let timing = timing_at(t0, OutputTerms::default());
        out.send_frames(&[at(1_000), at(2_000)], Some(&timing));
        let sent = log.lock().unwrap().clone();
        let events: Vec<&Outgoing> = sent
            .iter()
            .filter(|e| e.ump == note(0).1 || e.ump.message_type() == 0)
            .collect();
        assert!(events.len() >= 4, "a stamp for each event");
        for pair in sent.windows(2) {
            if pair[0].ump.message_type() == 0 && pair[1].ump == note(0).1 {
                assert_eq!(pair[0].send, pair[1].send, "a stamp rides with its event");
            }
        }
        assert!(
            sent.iter()
                .all(|e| matches!(e.send, SendAt::At(_)) || e.ump.message_type() == 0),
            "every note is scheduled"
        );
    }

    /// **An event beyond the cap is not sent and is reported refused**; one
    /// within it goes.
    #[test]
    fn an_event_beyond_the_cap_is_refused_and_counted() {
        let t0 = Instant::now() + Duration::from_secs(60);
        let (mut out, log) = timed_out();
        let timing = timing_at(t0, OutputTerms::default());
        let beyond = 48_000 * 3;
        let refused = out.send_frames(&[at(1_000), at(beyond)], Some(&timing));
        assert_eq!(refused, 1);
        assert_eq!(log.lock().unwrap().len(), 1, "only the one within the cap");
    }

    /// With no timing, every event is sent at once, as before.
    #[test]
    fn without_timing_every_event_is_sent_now() {
        let (mut out, log) = timed_out();
        out.send_frames(&[at(1_000), at(2_000)], None);
        assert!(log.lock().unwrap().iter().all(|e| e.send == SendAt::Now));
    }
}
