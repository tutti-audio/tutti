//! [`Shape`]: what a node looks like from the outside.

use std::num::NonZeroU32;

use tutti_buffers::{ChannelLayout, ChannelTopology};

use tutti_units::{Latency, Tail, UnitAddr, UnitParam};

use crate::node::Resolution;
use crate::node::bus::{Buses, InEvents, OutEvents, PortSpan};
use crate::node::cost::Cost;
use crate::node::param::ParamPorts;
use crate::node::rates::SampleRates;

/// What a node looks like from the outside: its ports, and the two figures the
/// compiler folds over the graph.
///
/// Returned by [`Node::shape`](crate::Node::shape), which takes `&self`, so asking costs nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    /// Audio input channels: the input buses' width, or more where
    /// [`ports`](Self::ports) said so. Read with
    /// [`audio_in`](Self::audio_in).
    audio_in: ChannelLayout,
    /// Audio output channels. Read with [`audio_out`](Self::audio_out).
    audio_out: ChannelLayout,
    /// The input buses the shape was built from. Read with
    /// [`buses_in`](Self::buses_in).
    buses_in: Buses,
    /// The output buses. Read with [`buses_out`](Self::buses_out).
    buses_out: Buses,
    /// Event input ports.
    pub event_in: u16,
    /// Event output ports.
    pub event_out: u16,
    /// The most events this node writes to **each** of its event output
    /// ports in one block: the declared
    /// capacity every buffer downstream of the port is sized from, at
    /// compile and prepare time, so nothing on the audio thread allocates.
    /// `None` takes the executor's default
    /// ([`DEFAULT_EVENT_CAPACITY`](crate::DEFAULT_EVENT_CAPACITY), or what
    /// [`Editor::with_event_capacity`](crate::Editor::with_event_capacity)
    /// set). Set it with [`event_capacity`](Self::event_capacity).
    ///
    /// It is also the port's declared **rate** — at most this many events
    /// per [`MaxBlock`](tutti_buffers::MaxBlock) frames — which is what a PDC delay or a feedback
    /// FIFO on an edge from the port is sized from: events in flight in a
    /// delay of `len` frames are
    /// at most `capacity × (⌈len / MaxBlock⌉ + 2)`.
    ///
    /// # Overflow: drop the newest, and count it
    ///
    /// The port's [`EventWriter`](crate::EventWriter) refuses the event
    /// that would exceed the capacity ([`EventRejected::Full`](crate::EventRejected::Full),
    /// returned to the node at once), and the executor counts it
    /// ([`Executor::dropped_event_count`](crate::Executor::dropped_event_count)),
    /// by port ([`Editor::take_event_losses`](crate::Editor::take_event_losses)). A
    /// note-off is refused only past [`note_off_room`](crate::note_off_room), the
    /// capacity and an eighth and 8 more: every buffer downstream is sized
    /// for it. A rate that is per second, not per block, is sized from the
    /// block in [`Node::prepare`](crate::Node::prepare) ([`EventDensity`](crate::EventDensity)). The
    /// events already written stand. Refusing at compile time is not an
    /// option: how many events a node emits is data (a chord, a burst of
    /// controller moves), not a property of the graph. What the compiler
    /// *does* guarantee is that the writer is the **only** place an event
    /// can be refused: every merge holds the sum of its sources' capacities
    /// and every delay FIFO the declared rate over its length (plus a
    /// note-off reserve), so a node that keeps to its declaration loses
    /// nothing anywhere downstream. Dropping the newest rather than the
    /// oldest keeps what the node already committed to — and the node,
    /// told `Full` at the push, can still choose what matters (a note-off
    /// before a controller move) for the room it has left.
    pub event_capacity: Option<NonZeroU32>,
    /// Whether this node writes param events ([`ParamEvent`](crate::ParamEvent):
    /// ramps and segments) to its event output ports, beside the events. The
    /// compiler reserves a param buffer, as large as the port's event
    /// capacity, on each slot an output of such a node can reach; an output of
    /// a node that does not declare it refuses every
    /// [`push_param`](crate::EventWriter::push_param). A node that declares it
    /// is also the one [`Editor::schedule`](crate::Editor::schedule) takes a
    /// param ramp or segment for, into its event inputs. Set it with
    /// [`param_events`](Self::param_events).
    pub param_events: bool,
    /// The most events per block this node accepts on its event inputs,
    /// **all of them together**: the size a host may allocate for what it is
    /// sent. `None` is unbounded: each input is sized from the edges into it.
    /// Set it with [`event_input_capacity`](Self::event_input_capacity).
    ///
    /// # Overflow: drop the latest, and count it
    ///
    /// What the node is sent in a block on all its event inputs together is
    /// the merge of the edges into each input, the events flushed into it
    /// when a delay vanished, and the commands scheduled onto it
    /// ([`Editor::schedule`](crate::Editor::schedule)), ordered by frame, then
    /// by input, then by source. When that holds more than the bound, the node
    /// is sent the earliest by frame (a note-off is kept ahead of any other
    /// event while the bound has room, so an end is never what is cut), and
    /// the executor counts each event cut as it counts a full output port
    /// ([`Executor::dropped_event_count`](crate::Executor::dropped_event_count)),
    /// and per node ([`Editor::take_event_losses`](crate::Editor::take_event_losses)).
    /// What the node is sent stays in frame order: note-offs first decides
    /// which events survive the cut, not their order. The cut falls on the same
    /// events on the serial and the parallel executor. Each input's merge slot is sized `min(bound, what its edges
    /// can send)` on the control side, and the cut over everything is made in
    /// buffers sized at a commit, so nothing allocates on the audio thread
    /// however the edges change.
    ///
    /// A bypassed node is sent only the closes of what it holds, at most the
    /// bound of them a block across its inputs; the rest stays held and is
    /// closed in the following blocks, never cut and never counted. Every cut
    /// event is charged to the node (`EventLoss` with no port). A node that
    /// computes its bound from the block reads it in [`Node::prepare`](crate::Node::prepare) ([`EventDensity`](crate::EventDensity)),
    /// as an output's capacity is.
    pub event_input_capacity: Option<NonZeroU32>,
    /// **Processing latency only** — frames the node buffers as a side effect
    /// (lookahead, FFT block, plugin pipeline). Never a musical delay: a
    /// 500 ms echo has latency zero, or PDC drags every parallel path 500 ms
    /// late. The type makes the mistake a compile
    /// error: a delay time is a `Samples` or a `Seconds`, not a [`Latency`].
    pub latency: Latency,
    /// How long the node keeps sounding after its input stops, **counted
    /// once its [`latency`](Self::latency) is compensated**: the tail excludes
    /// latency.
    ///
    /// A pure delay of `D` frames declared as latency (a lookahead) has
    /// `Tail::None`; a convolver with an `L`-tap response and latency `D`
    /// has `L - 1`, whatever `D` is. Export trims the latency and then extends
    /// the render by the tail, and the executor's silence skip waits for
    /// latency plus tail quiet frames before it trusts the node is silent.
    ///
    /// This is the node's **ceiling**: the longest it can ever ring, or
    /// [`Tail::Unbounded`] if it can hold forever. The compiler, PDC, the
    /// spec's `graph_tail` and export read it, and it never changes after
    /// [`prepare`](crate::Node::prepare). A node whose tail follows its params
    /// narrows what the silence skip waits for with
    /// [`Io::report_tail`](crate::Io::report_tail); the ceiling is what stays
    /// true when it does not.
    pub tail: Tail,
    /// Whether this node accepts **in-place** channels (see
    /// [`Io::in_place`](crate::Io::in_place)).
    ///
    /// Opt-in because it changes what the node is handed: on an aliased
    /// channel the input arrives *already in the output buffer*.
    pub in_place: bool,
    /// How finely the node honours the offsets of the events it is handed
    /// A declaration, not a request: the executor still
    /// hands every node its sorted events and whole block, and the compiler
    /// refuses an event edge that requires a finer resolution than its sink
    /// declares (see [`GraphSpec::require_resolution`](crate::GraphSpec::require_resolution)).
    pub event_resolution: Resolution,
    /// The params this node lets the graph modulate, in **port order** —
    /// the index [`Io::param`](crate::Io::param) and
    /// [`Node::param_base`](crate::Node::param_base) take (see
    /// [`GraphSpec::connect_param`](crate::GraphSpec::connect_param)). A
    /// param declared here reads [`ParamInput::Base`](crate::ParamInput::Base)
    /// until something is connected to it, and the node must then answer
    /// [`Node::param_base`](crate::Node::param_base) for it.
    pub params: ParamPorts,
    /// About how much work the node does per sample and channel: the compiler
    /// packs cheap siblings into one task. [`Cost::Heavy`] unless set with
    /// [`cost`](Self::cost): a node that declares nothing is assumed
    /// worth a thread of its own. A speed hint only — it never changes a
    /// sample.
    pub cost: Cost,
    /// Whether its event outputs may carry notes, hold pedals or a bend. The
    /// executor tracks what such a port holds so that an edge that ends
    /// can close them at its sink (see
    /// [`no_note_events`](Self::no_note_events)). `true` unless set
    /// otherwise: a node that declares nothing is assumed to carry them.
    ///
    /// The tracking costs about 9 KiB (8 KiB of keys and 1 KiB of controllers)
    /// for each of the unit's event output ports, in each of the tables it
    /// keeps: the running one, the one that owes closes, and, while a replace
    /// waits behind a fade, the waiting unit's. A node that never carries notes
    /// declares [`no_note_events`](Self::no_note_events).
    pub note_events: bool,
    /// The most events this node writes into its successor's first event
    /// input when it is replaced (see [`Node::chase`](crate::Node::chase) and
    /// [`chase`](Self::chase)). `None`: the node carries no state
    /// across a swap, and the executor sizes nothing for it.
    pub chase: Option<NonZeroU32>,
    /// Whether the node bypasses itself: the executor neither blends nor
    /// parks it in [`NodeMode::Bypassed`](crate::NodeMode::Bypassed), and it
    /// needs no pass to be bypassed. `false` unless set with
    /// [`own_bypass`](Self::own_bypass).
    pub own_bypass: bool,
    /// The sample rates the node runs at. Read with [`rates`](Self::rates).
    rates: SampleRates,
}

/// The ports [`Io::input`](crate::Io::input) and its kin cover, per side
/// ([`Shape::main_ports`]): compiled into each node call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MainPorts {
    pub(crate) inputs: PortSpan,
    pub(crate) outputs: PortSpan,
}

/// The ports of bus `index` of `buses`.
fn span_of(buses: &Buses, index: usize) -> Option<PortSpan> {
    let bus = buses.get(index)?;
    Some(PortSpan::new(buses.first_port(index)?, bus.width()))
}

/// The ports past `buses` on a side of `width` ports, if any.
fn spare(buses: &Buses, width: ChannelLayout) -> Option<PortSpan> {
    let (first, all) = (buses.width(), usize::from(width.count()));
    (all > first).then(|| PortSpan::new(first, all - first))
}

impl Shape {
    /// No audio and no event ports: the start of a node that only has events
    /// (`Shape::EMPTY.events(1, 0)`).
    pub const EMPTY: Self = Self::buses(Buses::NONE, Buses::NONE);

    /// A shape of these buses: its audio widths are theirs, and its dry path
    /// ([`pass`](Self::pass)) is the fold between the two
    /// [`Main`](crate::BusRole::Main) buses. No event ports, no latency, no tail,
    /// no in-place support, and [`Resolution::Sample`]: a new node is written to
    /// honour its event offsets, and says otherwise explicitly.
    ///
    /// A bus names its speakers or says it names none
    /// ([`Buses::unassigned`]): unplaced is a statement, never a default, so a
    /// node whose main buses differ in width and name no speakers has no pass,
    /// visibly in its own constructor.
    #[must_use]
    pub const fn buses(inputs: Buses, outputs: Buses) -> Self {
        Self {
            audio_in: ChannelLayout::from_count(inputs.width() as u16),
            audio_out: ChannelLayout::from_count(outputs.width() as u16),
            buses_in: inputs,
            buses_out: outputs,
            event_in: 0,
            event_out: 0,
            event_capacity: None,
            param_events: false,
            event_input_capacity: None,
            latency: Latency::ZERO,
            tail: Tail::None,
            in_place: false,
            event_resolution: Resolution::Sample,
            params: ParamPorts::NONE,
            cost: Cost::Heavy,
            note_events: true,
            chase: None,
            own_bypass: false,
            rates: SampleRates::ALL,
        }
    }

    /// One bus of `topology` in and the same out: output `c` is input `c` on
    /// the dry path. A stereo processor says [`ChannelTopology::STEREO`]; one that
    /// knows only a width says so with [`through_unassigned`](Self::through_unassigned).
    #[must_use]
    pub const fn through(topology: ChannelTopology) -> Self {
        Self::buses(Buses::main(topology), Buses::main(topology))
    }

    /// `width` channels in and out that name no speaker: a per-channel
    /// processor built for a width. Its dry path is output `c` = input `c`, and
    /// a bus edge folds it only to a bus of the same width (a width is not a
    /// placement). Past a bus's width
    /// ([`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS)) the channels are
    /// ports with no bus, passed by index all the same.
    #[must_use]
    pub const fn through_unassigned(width: ChannelLayout) -> Self {
        Self::buses(Buses::unplaced(width), Buses::unplaced(width)).ports(width, width)
    }

    /// `inputs` channels in and `outputs` out that name no speaker: a node built
    /// for widths, not placements (a sum of `inputs` channels into one). Past a
    /// bus's width ([`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS)) a side's
    /// channels are ports with no bus, reached by port. Unequal widths have no
    /// dry path to fold ([`PassError`](crate::PassError)).
    ///
    /// ```
    /// use tutti_graph::Shape;
    /// use tutti_buffers::ChannelLayout;
    ///
    /// // A sum of 24 channels into one: wider than a bus, so the inputs are ports.
    /// let s = Shape::unassigned(ChannelLayout::from_count(24), ChannelLayout::MONO);
    /// assert_eq!(s.audio_in().count(), 24);
    /// assert_eq!(s.audio_out().count(), 1);
    /// ```
    #[must_use]
    pub const fn unassigned(inputs: ChannelLayout, outputs: ChannelLayout) -> Self {
        Self::buses(Buses::unplaced(inputs), Buses::unplaced(outputs)).ports(inputs, outputs)
    }

    /// No audio input, one bus of `topology` out: nothing passes.
    #[must_use]
    pub const fn source(topology: ChannelTopology) -> Self {
        Self::buses(Buses::NONE, Buses::main(topology))
    }

    /// No audio input, `width` channels out that name no speaker (ports with
    /// no bus past a bus's width).
    #[must_use]
    pub const fn source_unassigned(width: ChannelLayout) -> Self {
        Self::buses(Buses::NONE, Buses::unplaced(width)).ports(ChannelLayout::EMPTY, width)
    }

    /// One bus of `topology` in, no audio output: nothing passes.
    #[must_use]
    pub const fn sink(topology: ChannelTopology) -> Self {
        Self::buses(Buses::main(topology), Buses::NONE)
    }

    /// `width` channels in that name no speaker, no audio output.
    #[must_use]
    pub const fn sink_unassigned(width: ChannelLayout) -> Self {
        Self::buses(Buses::unplaced(width), Buses::NONE).ports(width, ChannelLayout::EMPTY)
    }

    /// This shape with `audio_in` and `audio_out` ports, of which its buses
    /// cover a prefix: the channels past them are ports with no bus (a
    /// plugin's bus wider than [`MAX_CHANNELS`](tutti_buffers::MAX_CHANNELS),
    /// or past [`MAX_BUSES`](crate::MAX_BUSES)), reached by port. A side whose
    /// ports outnumber its buses has no main bus to fold, so a dry path across
    /// unequal widths is refused ([`PassError::Unplaced`](crate::PassError::Unplaced)).
    ///
    /// # Panics
    ///
    /// If a side is narrower than its buses (in a `const` context, at compile
    /// time): a bus over ports the node does not have.
    #[must_use]
    pub const fn ports(mut self, audio_in: ChannelLayout, audio_out: ChannelLayout) -> Self {
        assert!(
            self.buses_in.width() <= audio_in.count() as usize
                && self.buses_out.width() <= audio_out.count() as usize,
            "ports narrower than the buses over them"
        );
        self.audio_in = audio_in;
        self.audio_out = audio_out;
        self
    }

    /// Audio input channels. In a [`Topology`](crate::Topology) each channel
    /// is one port.
    #[must_use]
    pub const fn audio_in(&self) -> ChannelLayout {
        self.audio_in
    }

    /// Audio output channels.
    #[must_use]
    pub const fn audio_out(&self) -> ChannelLayout {
        self.audio_out
    }

    /// How the input ports group into buses, in port order: the buses the
    /// shape was built from.
    #[must_use]
    pub const fn buses_in(&self) -> &Buses {
        &self.buses_in
    }

    /// How the output ports group into buses.
    #[must_use]
    pub const fn buses_out(&self) -> &Buses {
        &self.buses_out
    }

    /// Input bus `index`, in port order: the handle a node reads it through
    /// ([`Io::bus_input`](crate::Io::bus_input)). `None` past the declared buses.
    #[must_use]
    pub fn input_bus(&self, index: usize) -> Option<crate::InBus> {
        span_of(&self.buses_in, index).map(crate::InBus)
    }

    /// Output bus `index`, in port order ([`Io::bus_output`](crate::Io::bus_output)).
    #[must_use]
    pub fn output_bus(&self, index: usize) -> Option<crate::OutBus> {
        span_of(&self.buses_out, index).map(crate::OutBus)
    }

    /// The input ports past the declared buses
    /// ([`ports`](Self::ports)): a plugin's channels a bus cannot
    /// describe. `None` when the buses cover every port.
    #[must_use]
    pub fn spare_in(&self) -> Option<crate::InBus> {
        spare(&self.buses_in, self.audio_in).map(crate::InBus)
    }

    /// The output ports past the declared buses.
    #[must_use]
    pub fn spare_out(&self) -> Option<crate::OutBus> {
        spare(&self.buses_out, self.audio_out).map(crate::OutBus)
    }

    /// Every input port as handles, in port order: one per declared bus, then
    /// one for the ports past them. A node whose ports are a list it was handed
    /// (a plugin's channels) reads them all through these.
    pub fn input_handles(&self) -> impl Iterator<Item = crate::InBus> + use<'_> {
        (0..self.buses_in.len())
            .filter_map(|i| self.input_bus(i))
            .chain(self.spare_in())
    }

    /// Every output port as handles, in port order (see
    /// [`input_handles`](Self::input_handles)).
    pub fn output_handles(&self) -> impl Iterator<Item = crate::OutBus> + use<'_> {
        (0..self.buses_out.len())
            .filter_map(|i| self.output_bus(i))
            .chain(self.spare_out())
    }

    /// Event input `index`: the handle a node reads it through
    /// ([`Io::events`](crate::Io::events)). `None` past
    /// [`event_in`](Self::event_in).
    #[must_use]
    pub const fn event_input(&self, index: u16) -> Option<InEvents> {
        if index < self.event_in {
            Some(InEvents(index))
        } else {
            None
        }
    }

    /// Event output `index` ([`Io::event_out`](crate::Io::event_out)). `None`
    /// past [`event_out`](Self::event_out).
    #[must_use]
    pub const fn event_output(&self, index: u16) -> Option<OutEvents> {
        if index < self.event_out {
            Some(OutEvents(index))
        } else {
            None
        }
    }

    /// Every event input as a handle, in port order.
    pub fn event_inputs(&self) -> impl Iterator<Item = InEvents> + use<> {
        (0..self.event_in).map(InEvents)
    }

    /// Every event output as a handle, in port order.
    pub fn event_outputs(&self) -> impl Iterator<Item = OutEvents> + use<> {
        (0..self.event_out).map(OutEvents)
    }

    /// The ports [`Io::input`](crate::Io::input) and
    /// [`Io::output`](crate::Io::output) cover: each side's
    /// [`Main`](crate::BusRole::Main) bus (its first band, for a split's),
    /// every port of a side that declares no bus, and none of a side whose
    /// buses name no main one.
    pub(crate) fn main_ports(&self) -> MainPorts {
        let side = |buses: &Buses, width: ChannelLayout| match buses.main_bus() {
            Some((index, bus)) => buses
                .first_port(index)
                .map_or_else(PortSpan::default, |first| {
                    PortSpan::new(first, bus.band_width())
                }),
            None if buses.is_empty() => PortSpan::new(0, usize::from(width.count())),
            None => PortSpan::default(),
        };
        MainPorts {
            inputs: side(&self.buses_in, self.audio_in),
            outputs: side(&self.buses_out, self.audio_out),
        }
    }

    /// The dry path: where each input channel goes when the node does not run
    /// (a bypass, and the dry side of a mix), or why it has none. Built from
    /// the main buses, so it is known as soon as the shape is: a node's own
    /// test asserts it, and the graph refuses a bypass or a mix of a node
    /// without one. Control side: a fold allocates.
    ///
    /// # Errors
    ///
    /// [`PassError`](crate::PassError) when the main buses differ in width and
    /// one names no speaker, or name speakers that do not fold.
    pub fn pass(&self) -> Result<crate::Pass, crate::PassError> {
        crate::Pass::of(self)
    }

    /// This shape with `event_in` / `event_out` event ports.
    #[must_use]
    pub const fn events(mut self, event_in: u16, event_out: u16) -> Self {
        self.event_in = event_in;
        self.event_out = event_out;
        self
    }

    /// This shape, writing at most `per_block` events to each event output
    /// port per block (see [`event_capacity`](Self::event_capacity) for
    /// what that sizes, and what happens past it).
    ///
    /// ```
    /// use tutti_graph::Shape;
    /// use tutti_buffers::ChannelLayout;
    /// use std::num::NonZeroU32;
    /// let arp = Shape::EMPTY
    ///     .events(1, 1)
    ///     .event_capacity(NonZeroU32::new(16).unwrap());
    /// assert_eq!(arp.event_capacity.map(|n| n.get()), Some(16));
    /// ```
    #[must_use]
    pub const fn event_capacity(mut self, per_block: NonZeroU32) -> Self {
        self.event_capacity = Some(per_block);
        self
    }

    /// This shape, writing param events to its event output ports (see
    /// [`param_events`](Self::param_events)).
    ///
    /// ```
    /// use tutti_graph::Shape;
    /// let lane = Shape::EMPTY.events(0, 1).param_events();
    /// assert!(lane.param_events);
    /// ```
    #[must_use]
    pub const fn param_events(mut self) -> Self {
        self.param_events = true;
        self
    }

    /// This shape, accepting at most `per_block` events per block on its event
    /// inputs together (see [`event_input_capacity`](Self::event_input_capacity)
    /// for what that bounds, and what happens past it).
    ///
    /// ```
    /// use tutti_graph::Shape;
    /// use std::num::NonZeroU32;
    /// let sink = Shape::EMPTY
    ///     .events(2, 0)
    ///     .event_input_capacity(NonZeroU32::new(64).unwrap());
    /// assert_eq!(sink.event_input_capacity.map(|n| n.get()), Some(64));
    /// ```
    #[must_use]
    pub const fn event_input_capacity(mut self, per_block: NonZeroU32) -> Self {
        self.event_input_capacity = Some(per_block);
        self
    }

    /// This shape, saying the node writes up to `events` events of its state
    /// into its successor when a replace swaps it out (see
    /// [`Node::chase`](crate::Node::chase); the executor sizes its buffer from this when it
    /// builds a plan, and counts what falls past it).
    ///
    /// ```
    /// use tutti_graph::Shape;
    /// let synth = Shape::EMPTY.events(1, 0).chase(512);
    /// assert_eq!(synth.chase.map(|n| n.get()), Some(512));
    /// ```
    ///
    /// # Panics
    ///
    /// If `events` is zero: a node that chases nothing declares nothing. (In
    /// a `const` context, at compile time.)
    #[must_use]
    pub const fn chase(mut self, events: u32) -> Self {
        match NonZeroU32::new(events) {
            Some(n) => self.chase = Some(n),
            None => panic!("a chase of zero events: declare nothing instead"),
        }
        self
    }

    /// This shape with processing latency.
    ///
    /// Takes a [`Latency`], never a frame count: a delay time is not a
    /// processing latency, and passing one is a type error —
    ///
    /// ```compile_fail
    /// use tutti_graph::Shape;
    /// use tutti_units::Samples;
    /// use tutti_buffers::ChannelLayout;
    /// let echo_time = Samples::new(24_000);
    /// let _ = Shape::through_unassigned(ChannelLayout::MONO).latency(echo_time);
    /// ```
    ///
    /// — and there is no conversion that would make it one silently:
    ///
    /// ```compile_fail
    /// use tutti_units::{Latency, Samples};
    /// let _: Latency = Samples::new(24_000).into();
    /// ```
    ///
    /// The one spelling is [`Latency::new`], a visible decision:
    ///
    /// ```
    /// use tutti_graph::Shape;
    /// use tutti_units::{Latency, Samples};
    /// use tutti_buffers::ChannelLayout;
    /// let lookahead = Latency::new(Samples::new(512));
    /// let s = Shape::through_unassigned(ChannelLayout::MONO).latency(lookahead);
    /// assert_eq!(s.latency.samples(), Samples::new(512));
    /// ```
    #[must_use]
    pub const fn latency(mut self, latency: Latency) -> Self {
        self.latency = latency;
        self
    }

    /// This shape with the given ring-out, which excludes latency: the
    /// frames the node keeps sounding after its input stops, counted once its
    /// [`latency`](Self::latency) is compensated (see [`tail`](Self::tail)).
    #[must_use]
    pub const fn tail(mut self, tail: Tail) -> Self {
        self.tail = tail;
        self
    }

    /// Frames of quiet input after which the node is silent for good: its
    /// latency, then its tail. The executor's silence skip waits this long.
    pub(crate) fn park_after(&self) -> Tail {
        Tail::Finite(self.latency.samples()).then(self.tail)
    }

    /// This shape for a node that runs at `rates` only: a session's
    /// `Graph::set_sample_rate` and a fork (a bounce or an export) refuse a
    /// rate outside them and change nothing. A node's rates do not depend on its `prepare`. Every rate
    /// unless set.
    ///
    /// ```
    /// use tutti_graph::{SampleRates, Shape};
    /// use tutti_units::SampleRate;
    /// let rates = SampleRates::of(SampleRate::SR_8K, SampleRate::SR_192K);
    /// assert_eq!(Shape::EMPTY.with_rates(rates).rates(), rates);
    /// assert_eq!(Shape::EMPTY.rates(), SampleRates::ALL);
    /// ```
    #[must_use]
    pub const fn with_rates(mut self, rates: SampleRates) -> Self {
        self.rates = rates;
        self
    }

    /// The sample rates the node runs at. See [`with_rates`](Self::with_rates).
    #[must_use]
    pub const fn rates(&self) -> SampleRates {
        self.rates
    }

    /// This shape, accepting in-place channels.
    #[must_use]
    pub const fn in_place(mut self) -> Self {
        self.in_place = true;
        self
    }

    /// This shape, honouring event offsets only as finely as `resolution`.
    #[must_use]
    pub const fn event_resolution(mut self, resolution: Resolution) -> Self {
        self.event_resolution = resolution;
        self
    }

    /// This shape, declaring about how much work the node does per sample
    /// and channel (see [`cost`](Self::cost)).
    ///
    /// ```
    /// use tutti_graph::{Cost, Shape};
    /// use tutti_buffers::ChannelLayout;
    /// let gain = Shape::through_unassigned(ChannelLayout::STEREO).cost(Cost::Light);
    /// assert_eq!(gain.cost, Cost::Light);
    /// ```
    #[must_use]
    pub const fn cost(mut self, cost: Cost) -> Self {
        self.cost = cost;
        self
    }

    /// This shape, declaring that the node handles
    /// [`NodeMode::Bypassed`](crate::NodeMode::Bypassed) itself, as a plugin
    /// with a native bypass parameter does.
    ///
    /// The executor then neither blends nor parks the node when its mode is
    /// `Bypassed`: it is called every block as in `Active`, with no crossfade
    /// and no reset when it leaves the mode, and its latency and tail stay what
    /// it reports, so compensation does not move. What it does with its input,
    /// its tail and its dry path is the node's, and the executor tells it
    /// nothing: the mode is [`Editor::mode`](crate::Editor::mode), on the
    /// control side, not on the audio thread. A plugin's host writes the
    /// plugin's own bypass when the mode changes (`tutti-plugin-host`'s
    /// `PluginHandle::set_bypassed`). A node with no pass can be set
    /// `Bypassed`.
    /// [`Muted`](crate::NodeMode::Muted) and a [`Mix`](tutti_units::Mix) stay
    /// the executor's.
    ///
    /// ```
    /// use tutti_graph::Shape;
    /// assert!(Shape::EMPTY.own_bypass().own_bypass);
    /// assert!(!Shape::EMPTY.own_bypass);
    /// ```
    #[must_use]
    pub const fn own_bypass(mut self) -> Self {
        self.own_bypass = true;
        self
    }

    /// This shape, declaring that its event outputs never carry a note, a hold
    /// pedal or a bend (a clock, a segment stream), so the executor keeps no
    /// held table for them. A node that then writes a note or a sustain pedal
    /// trips a debug assertion; in a release build nothing checks, and what it
    /// starts is never closed when its edge ends.
    ///
    /// ```
    /// use tutti_graph::Shape;
    /// use tutti_buffers::ChannelLayout;
    /// let lane = Shape::EMPTY
    ///     .events(0, 1)
    ///     .no_note_events();
    /// assert!(!lane.note_events);
    /// ```
    #[must_use]
    pub const fn no_note_events(mut self) -> Self {
        self.note_events = false;
        self
    }

    /// This shape, letting the graph modulate `params`, in this port order
    /// (see [`params`](Self::params)).
    ///
    /// # Panics
    ///
    /// If `params` holds more than [`MAX_PARAM_PORTS`](crate::MAX_PARAM_PORTS),
    /// or one param twice (in a `const` context, at compile time).
    #[must_use]
    pub const fn params(mut self, params: &[UnitParam]) -> Self {
        self.params = ParamPorts::new(params);
        self
    }

    /// [`params`](Self::params) for a node with several params of
    /// one kind: each [`UnitAddr`] is a kind and an index (a threshold per
    /// band is `UnitAddr::new(UnitParam::Threshold, band)`), in this port
    /// order.
    ///
    /// # Panics
    ///
    /// If `params` holds more than [`MAX_PARAM_PORTS`](crate::MAX_PARAM_PORTS),
    /// or one address (kind and index) twice (in a `const` context, at
    /// compile time).
    #[must_use]
    pub const fn param_addrs(mut self, params: &[UnitAddr]) -> Self {
        self.params = ParamPorts::of(params);
        self
    }
}
