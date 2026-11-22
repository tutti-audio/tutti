//! [`Io`]: one node call's buffers, reached through methods.
//!
//! # Why methods and not public fields
//!
//! The buffers are planar slices of the sample format `F`. Exposing them as
//! `&[&[F]]` fields would make that layout part of every node's source; as
//! methods (`input`, `output`, `split`, `channel`) the layout can change
//! without a breaking change. See [`PortKind`].
//!
//! # The length guarantee
//!
//! [`Io`] can only be built by the executor (or the reference interpreter),
//! through a constructor that asserts every slice is exactly
//! [`frames`](Io::frames) long and that `frames` is at most the
//! [`MaxBlock`] the node was prepared with. A node therefore never clamps.

use tutti_buffers::{BufferError, Float, PlanesMut};
use tutti_units::{Samples, Tail};

use crate::bus::{InBus, OutBus, PortSpan};
use crate::event::{EventWriter, SortedEvents, SubBlocks};
use crate::node::{ConstantMask, InPlaceMask, MainPorts, SilenceMask, Wired, WiredMask};
use crate::param::ParamInput;
use crate::rtlog::{LogSlots, RtLevel, RtRecord, RtTarget, RtValue};
use crate::time::Offset;
use tutti_buffers::MaxBlock;

/// What a port carries.
///
/// `Audio` is planar blocks of the graph's sample format: the format is the
/// `F` of the [`Node`](crate::Node) and its [`Io`], not a variant here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PortKind {
    /// Planar audio, one channel per port.
    Audio,
    /// Sorted [`Event`](crate::Event) streams.
    Event,
}

/// One node call's buffers: audio in and out, events in and out, and the
/// block's param values, handed to [`Node::process`](crate::Node::process).
///
/// Only the executor (or the `Reference` interpreter)
/// builds one, and it guarantees every audio slice is exactly
/// [`frames`](Self::frames) long, at most the [`MaxBlock`] the node was
/// prepared with — so a node never needs to clamp. Buffers are planar slices
/// of the sample format `F` (`f32` for a plain `impl Node`) and reached through
/// methods rather than fields: [`split`](Self::split) borrows inputs and outputs together, [`channel`](Self::channel) gives
/// one channel as a [`Channel`] (separate or in place), and
/// [`sub_blocks`](Self::sub_blocks) splits the block at an event port's
/// offsets for sample-accurate event handling.
///
/// The silence, constant and in-place masks say what the executor knows
/// about the inputs; a node may use them to skip work but must render the
/// same result without them.
///
/// # The main bus, and the others by handle
///
/// [`input`](Self::input), [`output`](Self::output), [`channel`](Self::channel),
/// [`split`](Self::split) and the masks cover the node's **main bus** on each
/// side ([`Shape::buses`](crate::Shape::buses)), or every port of a side that
/// declares no bus: channel `c` is the main bus's channel `c`. Any other bus
/// (a key, a crossover's further bands, a plugin's aux buses, the ports past
/// the buses) is reached only through its handle ([`InBus`], [`OutBus`], from
/// the node's shape) as a view bounded to it: [`bus_input`](Self::bus_input),
/// [`bus_output`](Self::bus_output), [`Inputs::bus`], [`Outputs::bus`]. So `c + width`
/// on the main view is out of range in every build, as a slice is, and never
/// another bus's port.
pub struct Io<'a, F: Float = f32> {
    frames: usize,
    inputs: &'a [&'a [F]],
    outputs: &'a mut [&'a mut [F]],
    /// The ports the main views cover.
    main: MainPorts,
    silent: SilenceMask,
    constant: ConstantMask,
    in_place: InPlaceMask,
    /// Which audio ports have a source or a consumer.
    wired: Wired,
    events_in: &'a [SortedEvents<'a>],
    events_out: &'a mut [EventWriter<'a>],
    /// Per declared param port, what it reads this block; a port past the
    /// end reads its base.
    params: &'a [ParamInput<'a, F>],
    /// Where [`count_lost`](Self::count_lost) adds; `None` where nothing
    /// counts (a hand-built `Io`).
    lost: Option<&'a std::cell::Cell<u32>>,
    /// Where [`report_tail`](Self::report_tail) stores; `None` where nothing
    /// reads it (a hand-built `Io`).
    tail: Option<&'a std::cell::Cell<Option<Tail>>>,
    /// Where [`log`](Self::log) writes; `None` where nothing reads it (a
    /// hand-built `Io`).
    log: Option<&'a LogSlots>,
}

impl<F: Float> std::fmt::Debug for Io<'_, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Io")
            .field("frames", &self.frames)
            .field("inputs", &self.inputs.len())
            .field("outputs", &self.outputs.len())
            .field("silent", &self.silent)
            .field("constant", &self.constant)
            .field("in_place", &self.in_place)
            .finish_non_exhaustive()
    }
}

/// The audio inputs, while the outputs are borrowed too: the main bus by
/// channel ([`get`](Self::get)), any other bus by its handle
/// ([`bus`](Self::bus)).
pub struct Inputs<'s, F: Float = f32> {
    slices: &'s [&'s [F]],
    main: PortSpan,
    in_place: InPlaceMask,
}

/// One input bus, read by channel: channel `c` of the bus, bounded to it.
pub struct BusInput<'s, F: Float = f32> {
    slices: &'s [&'s [F]],
    first: usize,
    in_place: InPlaceMask,
}

/// One output bus, written by channel: channel `c` of the bus, bounded to it.
pub struct BusOutput<'s, 'a, F: Float = f32> {
    slices: &'s mut [&'a mut [F]],
}

impl<F: Float> std::fmt::Debug for Inputs<'_, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inputs")
            .field("channels", &self.main.width)
            .field("in_place", &self.in_place)
            .finish_non_exhaustive()
    }
}

impl<F: Float> std::fmt::Debug for BusInput<'_, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BusInput")
            .field("channels", &self.slices.len())
            .finish_non_exhaustive()
    }
}

impl<F: Float> std::fmt::Debug for BusOutput<'_, '_, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BusOutput")
            .field("channels", &self.slices.len())
            .finish_non_exhaustive()
    }
}

/// The audio outputs, while the inputs are borrowed too: the main bus by
/// channel ([`get`](Self::get)), any other bus by its handle
/// ([`bus`](Self::bus)).
pub struct Outputs<'s, 'a, F: Float = f32> {
    slices: &'s mut [&'a mut [F]],
    main: PortSpan,
}

impl<F: Float> std::fmt::Debug for Outputs<'_, '_, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Outputs")
            .field("channels", &self.main.width)
            .finish_non_exhaustive()
    }
}

/// One channel's input and output together.
pub enum Channel<'s, F: Float = f32> {
    /// Separate buffers: read `input`, write `output`.
    Split {
        /// The input samples.
        input: &'s [F],
        /// Where the output goes.
        output: &'s mut [F],
    },
    /// One buffer: it holds the input now and must hold the output after.
    InPlace(&'s mut [F]),
}

impl<F: Float> std::fmt::Debug for Channel<'_, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Channel::Split { input, .. } => f
                .debug_struct("Channel::Split")
                .field("len", &input.len())
                .finish_non_exhaustive(),
            Channel::InPlace(buf) => f
                .debug_struct("Channel::InPlace")
                .field("len", &buf.len())
                .finish_non_exhaustive(),
        }
    }
}

impl<F: Float> Channel<'_, F> {
    /// Apply `f` sample by sample, whichever form this is.
    // `#[inline]` is load-bearing: without it the loop is compiled apart from
    // the node's closure, whose captured state (a filter's two state
    // variables, say) then lives behind pointers, and every sample pays a
    // store and a reload on the recurrence — measured 1.5× slower on a chain
    // of SVFs.
    #[inline]
    pub fn map(self, mut f: impl FnMut(F) -> F) {
        match self {
            Channel::Split { input, output } => {
                for (o, &i) in output.iter_mut().zip(input) {
                    *o = f(i);
                }
            }
            Channel::InPlace(buf) => {
                for x in buf.iter_mut() {
                    *x = f(*x);
                }
            }
        }
    }
}

impl<'a, F: Float> Io<'a, F> {
    /// Build a call's buffers, enforcing the length guarantee.
    ///
    /// # Panics
    ///
    /// Always, if `frames` is zero or exceeds `max` — the bound a node's
    /// scratch was sized from. In debug builds also if an output is not
    /// exactly `frames` long, or an input is not (an aliased input is empty):
    /// both executors build every slice from `frames` themselves, so that
    /// check is a guard on this crate's code, not on the caller's, and is not
    /// worth a per-channel loop on every node call in release.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        max: MaxBlock,
        frames: usize,
        inputs: &'a [&'a [F]],
        outputs: &'a mut [&'a mut [F]],
        silent: SilenceMask,
        constant: ConstantMask,
        in_place: InPlaceMask,
        events_in: &'a [SortedEvents<'a>],
        events_out: &'a mut [EventWriter<'a>],
    ) -> Self {
        assert!(
            frames > 0 && frames <= max.get(),
            "a block of {frames} frames against a prepared maximum of {}",
            max.get()
        );
        debug_assert!(outputs.iter().all(|o| o.len() == frames));
        debug_assert!(
            inputs
                .iter()
                .enumerate()
                .all(|(c, i)| i.len() == frames || (in_place.get(c) && i.is_empty()))
        );
        let main = MainPorts {
            inputs: PortSpan::new(0, inputs.len()),
            outputs: PortSpan::new(0, outputs.len()),
        };
        let wired = Wired::all(inputs.len(), outputs.len());
        Self {
            frames,
            inputs,
            outputs,
            main,
            silent,
            constant,
            in_place,
            wired,
            events_in,
            events_out,
            params: &[],
            lost: None,
            tail: None,
            log: None,
        }
    }

    /// These buffers, with `wired` as the plan's answer for the node's
    /// ports. Without it every port reads wired.
    #[inline]
    pub(crate) fn with_wired(mut self, wired: Wired) -> Self {
        self.wired = wired;
        self
    }

    /// These buffers, with the main views covering `main`: the node's
    /// [`Shape::main_ports`](crate::Shape). Without it they cover every port.
    ///
    /// # Panics
    ///
    /// If `main` reaches past the ports.
    #[inline]
    pub(crate) fn with_main(mut self, main: MainPorts) -> Self {
        assert!(
            main.inputs.range().end <= self.inputs.len()
                && main.outputs.range().end <= self.outputs.len(),
            "a main bus past the node's ports"
        );
        self.main = main;
        self
    }

    /// These buffers, with [`report_tail`](Self::report_tail) storing into
    /// `tail`.
    #[inline]
    pub(crate) fn with_tail(mut self, tail: &'a std::cell::Cell<Option<Tail>>) -> Self {
        self.tail = Some(tail);
        self
    }

    /// Narrows this node's tail for the silence skip: the frames it keeps
    /// sounding after its input stops, counted once its latency is
    /// compensated, **measured from the last non-quiet input frame, not from
    /// now**. The executor parks the node once its input has been quiet for
    /// its latency plus this figure, so a reverb with a one-second RT60
    /// reports one second on every call, however long its input has been
    /// quiet; reporting the half second that remains after half a second of
    /// quiet would park it at half its ring.
    ///
    /// **Report on every call or not at all.** A report holds until the next
    /// call, and a call that reports nothing restores the ceiling
    /// ([`Shape::tail`](crate::Shape::tail)), the safe default. A report above
    /// the ceiling is clamped to it, [`Tail::Unknown`] says nothing (the
    /// ceiling stands), and [`Tail::Unbounded`] under an `Unbounded` ceiling
    /// is never skipped. A node whose shape does not declare a tail with
    /// [`with_tail`](crate::Shape::with_tail) has the default ceiling and is
    /// never narrowed below it; one that forgets to declare any tail at all
    /// (`Tail::Unknown`) is never skipped, whatever it reports, because an
    /// `Unknown` ceiling bounds nothing for a report to clamp to.
    ///
    /// Only a node **without event inputs** is parked on its tail; one with
    /// event inputs parks only on its own [`Status::Idle`](crate::Status::Idle),
    /// and its report changes nothing.
    ///
    /// The report must cover the node's ring-out (see [`Node`](crate::Node),
    /// "A tail that follows the params"); `contract::assert_declared_tail`
    /// (feature `contract`) checks it. Audio thread: one store into the
    /// unit's own record, no allocation; the plan is untouched, and nothing
    /// but the task that runs the node reads it, so a pooled render parks the
    /// same blocks a serial one does.
    #[inline]
    pub fn report_tail(&self, tail: Tail) {
        if let Some(cell) = self.tail {
            cell.set(Some(tail));
        }
    }

    /// These buffers, with `params` as the declared params' inputs (every
    /// port past `params`' end reads its base).
    #[inline]
    pub(crate) fn with_params(mut self, params: &'a [ParamInput<'a, F>]) -> Self {
        self.params = params;
        self
    }

    /// These buffers, with [`log`](Self::log) writing into `log`.
    #[inline]
    pub(crate) fn with_log(mut self, log: &'a LogSlots) -> Self {
        self.log = Some(log);
        self
    }

    /// Logs a record of `level` from the audio thread: `message` is the call
    /// site's text, `offset` the frame in the block it is about, and
    /// `fields` up to [`MAX_FIELDS`](crate::MAX_FIELDS) named values.
    ///
    /// Takes `&self`, allocates nothing and never waits. A level the
    /// control side's `tracing` subscriber would not emit returns at once.
    /// The same call (same literal, level and field values) several times in
    /// one node call is one record whose `repeats` counts the rest; a fifth
    /// distinct record in one node call is counted lost. The executor fills
    /// in the node and the frame, and the session emits the record as a
    /// `tracing` event on its audio-thread target.
    ///
    /// ```text
    /// io.log(RtLevel::Warn, "non-finite sample", offset, &[("channel", c.into())]);
    /// ```
    #[inline]
    pub fn log(
        &self,
        level: RtLevel,
        message: &'static str,
        offset: Offset,
        fields: &[(&'static str, RtValue)],
    ) {
        self.log_to(RtTarget::Rt, level, message, offset, fields);
    }

    /// As [`log`](Self::log), for a hosted plugin's record: the session
    /// emits it on its plugin target, which a subscriber filters on its own.
    #[inline]
    pub fn log_plugin(
        &self,
        level: RtLevel,
        message: &'static str,
        offset: Offset,
        fields: &[(&'static str, RtValue)],
    ) {
        self.log_to(RtTarget::Plugin, level, message, offset, fields);
    }

    /// Logs `record` (a plugin's line, built with [`RtRecord::line`]) at
    /// `offset`, on the target it names. Its node and frame are the
    /// executor's, whatever the record held.
    #[inline]
    pub fn log_line(&self, record: &RtRecord, offset: Offset) {
        if let Some(log) = self.log {
            log.push(*record, offset.get());
        }
    }

    #[inline]
    fn log_to(
        &self,
        target: RtTarget,
        level: RtLevel,
        message: &'static str,
        offset: Offset,
        fields: &[(&'static str, RtValue)],
    ) {
        if let Some(log) = self.log {
            if !log.allows(target, level) {
                return;
            }
            log.push(RtRecord::new(level, target, message, fields), offset.get());
        }
    }

    /// These buffers, counting [`count_lost`](Self::count_lost) into `lost`.
    #[inline]
    pub(crate) fn with_lost(mut self, lost: &'a std::cell::Cell<u32>) -> Self {
        self.lost = Some(lost);
        self
    }

    /// Counts `n` events this node took from its inputs and did not deliver
    /// (a plugin's intake past its cap): the executor adds them to
    /// [`Executor::dropped_event_count`](crate::Executor::dropped_event_count) and
    /// reports them for the node ([`EventLoss`](crate::EventLoss), `port: None`),
    /// so an export that watches the count sees them. Audio thread: one add.
    #[inline]
    pub fn count_lost(&self, n: u64) {
        if let Some(c) = self.lost {
            c.set(c.get().saturating_add(u32::try_from(n).unwrap_or(u32::MAX)));
        }
    }

    /// The block length in frames — at most the prepared
    /// [`MaxBlock`]. A slice length, hence `usize`.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Channels of the main input bus.
    pub fn input_count(&self) -> usize {
        usize::from(self.main.inputs.width)
    }

    /// Channels of the main output bus.
    pub fn output_count(&self) -> usize {
        usize::from(self.main.outputs.width)
    }

    /// The port of main input channel `c`.
    fn in_port(&self, c: usize) -> usize {
        assert!(
            c < self.input_count(),
            "input {c} is past the main bus ({} channels); another bus is read by its handle",
            self.input_count()
        );
        usize::from(self.main.inputs.first) + c
    }

    /// The port of main output channel `c`.
    fn out_port(&self, c: usize) -> usize {
        assert!(
            c < self.output_count(),
            "output {c} is past the main bus ({} channels); another bus is written by its handle",
            self.output_count()
        );
        usize::from(self.main.outputs.first) + c
    }

    /// Main input channel `c`, wherever it is: its own buffer, or (when the
    /// executor aliased it in place) the output buffer that holds it.
    ///
    /// # Panics
    ///
    /// If `c` is not below [`input_count`](Self::input_count).
    pub fn input(&self, c: usize) -> &[F] {
        let p = self.in_port(c);
        if self.in_place.get(p) {
            &self.outputs[p][..]
        } else {
            self.inputs[p]
        }
    }

    /// Main output channel `c`.
    ///
    /// # Panics
    ///
    /// If `c` is not below [`output_count`](Self::output_count).
    pub fn output(&mut self, c: usize) -> &mut [F] {
        let p = self.out_port(c);
        &mut self.outputs[p][..]
    }

    /// Main input and output channel `c` together.
    ///
    /// # Panics
    ///
    /// If `c` is out of range for the main input or output bus.
    pub fn channel(&mut self, c: usize) -> Channel<'_, F> {
        let (i, o) = (self.in_port(c), self.out_port(c));
        if self.in_place.get(i) {
            debug_assert_eq!(i, o, "a channel in place is one port");
            Channel::InPlace(&mut self.outputs[o][..])
        } else {
            Channel::Split {
                input: self.inputs[i],
                output: &mut self.outputs[o][..],
            }
        }
    }

    /// Every input and every output at once, for a node that mixes across
    /// channels: the main buses by channel, any other bus by its handle. An
    /// aliased channel is reachable only through the outputs.
    pub fn split(&mut self) -> (Inputs<'_, F>, Outputs<'_, 'a, F>) {
        (
            Inputs {
                slices: self.inputs,
                main: self.main.inputs,
                in_place: self.in_place,
            },
            Outputs {
                slices: &mut *self.outputs,
                main: self.main.outputs,
            },
        )
    }

    /// Input bus `bus`, by its handle.
    ///
    /// # Panics
    ///
    /// If the handle reaches past this node's input ports: one taken from
    /// another shape.
    pub fn bus_input(&self, bus: InBus) -> BusInput<'_, F> {
        bus_input(self.inputs, bus, self.in_place)
    }

    /// Output bus `bus`, by its handle.
    ///
    /// # Panics
    ///
    /// If the handle reaches past this node's output ports.
    pub fn bus_output(&mut self, bus: OutBus) -> BusOutput<'_, 'a, F> {
        BusOutput {
            slices: &mut self.outputs[bus.0.range()],
        }
    }

    /// Main input channels that are exact silence this block.
    pub fn silent(&self) -> SilenceMask {
        SilenceMask(shift(self.silent.0, self.main.inputs))
    }

    /// Main input channels that hold one value throughout this block.
    pub fn constant(&self) -> ConstantMask {
        ConstantMask(shift(self.constant.0, self.main.inputs))
    }

    /// Main input channels with at least one source in the graph. An unset
    /// channel reads silence because nothing feeds it, not because a source
    /// is quiet; [`silent`](Self::silent) does not tell the two apart.
    ///
    /// Fixed between commits: a plan's answer, read as a mask like
    /// [`silent`](Self::silent). A hand-built `Io` reads every port wired.
    pub fn wired_inputs(&self) -> WiredMask {
        WiredMask(shift(self.wired.inputs.0, self.main.inputs))
    }

    /// Main output channels with at least one consumer: an edge, a main
    /// output, a tap or a meter. An unset channel is written for nobody. A
    /// hand-built `Io` reads every port wired.
    pub fn wired_outputs(&self) -> WiredMask {
        WiredMask(shift(self.wired.outputs.0, self.main.outputs))
    }

    /// Which channels of input bus `bus` have a source, from bit 0. A
    /// hand-built `Io` reads every port wired.
    ///
    /// # Panics
    ///
    /// If the handle reaches past this node's input ports.
    pub fn wired_bus_in(&self, bus: InBus) -> WiredMask {
        assert!(
            bus.0.range().end <= self.inputs.len(),
            "an input bus past the node's ports"
        );
        WiredMask(shift(self.wired.inputs.0, bus.0))
    }

    /// Which channels of output bus `bus` have a consumer, from bit 0. A
    /// hand-built `Io` reads every port wired.
    ///
    /// # Panics
    ///
    /// If the handle reaches past this node's output ports.
    pub fn wired_bus_out(&self, bus: OutBus) -> WiredMask {
        assert!(
            bus.0.range().end <= self.outputs.len(),
            "an output bus past the node's ports"
        );
        WiredMask(shift(self.wired.outputs.0, bus.0))
    }

    /// Main channels aliased in place: for each set `c`, output `c` **already
    /// holds input `c`**. Never set unless the node's
    /// [`Shape::in_place`](crate::Shape::in_place) is `true`.
    pub fn in_place(&self) -> InPlaceMask {
        InPlaceMask(shift(self.in_place.0, self.main.inputs))
    }

    /// Number of event input ports.
    pub fn event_input_count(&self) -> usize {
        self.events_in.len()
    }

    /// Event input port `p`, sorted by offset, every offset inside the block.
    ///
    /// # Panics
    ///
    /// If `p` is not below [`event_input_count`](Self::event_input_count).
    pub fn events(&self, p: usize) -> SortedEvents<'a> {
        self.events_in[p]
    }

    /// Number of event output ports.
    pub fn event_output_count(&self) -> usize {
        self.events_out.len()
    }

    /// The writer for event output port `p`.
    ///
    /// # Panics
    ///
    /// If `p` is not below [`event_output_count`](Self::event_output_count).
    pub fn event_out(&mut self, p: usize) -> &mut EventWriter<'a> {
        &mut self.events_out[p]
    }

    /// The block split at event input port `p`'s offsets: `(range, events)`
    /// chunks that tile the block, each carrying exactly the events at its
    /// first frame (see [`SubBlocks`]).
    ///
    /// The shape a sample-accurate node is written in — apply the chunk's
    /// events, then render its range — so honouring offsets is the loop's
    /// structure rather than a check the node has to remember. Borrows the
    /// events, not the `Io`, so the loop body can write the outputs:
    ///
    /// ```
    /// use tutti_graph::{Io, EventKind};
    /// fn render(mut io: Io<'_>, level: &mut f32) {
    ///     for (range, events) in io.sub_blocks(0) {
    ///         for e in events {
    ///             if let EventKind::Midi(_) = e.kind {
    ///                 *level = 1.0 - *level; // toggle on every event
    ///             }
    ///         }
    ///         io.output(0)[range].fill(*level);
    ///     }
    /// }
    /// ```
    pub fn sub_blocks(&self, p: usize) -> SubBlocks<'a> {
        self.events_in[p].sub_blocks(Samples::new(self.frames))
    }

    /// What declared param `k` ([`Shape::params`](crate::Shape::params))
    /// reads this block: [`ParamInput::Base`] when nothing modulates it — the
    /// node reads its own control, the fast path — or its value at every
    /// frame, base and modulation fused and clamped by the graph. A param
    /// nothing is connected to is always `Base`, never 0.
    #[inline]
    pub fn param(&self, k: usize) -> ParamInput<'a, F> {
        self.params.get(k).copied().unwrap_or(ParamInput::Base)
    }

    /// Frame `index` of this block as an [`Offset`], when it is inside it.
    pub fn offset(&self, index: usize) -> Option<Offset> {
        Offset::new(Samples::new(index), Samples::new(self.frames))
    }
}

impl<'s, F: Float> Inputs<'s, F> {
    /// Channels of the main input bus.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.main.width)
    }

    /// Whether the main input bus has none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.main.width == 0
    }

    /// Main input channel `c`.
    ///
    /// # Panics
    ///
    /// If `c` is aliased in place (read it through the outputs) or past the
    /// main bus.
    pub fn get(&self, c: usize) -> &[F] {
        assert!(
            c < self.len(),
            "input {c} is past the main bus ({} channels); another bus is read by its handle",
            self.len()
        );
        let p = usize::from(self.main.first) + c;
        assert!(
            !self.in_place.get(p),
            "input {c} is in place; it is in the output buffer"
        );
        self.slices[p]
    }

    /// Input bus `bus`, by its handle.
    ///
    /// # Panics
    ///
    /// If the handle reaches past this node's input ports.
    pub fn bus(&self, bus: InBus) -> BusInput<'s, F> {
        bus_input(self.slices, bus, self.in_place)
    }
}

impl<'s, 'a, F: Float> Outputs<'s, 'a, F> {
    /// Channels of the main output bus.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.main.width)
    }

    /// Whether the main output bus has none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.main.width == 0
    }

    /// Main output channel `c`.
    ///
    /// # Panics
    ///
    /// If `c` is past the main bus.
    pub fn get(&mut self, c: usize) -> &mut [F] {
        assert!(
            c < self.len(),
            "output {c} is past the main bus ({} channels); another bus is written by its handle",
            self.len()
        );
        &mut self.slices[usize::from(self.main.first) + c][..]
    }

    /// The main output bus as writable planes.
    ///
    /// # Errors
    ///
    /// [`BufferError`] if the bus has no channel or its channels differ in
    /// length.
    pub fn planes_mut(&mut self) -> Result<PlanesMut<'_, 'a, F>, BufferError> {
        PlanesMut::new(&mut self.slices[self.main.range()])
    }

    /// Every main output channel.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut [F]> + use<'_, 's, 'a, F> {
        self.slices[self.main.range()].iter_mut().map(|s| &mut **s)
    }

    /// Output bus `bus`, by its handle, while the inputs stay borrowed.
    ///
    /// # Panics
    ///
    /// If the handle reaches past this node's output ports.
    pub fn bus(&mut self, bus: OutBus) -> BusOutput<'_, 'a, F> {
        BusOutput {
            slices: &mut self.slices[bus.0.range()],
        }
    }
}

impl<'s, 'a, F: Float> Outputs<'s, 'a, F> {
    /// Several output buses at once, by their handles in port order, so a node
    /// that writes more than one bus in a pass (a split's bands, a plugin's
    /// buses) holds them together.
    ///
    /// # Panics
    ///
    /// When it reaches a handle out of port order or sharing a port with the
    /// one before, or one past this node's output ports.
    pub fn buses<'h>(
        &mut self,
        buses: &'h [OutBus],
    ) -> impl Iterator<Item = BusOutput<'_, 'a, F>> + use<'_, 'h, 's, 'a, F> {
        let mut rest: &mut [&'a mut [F]] = &mut self.slices[..];
        let mut at = 0;
        buses.iter().map(move |bus| {
            let span = bus.0.range();
            assert!(
                span.start >= at,
                "output buses are taken in port order, each once"
            );
            let (_, tail) = core::mem::take(&mut rest).split_at_mut(span.start - at);
            let (view, tail) = tail.split_at_mut(span.len());
            rest = tail;
            at = span.end;
            BusOutput { slices: view }
        })
    }
}

impl<'s, F: Float> BusInput<'s, F> {
    /// The bus's channels.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slices.len()
    }

    /// Whether the bus has none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slices.is_empty()
    }

    /// Every channel of the bus, in order.
    ///
    /// # Panics
    ///
    /// When it reaches a channel aliased in place.
    pub fn channels(self) -> impl Iterator<Item = &'s [F]> {
        (0..self.len()).map(move |c| self.get(c))
    }

    /// Channel `c` of the bus.
    ///
    /// # Panics
    ///
    /// If `c` is past the bus, or aliased in place.
    pub fn get(&self, c: usize) -> &'s [F] {
        assert!(
            !self.in_place.get(self.first + c),
            "input {c} of the bus is in place; it is in the output buffer"
        );
        self.slices[c]
    }
}

impl<'s, 'a, F: Float> BusOutput<'s, 'a, F> {
    /// The bus's channels.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slices.len()
    }

    /// Whether the bus has none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slices.is_empty()
    }

    /// Channel `c` of the bus.
    ///
    /// # Panics
    ///
    /// If `c` is past the bus.
    pub fn get(&mut self, c: usize) -> &mut [F] {
        &mut self.slices[c][..]
    }

    /// Every channel of the bus.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut [F]> + use<'_, 's, 'a, F> {
        self.slices.iter_mut().map(|s| &mut **s)
    }

    /// Every channel of the bus, for as long as the view was lent.
    pub fn into_channels(self) -> impl Iterator<Item = &'s mut [F]> + use<'s, 'a, F> {
        self.slices.iter_mut().map(|s| &mut **s)
    }
}

/// The view of input bus `bus` over `slices`.
fn bus_input<'s, F: Float>(
    slices: &'s [&'s [F]],
    bus: InBus,
    in_place: InPlaceMask,
) -> BusInput<'s, F> {
    BusInput {
        slices: &slices[bus.0.range()],
        first: usize::from(bus.0.first),
        in_place,
    }
}

/// `mask`'s bits for the ports of `span`, from bit 0.
fn shift(mask: u64, span: PortSpan) -> u64 {
    let first = u32::from(span.first);
    let bits = mask.checked_shr(first).unwrap_or(0);
    let width = usize::from(span.width);
    if width >= 64 {
        bits
    } else {
        bits & ((1u64 << width) - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Bus, BusRole, Buses, Shape};
    use tutti_buffers::{BusTopology, ChannelLayout};

    const FRAMES: usize = 4;

    /// A stereo main bus and a mono key in, a stereo main bus out, and one
    /// port past the buses on each side (a plugin's channels a bus cannot
    /// describe).
    fn shape() -> Shape {
        let main = Bus::new(BusTopology::STEREO, BusRole::Main);
        let key = Bus::new(
            BusTopology::unassigned(ChannelLayout::MONO).expect("mono fits"),
            BusRole::Sidechain,
        );
        let ins = Buses::new(&[main, key]).expect("two buses");
        Shape::buses(ins, Buses::main(BusTopology::STEREO))
            .with_ports(ChannelLayout::from_count(4), ChannelLayout::from_count(3))
    }

    /// Runs `f` over an `Io` of [`shape`]: input port `p` holds `p + 1`, every
    /// output 0; returns the outputs afterwards.
    fn with_io(f: impl FnOnce(&Shape, Io<'_>)) -> Vec<Vec<f32>> {
        let shape = shape();
        let ins: Vec<Vec<f32>> = (0..4).map(|p| vec![(p + 1) as f32; FRAMES]).collect();
        let in_refs: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
        let mut outs = vec![vec![0.0; FRAMES]; 3];
        {
            let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
            let io = Io::new(
                MaxBlock::of(Samples::new(FRAMES)),
                FRAMES,
                &in_refs,
                &mut out_refs,
                SilenceMask::NONE,
                ConstantMask::NONE,
                InPlaceMask::NONE,
                &[],
                &mut [],
            )
            .with_main(shape.main_ports());
            f(&shape, io);
        }
        outs
    }

    /// A split's main output is its first band: the flat view covers band 0,
    /// and the bus's handle every band.
    #[test]
    fn a_banded_main_bus_is_band_zero_in_the_flat_view() {
        let bands = Bus::banded(BusTopology::STEREO, BusRole::Main, 3).expect("three bands");
        let shape = Shape::buses(
            Buses::main(BusTopology::STEREO),
            Buses::NONE.with(bands).expect("one bus"),
        );
        assert_eq!(shape.audio_out(), ChannelLayout::from_count(6));
        assert_eq!(shape.main_ports().outputs, PortSpan::new(0, 2));
        assert_eq!(shape.output_bus(0).map(OutBus::width), Some(6));
    }

    /// Input and output `c` are the main bus's: two channels each side.
    #[test]
    fn the_flat_view_is_the_main_bus() {
        with_io(|_, io| {
            assert_eq!((io.input_count(), io.output_count()), (2, 2));
            assert_eq!(io.input(1)[0], 2.0);
        });
    }

    /// The port after the main bus is the key's, and it is not reachable as
    /// input 2: the flat view ends with the main bus.
    #[test]
    #[should_panic(expected = "input 2 is past the main bus")]
    fn a_port_past_the_main_bus_is_not_an_input() {
        with_io(|_, io| {
            let _ = io.input(2);
        });
    }

    /// Every input port in port order, a bus at a time: the main bus, the key,
    /// then the port past the buses.
    #[test]
    fn the_handles_cover_every_input_in_port_order() {
        with_io(|shape, io| {
            let read: Vec<f32> = shape
                .input_handles()
                .flat_map(|bus| io.bus_input(bus).channels())
                .map(|ch| ch[0])
                .collect();
            assert_eq!(read, [1.0, 2.0, 3.0, 4.0]);
        });
    }

    /// The output buses held together write every output port, each once, in
    /// port order.
    #[test]
    fn the_output_buses_held_together_reach_every_port_in_order() {
        let outs = with_io(|shape, mut io| {
            let handles: Vec<OutBus> = shape.output_handles().collect();
            let (_, mut outputs) = io.split();
            for (k, ch) in outputs
                .buses(&handles)
                .flat_map(BusOutput::into_channels)
                .enumerate()
            {
                ch.fill(k as f32 + 10.0);
            }
        });
        let first: Vec<f32> = outs.iter().map(|o| o[0]).collect();
        assert_eq!(first, [10.0, 11.0, 12.0]);
    }

    /// Two views of one port cannot be held at once: handles out of port
    /// order are refused.
    #[test]
    #[should_panic(expected = "in port order")]
    fn output_buses_out_of_order_are_refused() {
        with_io(|shape, mut io| {
            let handles = [
                shape.spare_out().expect("a port past the buses"),
                shape.output_bus(0).expect("the main bus"),
            ];
            let (_, mut outputs) = io.split();
            for _ in outputs.buses(&handles) {}
        });
    }
}
